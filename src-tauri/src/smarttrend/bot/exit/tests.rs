use super::adaptive::{evaluate_adaptive_exit, AdaptiveExitParams};
use crate::smarttrend::bot_safety::ExitReason;

fn params(quick: f64, profit: f64) -> AdaptiveExitParams {
    AdaptiveExitParams {
        quick_cut_loss_pct: quick,
        quick_cut_min_hold_secs: 60,
        profit_take_pct: profit,
        profit_take_min_hold_secs: 90,
    }
}

// ── Disabled rails ──

#[test]
fn adaptive_exit_disabled_returns_none() {
    // Both off → never fire even with extreme P/L.
    let p = params(0.0, 0.0);
    assert_eq!(evaluate_adaptive_exit(120, -50.0, &p), None);
    assert_eq!(evaluate_adaptive_exit(120, 50.0, &p), None);
}

// ── Quick-cut ──

#[test]
fn adaptive_exit_quick_cut_fires_when_loss_exceeds() {
    let p = params(0.5, 0.0);
    // Held 90 s, P/L -0.6% → fire QuickCut.
    assert_eq!(
        evaluate_adaptive_exit(90, -0.6, &p),
        Some(ExitReason::QuickCut)
    );
}

#[test]
fn adaptive_exit_quick_cut_blocked_below_min_hold() {
    let p = params(0.5, 0.0);
    // Held 30 s < 60 s → too early, do not fire.
    assert_eq!(evaluate_adaptive_exit(30, -2.0, &p), None);
}

#[test]
fn adaptive_exit_quick_cut_blocked_within_threshold() {
    let p = params(0.5, 0.0);
    // -0.4% < -0.5% → no fire.
    assert_eq!(evaluate_adaptive_exit(120, -0.4, &p), None);
}

#[test]
fn adaptive_exit_quick_cut_at_exact_threshold() {
    let p = params(0.5, 0.0);
    // Boundary: -0.5% must fire (≤ comparison).
    assert_eq!(
        evaluate_adaptive_exit(60, -0.5, &p),
        Some(ExitReason::QuickCut)
    );
}

#[test]
fn adaptive_exit_zero_or_negative_config_disables() {
    // `quick_cut_loss_pct <= 0.0` must disable the rail — opt-in
    // semantics avoid surprising the operator if a value sneaks
    // in via a corrupted settings.json.
    let mut p = params(0.5, 0.0);
    p.quick_cut_loss_pct = 0.0;
    assert_eq!(evaluate_adaptive_exit(120, -10.0, &p), None);
    p.quick_cut_loss_pct = -0.5;
    assert_eq!(evaluate_adaptive_exit(120, -10.0, &p), None);
}

// ── Profit-take ──

#[test]
fn adaptive_exit_profit_take_fires_above_threshold() {
    let p = params(0.0, 1.5);
    // Held 100 s, +1.6% → fire ProfitTake.
    assert_eq!(
        evaluate_adaptive_exit(100, 1.6, &p),
        Some(ExitReason::ProfitTake)
    );
}

#[test]
fn adaptive_exit_profit_take_blocked_below_min_hold() {
    let p = params(0.0, 1.5);
    // Held 60 s < 90 s → too early.
    assert_eq!(evaluate_adaptive_exit(60, 5.0, &p), None);
}

#[test]
fn adaptive_exit_profit_take_at_exact_threshold() {
    let p = params(0.0, 1.5);
    // Boundary: +1.5% must fire (≥ comparison).
    assert_eq!(
        evaluate_adaptive_exit(90, 1.5, &p),
        Some(ExitReason::ProfitTake)
    );
}

// ── Combined / priority ──

#[test]
fn adaptive_exit_both_enabled_quick_cut_wins_for_loss() {
    let p = params(0.5, 1.5);
    // Held 100 s, -2.0% → QuickCut (negative branch).
    assert_eq!(
        evaluate_adaptive_exit(100, -2.0, &p),
        Some(ExitReason::QuickCut)
    );
}

#[test]
fn adaptive_exit_both_enabled_profit_take_wins_for_gain() {
    let p = params(0.5, 1.5);
    // Held 100 s, +2.0% → ProfitTake (positive branch).
    assert_eq!(
        evaluate_adaptive_exit(100, 2.0, &p),
        Some(ExitReason::ProfitTake)
    );
}

#[test]
fn adaptive_exit_neutral_pnl_inside_band_returns_none() {
    let p = params(0.5, 1.5);
    // -0.2% to +1.0% range — neither rail fires.
    for pnl in [-0.2, 0.0, 0.5, 1.0] {
        assert_eq!(evaluate_adaptive_exit(120, pnl, &p), None, "pnl={pnl}");
    }
}

// ── Edge cases ──

#[test]
fn adaptive_exit_non_finite_pnl_returns_none() {
    let p = params(0.5, 1.5);
    assert_eq!(evaluate_adaptive_exit(120, f64::NAN, &p), None);
    assert_eq!(evaluate_adaptive_exit(120, f64::INFINITY, &p), None);
    assert_eq!(evaluate_adaptive_exit(120, f64::NEG_INFINITY, &p), None);
}

#[test]
fn adaptive_exit_zero_or_negative_held_returns_none() {
    let p = params(0.5, 1.5);
    assert_eq!(evaluate_adaptive_exit(0, -10.0, &p), None);
    assert_eq!(evaluate_adaptive_exit(-5, -10.0, &p), None);
}

// ── S17.6 — alpaca_side mapping (broker pre-check helper) ──

#[test]
fn s17_6_alpaca_side_long_to_buy_short_to_sell() {
    // Pin the Direction → Alpaca side mapping. The broker pre-check
    // (`has_open_close_order`) compares case-insensitive but the
    // helper must emit lower-case canonical strings.
    use crate::state::Direction;
    use super::alpaca_side;
    assert_eq!(alpaca_side(Direction::Long), "buy");
    assert_eq!(alpaca_side(Direction::Short), "sell");
}

#[test]
fn test_adaptive_trailing_stop_exits() {
    use super::adaptive::evaluate_trailing_stop;
    use crate::smarttrend::bot_safety::ExitReason;
    use std::collections::HashMap;

    let symbol = "QBTS";
    let ts_activation = 2.0;
    let ts_deviation = 1.0;
    let ts_min_hold_secs = 10;
    let mut peak_pnl_pcts = HashMap::new();

    // Held 5 s < 10 s (min hold) -> returns None
    assert_eq!(
        evaluate_trailing_stop(symbol, 5, 2.5, ts_activation, ts_deviation, ts_min_hold_secs, &mut peak_pnl_pcts),
        None
    );
    assert!(peak_pnl_pcts.is_empty());

    // Held 15 s, but P&L is 1.5% (< 2.0% activation threshold) -> returns None and does NOT record peak
    assert_eq!(
        evaluate_trailing_stop(symbol, 15, 1.5, ts_activation, ts_deviation, ts_min_hold_secs, &mut peak_pnl_pcts),
        None
    );
    assert!(!peak_pnl_pcts.contains_key(symbol));

    // Held 15 s, P&L is 2.5% (>= 2.0% activation threshold) -> returns None but records peak of 2.5
    assert_eq!(
        evaluate_trailing_stop(symbol, 15, 2.5, ts_activation, ts_deviation, ts_min_hold_secs, &mut peak_pnl_pcts),
        None
    );
    assert_eq!(*peak_pnl_pcts.get(symbol).unwrap(), 2.5);

    // Held 16 s, P&L rises to 3.0% -> returns None and updates peak to 3.0
    assert_eq!(
        evaluate_trailing_stop(symbol, 16, 3.0, ts_activation, ts_deviation, ts_min_hold_secs, &mut peak_pnl_pcts),
        None
    );
    assert_eq!(*peak_pnl_pcts.get(symbol).unwrap(), 3.0);

    // Held 17 s, P&L drops to 2.5% (drop of 0.5% is less than 1.0% deviation) -> returns None
    assert_eq!(
        evaluate_trailing_stop(symbol, 17, 2.5, ts_activation, ts_deviation, ts_min_hold_secs, &mut peak_pnl_pcts),
        None
    );
    assert_eq!(*peak_pnl_pcts.get(symbol).unwrap(), 3.0);

    // Held 18 s, P&L drops to 1.9% (drop of 1.1% from peak of 3.0% >= 1.0% deviation) -> returns TrailingStop exit reason
    assert_eq!(
        evaluate_trailing_stop(symbol, 18, 1.9, ts_activation, ts_deviation, ts_min_hold_secs, &mut peak_pnl_pcts),
        Some(ExitReason::TrailingStop)
    );
    assert_eq!(*peak_pnl_pcts.get(symbol).unwrap(), 3.0);
}

// ── 2026-08-11 — freshness-check side contract (tournament-exit fix) ──

/// Pins the contract that broke `check_tournament_exits`: the
/// `BotAction.direction` handed to `handle_exit` must be the POSITION
/// side, never the close side. `local_position_still_open` matches
/// symbol AND side, so a close-side action aborts every exit.
#[test]
fn local_position_still_open_matches_position_side_not_close_side() {
    use rust_decimal::Decimal;
    use crate::config::RuntimeConfig;
    use crate::state::{Direction, Position, TitanState};
    use super::local_position_still_open;

    let state = TitanState::new(RuntimeConfig::default());
    {
        let mut trading = state.trading.lock().unwrap();
        trading.positions.push(Position {
            symbol: "QBTS".into(),
            qty: Decimal::new(10, 0),
            avg_entry: Decimal::new(500, 2),
            current_price: 5.10,
            unrealized_pnl: 1.0,
            side: Direction::Long,
            opened_at: Some(1_700_000_000),
            source: "bot:frontier".into(),
            first_seen: 1_700_000_000,
            version: 1,
            trade_id: "t1".into(),
            is_swing_intent: false,
            entry_timeframe: "1m".into(),
            stop_loss: 4.80,
            take_profit: 5.60,
        });
    }

    // Position side → open.
    assert!(local_position_still_open(&state, "QBTS", Direction::Long));
    // Close side (what the old code passed) → NOT a match.
    assert!(!local_position_still_open(&state, "QBTS", Direction::Short));
    // Unknown symbol → not open.
    assert!(!local_position_still_open(&state, "SOXS", Direction::Long));

    // Zero-qty placeholder must not count as open.
    {
        let mut trading = state.trading.lock().unwrap();
        trading.positions[0].qty = Decimal::ZERO;
    }
    assert!(!local_position_still_open(&state, "QBTS", Direction::Long));
}

#[test]
fn test_evaluate_hedge_trigger() {
    use crate::state::Direction;
    use super::safety::evaluate_hedge_trigger;

    // 1. Long position, leader drops (change < 0), correlation positive (1.0) -> adverse impact = 1 * -0.004 * 1 = -0.004 < -0.003 -> triggers hedge (Short)
    assert_eq!(
        evaluate_hedge_trigger(1.0, -0.004, 1.0),
        Some(Direction::Short)
    );

    // 2. Long position, leader rises (change > 0), correlation positive -> no hedge (adverse impact is positive)
    assert_eq!(
        evaluate_hedge_trigger(1.0, 0.004, 1.0),
        None
    );

    // 3. Short position, leader rises (change > 0), correlation positive -> adverse impact = -1 * 0.004 * 1 = -0.004 < -0.003 -> triggers hedge (Long)
    assert_eq!(
        evaluate_hedge_trigger(-1.0, 0.004, 1.0),
        Some(Direction::Long)
    );

    // 4. Short position, leader drops (change < 0), correlation positive -> no hedge
    assert_eq!(
        evaluate_hedge_trigger(-1.0, -0.004, 1.0),
        None
    );

    // 5. Borderline case: adverse impact exactly -0.003 -> no trigger
    assert_eq!(
        evaluate_hedge_trigger(1.0, -0.003, 1.0),
        None
    );
}
