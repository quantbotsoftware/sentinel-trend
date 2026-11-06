//! PRIME F2c — Unified exit manager (pure).
//!
//! Corrects the empirically-measured asymmetry (avg win $4.20 vs avg
//! loss $9.20): losers were held to full stops while winners were cut
//! at the first wiggle. The policy here is the classic asymmetric
//! trade-management ladder:
//!
//! 1. **Hard stop** at −1R — never widen, never average down.
//! 2. **Breakeven** — once the trade reaches +1R, the stop moves to
//!    entry so an open winner can no longer become a loser.
//! 3. **ATR trail** — once the trade reaches +1.5R, trail the stop
//!    `trail_atr_mult × ATR` behind the high-water mark so winners run
//!    to +2R/+3R instead of being capped.
//! 4. **Time stop** — flat exits after `max_hold_minutes` when the
//!    trade never resolved (capital recycling).
//!
//! Everything is a pure function of a [`PositionView`] snapshot — no
//! locks, no Tauri, no broker calls (R4). The caller (bot exit loop)
//! owns wiring verdicts into `BotAction`s.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ExitPolicy {
    /// R-multiple at which the stop jumps to breakeven.
    pub breakeven_at_r: f64,
    /// R-multiple at which the ATR trail activates.
    pub trail_start_r: f64,
    /// Trail distance in ATR multiples behind the high-water mark.
    pub trail_atr_mult: f64,
    /// Flat time-stop in minutes (0 disables the ladder's time stop;
    /// per-signal `max_hold_minutes` still applies upstream).
    pub max_hold_minutes: u32,
    /// P0 Asymmetry Protocol — minimum R-multiple above which an
    /// FSM/strategy *signal* exit is deferred instead of executed
    /// (the ladder takes over with an armed breakeven stop). Below
    /// this the signal exit passes through untouched, preserving the
    /// FSM's fast loss-cutting.
    pub defer_min_r: f64,
}

impl Default for ExitPolicy {
    fn default() -> Self {
        Self {
            breakeven_at_r: 1.0,
            trail_start_r: 1.5,
            trail_atr_mult: 1.5,
            max_hold_minutes: 240,
            defer_min_r: 0.15,
        }
    }
}

/// Long/short agnostic snapshot of one open position. `initial_risk`
/// is |entry − initial_stop| (the R unit). `favorable_extreme` is the
/// high-water mark for longs / low-water mark for shorts.
#[derive(Debug, Clone, Copy)]
pub struct PositionView {
    pub is_long: bool,
    pub entry: f64,
    pub current_price: f64,
    pub current_stop: f64,
    pub initial_risk: f64,
    pub favorable_extreme: f64,
    pub atr: f64,
    pub held_minutes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitVerdict {
    Hold,
    /// Immediate flat exit with a stable reason label (journal + marker).
    Exit { reason: &'static str },
    /// Ratchet the protective stop (never loosens).
    MoveStop { new_stop: f64, reason: &'static str },
}

/// Signed R-multiple of the current price (positive = in profit).
pub fn r_multiple(view: &PositionView) -> f64 {
    if view.initial_risk <= 1e-9 {
        return 0.0;
    }
    let move_pts = if view.is_long {
        view.current_price - view.entry
    } else {
        view.entry - view.current_price
    };
    move_pts / view.initial_risk
}

/// Evaluate the asymmetric exit ladder. Pure; call once per bar per
/// open PRIME position.
pub fn evaluate(policy: &ExitPolicy, view: &PositionView) -> ExitVerdict {
    // 4) Time stop — capital recycling for unresolved trades.
    if policy.max_hold_minutes > 0 && view.held_minutes >= policy.max_hold_minutes {
        return ExitVerdict::Exit {
            reason: "prime_time_stop",
        };
    }

    // 1) Hard stop — price at/through the protective stop.
    let stop_hit = if view.is_long {
        view.current_price <= view.current_stop
    } else {
        view.current_price >= view.current_stop
    };
    if stop_hit && view.current_stop > 0.0 {
        return ExitVerdict::Exit {
            reason: "prime_hard_stop",
        };
    }

    let r = r_multiple(view);

    // 3) ATR trail after +trail_start_r (checked before breakeven so
    // the tighter of the two wins once both are active).
    if r >= policy.trail_start_r && view.atr > 0.0 {
        let trail = policy.trail_atr_mult * view.atr;
        let candidate = if view.is_long {
            view.favorable_extreme - trail
        } else {
            view.favorable_extreme + trail
        };
        let improves = if view.is_long {
            candidate > view.current_stop
        } else {
            candidate < view.current_stop || view.current_stop <= 0.0
        };
        // Trail never retreats below breakeven once earned.
        let candidate = if view.is_long {
            candidate.max(view.entry)
        } else {
            candidate.min(view.entry)
        };
        if improves {
            return ExitVerdict::MoveStop {
                new_stop: candidate,
                reason: "prime_atr_trail",
            };
        }
        return ExitVerdict::Hold;
    }

    // 2) Breakeven and Secured Breakeven Step
    let target_secure_r = policy.breakeven_at_r + 0.35;
    let secure_pnl = 0.25 * view.initial_risk;
    if r >= target_secure_r {
        let secure_stop = if view.is_long {
            view.entry + secure_pnl
        } else {
            view.entry - secure_pnl
        };
        let improves = if view.is_long {
            secure_stop > view.current_stop
        } else {
            secure_stop < view.current_stop || view.current_stop <= 0.0
        };
        if improves {
            return ExitVerdict::MoveStop {
                new_stop: secure_stop,
                reason: "prime_secure_breakeven",
            };
        }
    }

    if r >= policy.breakeven_at_r {
        let improves = if view.is_long {
            view.entry > view.current_stop
        } else {
            view.entry < view.current_stop || view.current_stop <= 0.0
        };
        if improves {
            return ExitVerdict::MoveStop {
                new_stop: view.entry,
                reason: "prime_breakeven",
            };
        }
    }

    ExitVerdict::Hold
}

/// P0 Asymmetry Protocol — verdict for an incoming FSM/strategy
/// *signal* exit against an open position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalExitVerdict {
    /// Let the signal exit through (loser or near-flat — cut fast).
    Execute,
    /// Suppress the signal exit; the PRIME ladder (with an armed
    /// breakeven stop) manages the winner from here.
    Defer { r: f64 },
}

/// Decide whether a signal exit executes or defers. Pure.
///
/// Empirical basis (2026-07-03 audit, 58 closed trades): signal exits
/// produced 38 losers (−$100.37) vs 16 winners (+$17.47) with the
/// average win capped at +$1.09 — the FSM's exit-on-first-red-bar
/// closes winners into noise. The rule keeps the FSM's genuinely good
/// half (fast loss-cutting) and removes the bad half:
///
/// * position at/below `defer_min_r` (default +0.15R) → **Execute**;
/// * position above it → **Defer** — the caller arms an immediate
///   breakeven floor on the ladder so the deferred winner can no
///   longer become a loser, and the trail lets it run to +2R/+3R.
///
/// Safe defaults: any degenerate input (no bracket stop — e.g. a
/// recon-materialised position with `stop_loss == 0` — bad prices,
/// zero risk unit) → **Execute**, i.e. the pre-P0 behaviour.
pub fn arbitrate_signal_exit(
    policy: &ExitPolicy,
    is_long: bool,
    entry: f64,
    bracket_stop: f64,
    price: f64,
) -> SignalExitVerdict {
    if entry <= 0.0
        || bracket_stop <= 0.0
        || !price.is_finite()
        || price <= 0.0
        || !entry.is_finite()
        || !bracket_stop.is_finite()
    {
        return SignalExitVerdict::Execute;
    }
    let risk = (entry - bracket_stop).abs();
    if risk <= entry * 1e-4 {
        return SignalExitVerdict::Execute;
    }
    let signed_move = if is_long { price - entry } else { entry - price };
    let r = signed_move / risk;
    if r > policy.defer_min_r {
        SignalExitVerdict::Defer { r }
    } else {
        SignalExitVerdict::Execute
    }
}

/// Stateless effective stop of the ladder, recomputable every tick
/// from `(entry, bracket_stop, favorable_extreme)` — no per-position
/// mutable state needed. Used by the live exit rail where the broker
/// bracket stop cannot be moved: the rail exits at market when price
/// crosses the *ladder* stop while the ladder is tighter than the
/// bracket.
///
/// * peak below `breakeven_at_r` → bracket stop unchanged.
/// * peak ≥ `breakeven_at_r`     → at least breakeven.
/// * peak ≥ `trail_start_r`      → trail `trail_atr_mult × atr` behind
///   the favorable extreme (never looser than breakeven).
pub fn ladder_stop(
    policy: &ExitPolicy,
    is_long: bool,
    entry: f64,
    bracket_stop: f64,
    favorable_extreme: f64,
    atr: f64,
) -> f64 {
    let initial_risk = (entry - bracket_stop).abs();
    if initial_risk <= 1e-9 || entry <= 0.0 {
        return bracket_stop;
    }
    let peak_r = if is_long {
        (favorable_extreme - entry) / initial_risk
    } else {
        (entry - favorable_extreme) / initial_risk
    };
    let mut stop = bracket_stop;
    if peak_r >= policy.breakeven_at_r {
        let target_secure_r = policy.breakeven_at_r + 0.35;
        let secure_pnl = 0.25 * initial_risk;
        let be_stop = if peak_r >= target_secure_r {
            if is_long { entry + secure_pnl } else { entry - secure_pnl }
        } else {
            entry
        };
        stop = if is_long { stop.max(be_stop) } else { stop.min(be_stop) };
    }
    if peak_r >= policy.trail_start_r && atr > 0.0 {
        let trail = policy.trail_atr_mult * atr;
        let cand = if is_long {
            (favorable_extreme - trail).max(entry)
        } else {
            (favorable_extreme + trail).min(entry)
        };
        stop = if is_long { stop.max(cand) } else { stop.min(cand) };
    }
    stop
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn long_view() -> PositionView {
        PositionView {
            is_long: true,
            entry: 100.0,
            current_price: 100.0,
            current_stop: 98.0,
            initial_risk: 2.0,
            favorable_extreme: 100.0,
            atr: 1.0,
            held_minutes: 10,
        }
    }

    #[test]
    fn hold_when_nothing_earned() {
        let p = ExitPolicy::default();
        assert_eq!(evaluate(&p, &long_view()), ExitVerdict::Hold);
    }

    #[test]
    fn hard_stop_fires_long_and_short() {
        let p = ExitPolicy::default();
        let mut v = long_view();
        v.current_price = 97.9;
        assert_eq!(
            evaluate(&p, &v),
            ExitVerdict::Exit {
                reason: "prime_hard_stop"
            }
        );

        let short = PositionView {
            is_long: false,
            entry: 100.0,
            current_price: 102.1,
            current_stop: 102.0,
            initial_risk: 2.0,
            favorable_extreme: 100.0,
            atr: 1.0,
            held_minutes: 10,
        };
        assert_eq!(
            evaluate(&p, &short),
            ExitVerdict::Exit {
                reason: "prime_hard_stop"
            }
        );
    }

    #[test]
    fn breakeven_after_one_r() {
        let p = ExitPolicy::default();
        let mut v = long_view();
        v.current_price = 102.0; // +1R
        v.favorable_extreme = 102.0;
        match evaluate(&p, &v) {
            ExitVerdict::MoveStop { new_stop, reason } => {
                assert_eq!(reason, "prime_breakeven");
                assert!((new_stop - v.entry).abs() < 1e-9);
            }
            other => panic!("expected breakeven, got {other:?}"),
        }
    }

    #[test]
    fn secured_breakeven_after_one_point_thirty_five_r() {
        let p = ExitPolicy::default();
        let mut v = long_view();
        v.current_price = 102.7; // +1.35R
        v.favorable_extreme = 102.7;
        match evaluate(&p, &v) {
            ExitVerdict::MoveStop { new_stop, reason } => {
                assert_eq!(reason, "prime_secure_breakeven");
                // 100 + 0.25 * 2.0 = 100.5
                assert!((new_stop - 100.5).abs() < 1e-9);
            }
            other => panic!("expected secured breakeven, got {other:?}"),
        }
    }

    #[test]
    fn trail_after_one_point_five_r() {
        let p = ExitPolicy::default();
        let mut v = long_view();
        v.current_price = 104.0; // +2R
        v.favorable_extreme = 104.5;
        v.current_stop = 100.0; // already breakeven
        match evaluate(&p, &v) {
            ExitVerdict::MoveStop { new_stop, reason } => {
                assert_eq!(reason, "prime_atr_trail");
                // 104.5 - 1.5*1.0 = 103.0
                assert!((new_stop - 103.0).abs() < 1e-9);
            }
            other => panic!("expected trail, got {other:?}"),
        }
    }

    #[test]
    fn trail_never_loosens() {
        let p = ExitPolicy::default();
        let mut v = long_view();
        v.current_price = 103.2; // +1.6R — trail active
        v.favorable_extreme = 103.2;
        v.current_stop = 102.5; // already tighter than 103.2-1.5=101.7
        assert_eq!(evaluate(&p, &v), ExitVerdict::Hold);
    }

    #[test]
    fn short_trail_mirrors() {
        let p = ExitPolicy::default();
        let v = PositionView {
            is_long: false,
            entry: 100.0,
            current_price: 96.0, // +2R short
            current_stop: 100.0,
            initial_risk: 2.0,
            favorable_extreme: 95.5,
            atr: 1.0,
            held_minutes: 10,
        };
        match evaluate(&p, &v) {
            ExitVerdict::MoveStop { new_stop, reason } => {
                assert_eq!(reason, "prime_atr_trail");
                // 95.5 + 1.5 = 97.0
                assert!((new_stop - 97.0).abs() < 1e-9);
            }
            other => panic!("expected short trail, got {other:?}"),
        }
    }

    #[test]
    fn time_stop_fires() {
        let p = ExitPolicy {
            max_hold_minutes: 60,
            ..Default::default()
        };
        let mut v = long_view();
        v.held_minutes = 61;
        assert_eq!(
            evaluate(&p, &v),
            ExitVerdict::Exit {
                reason: "prime_time_stop"
            }
        );
    }

    #[test]
    fn ladder_stop_progression_long() {
        let p = ExitPolicy::default();
        // Peak never reached +1R → bracket stop untouched.
        assert_eq!(ladder_stop(&p, true, 100.0, 98.0, 101.0, 1.0), 98.0);
        // Peak +1R → breakeven (100.0).
        assert_eq!(ladder_stop(&p, true, 100.0, 98.0, 102.0, 1.0), 100.0);
        // Peak +1.35R (102.7) → secured breakeven (100.5).
        assert_eq!(ladder_stop(&p, true, 100.0, 98.0, 102.7, 1.0), 100.5);
        // Peak +2.25R (104.5) → trail 1.5×1.0 behind: 103.0.
        assert!((ladder_stop(&p, true, 100.0, 98.0, 104.5, 1.0) - 103.0).abs() < 1e-9);
    }

    #[test]
    fn ladder_stop_progression_short() {
        let p = ExitPolicy::default();
        // Peak never reached +1R → bracket stop untouched.
        assert_eq!(ladder_stop(&p, false, 100.0, 102.0, 99.5, 1.0), 102.0);
        // Peak +1R → breakeven (100.0).
        assert_eq!(ladder_stop(&p, false, 100.0, 102.0, 98.0, 1.0), 100.0);
        // Peak +1.35R (97.3) → secured breakeven (99.5).
        assert_eq!(ladder_stop(&p, false, 100.0, 102.0, 97.3, 1.0), 99.5);
        // Peak +2.25R (95.5) → trail 1.5×1.0 behind: 97.0.
        assert!((ladder_stop(&p, false, 100.0, 102.0, 95.5, 1.0) - 97.0).abs() < 1e-9);
    }

    #[test]
    fn ladder_stop_unknown_bracket_is_passthrough() {
        let p = ExitPolicy::default();
        // bracket_stop == 0 → initial risk equals entry, peak_r tiny —
        // ladder must not fabricate a stop.
        assert_eq!(ladder_stop(&p, true, 100.0, 0.0, 110.0, 1.0), 0.0);
    }

    #[test]
    fn signal_exit_executes_below_threshold() {
        let p = ExitPolicy::default();
        // Loser: -0.5R → cut fast.
        assert_eq!(
            arbitrate_signal_exit(&p, true, 100.0, 98.0, 99.0),
            SignalExitVerdict::Execute
        );
        // Essentially flat (+0.1R < 0.15R default) → execute.
        assert_eq!(
            arbitrate_signal_exit(&p, true, 100.0, 98.0, 100.2),
            SignalExitVerdict::Execute
        );
    }

    #[test]
    fn signal_exit_defers_above_threshold() {
        let p = ExitPolicy::default();
        // +0.5R winner → defer, ladder takes over.
        match arbitrate_signal_exit(&p, true, 100.0, 98.0, 101.0) {
            SignalExitVerdict::Defer { r } => assert!((r - 0.5).abs() < 1e-9),
            other => panic!("expected defer, got {other:?}"),
        }
        // Short mirror: entry 100, stop 102, price 99 → +0.5R.
        match arbitrate_signal_exit(&p, false, 100.0, 102.0, 99.0) {
            SignalExitVerdict::Defer { r } => assert!((r - 0.5).abs() < 1e-9),
            other => panic!("expected short defer, got {other:?}"),
        }
    }

    #[test]
    fn signal_exit_degenerate_inputs_execute() {
        let p = ExitPolicy::default();
        // No bracket stop (recon-materialised position) → pre-P0 path.
        assert_eq!(
            arbitrate_signal_exit(&p, true, 100.0, 0.0, 105.0),
            SignalExitVerdict::Execute
        );
        // Zero-distance stop → no R unit.
        assert_eq!(
            arbitrate_signal_exit(&p, true, 100.0, 100.0, 105.0),
            SignalExitVerdict::Execute
        );
        assert_eq!(
            arbitrate_signal_exit(&p, true, 100.0, 98.0, f64::NAN),
            SignalExitVerdict::Execute
        );
    }

    #[test]
    fn zero_risk_never_panics() {
        let p = ExitPolicy::default();
        let mut v = long_view();
        v.initial_risk = 0.0;
        v.current_price = 150.0;
        assert_eq!(r_multiple(&v), 0.0);
        let _ = evaluate(&p, &v);
    }
}
