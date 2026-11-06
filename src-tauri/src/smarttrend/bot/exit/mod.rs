pub mod safety;
pub mod flat_by_close;
pub mod adaptive;

use std::sync::Arc;

use rust_decimal::Decimal;
use tauri::{AppHandle, Manager};

use crate::execution::order_exec::{
    send_order, ExecutionMetrics, OrderRequest,
};
use crate::state::{Direction, TitanState, TradingMode};
use super::loop_run::opposite_direction;
use super::{BotAction, ExitReason};
use super::loop_run::BotMetrics;

pub use safety::{
    check_blown_strategies, check_max_hold_positions, cancel_stale_partial_fills,
    check_causal_hedging,
};
pub use flat_by_close::check_flat_by_close;
pub use adaptive::{check_adaptive_exits, check_tournament_exits};

/// S17.6 — String key for Alpaca `side` field used by
/// `has_open_close_order`. Kept here (instead of impl Direction) so the
/// pure helper stays free of broker-protocol concerns.
pub(crate) fn alpaca_side(d: Direction) -> &'static str {
    match d {
        Direction::Long => "buy",
        Direction::Short => "sell",
    }
}

/// v2.4.1 — local-state freshness guard against position_recon races.
///
/// The exit loops snapshot a candidate Vec under `trading.lock()`, then
/// release the lock to await `is_tradable_now` / `has_open_close_order`
/// (both do network I/O). During that await window, `position_recon`
/// can observe a broker-side fill and remove the position entirely.
/// Without this check the next step would submit a market sell against
/// a flat book — Alpaca rejects with HTTP 422 42210000
/// `asset cannot be sold short` (or the symmetric variant for short
/// positions). Live evidence: ATRA on 2026-05-11T15:36:42 — recon
/// closed the position at .101 ms; the adaptive rail submitted the
/// stale sell at .300 ms and Alpaca rejected.
///
/// We match by symbol *and* side: a hypothetical flip from long→short
/// in the same tick should also be treated as "the trade we wanted to
/// exit is gone" rather than firing a sell against a freshly-opened
/// short. The qty check excludes zero-quantity placeholders.
pub(crate) fn local_position_still_open(state: &TitanState, symbol: &str, side: Direction) -> bool {
    let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
    trading
        .positions
        .iter()
        .any(|p| p.symbol == symbol && p.side == side && p.qty.abs() > Decimal::ZERO)
}

/// Clears the `pending_exits` cooldown row we set immediately before
/// the network call. Used when we *did* set the gate but then short-
/// circuited before `send_order` (e.g., position closed under us, or
/// broker already had an in-flight close). Without this the symbol
/// would be benched for `PENDING_EXIT_TIMEOUT_SECS` for no reason.
pub(crate) fn release_pending_exit_gate(state: &TitanState, symbol: &str) {
    let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
    trading.pending_exits.remove(symbol);
}

pub(super) async fn handle_exit(
    state: &Arc<TitanState>,
    action: &BotAction,
    reason: &ExitReason,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    // S17.0 — Manual mode gate.
    //
    // Signal-driven (FSM `SignalExit`) and stale-data exits should NOT
    // fire when the operator has the bot in Manual: those are strategy
    // decisions and Manual means "don't trade autonomously". Max-hold
    // and circuit-breaker exits remain active because they are safety
    // rails operating on positions the user already holds and may
    // need to fire even when the user steps away.
    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };
    let is_safety_exit = matches!(
        reason,
        ExitReason::MaxHold | ExitReason::CircuitBreaker | ExitReason::ManualClose | ExitReason::FlatByClose
    );
    if mode != TradingMode::Auto && !is_safety_exit {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            reason = %reason,
            "MANUAL mode — strategy exit signal logged, not executed"
        );
        let _ = app.emit_all("bot-signal", serde_json::json!({
            "symbol": action.symbol,
            "kind": "Exit",
            "reason": reason.to_string(),
            "mode": "manual",
        }));
        return;
    }

    // v2.4.4 — permanent exclusion list (TRUE-style phantom positions).
    // Local-only check before any network round-trip. Silent skip.
    {
        let excluded = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.market.excluded_symbols.clone()
        };
        if crate::config::is_symbol_excluded(&excluded, &action.symbol) {
            tracing::trace!(
                component = "bot",
                symbol = %action.symbol,
                "Exit skip — symbol permanently excluded"
            );
            return;
        }
    }

    // S17.6 — Double exit protection (critical race-condition fix).
    //
    // Live evidence: DFTX on 2026-05-05T14:42 — adaptive exit submitted
    // order buy-to-cover at .502 ms; momentum FSM ticked at .510 ms and
    // submitted duplicate buy-to-cover; Alpaca filled both, reversing
    // short into accidental long.
    //
    // Entry condition check: mark the symbol as pending-exit inside
    // `trading` lock before starting any network requests.
    let already_in_flight = {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        if trading.pending_exits.contains_key(&action.symbol) {
            true
        } else {
            trading.pending_exits.insert(action.symbol.clone(), chrono::Utc::now().timestamp());
            false
        }
    };
    if already_in_flight {
        tracing::debug!(
            component = "bot",
            symbol = %action.symbol,
            reason = %reason,
            "Exit skipped — exit sequence already in flight for this symbol"
        );
        return;
    }

    // S17.6 — Broker-side safety check.
    //
    // If Alpaca already has an active close order (e.g. bracket SL/TP
    // leg, or manual order submitted via web dashboard), we must NOT
    // send another market sell. Sending it would either double-exit
    // (if SL gets hit before market order fills) or get rejected with
    // "insufficient shares".
    //
    // `has_open_close_order` performs network I/O to query Alpaca. We
    // drop the lock during the await window. If it returns true, we
    // release the pending exit gate so future exit triggers can retry
    // if the existing order gets cancelled.
    let side_str = alpaca_side(action.direction);
    if crate::execution::order_exec::has_open_close_order(state, &action.symbol, side_str).await {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            reason = %reason,
            "Exit skipped — broker already has an open close order in flight"
        );
        release_pending_exit_gate(state, &action.symbol);
        return;
    }

    // v2.4.1 — local freshness check. Verify position was not closed by
    // recon while we were awaiting the broker query above.
    if !local_position_still_open(state, &action.symbol, action.direction) {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            reason = %reason,
            "Exit aborted — local position closed during broker query window"
        );
        release_pending_exit_gate(state, &action.symbol);
        return;
    }

    // ── Execute Market Exit ─────────────────────────────────────────
    //
    // Retrieve latest target details. Quantities are Decimal to
    // support fractional shares.
    let (qty, target_side) = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let pos = trading.positions.iter().find(|p| p.symbol == action.symbol);
        match pos {
            Some(p) if p.qty.abs() > Decimal::ZERO => {
                let exit_qty = p.qty.abs();
                let exit_side = opposite_direction(p.side);
                (exit_qty, exit_side)
            }
            _ => {
                tracing::warn!(
                    component = "bot",
                    symbol = %action.symbol,
                    "Exit aborted — no open position found in local state"
                );
                release_pending_exit_gate(state, &action.symbol);
                return;
            }
        }
    };

    let cid = format!(
        "st-exit-{}-{:?}",
        action.symbol.to_lowercase(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );

    tracing::info!(
        component = "bot",
        symbol = %action.symbol,
        qty = %qty,
        side = ?target_side,
        reason = %reason,
        cid = %cid,
        "Submitting AUTO exit order"
    );

    let req = OrderRequest {
        symbol: action.symbol.clone(),
        side: target_side,
        qty: qty.abs(),
        stop_loss: None,
        take_profit: None,
        source: String::new(),
        deducted_amount: Decimal::ZERO,
        order_type: "market".to_string(),
        limit_price: None,
        bar_time: None,
        notional: None,
        is_exit: true,
        is_swing_intent: false,
        entry_timeframe: String::new(),
        entry_score: None,
        conviction: None,
        // P5 — score the exit fill against the signal price that
        // triggered the exit (best decision-time anchor available).
        expected_price: action.price,
    };

    // S05: emit-after-drop pattern. We hold no state locks here.
    match send_order(Arc::clone(state), req, app.clone(), exec_metrics).await {
        Ok(result) => {
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                broker_id = %result.broker_id,
                cid = %result.client_order_id,
                "Exit order accepted"
            );
            metrics.record_exit(reason);

            let _ = app.emit_all(
                "bot-exit",
                serde_json::json!({
                    "symbol": action.symbol,
                    "qty": qty,
                    "side": format!("{:?}", target_side),
                    "reason": reason.to_string(),
                    "broker_id": result.broker_id,
                    "cid": cid,
                }),
            );
        }
        Err(e) => {
            tracing::error!(
                component = "bot",
                symbol = %action.symbol,
                error = %e,
                "Exit order FAILED"
            );
            // S17.6: release pending gate on failure so next tick can retry.
            release_pending_exit_gate(state, &action.symbol);
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
