use std::sync::Arc;

use rust_decimal::Decimal;
use tauri::{AppHandle, Manager};
use chrono::Timelike;

use crate::execution::order_exec::{
    has_open_close_order, send_order, ExecutionMetrics, OrderRequest,
};
use crate::state::{Direction, TitanState, TradingMode, PENDING_EXIT_TIMEOUT_SECS};
use crate::smarttrend::bot::loop_run::{opposite_direction, BotMetrics};
use crate::smarttrend::bot_safety::ExitReason;

use super::{alpaca_side, local_position_still_open, release_pending_exit_gate};

pub async fn check_flat_by_close(
    state: &Arc<TitanState>,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    let flat_by_close_minutes: u32 = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.smarttrend.flat_by_close_minutes.min(60)
    };

    let has_legacy_positions = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading.positions.iter().any(|p| p.qty != rust_decimal::Decimal::ZERO && p.source == "bot")
    };

    if flat_by_close_minutes == 0 && !has_legacy_positions {
        return;
    }

    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };
    if mode != TradingMode::Auto {
        return;
    }

    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let in_rth_now = crate::market::scanner::in_rth(now_et);
    if !in_rth_now {
        return;
    }

    let minutes_to_close: i32 =
        (16 - now_et.hour() as i32) * 60 - now_et.minute() as i32;

    let effective_max_minutes = if has_legacy_positions {
        flat_by_close_minutes.max(10)
    } else {
        flat_by_close_minutes
    };

    // Trigger window: [0, effective_max_minutes). Once we pass 16:00
    // (`minutes_to_close < 0`) RTH gate above already kicked us out,
    // so this branch only catches the intended pre-close window.
    if minutes_to_close < 0 || (minutes_to_close as u32) >= effective_max_minutes {
        return;
    }

    let now = chrono::Utc::now().timestamp();
    let excluded_for_filter: Vec<String> = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.market.excluded_symbols.clone()
    };
    // S25 (2026-05-28) — Bypass `pending_exits` anti-spam guard when we
    // are inside the critical close-window (T ≤ 2 minutes to 16:00 ET).
    //
    // Live evidence 2026-05-26 PTON case:
    //   * Position entered 10:02 ET, ran past max_hold (60min) and
    //     triggered `check_adaptive_exits` QuickCut → bot re-inserted
    //     PTON into `pending_exits` every 30s tick.
    //   * `check_flat_by_close` runs AFTER `check_adaptive_exits` in the
    //     same tick (loop_run.rs:246-254), so PTON was always present in
    //     `pending_exits` when this function ran → ALWAYS filtered out.
    //   * Result: flat-by-close never logged "window entered" and never
    //     attempted the C5 force-cancel & market-replace path. Operator
    //     had to close PTON manually via Alpaca API at 15:59 ET.
    //
    // Fix: at T ≤ 2min we override the anti-spam guard. Inside the
    // critical window we accept the risk of one extra `pending_exits`
    // re-entry per tick — being flat at the close beats anti-spam
    // hygiene. Outside the critical window the original guard remains
    // active so we don't storm Alpaca with duplicate cancel requests
    // during the 8-min C5 grace period (T-10min → T-2min).
    let near_close_critical = minutes_to_close <= 2;
    let candidates: Vec<(String, Direction, Decimal)> = {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading
            .pending_exits
            .retain(|_, ts| now.saturating_sub(*ts) < PENDING_EXIT_TIMEOUT_SECS);
        let pending = trading.pending_exits.clone();
        trading
            .positions
            .iter()
            .filter(|p| {
                let threshold = if p.source == "bot" {
                    flat_by_close_minutes.max(10)
                } else {
                    flat_by_close_minutes
                };
                if threshold == 0 {
                    return false;
                }
                // S25: critical window overrides pending_exits guard.
                (near_close_critical || !pending.contains_key(&p.symbol))
                    && p.qty != Decimal::ZERO
                    && (minutes_to_close as u32) < threshold
            })
            // v2.4.4 — pre-filter permanently excluded tickers so the WARN
            // log + recon attempts don't fire 30s for stranded phantoms.
            .filter(|p| !crate::config::is_symbol_excluded(&excluded_for_filter, &p.symbol))
            // S28 — skip swing-intent positions: they were intentionally
            // entered in the late-RTH PDT-bypass window to be held
            // overnight. Force-flatting them at 16:00 ET would turn each
            // into a day trade #4 → broker PDT flag → 90-day restriction.
            .filter(|p| {
                if p.is_swing_intent {
                    tracing::info!(
                        component = "bot",
                        symbol = %p.symbol,
                        "S28 FLAT-BY-CLOSE skipping swing-intent position — held overnight by design"
                    );
                    false
                } else {
                    true
                }
            })
            .map(|p| (p.symbol.clone(), p.side, p.qty))
            .collect()
    };

    if candidates.is_empty() {
        return;
    }

    tracing::warn!(
        component = "bot",
        now_et = %now_et.format("%H:%M:%S"),
        minutes_to_close,
        positions = candidates.len(),
        "FLAT-BY-CLOSE window entered — liquidating open positions"
    );

    let (key, secret, excluded) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.excluded_symbols.clone(),
        )
    };

    for (symbol, side, qty) in candidates {
        // v2.4.4 — permanent exclusion list. Silently skip phantom
        // tickers (e.g. delisted `TRUE`) that Alpaca leaves stranded.
        if crate::config::is_symbol_excluded(&excluded, &symbol) {
            tracing::trace!(component = "bot", symbol = %symbol, "FLAT-BY-CLOSE skip — symbol permanently excluded");
            continue;
        }
        if !key.is_empty() && !secret.is_empty() {
            let tradable = crate::market::scanner::is_tradable_now(
                &state.http_client,
                &key,
                &secret,
                &symbol,
            )
            .await;
            if !tradable {
                tracing::debug!(
                    component = "bot",
                    symbol = %symbol,
                    "FLAT-BY-CLOSE skip — asset not tradable"
                );
                continue;
            }
        }

        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_exits.insert(symbol.clone(), now);
        }

        let close_side = opposite_direction(side);

        // v2.5.0 NUCLEAR (C5) — Force-cancel & market-replace stale close
        // orders at T-2min from close.
        //
        // Live evidence 2026-05-14 19:58-19:59 UTC: bot logged
        // "FLAT-BY-CLOSE skip — broker already has an open close order"
        // on ARMG/BCRX over and over because earlier bracket SL orders
        // sat at limit prices that never filled. At 4:00 PM the market
        // closed and 16 positions remained open overnight. The fix:
        // when we're within 2 minutes of close and an open close order
        // exists, cancel it and resubmit as MARKET so the broker fills
        // at any price into the close.
        let close_order_present =
            has_open_close_order(state, &symbol, alpaca_side(close_side)).await;
        if close_order_present {
            if minutes_to_close <= 2 {
                tracing::warn!(
                    component = "bot",
                    symbol = %symbol,
                    minutes_to_close,
                    "FLAT-BY-CLOSE force-cancelling stale close orders — will resubmit as MARKET"
                );
                if let Err(e) = crate::execution::order_exec::cancel_open_orders_for_symbol(
                    state, &symbol,
                )
                .await
                {
                    tracing::warn!(
                        component = "bot",
                        symbol = %symbol,
                        error = %e,
                        "FLAT-BY-CLOSE cancel_open_orders_for_symbol failed — continuing anyway"
                    );
                }
                // Small grace period for the cancel to register at the broker.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                // S25 — re-check broker state after cancel. If the bracket
                // canceled successfully, we proceed to market submit. If a
                // race left another close order in flight (e.g. partial
                // fill chain), log and skip to avoid duplicate submits.
                if has_open_close_order(state, &symbol, alpaca_side(close_side)).await {
                    tracing::warn!(
                        component = "bot",
                        symbol = %symbol,
                        "FLAT-BY-CLOSE post-cancel re-check still shows open close order — skipping market submit"
                    );
                    release_pending_exit_gate(state, &symbol);
                    continue;
                }
            } else {
                tracing::info!(
                    component = "bot",
                    symbol = %symbol,
                    "FLAT-BY-CLOSE skip — broker already has an open close order (>2min to close, letting it work)"
                );
                release_pending_exit_gate(state, &symbol);
                continue;
            }
        }

        if !local_position_still_open(state, &symbol, side) {
            tracing::info!(
                component = "bot",
                symbol = %symbol,
                "FLAT-BY-CLOSE skip — position closed by broker between detection and submit"
            );
            release_pending_exit_gate(state, &symbol);
            continue;
        }

        tracing::warn!(
            component = "bot",
            symbol = %symbol,
            side = ?side,
            %qty,
            minutes_to_close,
            "FLAT-BY-CLOSE submitting close order"
        );

        let order = OrderRequest {
            symbol: symbol.clone(),
            side: close_side,
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
            expected_price: 0.0,
        };

        match send_order(Arc::clone(state), order, app.clone(), exec_metrics).await {
            Ok(result) => {
                metrics.record_exit(&ExitReason::FlatByClose);
                let _ = app.emit_all(
                    "bot-exit",
                    serde_json::json!({
                        "symbol": symbol,
                        "reason": "FLAT_BY_CLOSE",
                        "qty": qty.to_string(),
                        "broker_id": result.broker_id,
                    }),
                );
            }
            Err(e) => {
                tracing::error!(
                    component = "bot",
                    symbol = %symbol,
                    error = %e,
                    "Flat-by-close exit order FAILED"
                );
            }
        }
    }
}
