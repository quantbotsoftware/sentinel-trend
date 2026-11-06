//! Tournament tracker — per-strategy P/L attribution + sub-wallet
//! allocation + graceful deactivation state machine (Sesiunea 1).
//!
//! ## Why a tournament?
//!
//! With 8 strategies running in parallel we need a way to:
//!   1. Allocate paper capital fairly (start: $12.5K each = $100K)
//!   2. Track which strategies are working vs. losing
//!   3. Kill losers WITHOUT abandoning their open positions (drain mode)
//!   4. Surface live equity curves to the UI
//!
//! The tournament tracker is the source of truth for "is strategy X
//! pulling its weight?". The kill-switch policy lives in the bot's
//! decision loop, not here — the tracker only persists state. This
//! keeps the tracker testable in isolation.
//!
//! ## Strategy lifecycle
//!
//! ```text
//!   ┌────────┐  drain reason  ┌──────────┐  all closed  ┌──────┐
//!   │ Active │ ─────────────→ │ Draining │ ───────────→ │ Dead │
//!   └────────┘                └──────────┘              └──────┘
//!        ▲                          │
//!        │       resurrect          │
//!        └──────────────────────────┘  (manual override)
//! ```
//!
//! - `Active` — accepts new entries, manages open positions normally.
//! - `Draining` — refuses new entries, lets existing positions exit
//!   normally. Force-close timer kicks in if any position outlasts
//!   `force_exit_secs` (default 24 h).
//! - `Dead` — sub-wallet returned to the pool, no entries, no exits
//!   needed (all closed). Kept as a reserved slot in the registry.
//!
//! Resurrection (Dead → Active) is allowed by manual override only —
//! a strategy that earned its way into Dead doesn't get back without
//! deliberate human action.
//!
//! ## Persistence
//!
//! The whole tournament struct serialises to
//! `~/.local/share/sentinel-trend/tournament/state.json`. On bot
//! restart the tracker rehydrates from disk; on first run a fresh
//! tournament with default $12.5K sub-wallets is created.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::smarttrend::strategies::{StrategyId, MarketRegime};
use crate::smarttrend::strategy_evolver::ParamOverrides;

// ════════════════════════════════════════════════════════════════════
//  Status state machine
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyStatus {
    /// Trading normally.
    #[default]
    Active,
    /// No new entries; existing positions exit naturally; force-exit
    /// timer countdown engaged.
    Draining,
    /// Sub-wallet released back to pool; module retained for future
    /// reactivation.
    Dead,
}

// ════════════════════════════════════════════════════════════════════
//  Per-strategy stats
// ════════════════════════════════════════════════════════════════════

/// Real-time P/L statistics for a single strategy. Updated by
/// [`Tournament::record_trade_close`] on every closed round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategyStats {
    pub strategy: Option<StrategyId>,
    pub status: StrategyStatus,
    /// Initial sub-wallet allocation in USD.
    #[serde(with = "crate::serde_decimal")]
    pub initial_sub_wallet: Decimal,
    /// Current sub-wallet equity = initial + cumulative_pnl.
    #[serde(with = "crate::serde_decimal")]
    pub current_sub_wallet: Decimal,
    pub total_trades: u32,
    pub wins: u32,
    pub losses: u32,
    #[serde(with = "crate::serde_decimal")]
    pub cumulative_pnl: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub avg_win: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub avg_loss: Decimal,
    pub max_drawdown_pct: f64,
    /// Highest equity reached by this strategy (for DD computation).
    #[serde(with = "crate::serde_decimal")]
    pub peak_equity: Decimal,
    /// Unix timestamp of last trade close (for activity monitoring).
    pub last_trade_close_ts: i64,
    /// Unix timestamp this strategy entered `Draining` (used by the
    /// force-exit timer). 0 if not draining.
    pub draining_since: i64,
    /// Cumulative trade count where the strategy *won* a streak ≥ 3
    /// — bonus signal used by the kill-switch policy.
    pub winning_streak_3plus: u32,
    /// Same for losing streaks ≥ 3.
    pub losing_streak_3plus: u32,
    pub current_streak_len: i32, // positive = win streak, negative = loss
    /// Last 10 trade outcomes (true = win, false = loss) for live genetic mutation checks.
    #[serde(default)]
    pub recent_wins: Vec<bool>,
    /// Flag indicating if the strategy has a pending sandbox validation run (Phase II).
    #[serde(default)]
    pub pending_sandbox: bool,
    /// Store candidate parameters proposed during degradation mutation (Phase II).
    #[serde(default)]
    pub candidate_params: HashMap<String, f64>,
}

impl StrategyStats {
    pub fn is_degraded(&self) -> bool {
        if self.status != StrategyStatus::Active {
            return false;
        }
        if self.max_drawdown_pct > 15.0 {
            return true;
        }
        if self.recent_wins.len() >= 10 {
            let wins_count = self.recent_wins.iter().filter(|&&w| w).count();
            let win_rate = wins_count as f64 / self.recent_wins.len() as f64;
            if win_rate < 0.35 {
                return true;
            }
        }
        false
    }

    pub fn win_rate_pct(&self) -> f64 {
        if self.total_trades == 0 {
            0.0
        } else {
            (self.wins as f64 / self.total_trades as f64) * 100.0
        }
    }

    pub fn expectancy(&self) -> Decimal {
        if self.total_trades == 0 {
            Decimal::ZERO
        } else {
            self.cumulative_pnl / Decimal::from(self.total_trades)
        }
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tournament — root of the strategy ledger
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tournament {
    /// Total paper capital allocated across all strategies (snapshot).
    #[serde(with = "crate::serde_decimal")]
    pub paper_capital: Decimal,
    /// Per-strategy stats keyed by `StrategyId`.
    pub stats: HashMap<StrategyId, StrategyStats>,
    /// Free pool of capital not yet allocated to any strategy. Grows
    /// when a strategy goes Dead (its sub-wallet is returned).
    #[serde(with = "crate::serde_decimal")]
    pub free_pool: Decimal,
    /// Force-exit window for Draining → Dead transition (seconds).
    pub force_exit_secs: i64,
    /// Last persisted timestamp.
    pub last_saved_at: i64,

    // ════════════════════════════════════════════════════════════════
    //  v2.5.0 NUCLEAR (CB3) — anti-spam dispatch governance
    // ════════════════════════════════════════════════════════════════
    /// Per-(strategy, symbol) timestamp of last accepted dispatch.
    /// Used to enforce a per-strategy per-symbol cooldown so
    /// catalyst_frontrun cannot resubmit the same symbol every bar
    /// (live evidence Friday 2026-05-15: 152 entries from one strategy
    /// alone, mostly on a small handful of repeat symbols).
    ///
    /// Key is `format!("{strategy}:{symbol}")` (uppercase symbol);
    /// value is the unix timestamp (seconds) when the strategy last
    /// fired on that symbol.
    #[serde(default, skip)]
    pub last_strategy_symbol_dispatch: HashMap<String, i64>,
    /// Per-strategy daily entry counter, keyed by `StrategyId` and
    /// reset by [`reset_daily_counters`] each new ET trading day.
    /// Hard ceiling enforced by `dispatch_tournament_signals` — once a
    /// strategy hits `MAX_DAILY_ENTRIES_PER_STRATEGY`, no further
    /// signals are dispatched until tomorrow's reset.
    #[serde(default, skip)]
    pub daily_entries_per_strategy: HashMap<StrategyId, u32>,
    /// ET-day epoch the daily counters were last reset against
    /// (number of days since 1970-01-01 in America/New_York). Used
    /// to detect day rollover without per-tick datetime math.
    #[serde(default, skip)]
    pub daily_counters_et_day: i64,
}

impl Tournament {
    /// Build a fresh tournament with even sub-wallet allocation across
    /// all 8 strategies. Reserved 0% for `free_pool` initially.
    pub fn new_default(paper_capital: Decimal) -> Self {
        let n = Decimal::from(StrategyId::ALL.len());
        let per_strategy = paper_capital / n;
        let mut stats: HashMap<StrategyId, StrategyStats> = HashMap::new();
        for id in StrategyId::ALL {
            stats.insert(
                *id,
                StrategyStats {
                    strategy: Some(*id),
                    status: StrategyStatus::Active,
                    initial_sub_wallet: per_strategy,
                    current_sub_wallet: per_strategy,
                    total_trades: 0,
                    wins: 0,
                    losses: 0,
                    cumulative_pnl: Decimal::ZERO,
                    avg_win: Decimal::ZERO,
                    avg_loss: Decimal::ZERO,
                    max_drawdown_pct: 0.0,
                    peak_equity: per_strategy,
                    last_trade_close_ts: 0,
                    draining_since: 0,
                    winning_streak_3plus: 0,
                    losing_streak_3plus: 0,
                    current_streak_len: 0,
                    recent_wins: Vec::new(),
                    pending_sandbox: false,
                    candidate_params: HashMap::new(),
                },
            );
        }
        Self {
            paper_capital,
            stats,
            free_pool: Decimal::ZERO,
            force_exit_secs: 24 * 3600,
            last_saved_at: 0,
            last_strategy_symbol_dispatch: HashMap::new(),
            daily_entries_per_strategy: HashMap::new(),
            daily_counters_et_day: 0,
        }
    }

    /// v2.5.0 NUCLEAR (CB3) — Convenience: build the
    /// `last_strategy_symbol_dispatch` key for a `(strategy, symbol)`
    /// pair. Symbol is normalised to uppercase so the cooldown is
    /// case-insensitive.
    pub fn cooldown_key(strategy: StrategyId, symbol: &str) -> String {
        format!("{:?}:{}", strategy, symbol.trim().to_uppercase())
    }

    /// v2.5.0 NUCLEAR (CB3) — Reset per-day entry counters when the
    /// dispatcher detects a new ET trading day. Idempotent — pass
    /// the same `today_et_day` repeatedly without effect.
    pub fn reset_daily_counters_if_new_day(&mut self, today_et_day: i64) {
        if today_et_day != self.daily_counters_et_day {
            self.daily_entries_per_strategy.clear();
            self.daily_counters_et_day = today_et_day;
        }
    }

    /// v2.5.0 NUCLEAR (CB3) — Atomic check-and-claim: returns
    /// `Ok(())` if the strategy may dispatch on this symbol now and
    /// records the dispatch; returns `Err(reason)` otherwise. Caller
    /// must already hold the tournament lock.
    pub fn try_claim_dispatch(
        &mut self,
        strategy: StrategyId,
        symbol: &str,
        now_unix: i64,
        cooldown_secs: i64,
        max_daily: u32,
    ) -> Result<(), &'static str> {
        let count = self
            .daily_entries_per_strategy
            .get(&strategy)
            .copied()
            .unwrap_or(0);
        if count >= max_daily {
            return Err("daily_entry_cap");
        }
        let key = Self::cooldown_key(strategy, symbol);
        if let Some(prev_ts) = self.last_strategy_symbol_dispatch.get(&key).copied() {
            if now_unix.saturating_sub(prev_ts) < cooldown_secs {
                return Err("cooldown");
            }
        }
        self.last_strategy_symbol_dispatch.insert(key, now_unix);
        Ok(())
    }

    /// S45 — record one actually-opened entry against the per-strategy
    /// daily cap. Called from the bot entry success path on order accept,
    /// so the cap reflects real fills rather than dispatch attempts.
    /// Resets the counters first if the ET trading day has rolled over.
    pub fn record_daily_entry(&mut self, strategy: StrategyId) {
        let today_et_day = chrono::Datelike::num_days_from_ce(
            &chrono::Utc::now()
                .with_timezone(&chrono_tz::US::Eastern)
                .date_naive(),
        ) as i64;
        self.reset_daily_counters_if_new_day(today_et_day);
        *self
            .daily_entries_per_strategy
            .entry(strategy)
            .or_insert(0) += 1;
    }

    /// Record a closed trade. Updates wins/losses, cumulative P/L,
    /// drawdown, streak. `pnl` is the realised dollar P/L (positive
    /// for wins, negative for losses).
    pub fn record_trade_close(&mut self, strategy: StrategyId, pnl: Decimal, closed_at: i64) {
        let s = self.stats.entry(strategy).or_insert_with(|| StrategyStats {
            strategy: Some(strategy),
            ..Default::default()
        });
        s.total_trades = s.total_trades.saturating_add(1);
        if pnl > Decimal::ZERO {
            s.wins = s.wins.saturating_add(1);
            let wins_dec = Decimal::from(s.wins);
            let prev_wins_dec = Decimal::from(s.wins - 1);
            s.avg_win = (s.avg_win * prev_wins_dec + pnl) / wins_dec;
            // Streak handling.
            if s.current_streak_len >= 0 {
                s.current_streak_len += 1;
            } else {
                s.current_streak_len = 1;
            }
            if s.current_streak_len == 3 {
                s.winning_streak_3plus = s.winning_streak_3plus.saturating_add(1);
            }
        } else if pnl < Decimal::ZERO {
            s.losses = s.losses.saturating_add(1);
            let losses_dec = Decimal::from(s.losses);
            let prev_losses_dec = Decimal::from(s.losses - 1);
            s.avg_loss = (s.avg_loss * prev_losses_dec + pnl) / losses_dec;
            if s.current_streak_len <= 0 {
                s.current_streak_len -= 1;
            } else {
                s.current_streak_len = -1;
            }
            if s.current_streak_len == -3 {
                s.losing_streak_3plus = s.losing_streak_3plus.saturating_add(1);
            }
        } else {
            // exactly zero P/L (rare) — counted as a trade but no streak update
        }
        s.cumulative_pnl += pnl;
        s.current_sub_wallet = s.initial_sub_wallet + s.cumulative_pnl;
        if s.current_sub_wallet > s.peak_equity {
            s.peak_equity = s.current_sub_wallet;
        } else if s.peak_equity > Decimal::ZERO {
            use rust_decimal::prelude::ToPrimitive;
            let dd_dec = ((s.peak_equity - s.current_sub_wallet) / s.peak_equity) * Decimal::from(100);
            if let Some(dd) = dd_dec.to_f64() {
                if dd > s.max_drawdown_pct {
                    s.max_drawdown_pct = dd;
                }
            }
        }
        s.last_trade_close_ts = closed_at;
        if pnl > Decimal::ZERO {
            s.recent_wins.push(true);
        } else if pnl < Decimal::ZERO {
            s.recent_wins.push(false);
        }
        if s.recent_wins.len() > 10 {
            s.recent_wins.remove(0);
        }
    }

    pub fn run_bandit_allocation(&mut self, current_regime: MarketRegime) {
        // 1. Group active strategies
        let mut active_trend = Vec::new();
        let mut active_mean_revert = Vec::new();
        let mut active_catalyst = Vec::new();

        for id in StrategyId::ALL {
            if let Some(s) = self.stats.get(id) {
                if s.status == StrategyStatus::Active {
                    match id {
                        StrategyId::Apex
                        | StrategyId::LeveragedEtf
                        | StrategyId::OpeningRangeBreakout => {
                            active_trend.push(*id);
                        }
                        StrategyId::GapFade | StrategyId::EtfPair | StrategyId::AfternoonFade | StrategyId::StatArb50 => {
                            active_mean_revert.push(*id);
                        }
                        StrategyId::CatalystFrontRun
                        | StrategyId::HaltResume
                        | StrategyId::ExtendedNews
                        | StrategyId::Frontier => {
                            active_catalyst.push(*id);
                        }
                    }
                }
            }
        }

        // 2. Perform capital reallocation based on current regime
        let mut deltas: HashMap<StrategyId, Decimal> = HashMap::new();

        match current_regime {
            MarketRegime::Calm => {
                // Regime Calm: Transfer 15% from trend to mean-reverting
                if !active_trend.is_empty() && !active_mean_revert.is_empty() {
                    let mut total_transfer = Decimal::ZERO;
                    for id in &active_trend {
                        if let Some(s) = self.stats.get(id) {
                            let amt = (s.initial_sub_wallet * dec!(0.15)).round_dp(4);
                            deltas.insert(*id, -amt);
                            total_transfer += amt;
                        }
                    }
                    let count = active_mean_revert.len();
                    let shares = distribute_exact(total_transfer, count);
                    for (i, id) in active_mean_revert.iter().enumerate() {
                        deltas.insert(*id, shares[i]);
                    }
                }
            }
            MarketRegime::Trending => {
                // Regime Trending: Transfer 15% from mean-reverting to trend
                if !active_mean_revert.is_empty() && !active_trend.is_empty() {
                    let mut total_transfer = Decimal::ZERO;
                    for id in &active_mean_revert {
                        if let Some(s) = self.stats.get(id) {
                            let amt = (s.initial_sub_wallet * dec!(0.15)).round_dp(4);
                            deltas.insert(*id, -amt);
                            total_transfer += amt;
                        }
                    }
                    let count = active_trend.len();
                    let shares = distribute_exact(total_transfer, count);
                    for (i, id) in active_trend.iter().enumerate() {
                        deltas.insert(*id, shares[i]);
                    }
                }
            }
            MarketRegime::Volatile | MarketRegime::Panic => {
                // Regime Volatile/Panic: Transfer 15% from trend and 15% from mean-reverting to catalyst
                if !active_catalyst.is_empty() {
                    let mut total_transfer = Decimal::ZERO;
                    for id in &active_trend {
                        if let Some(s) = self.stats.get(id) {
                            let amt = (s.initial_sub_wallet * dec!(0.15)).round_dp(4);
                            deltas.insert(*id, deltas.get(id).copied().unwrap_or(Decimal::ZERO) - amt);
                            total_transfer += amt;
                        }
                    }
                    for id in &active_mean_revert {
                        if let Some(s) = self.stats.get(id) {
                            let amt = (s.initial_sub_wallet * dec!(0.15)).round_dp(4);
                            deltas.insert(*id, deltas.get(id).copied().unwrap_or(Decimal::ZERO) - amt);
                            total_transfer += amt;
                        }
                    }
                    let count = active_catalyst.len();
                    let shares = distribute_exact(total_transfer, count);
                    for (i, id) in active_catalyst.iter().enumerate() {
                        deltas.insert(*id, shares[i]);
                    }
                }
            }
        }

        // 3. Apply the deltas and log the reallocation
        if !deltas.is_empty() {
            let mut logged_allocations = Vec::new();
            for (id, delta) in deltas {
                if let Some(s) = self.stats.get_mut(&id) {
                    s.initial_sub_wallet += delta;
                    s.current_sub_wallet += delta;
                    logged_allocations.push(format!("{:?}: {}", id, delta));
                }
            }
            tracing::info!(
                component = "tournament",
                regime = ?current_regime,
                allocations = ?logged_allocations,
                "Bandit Capital Allocation complete"
            );
        }
    }

    /// Move a strategy from Active to Draining. No-op if already
    /// draining or dead. `reason` is logged but not persisted on the
    /// stats struct (kill_switch_log.rs handles narrative history).
    pub fn drain(&mut self, strategy: StrategyId, now: i64) -> bool {
        if let Some(s) = self.stats.get_mut(&strategy) {
            if s.status == StrategyStatus::Active {
                s.status = StrategyStatus::Draining;
                s.draining_since = now;
                return true;
            }
        }
        false
    }

    /// Move a strategy from Draining to Dead and return its current
    /// sub-wallet to the free pool. Returns true on transition.
    pub fn kill(&mut self, strategy: StrategyId) -> bool {
        if let Some(s) = self.stats.get_mut(&strategy) {
            if matches!(s.status, StrategyStatus::Draining | StrategyStatus::Active) {
                s.status = StrategyStatus::Dead;
                self.free_pool += s.current_sub_wallet.max(Decimal::ZERO);
                s.current_sub_wallet = Decimal::ZERO;
                return true;
            }
        }
        false
    }

    /// Manual override — bring a Dead/Draining strategy back to Active
    /// with a fresh sub-wallet from `free_pool` (or `paper_capital/N`
    /// if pool exhausted). Returns true on transition.
    pub fn resurrect(&mut self, strategy: StrategyId) -> bool {
        let n_dec = Decimal::from(StrategyId::ALL.len());
        let target = self.paper_capital / n_dec;
        let min_alloc = target * Decimal::from_str_exact("0.25").unwrap();
        let allocate = self.free_pool.min(target).max(min_alloc);
        self.free_pool = (self.free_pool - allocate).max(Decimal::ZERO);
        if let Some(s) = self.stats.get_mut(&strategy) {
            if !matches!(s.status, StrategyStatus::Active) {
                s.status = StrategyStatus::Active;
                s.draining_since = 0;
                s.initial_sub_wallet = allocate;
                s.current_sub_wallet = allocate;
                s.peak_equity = allocate;
                return true;
            }
        }
        false
    }

    /// P0-A (audit 2026-05-29) — Bounded sub-wallet top-up from the
    /// free pool only.
    pub fn topup_strategy_from_pool(&mut self, strategy: StrategyId) -> Option<Decimal> {
        if self.free_pool <= Decimal::ZERO {
            return None;
        }
        let n_dec = Decimal::from(StrategyId::ALL.len());
        let fair_share = self.paper_capital / n_dec;
        let amount = self.free_pool.min(fair_share);
        if amount <= Decimal::ZERO {
            return None;
        }
        let s = self.stats.get_mut(&strategy)?;
        s.initial_sub_wallet += amount;
        s.current_sub_wallet = s.initial_sub_wallet + s.cumulative_pnl;
        self.free_pool -= amount;
        Some(amount)
    }

    /// Returns strategies in Draining whose timer has expired (caller
    /// kills them via `kill`).
    pub fn drain_timeouts(&self, now: i64) -> Vec<StrategyId> {
        self.stats
            .iter()
            .filter_map(|(id, s)| {
                if s.status == StrategyStatus::Draining
                    && s.draining_since > 0
                    && now - s.draining_since >= self.force_exit_secs
                {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns the leaderboard sorted by expectancy DESC. Callers
    /// can render this directly in the UI without re-sorting.
    pub fn leaderboard(&self) -> Vec<(StrategyId, StrategyStats)> {
        let mut entries: Vec<_> = self
            .stats
            .iter()
            .map(|(id, s)| (*id, s.clone()))
            .collect();
        entries.sort_by(|a, b| {
            b.1.expectancy()
                .cmp(&a.1.expectancy())
        });
        entries
    }

    /// v2.5.0 — atomic persistence. Switch to temp-file + atomic rename.
    pub fn save(&mut self, base_dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(base_dir)?;
        self.last_saved_at = chrono::Utc::now().timestamp();
        let path = base_dir.join("state.json");
        let tmp_path = base_dir.join("state.json.tmp");
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp_path, &path)?;
        Ok(path)
    }

    pub fn load(base_dir: &Path) -> std::io::Result<Self> {
        let path = base_dir.join("state.json");
        let body = std::fs::read_to_string(&path)?;
        serde_json::from_str(&body).map_err(std::io::Error::other)
    }

    pub fn mutate_degraded_strategy(
        &mut self,
        strategy: StrategyId,
        leader: Option<StrategyId>,
        overrides: &ParamOverrides,
    ) {
        use crate::smarttrend::genetic_optimizer::{default_specs_for, SplitMix64};

        let specs = default_specs_for(strategy);
        if specs.is_empty() {
            return;
        }

        // Initialize SplitMix64 using a non-zero time-based seed
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xCAFE);
        let mut rng = SplitMix64::new(seed);

        // 1. Get current params for degraded strategy
        let mut degraded_params = HashMap::new();
        for spec in &specs {
            let val = overrides.get(strategy, &spec.name)
                .unwrap_or_else(|| (spec.min + spec.max) / 2.0);
            degraded_params.insert(spec.name.clone(), val);
        }

        // 2. Get params for leader strategy if available
        let mut leader_params = HashMap::new();
        if let Some(l_id) = leader {
            let leader_specs = default_specs_for(l_id);
            for spec in &leader_specs {
                let val = overrides.get(l_id, &spec.name)
                    .unwrap_or_else(|| (spec.min + spec.max) / 2.0);
                leader_params.insert(spec.name.clone(), val);
            }
        }

        // 3. Crossover and mutate parameters
        let mut new_params = HashMap::new();
        for spec in &specs {
            let degraded_val = degraded_params.get(&spec.name).copied().unwrap_or(spec.min);

            let inherited = if let Some(&leader_val) = leader_params.get(&spec.name) {
                if rng.next_bool() { degraded_val } else { leader_val }
            } else {
                degraded_val
            };

            let mutated = if rng.next_f64() < 0.1 {
                let direction = if rng.next_bool() { 1.0 } else { -1.0 };
                inherited + direction * spec.step
            } else {
                inherited
            };

            new_params.insert(spec.name.clone(), spec.clamp(mutated));
        }

        tracing::warn!(
            component = "tournament",
            strategy = ?strategy,
            leader = ?leader,
            new_params = ?new_params,
            "Autonomous genetic mutation proposed for degraded strategy (pending sandbox validation)"
        );

        if let Some(s) = self.stats.get_mut(&strategy) {
            s.pending_sandbox = true;
            s.candidate_params = new_params;
        }
    }
}

/// Default path for tournament state.
pub fn default_base_dir() -> PathBuf {
    let mut p = if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h)
    } else {
        PathBuf::from(".")
    };
    p.push(".local/share/sentinel-trend/tournament");
    p
}

fn distribute_exact(total: Decimal, count: usize) -> Vec<Decimal> {
    if count == 0 {
        return vec![];
    }
    if count == 1 {
        return vec![total];
    }
    let count_dec = Decimal::from(count);
    let share = (total / count_dec).round_dp(4);
    let mut shares = vec![share; count];
    let distributed = share * Decimal::from(count - 1);
    shares[count - 1] = total - distributed;
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use crate::smarttrend::dplus::{parse_strategy_from_source, source_for_strategy};
    use rust_decimal::prelude::ToPrimitive;

    #[test]
    fn new_default_allocates_evenly() {
        let t = Tournament::new_default(dec!(100000));
        let n = StrategyId::ALL.len();
        assert_eq!(t.stats.len(), n);
        let per_strategy = dec!(100000) / Decimal::from(n);
        for s in t.stats.values() {
            assert_eq!(s.initial_sub_wallet, per_strategy);
            assert_eq!(s.current_sub_wallet, per_strategy);
            assert_eq!(s.status, StrategyStatus::Active);
        }
    }

    #[test]
    fn record_trade_close_updates_stats() {
        let mut t = Tournament::new_default(dec!(100000));
        t.record_trade_close(StrategyId::GapFade, dec!(50), 1000);
        t.record_trade_close(StrategyId::GapFade, dec!(-20), 1100);
        let s = &t.stats[&StrategyId::GapFade];
        assert_eq!(s.total_trades, 2);
        assert_eq!(s.wins, 1);
        assert_eq!(s.losses, 1);
        assert_eq!(s.cumulative_pnl, dec!(30));
    }

    #[test]
    fn winning_streak_three_increments() {
        let mut t = Tournament::new_default(dec!(100000));
        for _ in 0..3 {
            t.record_trade_close(StrategyId::GapFade, dec!(10), 1000);
        }
        let s = &t.stats[&StrategyId::GapFade];
        assert_eq!(s.winning_streak_3plus, 1);
        assert_eq!(s.current_streak_len, 3);
        // Loss breaks streak
        t.record_trade_close(StrategyId::GapFade, dec!(-5), 2000);
        let s = &t.stats[&StrategyId::GapFade];
        assert_eq!(s.current_streak_len, -1);
    }

    #[test]
    fn losing_streak_three_increments() {
        let mut t = Tournament::new_default(dec!(100000));
        for _ in 0..3 {
            t.record_trade_close(StrategyId::GapFade, dec!(-8), 1000);
        }
        let s = &t.stats[&StrategyId::GapFade];
        assert_eq!(s.losing_streak_3plus, 1);
        assert_eq!(s.current_streak_len, -3);
    }

    #[test]
    fn drawdown_grows_on_losses() {
        let mut t = Tournament::new_default(dec!(100000));
        let initial = t.stats[&StrategyId::GapFade].current_sub_wallet;
        let win = dec!(100);
        let loss = dec!(600);
        t.record_trade_close(StrategyId::GapFade, win, 1000);
        t.record_trade_close(StrategyId::GapFade, -loss, 2000);
        let s = &t.stats[&StrategyId::GapFade];
        let expected_dd_pct = (loss / (initial + win) * dec!(100)).to_f64().unwrap();
        assert!(
            (s.max_drawdown_pct - expected_dd_pct).abs() < 0.05,
            "got {}, expected ≈ {}",
            s.max_drawdown_pct,
            expected_dd_pct
        );
    }

    #[test]
    fn drain_kill_resurrect_lifecycle() {
        let mut t = Tournament::new_default(dec!(100000));
        assert!(t.drain(StrategyId::GapFade, 100));
        assert_eq!(t.stats[&StrategyId::GapFade].status, StrategyStatus::Draining);
        assert!(!t.drain(StrategyId::GapFade, 200));
        assert!(t.kill(StrategyId::GapFade));
        assert_eq!(t.stats[&StrategyId::GapFade].status, StrategyStatus::Dead);
        assert!(t.free_pool > Decimal::ZERO);
        assert!(t.resurrect(StrategyId::GapFade));
        assert_eq!(t.stats[&StrategyId::GapFade].status, StrategyStatus::Active);
    }

    #[test]
    fn drain_timeouts_returns_expired_strategies() {
        let mut t = Tournament::new_default(dec!(100000));
        t.force_exit_secs = 10;
        t.drain(StrategyId::GapFade, 1000);
        assert!(t.drain_timeouts(1005).is_empty());
        let timeouts = t.drain_timeouts(1015);
        assert_eq!(timeouts.len(), 1);
        assert_eq!(timeouts[0], StrategyId::GapFade);
    }

    #[test]
    fn leaderboard_sorted_by_expectancy() {
        let mut t = Tournament::new_default(dec!(100000));
        t.record_trade_close(StrategyId::GapFade, dec!(50), 1000);
        t.record_trade_close(StrategyId::EtfPair, dec!(-10), 1000);
        t.record_trade_close(StrategyId::AfternoonFade, dec!(100), 1000);
        let lb = t.leaderboard();
        assert_eq!(lb[0].0, StrategyId::AfternoonFade);
        assert!(lb[0].1.expectancy() > lb[1].1.expectancy());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir();
        let mut t = Tournament::new_default(dec!(100000));
        t.record_trade_close(StrategyId::GapFade, dec!(25.5), 1000);
        let path = t.save(&dir).unwrap();
        assert!(path.exists());
        let loaded = Tournament::load(&dir).unwrap();
        assert_eq!(loaded.stats[&StrategyId::GapFade].cumulative_pnl, dec!(25.5));
    }

    #[test]
    fn s33_source_roundtrip_for_all_strategies() {
        for sid in StrategyId::ALL {
            let source = source_for_strategy(*sid);
            assert!(
                source.starts_with("s2:"),
                "source for {:?} missing s2: prefix: {}",
                sid,
                source
            );
            let parsed = parse_strategy_from_source(&source);
            assert_eq!(
                parsed,
                Some(*sid),
                "roundtrip failed for {:?}: source={} parsed={:?}",
                sid,
                source,
                parsed
            );
        }
    }

    #[test]
    fn s33_record_close_attributes_pnl_to_correct_strategy() {
        let mut t = Tournament::new_default(dec!(100000));
        let initial_gap_pnl = t.stats[&StrategyId::GapFade].cumulative_pnl;
        let initial_etfpair_pnl = t.stats[&StrategyId::EtfPair].cumulative_pnl;
        let initial_extnews_trades = t.stats[&StrategyId::ExtendedNews].total_trades;

        let source = "s2:gap_fade";
        let parsed = parse_strategy_from_source(source).expect("must parse");
        assert_eq!(parsed, StrategyId::GapFade);

        t.record_trade_close(parsed, dec!(35), 1234);

        assert_eq!(t.stats[&StrategyId::GapFade].cumulative_pnl, initial_gap_pnl + dec!(35));
        assert_eq!(t.stats[&StrategyId::GapFade].total_trades, 1);
        assert_eq!(t.stats[&StrategyId::GapFade].wins, 1);

        assert_eq!(t.stats[&StrategyId::EtfPair].cumulative_pnl, initial_etfpair_pnl);
        assert_eq!(
            t.stats[&StrategyId::ExtendedNews].total_trades,
            initial_extnews_trades
        );
    }

    #[test]
    fn s33_legacy_source_skips_tournament_recording() {
        assert!(parse_strategy_from_source("bot").is_none());
        assert!(parse_strategy_from_source("").is_none());
        assert!(parse_strategy_from_source("user").is_none());
    }

    #[test]
    fn s33_sub_wallet_blown_threshold_is_zero() {
        let mut t = Tournament::new_default(dec!(100000));
        let initial_sub = t.stats[&StrategyId::GapFade].current_sub_wallet;
        assert!(initial_sub > Decimal::ZERO);

        t.record_trade_close(StrategyId::GapFade, -initial_sub, 1000);
        assert_eq!(t.stats[&StrategyId::GapFade].current_sub_wallet, Decimal::ZERO);

        let blown_at_zero = t.stats[&StrategyId::GapFade].current_sub_wallet < Decimal::ZERO;
        assert!(!blown_at_zero, "exactly zero must not be classified as blown");

        t.record_trade_close(StrategyId::GapFade, dec!(-0.01), 1001);
        let blown_at_neg = t.stats[&StrategyId::GapFade].current_sub_wallet < Decimal::ZERO;
        assert!(blown_at_neg, "any negative sub_wallet must be classified as blown");
    }

    #[test]
    fn s33_blown_strategy_drain_is_idempotent_and_isolated() {
        let mut t = Tournament::new_default(dec!(100000));
        t.record_trade_close(StrategyId::GapFade, dec!(-20000), 1000);
        assert!(t.stats[&StrategyId::GapFade].current_sub_wallet < Decimal::ZERO);

        let first = t.drain(StrategyId::GapFade, 2000);
        assert!(first);
        assert_eq!(
            t.stats[&StrategyId::GapFade].status,
            StrategyStatus::Draining
        );

        let second = t.drain(StrategyId::GapFade, 2001);
        assert!(!second, "drain on Draining strategy must be a no-op");
        assert_eq!(
            t.stats[&StrategyId::GapFade].status,
            StrategyStatus::Draining
        );

        for sid in StrategyId::ALL {
            if *sid == StrategyId::GapFade {
                continue;
            }
            assert_eq!(
                t.stats[sid].status,
                StrategyStatus::Active,
                "strategy {:?} must remain Active",
                sid
            );
            assert_eq!(t.stats[sid].current_sub_wallet, t.stats[sid].initial_sub_wallet);
        }
    }

    #[test]
    fn s33_audit_q3_sub_wallet_can_go_negative_after_open_trade() {
        let mut t = Tournament::new_default(dec!(100000));
        let initial = t.stats[&StrategyId::GapFade].current_sub_wallet;
        let big_loss = initial + dec!(500);
        t.record_trade_close(StrategyId::GapFade, -big_loss, 1000);

        let neg = t.stats[&StrategyId::GapFade].current_sub_wallet;
        assert!(
            neg < dec!(-400),
            "expected sub_wallet ≈ -500, got {}",
            neg
        );
    }

    #[test]
    fn win_rate_pct_handles_zero_trades() {
        let s = StrategyStats::default();
        assert_eq!(s.win_rate_pct(), 0.0);
    }

    #[test]
    fn expectancy_handles_zero_trades() {
        let s = StrategyStats::default();
        assert_eq!(s.expectancy(), Decimal::ZERO);
    }

    #[test]
    fn test_is_degraded_and_mutation() {
        let mut t = Tournament::new_default(dec!(100000));
        let stats = t.stats.get_mut(&StrategyId::GapFade).unwrap();

        // 1. Initially not degraded
        assert!(!stats.is_degraded());

        // 2. Degraded by drawdown > 15%
        stats.max_drawdown_pct = 16.0;
        assert!(stats.is_degraded());
        stats.max_drawdown_pct = 0.0; // reset

        // 3. Degraded by last 10 trades winrate < 35%
        for _ in 0..7 {
            stats.recent_wins.push(false); // 7 losses
        }
        for _ in 0..3 {
            stats.recent_wins.push(true); // 3 wins
        }
        assert_eq!(stats.recent_wins.len(), 10);
        assert!(stats.is_degraded());

        // 4. Test mutation function
        let overrides = ParamOverrides::new();
        let mut initial_params = HashMap::new();
        initial_params.insert("min_gap_pct".to_string(), 5.0);
        overrides.set(StrategyId::GapFade, initial_params);

        // Call mutate_degraded_strategy
        t.mutate_degraded_strategy(StrategyId::GapFade, Some(StrategyId::EtfPair), &overrides);

        // Verify that overrides were NOT updated directly
        let current_overrides = overrides.snapshot().get(&StrategyId::GapFade).cloned().unwrap();
        assert_eq!(current_overrides.len(), 1);
        assert!(current_overrides.contains_key("min_gap_pct"));
        assert!(!current_overrides.contains_key("min_rvol"));

        // Verify that stats were updated with pending_sandbox and candidate_params
        let stats = t.stats.get(&StrategyId::GapFade).unwrap();
        assert!(stats.pending_sandbox);
        assert!(stats.candidate_params.contains_key("min_gap_pct"));
        assert!(stats.candidate_params.contains_key("min_rvol"));
        assert!(stats.candidate_params.contains_key("atr_stop_mult"));
    }

    #[test]
    fn test_distribute_exact() {
        let shares = distribute_exact(dec!(10.0), 3);
        assert_eq!(shares.len(), 3);
        assert_eq!(shares[0], dec!(3.3333));
        assert_eq!(shares[1], dec!(3.3333));
        assert_eq!(shares[2], dec!(3.3334));
        assert_eq!(shares.iter().sum::<Decimal>(), dec!(10.0));

        let shares_neg = distribute_exact(dec!(-5.0), 2);
        assert_eq!(shares_neg.len(), 2);
        assert_eq!(shares_neg[0], dec!(-2.5));
        assert_eq!(shares_neg[1], dec!(-2.5));
        assert_eq!(shares_neg.iter().sum::<Decimal>(), dec!(-5.0));
    }

    #[test]
    fn test_run_bandit_allocation() {
        let mut t = Tournament::new_default(dec!(90000));
        let initial_sum: Decimal = t.stats.values().map(|s| s.initial_sub_wallet).sum();
        assert_eq!(initial_sum, dec!(90000));

        // Let's run Calm regime (should shift Trend -> Mean Revert)
        let before_trend = t.stats[&StrategyId::LeveragedEtf].initial_sub_wallet;
        let before_mean = t.stats[&StrategyId::GapFade].initial_sub_wallet;

        t.run_bandit_allocation(MarketRegime::Calm);

        let after_trend = t.stats[&StrategyId::LeveragedEtf].initial_sub_wallet;
        let after_mean = t.stats[&StrategyId::GapFade].initial_sub_wallet;

        assert!(after_trend < before_trend, "Trend capital should decrease in Calm regime");
        assert!(after_mean > before_mean, "Mean revert capital should increase in Calm regime");

        let after_sum: Decimal = t.stats.values().map(|s| s.initial_sub_wallet).sum();
        assert_eq!(after_sum, dec!(90000), "Total capital must be conserved");
        
        // Check current_sub_wallet equals initial_sub_wallet + cumulative_pnl
        for s in t.stats.values() {
            assert_eq!(s.current_sub_wallet, s.initial_sub_wallet + s.cumulative_pnl);
        }
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sentinel-test-tour-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
