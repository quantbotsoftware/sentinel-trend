use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use tauri::{AppHandle, Manager};

use crate::execution::order_exec::{
    has_open_close_order, send_order, ExecutionMetrics, OrderRequest, cancel_order_by_cid,
};
use crate::state::{Direction, TitanState, TradingMode, PENDING_EXIT_TIMEOUT_SECS};
use crate::smarttrend::bot::loop_run::{opposite_direction, BotMetrics};
use crate::smarttrend::bot_safety::ExitReason;

use super::{alpaca_side, local_position_still_open, release_pending_exit_gate};

const PARTIAL_FILL_TIMEOUT_SECS: i64 = 60;

pub async fn check_blown_strategies(
    state: &Arc<TitanState>,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };
    if mode != TradingMode::Auto {
        return;
    }
    let Some(dplus) = state.dplus() else {
        return;
    };

    let now_ts = chrono::Utc::now().timestamp();

    // Phase 1 — find blown strategies under Tournament lock; transition
    // them to Draining (idempotent via `Tournament::drain`).
    let blown_ids: Vec<crate::smarttrend::strategies::StrategyId> = {
        let mut t = dplus.tournament.lock().unwrap_or_else(|p| p.into_inner());
        let blown: Vec<_> = t
            .stats
            .iter()
            .filter(|(_, s)| {
                s.status == crate::smarttrend::tournament::StrategyStatus::Active
                    && s.current_sub_wallet < Decimal::ZERO
            })
            .map(|(sid, _)| *sid)
            .collect();
        for sid in &blown {
            let sub_wallet = t.stats.get(sid).map(|s| s.current_sub_wallet).unwrap_or(Decimal::ZERO);
            t.drain(*sid, now_ts);
            tracing::error!(
                component = "tournament",
                strategy = ?sid,
                sub_wallet = %sub_wallet,
                "S31 EMERGENCY DRAIN — current_sub_wallet went negative, transitioning Active → Draining and force-closing all open positions"
            );
        }
        blown
    };

    if blown_ids.is_empty() {
        return;
    }

    // Phase 2 — find open positions tagged to blown strategies.
    let excluded_for_filter: Vec<String> = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.market.excluded_symbols.clone()
    };
    let positions_to_close: Vec<(String, Direction, Decimal, crate::smarttrend::strategies::StrategyId)> = {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading
            .pending_exits
            .retain(|_, ts| now_ts.saturating_sub(*ts) < PENDING_EXIT_TIMEOUT_SECS);
        let pending = trading.pending_exits.clone();
        trading
            .positions
            .iter()
            .filter(|p| p.qty.abs() > Decimal::ZERO)
            .filter(|p| !pending.contains_key(&p.symbol))
            .filter(|p| !crate::config::is_symbol_excluded(&excluded_for_filter, &p.symbol))
            .filter_map(|p| {
                let sid = crate::smarttrend::dplus::parse_strategy_from_source(&p.source)?;
                if blown_ids.contains(&sid) {
                    Some((p.symbol.clone(), p.side, p.qty, sid))
                } else {
                    None
                }
            })
            .collect()
    };

    if positions_to_close.is_empty() {
        tracing::warn!(
            component = "tournament",
            blown_count = blown_ids.len(),
            "S31 strategies drained but no open positions to liquidate"
        );
        return;
    }

    let (key, secret) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };

    for (symbol, side, qty, sid) in positions_to_close {
        // Tradable check (delisted phantom guard, mirror max-hold path).
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
                    component = "tournament",
                    symbol = %symbol,
                    strategy = ?sid,
                    "S31 blown-strategy exit skipped — asset not tradable"
                );
                continue;
            }
        }

        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_exits.insert(symbol.clone(), now_ts);
        }

        let close_side = opposite_direction(side);

        if has_open_close_order(state, &symbol, alpaca_side(close_side)).await {
            tracing::info!(
                component = "tournament",
                symbol = %symbol,
                "S31 blown-strategy exit skipped — broker already has open close order"
            );
            continue;
        }

        if !local_position_still_open(state, &symbol, side) {
            tracing::info!(
                component = "tournament",
                symbol = %symbol,
                "S31 blown-strategy exit skipped — position closed between detection and submit"
            );
            release_pending_exit_gate(state, &symbol);
            continue;
        }

        tracing::error!(
            component = "tournament",
            symbol = %symbol,
            strategy = ?sid,
            qty = %qty,
            "S31 FORCE-CLOSING position — parent strategy sub-wallet blown"
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
                metrics.record_exit(&ExitReason::TournamentExit);
                let _ = app.emit_all(
                    "bot-exit",
                    serde_json::json!({
                        "symbol": symbol,
                        "reason": "TOURNAMENT_SUB_WALLET_BLOWN",
                        "strategy": format!("{:?}", sid),
                        "qty": qty.to_string(),
                        "broker_id": result.broker_id,
                    }),
                );
            }
            Err(e) => {
                tracing::error!(
                    component = "tournament",
                    symbol = %symbol,
                    error = %e,
                    "S31 blown-strategy exit FAILED — gate retained for 60s cooldown"
                );
            }
        }
    }
}

pub async fn check_max_hold_positions(
    state: &Arc<TitanState>,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    let max_hold_secs = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        i64::from(cfg.smarttrend.max_hold_minutes) * 60
    };

    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };

    if mode != TradingMode::Auto {
        return;
    }

    let now = chrono::Utc::now().timestamp();

    // S16.6 — same duplicate-exit gate as `handle_exit`. We evict stale
    // entries first, then collect only positions that aren't already in
    // a pending-exit state.
    let excluded_for_filter: Vec<String> = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.market.excluded_symbols.clone()
    };
    let expired: Vec<(String, Direction, Decimal)> = {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading
            .pending_exits
            .retain(|_, ts| now.saturating_sub(*ts) < PENDING_EXIT_TIMEOUT_SECS);
        let pending = trading.pending_exits.clone();
        trading
            .positions
            .iter()
            .filter(|p| {
                let anchor = p.opened_at.unwrap_or(p.first_seen);
                anchor > 0
                    && now - anchor >= max_hold_secs
                    && !pending.contains_key(&p.symbol)
            })
            // v2.4.4 — permanent exclusion (delisted phantoms).
            .filter(|p| !crate::config::is_symbol_excluded(&excluded_for_filter, &p.symbol))
            .map(|p| (p.symbol.clone(), p.side, p.qty))
            .collect()
    };

    for (symbol, side, qty) in expired {
        // S17.0 — skip non-tradable assets (delisted, halted, acquired).
        //
        // Live evidence (2026-05-05 14:00 UTC): TRUE is a delisted
        // ticker (TrueCar acquired Q4 2024) that's stuck in a paper
        // account with `status="inactive", tradable=false`. The
        // max-hold loop fired every 30 s, broker returned HTTP 422
        // `asset TRUE is not active`, exit failed, gate cleared after
        // 30 s, retry → 96 identical errors in 1 h plus 1231 noisy
        // log lines. Pre-flight the asset cache and skip if not
        // tradable. The position is dead-equity until corporate
        // actions resolve; nothing the bot does can change that.
        let (key, secret) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
            )
        };
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
                    "MAX HOLD skip — asset not tradable (delisted/halted)"
                );
                continue;
            }
        }

        tracing::warn!(
            component = "bot",
            symbol = %symbol,
            hold_limit_min = max_hold_secs / 60,
            "MAX HOLD expired — closing position"
        );

        // Mark pending before the network call; cleared on success by
        // trade_updates / position_recon, or below on submit failure.
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_exits.insert(symbol.clone(), now);
        }

        let close_side = opposite_direction(side);

        // S17.6 — broker-side orphan-exit guard. See the comment on
        // the same check in `handle_exit` for the live-evidence
        // rationale.
        if has_open_close_order(state, &symbol, alpaca_side(close_side)).await {
            tracing::info!(
                component = "bot",
                symbol = %symbol,
                "MAX HOLD exit skipped — broker already has an open close order in flight"
            );
            continue;
        }

        // v2.4.1 — local-state freshness re-check (see
        // `local_position_still_open`). Identical race as adaptive
        // exit: candidates snapshot, then network await flushes
        // `position_recon`'s closure event past us.
        if !local_position_still_open(state, &symbol, side) {
            tracing::info!(
                component = "bot",
                symbol = %symbol,
                "MAX HOLD exit skipped — position closed by broker between detection and submit"
            );
            release_pending_exit_gate(state, &symbol);
            continue;
        }

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

        match send_order(
            Arc::clone(state), order, app.clone(), exec_metrics,
        ).await {
            Ok(result) => {
                metrics.record_exit(&ExitReason::MaxHold);
                let _ = app.emit_all("bot-exit", serde_json::json!({
                    "symbol": symbol,
                    "reason": "MAX_HOLD",
                    "qty": qty.to_string(),
                    "broker_id": result.broker_id,
                }));
            }
            Err(e) => {
                tracing::error!(
                    component = "bot",
                    symbol = %symbol,
                    error = %e,
                    "Max-hold exit order FAILED"
                );
                // S17.0 — KEEP gate set; let it expire after 30 s (see
                // exit-on-Err comment in `handle_exit`). Releasing
                // immediately produces a 130 ms-period retry storm.
                tracing::warn!(
                    component = "bot",
                    symbol = %symbol,
                    "Max-hold exit gate retained for 60 s cooldown"
                );
            }
        }
    }
}

pub async fn cancel_stale_partial_fills(state: &Arc<TitanState>, app: &AppHandle) {
    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };
    if mode != TradingMode::Auto {
        return;
    }

    let now = chrono::Utc::now().timestamp();
    let stale_cids: Vec<(String, String)> = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading
            .pending_orders
            .iter()
            .filter(|po| {
                if let Some(last_fill) = po.last_fill_at {
                    now - last_fill > PARTIAL_FILL_TIMEOUT_SECS
                } else {
                    false
                }
            })
            .map(|po| (po.client_order_id.clone(), po.symbol.clone()))
            .collect()
    };

    for (cid, symbol) in stale_cids {
        if cid.is_empty() {
            continue;
        }
        tracing::warn!(
            component = "bot",
            symbol = %symbol,
            cid = %cid,
            "Partial-fill stale for >60s — cancelling"
        );
        match cancel_order_by_cid(state, &cid).await {
            Ok(true) => {
                let _ = app.emit_all("log-message", format!(
                    "BOT: Cancelled stale partial-fill order for {symbol} (CID {cid})"
                ));
            }
            Ok(false) => {
                tracing::debug!(component = "bot", cid = %cid, "Stale partial order already resolved");
            }
            Err(e) => {
                tracing::error!(component = "bot", cid = %cid, error = %e, "Failed to cancel stale partial-fill");
            }
        }
    }
}

pub fn evaluate_hedge_trigger(side_sign: f64, change: f64, corr: f64) -> Option<Direction> {
    let adverse_impact = side_sign * change * corr;
    if adverse_impact < -0.003 {
        if change < 0.0 {
            Some(Direction::Short)
        } else {
            Some(Direction::Long)
        }
    } else {
        None
    }
}

pub async fn check_causal_hedging(
    state: &Arc<TitanState>,
    _market: &Arc<crate::market::market_state::MarketDataState>,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };
    if mode != TradingMode::Auto {
        return;
    }

    let Some(dplus) = state.dplus() else {
        return;
    };

    let now_ts = chrono::Utc::now().timestamp();

    // 1. Gather all positions (hedges vs primary) under trading lock
    let (hedges, primary_positions, positions_to_evaluate) = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let mut hedges = Vec::new();
        let mut primary = Vec::new();
        let mut evaluate = Vec::new();
        for p in &trading.positions {
            if p.qty.abs() > Decimal::ZERO {
                if p.source.starts_with("hedge:") {
                    hedges.push((p.symbol.clone(), p.side, p.qty, p.source.clone()));
                } else {
                    primary.push(p.symbol.clone());
                    evaluate.push(p.clone());
                }
            }
        }
        (hedges, primary, evaluate)
    };

    // 2. Close hedges whose primary position has closed
    for (hedge_symbol, side, qty, source) in hedges {
        let primary_symbol = source.strip_prefix("hedge:").unwrap_or("");
        if !primary_positions.contains(&primary_symbol.to_string()) {
            tracing::info!(
                component = "bot",
                hedge_symbol = %hedge_symbol,
                primary_symbol = %primary_symbol,
                "Causal Hedging: primary position closed, closing hedge position"
            );

            // Mark pending exit
            {
                let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                trading.pending_exits.insert(hedge_symbol.clone(), now_ts);
            }

            let close_side = opposite_direction(side);

            if has_open_close_order(state, &hedge_symbol, alpaca_side(close_side)).await {
                tracing::info!(
                    component = "bot",
                    symbol = %hedge_symbol,
                    "Hedge exit skipped — broker already has open close order"
                );
                continue;
            }

            if !local_position_still_open(state, &hedge_symbol, side) {
                release_pending_exit_gate(state, &hedge_symbol);
                continue;
            }

            let order = OrderRequest {
                symbol: hedge_symbol.clone(),
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
                    metrics.record_exit(&ExitReason::CircuitBreaker);
                    let _ = app.emit_all(
                        "bot-exit",
                        serde_json::json!({
                            "symbol": hedge_symbol,
                            "reason": "CIRCUIT_BREAKER",
                            "qty": qty.to_string(),
                            "broker_id": result.broker_id,
                        }),
                    );
                }
                Err(e) => {
                    tracing::error!(
                        component = "bot",
                        symbol = %hedge_symbol,
                        error = %e,
                        "Hedge exit FAILED"
                    );
                    release_pending_exit_gate(state, &hedge_symbol);
                }
            }
        }
    }

    // 3. Open hedges for lagger positions if leader has a sharp adverse movement
    let mut to_hedge: Vec<(String, Direction, Decimal, String, Decimal)> = Vec::new();
    {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading.pending_entries.retain(|_, ts| now_ts.saturating_sub(*ts) < PENDING_EXIT_TIMEOUT_SECS);
        let pending_entries = trading.pending_entries.clone();

        let causal_lock = dplus.causal.lock().unwrap_or_else(|p| p.into_inner());

        for p in &positions_to_evaluate {
            let symbol_b = &p.symbol;
            let direction_b = p.side;
            let side_sign = match direction_b {
                Direction::Long => 1.0,
                Direction::Short => -1.0,
            };

            let leaders = causal_lock.leaders_for(symbol_b);
            for edge in &leaders {
                let symbol_a = &edge.leader;
                let has_pos_a = trading.positions.iter().any(|pos| pos.symbol == *symbol_a && pos.qty.abs() > Decimal::ZERO);
                let is_pending_a = pending_entries.contains_key(symbol_a);
                if !has_pos_a && !is_pending_a {
                    if let Some(change) = causal_lock.recent_change(symbol_a, 10) {
                        if let Some(hedge_direction) = evaluate_hedge_trigger(side_sign, change, edge.corr) {
                            let leader_price = causal_lock.buffers.get(symbol_a).map(|buf| buf.last()).unwrap_or(0.0);
                            if leader_price > 0.0 {
                                let primary_size_usd = p.qty.to_f64().unwrap_or(0.0) * p.current_price;
                                let hedge_size_usd = primary_size_usd * edge.corr.abs() * 0.5;
                                let hedge_qty = Decimal::from_f64((hedge_size_usd / leader_price).trunc()).unwrap_or(Decimal::ZERO);
                                if hedge_qty >= Decimal::ONE {
                                    let est_cost = hedge_qty * Decimal::from_f64(leader_price).unwrap_or(Decimal::ZERO);
                                    let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                                    if st.bot_wallet.debit(est_cost).is_ok() {
                                        let _ = app.emit_all("wallet-changed", serde_json::json!({}));
                                        trading.pending_entries.insert(symbol_a.clone(), now_ts);
                                        to_hedge.push((symbol_a.clone(), hedge_direction, hedge_qty, symbol_b.clone(), est_cost));
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Submit hedge entry orders
    for (symbol_a, hedge_direction, hedge_qty, symbol_b, est_cost) in to_hedge {
        tracing::warn!(
            component = "bot",
            symbol = %symbol_a,
            primary = %symbol_b,
            qty = %hedge_qty,
            direction = ?hedge_direction,
            "Causal Hedging: opening hedge position on leader"
        );

        let order = OrderRequest {
            symbol: symbol_a.clone(),
            side: hedge_direction,
            qty: hedge_qty,
            stop_loss: None,
            take_profit: None,
            source: format!("hedge:{}", symbol_b),
            deducted_amount: Decimal::ZERO,
            order_type: "market".to_string(),
            limit_price: None,
            bar_time: None,
            notional: None,
            is_exit: false,
            is_swing_intent: false,
            entry_timeframe: String::new(),
            entry_score: None,
            conviction: None,
            expected_price: 0.0,
        };

        match send_order(Arc::clone(state), order, app.clone(), exec_metrics).await {
            Ok(result) => {
                metrics.record_entry_executed();
                let _ = app.emit_all("bot-order", serde_json::json!({
                    "symbol": symbol_a,
                    "kind": "Entry",
                    "direction": format!("{:?}", hedge_direction),
                    "qty": hedge_qty.to_string(),
                    "broker_id": result.broker_id,
                }));
            }
            Err(e) => {
                tracing::error!(
                    component = "bot",
                    symbol = %symbol_a,
                    error = %e,
                    "Hedge entry order FAILED, refunding wallet"
                );
                {
                    let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                    st.bot_wallet.balance += est_cost;
                }
                let _ = app.emit_all("wallet-changed", serde_json::json!({}));
                {
                    let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                    trading.pending_entries.remove(&symbol_a);
                }
            }
        }
    }
}
