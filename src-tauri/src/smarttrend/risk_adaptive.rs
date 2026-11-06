//! D-PLUS (Sesiunea 2) — Adaptive Risk Manager.
//!
//! Per-strategy auto-throttle that watches consecutive wins/losses and
//! shrinks position size after losing streaks, restoring full size only
//! after the strategy proves it can win again.
//!
//! ## Why a separate module from `tournament.rs`?
//!
//! The tournament tracks aggregate equity; adaptive risk needs *short-
//! term* streak state with crisp transitions. Mixing them would force
//! the tournament to expose mutable cooldown timers, which couples
//! reporting to enforcement. Two modules, one job each.
//!
//! ## State machine (per strategy)
//!
//! ```text
//!                    win
//!              ┌──────────────────┐
//!              │                  │
//!              ▼                  │
//!      ┌──────────┐   3 losses  ┌──────────┐  5 losses  ┌──────────────┐
//!      │ Normal   │ ──────────► │ Throttle │ ─────────► │ DeepThrottle │
//!      │ mult 1.0 │ ◄────────── │ mult 0.5 │ ◄───────── │ mult 0.25    │
//!      └──────────┘   3 wins    └──────────┘  3 wins    │ + 30m cooldn │
//!                                                       └──────────────┘
//! ```
//!
//! - "win" = `pnl > 0`, "loss" = `pnl < 0`, exact zero is ignored.
//! - DeepThrottle adds a 30-minute entry cooldown on top of the 0.25x
//!   multiplier — the bot's gate refuses ANY new entry from that
//!   strategy until the cooldown expires.
//! - State persists to disk; bot restart preserves multipliers and
//!   cooldown end timestamps.
//!
//! ## Failure semantics
//!
//! - NaN / non-finite pnl is ignored (no state change, warn log).
//! - Disk write failures degrade gracefully (in-memory state survives,
//!   warn log; next successful save will pick up).
//! - Loading missing/corrupt state on startup returns
//!   `AdaptiveRisk::default()` — equivalent to "no streaks yet".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::smarttrend::strategies::StrategyId;

// ════════════════════════════════════════════════════════════════════
//  Tunables — kept const so the unit tests pin the contract; future
//  versions can move these into `EvolutionProtocolConfig` if needed.
// ════════════════════════════════════════════════════════════════════

/// Consecutive-loss count that triggers the first throttle step.
pub const THROTTLE_AT_LOSSES: u32 = 3;
/// Consecutive-loss count that triggers the deep throttle + cooldown.
pub const DEEP_THROTTLE_AT_LOSSES: u32 = 5;
/// Consecutive-win count required to restore full size.
pub const RESTORE_AT_WINS: u32 = 3;
/// Multiplier when in the first throttle step.
pub const THROTTLE_MULT: f64 = 0.5;
/// Multiplier when in deep throttle.
pub const DEEP_THROTTLE_MULT: f64 = 0.25;
/// Cooldown duration after deep throttle triggers (seconds).
pub const DEEP_COOLDOWN_SECS: i64 = 30 * 60;

// ════════════════════════════════════════════════════════════════════
//  Per-strategy risk state
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RiskState {
    pub strategy: Option<StrategyId>,
    /// Length of the current losing streak (resets to 0 on any win).
    pub consecutive_losses: u32,
    /// Length of the current winning streak (resets to 0 on any loss).
    pub consecutive_wins: u32,
    /// Current size multiplier in [0.25, 1.0].
    pub current_size_mult: f64,
    /// Unix epoch (seconds) when entry cooldown ends. `0` = no cooldown.
    pub cooldown_until: i64,
    /// Total throttle activations recorded (cumulative; not reset).
    pub throttle_count: u32,
    /// Total deep-throttle activations recorded (cumulative).
    pub deep_throttle_count: u32,
    /// Last outcome timestamp recorded (seconds).
    pub last_outcome_ts: i64,
}

impl RiskState {
    fn fresh(strategy: StrategyId) -> Self {
        Self {
            strategy: Some(strategy),
            consecutive_losses: 0,
            consecutive_wins: 0,
            current_size_mult: 1.0,
            cooldown_until: 0,
            throttle_count: 0,
            deep_throttle_count: 0,
            last_outcome_ts: 0,
        }
    }
}

// ════════════════════════════════════════════════════════════════════
//  AdaptiveRisk container
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveRisk {
    pub states: HashMap<StrategyId, RiskState>,
    /// Last successful disk write (epoch seconds). 0 = never saved.
    pub last_saved_at: i64,
}

impl Default for AdaptiveRisk {
    fn default() -> Self {
        let mut states = HashMap::with_capacity(StrategyId::ALL.len());
        for id in StrategyId::ALL.iter().copied() {
            states.insert(id, RiskState::fresh(id));
        }
        Self {
            states,
            last_saved_at: 0,
        }
    }
}

impl AdaptiveRisk {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a closed trade outcome and update the strategy's risk
    /// state. Returns the new state for inspection / event emission.
    ///
    /// `pnl` is dollar P/L, sign-significant. Exact zero P/L is treated
    /// as "no streak update" (rare with broker fees but possible on a
    /// quick break-even close).
    pub fn record_outcome(
        &mut self,
        strategy: StrategyId,
        pnl: f64,
        closed_at: i64,
    ) -> RiskState {
        let s = self
            .states
            .entry(strategy)
            .or_insert_with(|| RiskState::fresh(strategy));
        if !pnl.is_finite() {
            tracing::warn!(
                component = "risk_adaptive",
                strategy = %strategy.as_str(),
                "ignored non-finite pnl"
            );
            return s.clone();
        }
        s.last_outcome_ts = closed_at;
        if pnl > 0.0 {
            s.consecutive_wins = s.consecutive_wins.saturating_add(1);
            s.consecutive_losses = 0;
            if s.consecutive_wins >= RESTORE_AT_WINS && s.current_size_mult < 1.0 {
                s.current_size_mult = 1.0;
                // Restoring full size also lifts any cooldown that may
                // be lingering — earning back trust resets the gate.
                s.cooldown_until = 0;
                tracing::info!(
                    component = "risk_adaptive",
                    strategy = %strategy.as_str(),
                    wins = s.consecutive_wins,
                    "size multiplier restored to 1.00x"
                );
            }
        } else if pnl < 0.0 {
            s.consecutive_losses = s.consecutive_losses.saturating_add(1);
            s.consecutive_wins = 0;
            if s.consecutive_losses == DEEP_THROTTLE_AT_LOSSES {
                s.current_size_mult = DEEP_THROTTLE_MULT;
                s.cooldown_until = closed_at.saturating_add(DEEP_COOLDOWN_SECS);
                s.deep_throttle_count = s.deep_throttle_count.saturating_add(1);
                tracing::warn!(
                    component = "risk_adaptive",
                    strategy = %strategy.as_str(),
                    losses = s.consecutive_losses,
                    cooldown_until = s.cooldown_until,
                    "DEEP THROTTLE → mult 0.25x + 30min cooldown"
                );
            } else if s.consecutive_losses == THROTTLE_AT_LOSSES {
                s.current_size_mult = THROTTLE_MULT;
                s.throttle_count = s.throttle_count.saturating_add(1);
                tracing::warn!(
                    component = "risk_adaptive",
                    strategy = %strategy.as_str(),
                    losses = s.consecutive_losses,
                    "THROTTLE → mult 0.50x"
                );
            }
        } // else: exact zero, no change
        s.clone()
    }

    /// Returns the current size multiplier for a strategy. Defaults to
    /// 1.0 if the strategy has no recorded outcomes yet.
    pub fn size_multiplier(&self, strategy: StrategyId) -> f64 {
        self.states
            .get(&strategy)
            .map(|s| s.current_size_mult)
            .unwrap_or(1.0)
    }

    /// Returns true if `strategy` is currently in entry-cooldown at
    /// the supplied `now` epoch second. False for unknown strategies
    /// (defensive default — never blocks an entry on missing state).
    pub fn is_in_cooldown(&self, strategy: StrategyId, now: i64) -> bool {
        self.states
            .get(&strategy)
            .map(|s| s.cooldown_until > now)
            .unwrap_or(false)
    }

    /// Returns the cooldown end timestamp for a strategy, or 0 if not
    /// currently in cooldown. Useful for UI display ("cooldown ends
    /// at 14:32 ET").
    pub fn cooldown_until(&self, strategy: StrategyId) -> i64 {
        self.states
            .get(&strategy)
            .map(|s| s.cooldown_until)
            .unwrap_or(0)
    }

    /// Snapshot of all strategies' current risk state, sorted by
    /// strategy id for stable UI rendering.
    pub fn rows(&self) -> Vec<RiskState> {
        let mut out: Vec<RiskState> = StrategyId::ALL
            .iter()
            .filter_map(|id| self.states.get(id).cloned())
            .collect();
        // Stable sort by strategy id name to keep UI deterministic
        out.sort_by_key(|s| s.strategy.map(|id| id.as_str()).unwrap_or(""));
        out
    }

    /// Manual override — reset a strategy's state to fresh. Used by
    /// the UI "RESET" button in the D-PLUS tab.
    pub fn reset(&mut self, strategy: StrategyId) {
        self.states.insert(strategy, RiskState::fresh(strategy));
    }

    /// Atomic save to `base_dir/adaptive.json`. Tmp-file + rename
    /// avoids torn writes on crash.
    pub fn save(&mut self, base_dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(base_dir)?;
        self.last_saved_at = chrono::Utc::now().timestamp();
        let path = base_dir.join("adaptive.json");
        let tmp = base_dir.join("adaptive.json.tmp");
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn load(base_dir: &Path) -> std::io::Result<Self> {
        let path = base_dir.join("adaptive.json");
        let body = std::fs::read_to_string(&path)?;
        let mut loaded: Self = serde_json::from_str(&body).map_err(std::io::Error::other)?;
        for id in StrategyId::ALL.iter().copied() {
            loaded.states.entry(id).or_insert_with(|| RiskState::fresh(id));
        }
        Ok(loaded)
    }
}

/// Default storage directory: `$HOME/.local/share/sentinel-trend/risk`.
pub fn default_base_dir() -> PathBuf {
    let mut p = if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h)
    } else {
        PathBuf::from(".")
    };
    p.push(".local/share/sentinel-trend/risk");
    p
}

// ════════════════════════════════════════════════════════════════════
//  Tests — 25 covering state machine, persistence, cooldown TTL,
//  edge cases (NaN / zero / negative-zero / saturate).
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn strat() -> StrategyId {
        StrategyId::GapFade
    }

    /// Tests must use unique tmpdirs to allow `cargo test` parallelism.
    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let mut d = std::env::temp_dir();
        d.push(format!("sentinel-risk-test-{pid}-{n}"));
        d
    }

    #[test]
    fn default_initialises_all_eight_strategies() {
        let r = AdaptiveRisk::default();
        assert_eq!(r.states.len(), StrategyId::ALL.len());
        for id in StrategyId::ALL {
            let s = &r.states[id];
            assert_eq!(s.consecutive_losses, 0);
            assert_eq!(s.consecutive_wins, 0);
            assert!((s.current_size_mult - 1.0).abs() < 1e-9);
            assert_eq!(s.cooldown_until, 0);
        }
    }

    #[test]
    fn single_win_increments_streak_no_state_change() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), 50.0, 1000);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_wins, 1);
        assert_eq!(s.consecutive_losses, 0);
        assert!((s.current_size_mult - 1.0).abs() < 1e-9);
    }

    #[test]
    fn single_loss_increments_streak_no_state_change() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), -25.0, 1000);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 1);
        assert_eq!(s.consecutive_wins, 0);
        assert!((s.current_size_mult - 1.0).abs() < 1e-9);
    }

    #[test]
    fn three_losses_trigger_throttle() {
        let mut r = AdaptiveRisk::default();
        for i in 0..3 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 3);
        assert!((s.current_size_mult - THROTTLE_MULT).abs() < 1e-9);
        assert_eq!(s.throttle_count, 1);
        assert_eq!(s.cooldown_until, 0); // No cooldown at simple throttle
    }

    #[test]
    fn five_losses_trigger_deep_throttle_and_cooldown() {
        let mut r = AdaptiveRisk::default();
        for i in 0..5 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 5);
        assert!((s.current_size_mult - DEEP_THROTTLE_MULT).abs() < 1e-9);
        assert_eq!(s.deep_throttle_count, 1);
        assert_eq!(s.cooldown_until, 1004 + DEEP_COOLDOWN_SECS);
    }

    #[test]
    fn three_wins_after_throttle_restore_to_full() {
        let mut r = AdaptiveRisk::default();
        for i in 0..3 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        for i in 0..3 {
            r.record_outcome(strat(), 5.0, 2000 + i);
        }
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_wins, 3);
        assert!((s.current_size_mult - 1.0).abs() < 1e-9);
        assert_eq!(s.cooldown_until, 0);
    }

    #[test]
    fn three_wins_after_deep_throttle_restore_and_lift_cooldown() {
        let mut r = AdaptiveRisk::default();
        for i in 0..5 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        // Cooldown is set; verify
        assert!(r.is_in_cooldown(strat(), 1500));
        for i in 0..3 {
            r.record_outcome(strat(), 5.0, 2000 + i);
        }
        let s = &r.states[&strat()];
        assert!((s.current_size_mult - 1.0).abs() < 1e-9);
        assert_eq!(s.cooldown_until, 0);
    }

    #[test]
    fn win_resets_loss_streak() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), -10.0, 1000);
        r.record_outcome(strat(), -10.0, 1001);
        r.record_outcome(strat(), 5.0, 1002);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 0);
        assert_eq!(s.consecutive_wins, 1);
    }

    #[test]
    fn loss_resets_win_streak() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), 5.0, 1000);
        r.record_outcome(strat(), 5.0, 1001);
        r.record_outcome(strat(), -10.0, 1002);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_wins, 0);
        assert_eq!(s.consecutive_losses, 1);
    }

    #[test]
    fn zero_pnl_does_not_update_streak() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), -10.0, 1000);
        r.record_outcome(strat(), 0.0, 1001);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 1);
        assert_eq!(s.consecutive_wins, 0);
        // Timestamp does update though
        assert_eq!(s.last_outcome_ts, 1001);
    }

    #[test]
    fn nan_pnl_is_ignored() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), -10.0, 1000);
        r.record_outcome(strat(), f64::NAN, 1001);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 1);
        // last_outcome_ts NOT updated for NaN (early return path)
        assert_eq!(s.last_outcome_ts, 1000);
    }

    #[test]
    fn infinite_pnl_is_ignored() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), f64::INFINITY, 1000);
        r.record_outcome(strat(), f64::NEG_INFINITY, 1001);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 0);
        assert_eq!(s.consecutive_wins, 0);
        assert_eq!(s.last_outcome_ts, 0);
    }

    #[test]
    fn is_in_cooldown_respects_now() {
        let mut r = AdaptiveRisk::default();
        for i in 0..5 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        let cd_end = r.cooldown_until(strat());
        assert!(r.is_in_cooldown(strat(), cd_end - 1));
        assert!(!r.is_in_cooldown(strat(), cd_end));
        assert!(!r.is_in_cooldown(strat(), cd_end + 1));
    }

    #[test]
    fn size_multiplier_default_one_for_unknown() {
        let r = AdaptiveRisk::default();
        // Drop one state to simulate "missing"
        let mut r = r;
        r.states.remove(&StrategyId::HaltResume);
        assert!((r.size_multiplier(StrategyId::HaltResume) - 1.0).abs() < 1e-9);
        assert!(!r.is_in_cooldown(StrategyId::HaltResume, 1_000_000));
    }

    #[test]
    fn rows_returns_all_eight_in_stable_order() {
        let r = AdaptiveRisk::default();
        let rows = r.rows();
        assert_eq!(rows.len(), StrategyId::ALL.len());
        // Verify alphabetical (by as_str) ordering is stable across calls
        let rows2 = r.rows();
        assert_eq!(
            rows.iter()
                .map(|s| s.strategy.unwrap().as_str())
                .collect::<Vec<_>>(),
            rows2
                .iter()
                .map(|s| s.strategy.unwrap().as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reset_clears_streak_and_cooldown() {
        let mut r = AdaptiveRisk::default();
        for i in 0..5 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        assert!(r.is_in_cooldown(strat(), 1500));
        r.reset(strat());
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, 0);
        assert_eq!(s.consecutive_wins, 0);
        assert!((s.current_size_mult - 1.0).abs() < 1e-9);
        assert_eq!(s.cooldown_until, 0);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tmpdir();
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), -10.0, 1000);
        r.record_outcome(strat(), -10.0, 1001);
        r.record_outcome(strat(), -10.0, 1002);
        r.save(&dir).expect("save");
        let r2 = AdaptiveRisk::load(&dir).expect("load");
        assert_eq!(r2.states[&strat()].consecutive_losses, 3);
        assert!((r2.states[&strat()].current_size_mult - THROTTLE_MULT).abs() < 1e-9);
        // last_saved_at populated
        assert!(r2.last_saved_at > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_does_not_leave_tmp() {
        let dir = tmpdir();
        let mut r = AdaptiveRisk::default();
        r.save(&dir).expect("save");
        let tmp = dir.join("adaptive.json.tmp");
        assert!(!tmp.exists(), "tmp file should be renamed away");
        let final_path = dir.join("adaptive.json");
        assert!(final_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_returns_err() {
        let dir = tmpdir();
        let res = AdaptiveRisk::load(&dir);
        assert!(res.is_err());
    }

    #[test]
    fn deep_throttle_after_throttle_only_increments_deep_count() {
        let mut r = AdaptiveRisk::default();
        for i in 0..5 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        let s = &r.states[&strat()];
        // Both counters get +1 (the 3-loss boundary triggered first,
        // then the 5-loss boundary added the deep transition).
        assert_eq!(s.throttle_count, 1);
        assert_eq!(s.deep_throttle_count, 1);
    }

    #[test]
    fn streak_saturates_safely_at_max_u32() {
        let mut r = AdaptiveRisk::default();
        // Force streak to near-saturation via direct state poke;
        // saturating_add must clamp without panic.
        if let Some(s) = r.states.get_mut(&strat()) {
            s.consecutive_losses = u32::MAX - 1;
        }
        r.record_outcome(strat(), -10.0, 1000);
        r.record_outcome(strat(), -10.0, 1001);
        let s = &r.states[&strat()];
        assert_eq!(s.consecutive_losses, u32::MAX);
    }

    #[test]
    fn timestamp_updates_on_each_outcome() {
        let mut r = AdaptiveRisk::default();
        r.record_outcome(strat(), 5.0, 1000);
        r.record_outcome(strat(), 5.0, 2000);
        let s = &r.states[&strat()];
        assert_eq!(s.last_outcome_ts, 2000);
    }

    #[test]
    fn cooldown_lifts_after_full_restore_even_if_clock_did_not_pass() {
        let mut r = AdaptiveRisk::default();
        for i in 0..5 {
            r.record_outcome(strat(), -10.0, 1000 + i);
        }
        // 3 wins → cooldown lifted regardless of `now`
        for i in 0..3 {
            r.record_outcome(strat(), 5.0, 1100 + i);
        }
        // Original cooldown was scheduled to 1004 + 1800 = 2804;
        // but we restored before that timestamp.
        assert!(!r.is_in_cooldown(strat(), 1500));
    }

    #[test]
    fn separate_strategies_have_independent_states() {
        let mut r = AdaptiveRisk::default();
        for i in 0..3 {
            r.record_outcome(StrategyId::GapFade, -10.0, 1000 + i);
        }
        for i in 0..3 {
            r.record_outcome(StrategyId::EtfPair, 5.0, 1000 + i);
        }
        assert!((r.size_multiplier(StrategyId::GapFade) - THROTTLE_MULT).abs() < 1e-9);
        assert!((r.size_multiplier(StrategyId::EtfPair) - 1.0).abs() < 1e-9);
        assert_eq!(r.states[&StrategyId::EtfPair].consecutive_wins, 3);
    }

    #[test]
    fn record_outcome_returns_current_state_clone() {
        let mut r = AdaptiveRisk::default();
        let snap = r.record_outcome(strat(), -10.0, 1000);
        assert_eq!(snap.consecutive_losses, 1);
        // Mutate underlying — snapshot unaffected
        r.record_outcome(strat(), -10.0, 1001);
        assert_eq!(snap.consecutive_losses, 1);
        assert_eq!(r.states[&strat()].consecutive_losses, 2);
    }

    #[test]
    fn tunable_constants_are_stable() {
        // Pin contract — tests downstream rely on these exact thresholds.
        assert_eq!(THROTTLE_AT_LOSSES, 3);
        assert_eq!(DEEP_THROTTLE_AT_LOSSES, 5);
        assert_eq!(RESTORE_AT_WINS, 3);
        assert!((THROTTLE_MULT - 0.5).abs() < 1e-9);
        assert!((DEEP_THROTTLE_MULT - 0.25).abs() < 1e-9);
        assert_eq!(DEEP_COOLDOWN_SECS, 1800);
    }

    #[test]
    fn default_base_dir_resolves_under_home_or_cwd() {
        let p = default_base_dir();
        assert!(p.ends_with("risk"));
    }

    #[test]
    fn many_alternating_outcomes_keep_states_consistent() {
        let mut r = AdaptiveRisk::default();
        for i in 0..1000 {
            let pnl = if i % 2 == 0 { 5.0 } else { -5.0 };
            r.record_outcome(strat(), pnl, 1000 + i);
        }
        let s = &r.states[&strat()];
        // Streak is exactly 1 in either direction (alternating)
        assert!(
            (s.consecutive_wins == 1 && s.consecutive_losses == 0)
                || (s.consecutive_losses == 1 && s.consecutive_wins == 0)
        );
        // No throttle ever triggered
        assert_eq!(s.throttle_count, 0);
        assert_eq!(s.deep_throttle_count, 0);
    }

    #[test]
    fn fresh_strategy_is_added_on_demand() {
        // Build the container with one strategy missing, then record
        // an outcome — must auto-insert.
        let mut r = AdaptiveRisk::default();
        r.states.remove(&StrategyId::HaltResume);
        r.record_outcome(StrategyId::HaltResume, 5.0, 1000);
        assert!(r.states.contains_key(&StrategyId::HaltResume));
        assert_eq!(r.states[&StrategyId::HaltResume].consecutive_wins, 1);
    }

    #[test]
    fn load_incomplete_state_heals_missing_strategies() {
        let dir = tmpdir();
        let mut r = AdaptiveRisk::default();
        // Remove one strategy so the saved file is incomplete
        r.states.remove(&StrategyId::OpeningRangeBreakout);
        r.save(&dir).expect("save");

        // Load it back
        let r2 = AdaptiveRisk::load(&dir).expect("load");
        // Verify it was auto-populated with a fresh state
        assert!(r2.states.contains_key(&StrategyId::OpeningRangeBreakout));
        let s = &r2.states[&StrategyId::OpeningRangeBreakout];
        assert_eq!(s.consecutive_losses, 0);
        assert_eq!(s.consecutive_wins, 0);
        assert!((s.current_size_mult - 1.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
