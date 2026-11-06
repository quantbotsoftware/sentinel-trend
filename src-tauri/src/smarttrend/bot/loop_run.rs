use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::*;
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::execution::order_exec::ExecutionMetrics;
use crate::state::{Direction, TitanState};

use super::entry::handle_entry;
use super::exit::{
    cancel_stale_partial_fills, check_adaptive_exits, check_blown_strategies,
    check_flat_by_close, check_max_hold_positions, handle_exit, check_tournament_exits,
    check_causal_hedging,
};
use super::{BotAction, BotActionKind, ExitReason, SkipReason};

// ═══════════════════════════════════════════════════════════════════════
//  BONUS: BotMetrics
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default, Serialize)]
pub struct BotMetrics {
    pub entries_attempted: u64,
    pub entries_executed: u64,
    pub entries_blocked: u64,
    pub entries_blocked_max_pos: u64,
    pub entries_blocked_duplicate: u64,
    // S02: dimensional counters for compliance gates. All `u64`/`Default::default()`
    // so `BotMetrics` JSON keeps backward compat with pre-S02 consumers.
    #[serde(default)]
    pub entries_blocked_pdt: u64,
    #[serde(default)]
    pub entries_blocked_buying_power: u64,
    #[serde(default)]
    pub entries_blocked_short_disabled: u64,
    #[serde(default)]
    pub entries_blocked_not_shortable: u64,
    #[serde(default)]
    pub entries_blocked_not_fractionable: u64,
    #[serde(default)]
    pub entries_blocked_asset_unknown: u64,
    #[serde(default)]
    pub entries_blocked_account_stale: u64,
    #[serde(default)]
    pub entries_blocked_breaker_halt: u64,
    #[serde(default)]
    pub entries_blocked_caution_block: u64,
    pub exits_executed: u64,
    pub exits_signal: u64,
    pub exits_max_hold: u64,
    pub exits_breaker: u64,
    pub exits_stale: u64,
    pub exits_manual: u64,
    /// S17.4 — adaptive QuickCut counter (held ≥ N s, P/L ≤ -X%).
    #[serde(default)]
    pub exits_quick_cut: u64,
    /// S17.4 — adaptive ProfitTake counter (held ≥ N s, P/L ≥ +X%).
    #[serde(default)]
    pub exits_profit_take: u64,
    /// v2.4.2 — flat-by-close exits triggered at 15:55 ET (or
    /// whatever `flat_by_close_minutes` declares before 16:00 ET).
    #[serde(default)]
    pub exits_flat_by_close: u64,
    /// S31 (AUDIT RISK 5, 2026-05-28) — Tournament sub-wallet kill-switch.
    pub exits_tournament_blown: u64,
    /// S44 — Smart Trailing Stop exits.
    #[serde(default)]
    pub exits_trailing_stop: u64,
    pub total_actions: u64,
}

impl BotMetrics {
    pub(super) fn record_entry_blocked(&mut self) {
        self.entries_attempted += 1;
        self.entries_blocked += 1;
        self.total_actions += 1;
    }

    pub(super) fn record_entry_executed(&mut self) {
        self.entries_attempted += 1;
        self.entries_executed += 1;
        self.total_actions += 1;
    }

    /// S02: increment dimensional counter for a compliance-gate skip and
    /// the global `entries_blocked` aggregate.
    pub(super) fn record_skipped(&mut self, reason: &SkipReason) {
        match reason {
            SkipReason::PdtBlocked => self.entries_blocked_pdt += 1,
            SkipReason::BuyingPowerInsufficient => self.entries_blocked_buying_power += 1,
            SkipReason::ShortDisabled => self.entries_blocked_short_disabled += 1,
            SkipReason::NotShortable => self.entries_blocked_not_shortable += 1,
            SkipReason::NotFractionable => self.entries_blocked_not_fractionable += 1,
            SkipReason::AssetUnknown => self.entries_blocked_asset_unknown += 1,
            SkipReason::AccountStale => self.entries_blocked_account_stale += 1,
            SkipReason::BreakerHalt => self.entries_blocked_breaker_halt += 1,
            SkipReason::CautionBlock => self.entries_blocked_caution_block += 1,
        }
        self.record_entry_blocked();
    }

    pub(super) fn record_exit(&mut self, reason: &ExitReason) {
        self.exits_executed += 1;
        self.total_actions += 1;
        match reason {
            ExitReason::SignalExit => self.exits_signal += 1,
            ExitReason::MaxHold => self.exits_max_hold += 1,
            ExitReason::CircuitBreaker => self.exits_breaker += 1,
            ExitReason::StaleData => self.exits_stale += 1,
            ExitReason::ManualClose => self.exits_manual += 1,
            ExitReason::QuickCut => self.exits_quick_cut += 1,
            ExitReason::ProfitTake => self.exits_profit_take += 1,
            ExitReason::FlatByClose => self.exits_flat_by_close += 1,
            ExitReason::TournamentExit => self.exits_tournament_blown += 1,
            ExitReason::TrailingStop => self.exits_trailing_stop += 1,
            // PRIME F2c — ladder exits share the trailing-stop counter
            // (both are protective-stop rails; per-reason attribution
            // lives in the journal / decision log).
            ExitReason::PrimeLadder => self.exits_trailing_stop += 1,
        }
    }

    pub(super) fn should_emit(&self) -> bool {
        self.total_actions > 0 && self.total_actions.is_multiple_of(10)
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Bot Loop
// ═══════════════════════════════════════════════════════════════════════

/// S15.6: unconditional watchdog tick cadence for the bot loop.
///
/// The loop's `tokio::select!` formerly only ticked on `bot_rx.recv()`,
/// so during quiet periods (no Entry / Exit signals) the watchdog
/// flagged STALE at 60s and DEAD at 120s. 15s keeps cadence at 1/4 of
/// the WARN threshold, leaving generous headroom for jitter.
const BOT_TICK_INTERVAL_SECS: u64 = 15;

pub(super) fn compute_stop_loss(price: f64, atr: f64, mult: f64, direction: Direction) -> Decimal {
    let sl_f64 = match direction {
        Direction::Long => (price - atr * mult).max(0.01),
        Direction::Short => price + atr * mult,
    };
    Decimal::from_f64(sl_f64).unwrap_or(Decimal::ZERO)
}

/// S15.11: take-profit price for bot AUTO entries. Mirrors
/// `compute_stop_loss` shape but on the opposite side of `price`,
/// scaled by a hardcoded 1:2 risk-reward ratio. For
/// `bracket_atr_mult = 2.0x` (the default), SL is 2 ATR away
/// and TP is 4 ATR away — i.e. risk one to make two.
pub(super) fn compute_take_profit(
    price: f64,
    atr: f64,
    mult: f64,
    direction: Direction,
) -> Decimal {
    // Hardcoded 1:2 R:R; adjust here to widen/narrow targets.
    const TAKE_PROFIT_RR: f64 = 2.0;

    let tp_f64 = match direction {
        Direction::Long => price + atr * mult * TAKE_PROFIT_RR,
        Direction::Short => (price - atr * mult * TAKE_PROFIT_RR).max(0.01),
    };
    Decimal::from_f64(tp_f64)
        .map(|d| d.round_dp(2))
        .unwrap_or(Decimal::ZERO)
}

pub(super) fn compute_qty(size_usd: f64, price: f64) -> Decimal {
    if price <= 0.0 {
        return Decimal::ZERO;
    }
    Decimal::from_f64((size_usd / price).max(0.0)).unwrap_or(Decimal::ZERO)
}

pub(super) fn opposite_direction(d: Direction) -> Direction {
    match d {
        Direction::Long => Direction::Short,
        Direction::Short => Direction::Long,
    }
}

/// S15.6: poison-tolerant single-shot watchdog tick on the "bot" loop.
/// Tier-A `state.system` lock acquired alone (boundary rule).
fn tick_bot_watchdog(state: &TitanState) {
    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(h) = sys.loops.get_mut("bot") {
        h.tick();
    }
}

pub async fn run_bot_loop(
    state: Arc<TitanState>,
    market: Arc<crate::market::market_state::MarketDataState>,
    bot_rx: tokio::sync::mpsc::Receiver<BotAction>,
    app: AppHandle,
) {
    tracing::info!(component = "bot", "SmartTrend bot loop started");

    let mut bot_rx = bot_rx;
    let mut metrics = BotMetrics::default();
    let mut exec_metrics = ExecutionMetrics::default();
    let mut max_hold_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    let mut partial_fill_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    // S15.6: dedicated cadence to keep the watchdog green during quiet
    // periods. See `BOT_TICK_INTERVAL_SECS` for the rationale.
    let mut tick_interval =
        tokio::time::interval(std::time::Duration::from_secs(BOT_TICK_INTERVAL_SECS));

    {
        let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
        sys.loops
            .entry("bot".to_string())
            .or_insert_with(crate::state::LoopHealth::new)
            .tick();
    }

    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => {
                tracing::info!(component = "bot", "Bot loop shutdown");
                break;
            }

            action = bot_rx.recv() => {
                let Some(action) = action else {
                    tracing::warn!(component = "bot", "Bot channel closed");
                    break;
                };

                tick_bot_watchdog(&state);

                match action.kind {
                    BotActionKind::Entry => {
                        handle_entry(&state, &market, &action, &app, &mut metrics, &mut exec_metrics).await;
                    }
                    BotActionKind::Exit(ref reason) => {
                        handle_exit(&state, &action, reason, &app, &mut metrics, &mut exec_metrics).await;
                    }
                }

                if metrics.should_emit() {
                    let _ = app.emit_all("bot-metrics", &metrics);
                }
            }

            _ = max_hold_interval.tick() => {
                // S15.6: defensive tick — harmless duplicate if the
                // dedicated tick arm fired in the same poll batch.
                tick_bot_watchdog(&state);
                // S17.4 — adaptive exits run BEFORE max-hold so that
                // a position simultaneously eligible for QuickCut and
                // MaxHold gets the more-specific reason in metrics.
                //
                // S31 (AUDIT RISK 5, 2026-05-28) — sub-wallet kill-switch
                // runs FIRST so a blown strategy's positions are exited
                // before any other exit rail can attribute them to a
                // softer reason. Strategy-level risk envelope trumps
                // position-level signals.
                check_blown_strategies(&state, &app, &mut metrics, &mut exec_metrics).await;
                check_tournament_exits(&state, &market, &app, &mut metrics, &mut exec_metrics).await;
                check_causal_hedging(&state, &market, &app, &mut metrics, &mut exec_metrics).await;
                check_adaptive_exits(&state, &app, &mut metrics, &mut exec_metrics).await;
                check_max_hold_positions(&state, &app, &mut metrics, &mut exec_metrics).await;
                // v2.4.2 — flat-by-close. Last so that QuickCut /
                // ProfitTake / MaxHold get a chance to attribute the
                // exit to their more-specific reason. The flat-by-close
                // rail only fires inside the configured pre-16:00 ET
                // window and is a no-op outside it.
                check_flat_by_close(&state, &app, &mut metrics, &mut exec_metrics).await;
            }

            _ = partial_fill_interval.tick() => {
                tick_bot_watchdog(&state);
                cancel_stale_partial_fills(&state, &app).await;
            }

            _ = tick_interval.tick() => {
                // S15.6: unconditional 15s watchdog tick — fires even
                // when no actions / 30s timers run.
                tick_bot_watchdog(&state);
            }
        }
    }

    tracing::info!(component = "bot", ?metrics, "Bot loop terminated");
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use crate::smarttrend::bot_safety::{ExitReason, SkipReason};
    use crate::state::Direction;

    use super::{
        compute_qty, compute_stop_loss, compute_take_profit, opposite_direction, BotMetrics,
        BOT_TICK_INTERVAL_SECS,
    };

    /// S15.6 regression-lock: the bot's dedicated tick cadence must
    /// stay safely under the watchdog WARN threshold (60s). 2× cadence
    /// must still be < 60s so a single missed tick under load is fine.
    #[test]
    fn bot_tick_interval_is_below_stale_threshold() {
        assert!(
            BOT_TICK_INTERVAL_SECS * 2 < 60,
            "BOT_TICK_INTERVAL_SECS * 2 = {} must be < 60s STALE_WARN",
            BOT_TICK_INTERVAL_SECS * 2
        );
    }

    #[test]
    fn stop_loss_long() {
        let sl = compute_stop_loss(100.0, 2.0, 2.0, Direction::Long);
        assert_eq!(sl, dec!(96));
    }

    #[test]
    fn stop_loss_short() {
        let sl = compute_stop_loss(100.0, 2.0, 2.0, Direction::Short);
        assert_eq!(sl, dec!(104));
    }

    #[test]
    fn stop_loss_clamped_positive() {
        let sl = compute_stop_loss(1.0, 5.0, 2.0, Direction::Long);
        assert_eq!(sl, dec!(0.01));
    }

    // ── S15.11: compute_take_profit ──

    #[test]
    fn take_profit_long() {
        let tp = compute_take_profit(100.0, 2.0, 2.0, Direction::Long);
        // 100 + 2*2*2 = 108
        assert_eq!(tp, dec!(108));
    }

    #[test]
    fn take_profit_short() {
        let tp = compute_take_profit(100.0, 2.0, 2.0, Direction::Short);
        // 100 - 2*2*2 = 92
        assert_eq!(tp, dec!(92));
    }

    #[test]
    fn take_profit_short_clamped_positive() {
        let tp = compute_take_profit(1.0, 5.0, 2.0, Direction::Short);
        // 1 - 5*2*2 = -19 → clamped to 0.01
        assert_eq!(tp, dec!(0.01));
    }

    #[test]
    fn qty_calculation() {
        let qty = compute_qty(1000.0, 200.0);
        assert_eq!(qty, dec!(5));
    }

    #[test]
    fn qty_zero_price() {
        assert_eq!(compute_qty(1000.0, 0.0), Decimal::ZERO);
    }

    #[test]
    fn opposite_direction_works() {
        assert_eq!(opposite_direction(Direction::Long), Direction::Short);
        assert_eq!(opposite_direction(Direction::Short), Direction::Long);
    }

    #[test]
    fn metrics_entry_blocked() {
        let mut m = BotMetrics::default();
        m.record_entry_blocked();
        assert_eq!(m.entries_attempted, 1);
        assert_eq!(m.entries_blocked, 1);
        assert_eq!(m.entries_executed, 0);
    }

    #[test]
    fn metrics_entry_executed() {
        let mut m = BotMetrics::default();
        m.record_entry_executed();
        assert_eq!(m.entries_attempted, 1);
        assert_eq!(m.entries_executed, 1);
    }

    #[test]
    fn metrics_exit_counts() {
        let mut m = BotMetrics::default();
        m.record_exit(&ExitReason::MaxHold);
        m.record_exit(&ExitReason::SignalExit);
        m.record_exit(&ExitReason::SignalExit);
        assert_eq!(m.exits_executed, 3);
        assert_eq!(m.exits_max_hold, 1);
        assert_eq!(m.exits_signal, 2);
    }

    #[test]
    fn metrics_emit_at_10() {
        let mut m = BotMetrics::default();
        for _ in 0..9 {
            m.record_entry_executed();
        }
        assert!(!m.should_emit());
        m.record_entry_executed();
        assert!(m.should_emit());
    }

    #[test]
    fn metrics_blocked_counts() {
        let mut m = BotMetrics::default();
        m.entries_blocked_max_pos += 1;
        m.entries_blocked_duplicate += 1;
        m.record_entry_blocked();
        m.record_entry_blocked();
        assert_eq!(m.entries_blocked, 2);
        assert_eq!(m.entries_blocked_max_pos, 1);
        assert_eq!(m.entries_blocked_duplicate, 1);
    }

    // ── S02: BotMetrics dimensional counters ──

    #[test]
    fn bot_metrics_record_skipped_increments_counters() {
        let mut m = BotMetrics::default();
        m.record_skipped(&SkipReason::PdtBlocked);
        m.record_skipped(&SkipReason::PdtBlocked);
        m.record_skipped(&SkipReason::BuyingPowerInsufficient);
        m.record_skipped(&SkipReason::ShortDisabled);
        m.record_skipped(&SkipReason::NotShortable);
        m.record_skipped(&SkipReason::NotFractionable);
        m.record_skipped(&SkipReason::AssetUnknown);
        m.record_skipped(&SkipReason::AccountStale);

        assert_eq!(m.entries_blocked_pdt, 2);
        assert_eq!(m.entries_blocked_buying_power, 1);
        assert_eq!(m.entries_blocked_short_disabled, 1);
        assert_eq!(m.entries_blocked_not_shortable, 1);
        assert_eq!(m.entries_blocked_not_fractionable, 1);
        assert_eq!(m.entries_blocked_asset_unknown, 1);
        assert_eq!(m.entries_blocked_account_stale, 1);
        assert_eq!(m.entries_blocked, 8);
        assert_eq!(m.entries_attempted, 8);
        assert_eq!(m.total_actions, 8);
    }

    #[test]
    fn bot_metrics_serde_backward_compat() {
        let m = BotMetrics::default();
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("entries_blocked_pdt"));
        assert!(json.contains("entries_blocked_buying_power"));
        assert!(json.contains("entries_blocked_short_disabled"));
        assert!(json.contains("entries_blocked_not_shortable"));
        assert!(json.contains("entries_blocked_not_fractionable"));
        assert!(json.contains("entries_blocked_asset_unknown"));
        assert!(json.contains("entries_blocked_account_stale"));
        assert!(json.contains("entries_attempted"));
        assert!(json.contains("entries_executed"));
    }
}
