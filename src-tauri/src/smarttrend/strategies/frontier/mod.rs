//! F3 — Frontier Absolute: three pure setups with fail-closed context.
//!
//! Gate ownership lives here. The generic tournament stack (anti_chase,
//! tod_gate, fsm_*, ofi_gate, rvol_gate) does NOT apply. Rails only
//! (breaker / hospital / capital / PDT / flat 15:55) run at dispatch.
//!
//! Numeric thresholds are calibration intervals (F4), not final constants.

mod open_sweep;
mod premarket_ignition;
mod tape_ignition;
mod true_resume;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::market::micro::MicroSnapshot;
use crate::state::{Direction, Position};

use super::{pure_atr, risk_based_size, EntrySignal, ExitSignal, Strategy, StrategyId, TradingWindow};
use titan_types::Candle;

pub use open_sweep::eval_open_sweep_full;
pub use premarket_ignition::eval_premarket_ignition_full;
pub use tape_ignition::eval_tape_ignition_full;
pub use true_resume::eval_true_resume_full;

/// Named Frontier setup (journal / marker / shadow bucket).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontierSetup {
    TapeIgnition,
    TrueResume,
    OpenSweep,
    PremarketIgnition,
}

impl FrontierSetup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TapeIgnition => "tape_ignition",
            Self::TrueResume => "true_resume",
            Self::OpenSweep => "open_sweep",
            Self::PremarketIgnition => "premarket_ignition",
        }
    }
}

/// Fail-closed snapshot for Frontier evaluation. Builder returns `None`
/// when micro / halt / price data is missing — never synthesises
/// `spread=0` / `is_halted=false`.
#[derive(Debug, Clone)]
pub struct FrontierContext {
    pub symbol: String,
    pub price: f64,
    pub atr: f64,
    pub candles_1m: Vec<Candle>,
    pub micro: MicroSnapshot,
    pub now_et_min: u32,
    pub now_utc: DateTime<Utc>,
    /// Seconds since MicroBook warmup started for this symbol (promotion).
    pub micro_age_secs: u64,
    /// Optional resume state (TRUE_RESUME). Absent ⇒ setup skips.
    pub resume: Option<ResumeSnapshot>,
    /// Prior close for gap / open-sweep.
    pub prev_close: f64,
    /// Session open (RTH) when known.
    pub session_open: f64,
    /// Baseline prints/sec from seeded 30m history (0 = unavailable).
    pub baseline_pps_5m: f64,
    /// Setup names already entered today on this symbol (once-per-day
    /// guards, keyed by `FrontierSetup::as_str`).
    pub entered_today: std::collections::HashSet<&'static str>,
    /// Catalyst: fresh news headline on this symbol (watchlist metrics
    /// B6; `false` when the symbol is not on the watchlist).
    pub catalyst_news: bool,
    /// Catalyst: scanner gap vs prior daily close, percent (0 when
    /// unknown). Distinct from the intraday open-sweep gap.
    pub catalyst_gap_pct: f64,
}

#[derive(Debug, Clone)]
pub struct ResumeSnapshot {
    pub resumed_at: DateTime<Utc>,
    pub halt_code: Option<String>,
    pub pre_halt_price: Option<f64>,
    pub halted_since: DateTime<Utc>,
    pub first_prints: Vec<(f64, f64, bool)>, // price, size, is_buy
}

/// One condition in the F4 condition vector.
#[derive(Debug, Clone, Serialize)]
pub struct ConditionHit {
    pub name: &'static str,
    pub passed: bool,
    pub value: f64,
    pub threshold: f64,
}

#[derive(Debug, Clone)]
pub struct FrontierCandidate {
    pub setup: FrontierSetup,
    pub signal: EntrySignal,
    pub conditions: Vec<ConditionHit>,
    pub conviction: f64,
}

/// Per-setup evaluation result — the condition vector is present even
/// when the setup fails, so F4 can compute per-condition kill-rates.
/// An empty vector means the setup was not applicable (e.g. no resume
/// snapshot), which is different from "killed by a condition".
#[derive(Debug, Clone)]
pub struct SetupEval {
    pub setup: FrontierSetup,
    pub conditions: Vec<ConditionHit>,
    pub candidate: Option<FrontierCandidate>,
}

/// Calibratable thresholds (F4 tunes these; defaults are mid-interval).
#[derive(Debug, Clone)]
pub struct FrontierThresholds {
    pub tape_min_pps_15s: f64,
    pub tape_min_signed_notional_15s: f64,
    pub tape_max_spread_bps: f64,
    pub tape_min_imbalance: f64,
    pub tape_baseline_mult: f64,
    pub tape_min_micro_age_secs: u64,
    pub resume_max_age_secs: u64,
    pub resume_max_spread_bps: f64,
    pub resume_min_print_notional: f64,
    pub sweep_window_start_et: u32,
    pub sweep_window_end_et: u32,
    pub sweep_min_gap_pct: f64,
    pub sweep_max_spread_bps: f64,
    pub pm_window_start_et: u32,
    pub pm_window_end_et: u32,
    pub pm_min_gap_pct: f64,
    pub pm_min_pps: f64,
    pub pm_min_notional: f64,
    pub pm_max_spread_bps: f64,
    pub stop_atr_mult_lo: f64,
    pub stop_atr_mult_hi: f64,
    pub target_r: f64,
    pub risk_dollars: f64,
}

impl Default for FrontierThresholds {
    fn default() -> Self {
        Self {
            tape_min_pps_15s: 2.0,
            tape_min_signed_notional_15s: 25_000.0,
            tape_max_spread_bps: 40.0,
            tape_min_imbalance: 0.25,
            tape_baseline_mult: 2.5,
            tape_min_micro_age_secs: 180,
            resume_max_age_secs: 90,
            resume_max_spread_bps: 100.0,
            resume_min_print_notional: 5_000.0,
            sweep_window_start_et: 9 * 60 + 35,
            sweep_window_end_et: 11 * 60 + 15,
            sweep_min_gap_pct: 3.0,
            sweep_max_spread_bps: 35.0,
            pm_window_start_et: 7 * 60,
            pm_window_end_et: 9 * 60 + 28,
            pm_min_gap_pct: 5.0,
            pm_min_pps: 1.5,
            pm_min_notional: 15_000.0,
            pm_max_spread_bps: 90.0,
            stop_atr_mult_lo: 0.75,
            stop_atr_mult_hi: 1.75,
            target_r: 2.5,
            risk_dollars: 40.0,
        }
    }
}

/// F4 — calibration intervals for the operator-overridable thresholds:
/// `(override key, lo, hi)`. Overrides are clamped into these bands at
/// read time. Risk / rails fields (`stop_atr_*`, `target_r`,
/// `risk_dollars`, sweep window) are deliberately NOT overridable —
/// calibration tunes the candidate rate, never the risk envelope.
pub const CALIBRATION_BOUNDS: &[(&str, f64, f64)] = &[
    ("tape_min_pps_15s", 1.0, 6.0),
    ("tape_min_signed_notional_15s", 10_000.0, 100_000.0),
    ("tape_max_spread_bps", 15.0, 80.0),
    ("tape_min_imbalance", 0.10, 0.60),
    ("tape_baseline_mult", 1.5, 5.0),
    ("tape_min_micro_age_secs", 60.0, 600.0),
    ("resume_max_age_secs", 30.0, 300.0),
    ("resume_max_spread_bps", 40.0, 200.0),
    ("resume_min_print_notional", 1_000.0, 50_000.0),
    ("sweep_min_gap_pct", 1.5, 8.0),
    ("sweep_max_spread_bps", 15.0, 80.0),
    ("pm_min_gap_pct", 2.0, 15.0),
    ("pm_min_pps", 0.5, 5.0),
    ("pm_min_notional", 5_000.0, 100_000.0),
    ("pm_max_spread_bps", 40.0, 200.0),
];

impl FrontierThresholds {
    /// Defaults + operator overrides from
    /// `evolution_protocol.frontier_overrides`, clamped into
    /// [`CALIBRATION_BOUNDS`]. Unknown keys / non-finite values ignored.
    pub fn with_overrides(overrides: &std::collections::HashMap<String, f64>) -> Self {
        let mut th = Self::default();
        for (key, lo, hi) in CALIBRATION_BOUNDS {
            let Some(v) = overrides.get(*key).copied().filter(|v| v.is_finite()) else {
                continue;
            };
            let v = v.clamp(*lo, *hi);
            match *key {
                "tape_min_pps_15s" => th.tape_min_pps_15s = v,
                "tape_min_signed_notional_15s" => th.tape_min_signed_notional_15s = v,
                "tape_max_spread_bps" => th.tape_max_spread_bps = v,
                "tape_min_imbalance" => th.tape_min_imbalance = v,
                "tape_baseline_mult" => th.tape_baseline_mult = v,
                "tape_min_micro_age_secs" => th.tape_min_micro_age_secs = v.round() as u64,
                "resume_max_age_secs" => th.resume_max_age_secs = v.round() as u64,
                "resume_max_spread_bps" => th.resume_max_spread_bps = v,
                "resume_min_print_notional" => th.resume_min_print_notional = v,
                "sweep_min_gap_pct" => th.sweep_min_gap_pct = v,
                "sweep_max_spread_bps" => th.sweep_max_spread_bps = v,
                "pm_min_gap_pct" => th.pm_min_gap_pct = v,
                "pm_min_pps" => th.pm_min_pps = v,
                "pm_min_notional" => th.pm_min_notional = v,
                "pm_max_spread_bps" => th.pm_max_spread_bps = v,
                _ => {}
            }
        }
        th
    }

    /// Current value of an overridable threshold by key (calibration
    /// engine reads these to build proposals against live values).
    pub fn get(&self, key: &str) -> Option<f64> {
        Some(match key {
            "tape_min_pps_15s" => self.tape_min_pps_15s,
            "tape_min_signed_notional_15s" => self.tape_min_signed_notional_15s,
            "tape_max_spread_bps" => self.tape_max_spread_bps,
            "tape_min_imbalance" => self.tape_min_imbalance,
            "tape_baseline_mult" => self.tape_baseline_mult,
            "tape_min_micro_age_secs" => self.tape_min_micro_age_secs as f64,
            "resume_max_age_secs" => self.resume_max_age_secs as f64,
            "resume_max_spread_bps" => self.resume_max_spread_bps,
            "resume_min_print_notional" => self.resume_min_print_notional,
            "sweep_min_gap_pct" => self.sweep_min_gap_pct,
            "sweep_max_spread_bps" => self.sweep_max_spread_bps,
            "pm_min_gap_pct" => self.pm_min_gap_pct,
            "pm_min_pps" => self.pm_min_pps,
            "pm_min_notional" => self.pm_min_notional,
            "pm_max_spread_bps" => self.pm_max_spread_bps,
            _ => return None,
        })
    }
}

/// Clamp structural stop distance into `[lo, hi] × ATR`.
pub(crate) fn clamp_stop_dist(raw: f64, atr: f64, lo: f64, hi: f64) -> f64 {
    if atr <= 0.0 {
        return raw.max(0.01);
    }
    raw.clamp(lo * atr, hi * atr)
}

pub(crate) fn bracket(
    side: Direction,
    price: f64,
    stop_dist: f64,
    target_r: f64,
) -> (f64, f64) {
    let target_dist = stop_dist * target_r;
    match side {
        Direction::Long => (price - stop_dist, price + target_dist),
        Direction::Short => (price + stop_dist, price - target_dist),
    }
}

/// Evaluate all setups with full condition vectors (F4). Empty when
/// the fail-closed guards reject the context outright.
pub fn evaluate_frontier_verbose(
    ctx: &FrontierContext,
    th: &FrontierThresholds,
) -> Vec<SetupEval> {
    if !ctx.price.is_finite() || ctx.price <= 0.0 {
        return Vec::new();
    }
    if !ctx.micro.fresh || ctx.micro.spread_bps <= 0.0 {
        return Vec::new();
    }
    vec![
        eval_tape_ignition_full(ctx, th),
        eval_true_resume_full(ctx, th),
        eval_open_sweep_full(ctx, th),
        eval_premarket_ignition_full(ctx, th),
    ]
}

/// Evaluate all setups; at most one candidate (highest conviction).
pub fn evaluate_frontier(
    ctx: &FrontierContext,
    th: &FrontierThresholds,
) -> Option<FrontierCandidate> {
    evaluate_frontier_verbose(ctx, th)
        .into_iter()
        .filter_map(|e| e.candidate)
        .max_by(|a, b| {
            a.conviction
                .partial_cmp(&b.conviction)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Strategy trait shim — live evaluation is owned by `frontier_loop`.
/// `evaluate_entry` always returns `None` so the tournament stack never
/// double-fires Frontier through the generic gate path.
#[derive(Debug, Clone, Default)]
pub struct Frontier;

impl Strategy for Frontier {
    fn id(&self) -> StrategyId {
        StrategyId::Frontier
    }

    fn allowed_window(&self) -> TradingWindow {
        // PREMARKET_IGNITION needs the extended session; each setup
        // gates its own ET window in its condition vector, so this
        // does not widen tape/resume/sweep (their conditions bind).
        TradingWindow::ExtendedAndRth
    }

    fn evaluate_entry(&self, _ctx: &super::MarketContext) -> Option<EntrySignal> {
        None
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &super::MarketContext) -> Option<ExitSignal> {
        // Flat 15:55 ET hard rail (also enforced in bot).
        if ctx.now_et_min >= 15 * 60 + 55 {
            return Some(ExitSignal {
                strategy: StrategyId::Frontier,
                limit_price: None,
                reason: "frontier_flat_1555",
            });
        }
        if let Some(opened_at) = pos.opened_at {
            let held_min = (chrono::Utc::now().timestamp() - opened_at).max(0) / 60;
            // Default tape time-stop; resume uses shorter via max_hold on entry.
            if held_min as u32 >= 45 {
                return Some(ExitSignal {
                    strategy: StrategyId::Frontier,
                    limit_price: None,
                    reason: "frontier_time_stop",
                });
            }
        }
        None
    }

    fn position_size(&self, ctx: &super::MarketContext, sub_wallet: f64) -> f64 {
        if !ctx.atr.is_finite() || ctx.price <= 0.0 || !ctx.price.is_finite() {
            return 0.0;
        }
        let atr = if ctx.atr > 0.0 {
            ctx.atr
        } else {
            let a = pure_atr(&ctx.candles, 14);
            if !a.is_finite() || a <= 0.0 {
                return 0.0;
            }
            a
        };
        let stop = ctx.price - atr.max(ctx.price * 0.005);
        // Premarket fills carry spread + halt risk RTH does not — run
        // half risk before 9:30 ET (PREMARKET_IGNITION entries).
        let risk = if ctx.now_et_min < 9 * 60 + 30 { 20.0 } else { 40.0 };
        risk_based_size(risk, ctx.price, stop, 0.25, sub_wallet)
    }

    fn max_spread_bps(&self) -> f64 {
        100.0
    }
}

#[cfg(test)]
mod tests;
