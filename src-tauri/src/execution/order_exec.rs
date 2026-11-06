use std::sync::Arc;
use std::time::Instant;

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::error::TitanError;
use crate::state::{Direction, OrderHistoryEntry, PendingOrder, TitanState};

// ═══════════════════════════════════════════════════════════════════════
//  Types
// ═══════════════════════════════════════════════════════════════════════

/// Everything needed to submit an order. Built by bot (Session 9) or
/// manual trade command (Session 10).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: Direction,
    pub qty: Decimal,
    pub stop_loss: Option<Decimal>,
    pub take_profit: Option<Decimal>,
    /// "bot" | "user" — propagated to PendingOrder and then Position
    #[allow(dead_code)]
    pub source: String,
    /// How much was withdrawn from the source wallet. Propagated to PendingOrder
    /// so rejected/canceled orders can refund the correct amount.
    pub deducted_amount: Decimal,
    /// "market" or "limit"
    pub order_type: String,
    pub limit_price: Option<Decimal>,
    /// S03: bar_time from signal for deterministic CID on bot orders.
    /// None for manual/user orders (uses UUID).
    pub bar_time: Option<i64>,
    /// S08: dollar-notional for sub-1-share fractionable orders.
    /// When `Some`, body uses `"notional"` instead of `"qty"`.
    /// Bracket orders (stop_loss/take_profit) are incompatible.
    pub notional: Option<Decimal>,
    /// S16.6 — set by exit.rs / manual close paths. When true:
    ///   * the asset-metadata cache miss is downgraded from a hard reject
    ///     to a warn-and-proceed (we already own the position so it must
    ///     have been a valid asset at entry time).
    ///   * any open orders for this symbol (e.g. bracket TP/SL legs from
    ///     the original entry) are cancelled BEFORE the closing order is
    ///     submitted, otherwise Alpaca returns 403 with `held_for_orders`
    ///     equal to the position qty and the close fails.
    pub is_exit: bool,
    /// S28: swing-intent entry permitted only by the PDT swing-only
    /// override. Propagated to PendingOrder → Position so downstream
    /// (`check_flat_by_close`) can skip the position at 16:00 ET and
    /// let it cross overnight. `false` for every other entry.
    pub is_swing_intent: bool,
    /// S37 (2026-05-28 22:55 ET): the timeframe of the FSM that produced
    /// the entry signal authoring this order. Propagated to PendingOrder
    /// → Position so `bot/exit.rs::handle_exit` can gate `SignalExit`
    /// against cross-TF FSM closes. Empty string for manual orders /
    /// closes / legacy callers — gate is bypassed when empty.
    pub entry_timeframe: String,
    /// Strategy-side entry score that authored this order. `Some` for
    /// Tournament strategy entries; `None` for legacy momentum / manual
    /// / exit orders. Propagated to `OrderHistoryEntry` so
    /// `position_recon` can journal the real signal strength even when
    /// the WebSocket fill stream is silent (paper accounts).
    pub entry_score: Option<f64>,
    /// PRIME F3 — fused conviction of the arbitration winner that
    /// authored this order. Propagated to `PendingOrder` → journal →
    /// chart marker. `None` for manual / exit / legacy orders.
    pub conviction: Option<f64>,
    /// P5 — live price observed when the order was authored (the
    /// price the DECISION was made at). The slippage ledger scores
    /// the eventual fill against it. `0.0` = unknown (no slippage
    /// row recorded).
    pub expected_price: f64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct OrderResult {
    pub broker_id: String,
    pub client_order_id: String,
    pub status: String,
    pub symbol: String,
    #[serde(with = "crate::serde_decimal")]
    pub qty: Decimal,
    pub side: String,
}

// ═══════════════════════════════════════════════════════════════════════
//  Execution Metrics (BONUS)
// ═══════════════════════════════════════════════════════════════════════

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecutionMetrics {
    pub orders_submitted: u64,
    pub orders_filled: u64,
    pub orders_rejected: u64,
    pub total_latency_ms: u64,
    pub last_latency_ms: u64,
}

impl ExecutionMetrics {
    pub fn avg_latency_ms(&self) -> f64 {
        if self.orders_submitted == 0 {
            return 0.0;
        }
        self.total_latency_ms as f64 / self.orders_submitted as f64
    }

    fn record(&mut self, latency_ms: u64) {
        self.orders_submitted += 1;
        self.total_latency_ms += latency_ms;
        self.last_latency_ms = latency_ms;
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Order Audit Trail
// ═══════════════════════════════════════════════════════════════════════

struct OrderLogEntry<'a> {
    symbol: &'a str,
    side: &'a str,
    qty: Decimal,
    payload: &'a serde_json::Value,
    response: &'a str,
    latency_ms: u64,
    client_order_id: &'a str,
    broker_id: Option<&'a str>,
    retry_count: u32,
}

#[allow(dead_code)]
fn log_order_attempt(e: &OrderLogEntry<'_>) {
    let log_dir = crate::paths::data_dir().join("orders");
    let _ = std::fs::create_dir_all(&log_dir);
    let json_entry = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "symbol": e.symbol,
        "side": e.side,
        "qty": e.qty,
        "payload": e.payload,
        "response": e.response,
        "latency_ms": e.latency_ms,
        "client_order_id": e.client_order_id,
        "broker_id": e.broker_id,
        "retry_count": e.retry_count,
    });
    let path = log_dir.join("order_log.jsonl");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{}", json_entry);
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Order Submission
// ═══════════════════════════════════════════════════════════════════════

#[allow(dead_code)]
fn side_str(d: &Direction) -> &'static str {
    match d {
        Direction::Long => "buy",
        Direction::Short => "sell",
    }
}

#[allow(dead_code)]
fn validate_order(order: &OrderRequest) -> Result<(), TitanError> {
    if order.symbol.is_empty() {
        return Err(TitanError::Validation("Symbol cannot be empty".into()));
    }
    // notional path: qty may be zero when notional is set
    if order.notional.is_none() && order.qty <= Decimal::ZERO {
        return Err(TitanError::Validation(format!(
            "Invalid qty={} for {}",
            order.qty, order.symbol
        )));
    }
    // S15.11: bracket orders require BOTH legs (Alpaca constraint).
    // Reject single-leg bracket fast — this is the regression-lock
    // that pins the S15.11 fix in entry.rs.
    let has_sl = order.stop_loss.is_some();
    let has_tp = order.take_profit.is_some();
    if has_sl != has_tp {
        return Err(TitanError::Validation(
            "Bracket order requires BOTH take_profit and stop_loss \
             (Alpaca limitation: code 40010001). Set both or neither."
                .into(),
        ));
    }
    // Bracket + notional incompatibility (Alpaca limitation)
    if order.notional.is_some() && (order.stop_loss.is_some() || order.take_profit.is_some()) {
        return Err(TitanError::Validation(
            "Bracket+notional incompatible (Alpaca limitation): qty<1 fractionable cannot use bracket; size up or split entry".into(),
        ));
    }
    if let Some(tp) = order.take_profit {
        if tp <= Decimal::ZERO {
            return Err(TitanError::Validation(format!(
                "Invalid take_profit={} for {}",
                tp, order.symbol
            )));
        }
    }
    if let Some(sl) = order.stop_loss {
        if sl <= Decimal::ZERO {
            return Err(TitanError::Validation(format!(
                "Invalid stop_loss={} for {}",
                sl, order.symbol
            )));
        }
    }
    if order.order_type == "limit" {
        match order.limit_price {
            Some(lp) if lp > Decimal::ZERO => {}
            _ => {
                return Err(TitanError::Validation(format!(
                    "Limit order requires a valid limit_price > 0 for {}",
                    order.symbol
                )));
            }
        }
    }
    Ok(())
}

#[allow(dead_code, clippy::too_many_arguments)]
fn build_order_body(
    symbol: &str,
    side: &str,
    qty: Decimal,
    stop_loss: Option<Decimal>,
    take_profit: Option<Decimal>,
    client_order_id: &str,
    order_type: &str,
    limit_price: Option<Decimal>,
    notional: Option<Decimal>,
) -> serde_json::Value {
    fn fmt2(d: Decimal) -> String {
        format!("{:.2}", d)
    }
    fn fmt6(d: Decimal) -> String {
        d.round_dp(6).to_string()
    }

    let has_bracket = stop_loss.is_some() || take_profit.is_some();

    if let Some(n) = notional {
        let mut body = serde_json::json!({
            "symbol": symbol,
            "notional": fmt2(n),
            "side": side,
            "type": order_type,
            "time_in_force": "day",
            "client_order_id": client_order_id,
        });
        if order_type == "limit" {
            if let Some(lp) = limit_price {
                body["limit_price"] = serde_json::json!(fmt2(lp));
            }
        }
        return body;
    }

    let qty_str = fmt6(qty);

    if has_bracket {
        let mut body = serde_json::json!({
            "symbol": symbol,
            "qty": qty_str,
            "side": side,
            "type": order_type,
            "time_in_force": "day",
            "order_class": "bracket",
            "client_order_id": client_order_id,
        });
        if order_type == "limit" {
            if let Some(lp) = limit_price {
                body["limit_price"] = serde_json::json!(fmt2(lp));
            }
        }
        if let Some(tp) = take_profit {
            body["take_profit"] = serde_json::json!({ "limit_price": fmt2(tp) });
        }
        if let Some(sl) = stop_loss {
            body["stop_loss"] = serde_json::json!({ "stop_price": fmt2(sl) });
        }
        body
    } else {
        let mut body = serde_json::json!({
            "symbol": symbol,
            "qty": qty_str,
            "side": side,
            "type": order_type,
            "time_in_force": "day",
            "client_order_id": client_order_id,
        });
        if order_type == "limit" {
            if let Some(lp) = limit_price {
                body["limit_price"] = serde_json::json!(fmt2(lp));
            }
        }
        body
    }
}

const MAX_ORDER_HISTORY: usize = 500;

fn push_history(state: &TitanState, entry: OrderHistoryEntry) {
    let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
    if trading.order_history.len() >= MAX_ORDER_HISTORY {
        trading.order_history.remove(0);
    }
    trading.order_history.push(entry);
}

/// Submit an order to Alpaca. Handles dry-run, validation, bracket
/// construction, retry on 5xx, and audit trail logging.
pub async fn send_order(
    state: Arc<TitanState>,
    order: OrderRequest,
    app: AppHandle,
    metrics: &mut ExecutionMetrics,
) -> Result<OrderResult, TitanError> {
    validate_order(&order)?;

    let symbol = order.symbol.trim().to_uppercase();
    let side = side_str(&order.side);
    let qty = order.qty;

    // ── S02 / S16.7: Fractionable gate ──
    //
    // For exits we relax the cache-miss reject to a warn: by definition we
    // already hold this symbol, so it was tradable at some point. Refusing
    // to close because the metadata cache hasn't been refreshed yet leaks
    // capital into a position the bot can never exit (observed live with
    // `TRUE` after the symbol's snapshot was missed during refresh).
    let asset = crate::market::scanner::lookup_asset(&symbol).await;
    match asset.as_ref() {
        None if order.is_exit => {
            tracing::warn!(
                component = "order",
                symbol = %symbol,
                "Asset metadata cache miss on EXIT — proceeding anyway (we already own qty)"
            );
        }
        None => {
            return Err(TitanError::Validation(format!(
                "{symbol}: asset metadata unknown (cache miss); rejecting conservatively"
            )));
        }
        Some(a) if !a.fractionable && qty.fract() != Decimal::ZERO => {
            return Err(TitanError::Validation(format!(
                "{symbol} not fractionable but qty={qty} (fractional); use whole shares only"
            )));
        }
        _ => {}
    }

    // ── S16.6: Cancel held bracket legs before exit ──
    //
    // Alpaca holds the position qty in `held_for_orders` while the bracket
    // TP/SL legs are open, so submitting a fresh closing order returns 403
    // `insufficient qty available`. We cancel any open orders for this
    // symbol first; failures during cleanup are logged but non-fatal so a
    // partially-cancelled state still gets a chance to close.
    if order.is_exit {
        match cancel_open_orders_for_symbol(&state, &symbol).await {
            Ok(n) if n > 0 => {
                tracing::info!(
                    component = "order",
                    symbol = %symbol,
                    cancelled = n,
                    "Cancelled open bracket legs before exit"
                );
                // Brief pause to give Alpaca's `held_for_orders` accounting
                // a moment to release. 800ms is highly reliable for paper trading.
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            Ok(_) => {
                // No open orders — fast path, nothing to cancel.
            }
            Err(e) => {
                tracing::warn!(
                    component = "order",
                    symbol = %symbol,
                    error = %e,
                    "cancel_open_orders_for_symbol failed; attempting exit anyway"
                );
            }
        }
    }

    // ── Build payload ──
    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };
    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation("API keys missing".into()));
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    let client_order_id = if let Some(bt) = order.bar_time {
        let mut hasher = Sha256::new();
        hasher.update(format!("{}|{}|{}|smarttrend", symbol, side, bt));
        let hash = hasher.finalize();
        let hex_str: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
        format!("st-{hex_str}")
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    let body = build_order_body(
        &symbol,
        side,
        qty,
        order.stop_loss,
        order.take_profit,
        &client_order_id,
        &order.order_type,
        order.limit_price,
        order.notional,
    );

    let url = format!("{}/v2/orders", base_url);
    let has_bracket = order.stop_loss.is_some() || order.take_profit.is_some();

    let _ = app.emit_all(
        "log-message",
        format!("ORDER: {} {} x{}", side.to_uppercase(), symbol, qty),
    );

    let intent = crate::execution::order_wal::OrderIntent {
        client_order_id: client_order_id.clone(),
        symbol: symbol.clone(),
        qty,
        side: order.side,
        strategy_id: if order.source.starts_with("s2:") {
            order.source.strip_prefix("s2:").map(|s| s.to_string())
        } else {
            None
        },
        timestamp: chrono::Utc::now().timestamp(),
        deducted_amount: order.deducted_amount,
        order_type: order.order_type.clone(),
        limit_price: order.limit_price,
        stop_loss: order.stop_loss,
        take_profit: order.take_profit,
        is_exit: order.is_exit,
        is_swing_intent: order.is_swing_intent,
        entry_timeframe: order.entry_timeframe.clone(),
        entry_score: order.entry_score,
    };

    if let Err(e) = crate::execution::order_wal::write_intent(&intent) {
        tracing::error!(
            component = "order",
            client_order_id = %client_order_id,
            error = %e,
            "Failed to write WAL intent; refusing to submit order"
        );
        return Err(TitanError::Validation("WAL write failed".into()));
    }

    // ── Submit with retry ──
    const MAX_RETRIES: u32 = 2;
    let mut last_err = String::new();

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tracing::warn!(
                component = "order",
                symbol = %symbol,
                attempt,
                "Retrying order submission (idempotent via client_order_id)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt))).await;
        }

        let t0 = Instant::now();
        let send_result = state
            .http_client
            .post(&url)
            .header("APCA-API-KEY-ID", &key)
            .header("APCA-API-SECRET-KEY", &secret)
            .json(&body)
            .send()
            .await;
        let latency_ms = t0.elapsed().as_millis() as u64;
        metrics.record(latency_ms);

        match send_result {
            Err(e) => {
                last_err = format!("Network error: {e}");
                log_order_attempt(&OrderLogEntry {
                    symbol: &symbol,
                    side,
                    qty,
                    payload: &body,
                    response: &format!("NET_ERR attempt {attempt}: {e}"),
                    latency_ms,
                    client_order_id: &client_order_id,
                    broker_id: None,
                    retry_count: attempt,
                });
                if attempt < MAX_RETRIES {
                    continue;
                }
                // S03: persist PendingOrder with CID even on network failure.
                // The order may have reached the broker despite the network
                // error. cancel_by_cid can clean it up later.
                {
                    let now = chrono::Utc::now().timestamp();
                    let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                    trading.pending_orders.push(PendingOrder {
                        order_id: String::new(),
                        symbol: symbol.clone(),
                        side: order.side,
                        qty,
                        submitted_at: now,
                        source: order.source.clone(),
                        deducted_amount: order.deducted_amount,
                        client_order_id: client_order_id.clone(),
                        last_fill_at: None,
                        is_swing_intent: order.is_swing_intent,
                        entry_timeframe: order.entry_timeframe.clone(),
                        stop_loss: order.stop_loss.and_then(|d| d.to_f64()).unwrap_or(0.0),
                        take_profit: order.take_profit.and_then(|d| d.to_f64()).unwrap_or(0.0),
                        conviction: order.conviction,
                        expected_price: order.expected_price,
                    });
                }
                push_history(
                    &state,
                    OrderHistoryEntry {
                        order_id: client_order_id.clone(),
                        symbol: symbol.clone(),
                        side: side.into(),
                        qty,
                        order_type: order.order_type.clone(),
                        limit_price: order.limit_price,
                        status: "failed".into(),
                        source: order.source.clone(),
                        submitted_at: chrono::Utc::now().timestamp(),
                        filled_price: None,
                        filled_at: None,
                        reject_reason: Some(last_err.clone()),
                        entry_score: order.entry_score,
                        entry_timeframe: order.entry_timeframe.clone(),
                    },
                );
                return Err(TitanError::Network(last_err));
            }
            Ok(res) => {
                let status_code = res.status();

                if status_code.is_server_error() && attempt < MAX_RETRIES {
                    let txt = res.text().await.unwrap_or_default();
                    last_err = format!("Server error {status_code}: {txt}");
                    log_order_attempt(&OrderLogEntry {
                        symbol: &symbol,
                        side,
                        qty,
                        payload: &body,
                        response: &format!("5xx attempt {attempt}: {status_code} {txt}"),
                        latency_ms,
                        client_order_id: &client_order_id,
                        broker_id: None,
                        retry_count: attempt,
                    });
                    continue;
                }

                if !status_code.is_success() {
                    let txt = res.text().await.unwrap_or_default();
                    log_order_attempt(&OrderLogEntry {
                        symbol: &symbol,
                        side,
                        qty,
                        payload: &body,
                        response: &format!("ERR {status_code}: {txt}"),
                        latency_ms,
                        client_order_id: &client_order_id,
                        broker_id: None,
                        retry_count: attempt,
                    });
                    let reason = format!("Alpaca API {status_code}: {txt}");
                    push_history(
                        &state,
                        OrderHistoryEntry {
                            order_id: client_order_id.clone(),
                            symbol: symbol.clone(),
                            side: side.into(),
                            qty,
                            order_type: order.order_type.clone(),
                            limit_price: order.limit_price,
                            status: "failed".into(),
                            source: order.source.clone(),
                            submitted_at: chrono::Utc::now().timestamp(),
                            filled_price: None,
                            filled_at: None,
                            reject_reason: Some(reason.clone()),
                            entry_score: order.entry_score,
                            entry_timeframe: order.entry_timeframe.clone(),
                        },
                    );
                    let _ = crate::execution::order_wal::clear_intent(&client_order_id);
                    return Err(TitanError::Broker(reason));
                }

                let resp_json: serde_json::Value = res.json().await.map_err(|e| {
                    TitanError::Broker(format!("Alpaca returned invalid JSON: {e}"))
                })?;
                let broker_id = resp_json["id"].as_str().unwrap_or("").to_string();
                let order_status = resp_json["status"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();

                let suffix = if has_bracket { " (BRACKET: TP+SL)" } else { "" };
                log_order_attempt(&OrderLogEntry {
                    symbol: &symbol,
                    side,
                    qty,
                    payload: &body,
                    response: &format!("OK{suffix} broker_id={broker_id} status={order_status}"),
                    latency_ms,
                    client_order_id: &client_order_id,
                    broker_id: Some(&broker_id),
                    retry_count: attempt,
                });

                // Track pending order + history
                {
                    let now = chrono::Utc::now().timestamp();
                    let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                    trading.pending_orders.push(PendingOrder {
                        order_id: broker_id.clone(),
                        symbol: symbol.clone(),
                        side: order.side,
                        qty,
                        submitted_at: now,
                        source: order.source.clone(),
                        deducted_amount: order.deducted_amount,
                        client_order_id: client_order_id.clone(),
                        last_fill_at: None,
                        is_swing_intent: order.is_swing_intent,
                        entry_timeframe: order.entry_timeframe.clone(),
                        stop_loss: order.stop_loss.and_then(|d| d.to_f64()).unwrap_or(0.0),
                        take_profit: order.take_profit.and_then(|d| d.to_f64()).unwrap_or(0.0),
                        conviction: order.conviction,
                        expected_price: order.expected_price,
                    });
                    if trading.order_history.len() >= MAX_ORDER_HISTORY {
                        trading.order_history.remove(0);
                    }
                    trading.order_history.push(OrderHistoryEntry {
                        order_id: broker_id.clone(),
                        symbol: symbol.clone(),
                        side: side.into(),
                        qty,
                        order_type: order.order_type.clone(),
                        limit_price: order.limit_price,
                        status: order_status.clone(),
                        source: order.source.clone(),
                        submitted_at: now,
                        filled_price: None,
                        filled_at: None,
                        reject_reason: None,
                        entry_score: order.entry_score,
                        entry_timeframe: order.entry_timeframe.clone(),
                    });
                }

                let _ = app.emit_all(
                    "log-message",
                    format!(
                        "ORDER ACCEPTED{}: {} {} x{} [{}]",
                        suffix,
                        side.to_uppercase(),
                        symbol,
                        qty,
                        &broker_id[..broker_id.len().min(8)]
                    ),
                );

                let _ = app.emit_all(
                    "order-lifecycle",
                    serde_json::json!({
                        "event": "submitted",
                        "broker_id": broker_id,
                        "client_order_id": client_order_id,
                        "symbol": symbol,
                        "side": side,
                        "qty": qty.to_string(),
                        "status": order_status,
                        "latency_ms": latency_ms,
                        "retries": attempt,
                    }),
                );

                // Emit metrics every 10 orders
                if metrics.orders_submitted.is_multiple_of(10) {
                    let _ = app.emit_all(
                        "execution-metrics",
                        serde_json::json!({
                            "orders_submitted": metrics.orders_submitted,
                            "orders_filled": metrics.orders_filled,
                            "orders_rejected": metrics.orders_rejected,
                            "avg_latency_ms": metrics.avg_latency_ms(),
                            "last_latency_ms": metrics.last_latency_ms,
                        }),
                    );
                }

                return Ok(OrderResult {
                    broker_id,
                    client_order_id,
                    status: order_status,
                    symbol,
                    qty,
                    side: side.into(),
                });
            }
        }
    }

    let reason = format!("Order failed after {MAX_RETRIES} retries: {last_err}");
    push_history(
        &state,
        OrderHistoryEntry {
            order_id: String::new(),
            symbol,
            side: side.into(),
            qty,
            order_type: order.order_type.clone(),
            limit_price: order.limit_price,
            status: "failed".into(),
            source: order.source.clone(),
            submitted_at: chrono::Utc::now().timestamp(),
            filled_price: None,
            filled_at: None,
            reject_reason: Some(reason.clone()),
            entry_score: order.entry_score,
            entry_timeframe: order.entry_timeframe.clone(),
        },
    );
    Err(TitanError::Network(reason))
}

// ═══════════════════════════════════════════════════════════════════════
//  Cancel all open orders (S03: cancel-before-reset)
// ═══════════════════════════════════════════════════════════════════════

/// Cancel all open orders at the broker via `DELETE /v2/orders`.
/// Returns the number of orders that the broker reported as cancelled.
/// Called by daily_reset before clearing local state.
pub async fn cancel_all_open_orders(state: &TitanState) -> Result<u32, TitanError> {
    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    if key.is_empty() || secret.is_empty() {
        tracing::warn!(
            component = "order",
            "cancel_all_open_orders: API keys missing, skipping"
        );
        return Ok(0);
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    // MERIDIAN (2026-09-16): its auction orders (`mrd-*`: market-on-open
    // exits, market-on-close protection) must survive the legacy reset, so
    // cancel individually and skip that prefix instead of `DELETE /v2/orders`.
    let list = state
        .http_client
        .get(format!("{base_url}/v2/orders"))
        .query(&[("status", "open"), ("limit", "500")])
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("cancel_all_open_orders list: {e}")))?;
    let status = list.status();
    if !status.is_success() {
        let txt = list.text().await.unwrap_or_default();
        tracing::error!(component = "order", %status, "cancel_all_open_orders list failed: {txt}");
        return Err(TitanError::Broker(format!("cancel_all_open_orders: Alpaca API {status}")));
    }
    let body: serde_json::Value = list.json().await.unwrap_or(serde_json::json!([]));
    let mut cancelled = 0u32;
    let mut kept = 0u32;
    for o in body.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let cid = o.get("client_order_id").and_then(|v| v.as_str()).unwrap_or("");
        if is_meridian_order(cid) {
            kept += 1;
            continue;
        }
        let Some(id) = o.get("id").and_then(|v| v.as_str()) else { continue };
        let res = state
            .http_client
            .delete(format!("{base_url}/v2/orders/{id}"))
            .header("APCA-API-KEY-ID", &key)
            .header("APCA-API-SECRET-KEY", &secret)
            .send()
            .await
            .map_err(|e| TitanError::Network(format!("cancel_all_open_orders: {e}")))?;
        if res.status().is_success() {
            cancelled += 1;
        }
    }
    tracing::info!(component = "order", cancelled, kept_meridian = kept, "cancel_all_open_orders complete");
    Ok(cancelled)
}

/// Orders owned by the MERIDIAN driver (`src/meridian`) — never cancelled
/// by legacy housekeeping.
pub fn is_meridian_order(client_order_id: &str) -> bool {
    client_order_id.starts_with("mrd-")
}

// ═══════════════════════════════════════════════════════════════════════
//  S16.6 — Cancel all open orders for a single symbol
// ═══════════════════════════════════════════════════════════════════════

/// Cancel every open order for `symbol` at the broker. Used by the
/// exit path to release `held_for_orders` from any active bracket legs
/// (TP/SL) before submitting a fresh closing order.
///
/// Returns the count of orders that we attempted to cancel and Alpaca
/// confirmed (2xx); 4xx individual cancels (already-filled / not found)
/// are swallowed so a partially-clean state still progresses.
pub async fn cancel_open_orders_for_symbol(
    state: &TitanState,
    symbol: &str,
) -> Result<u32, TitanError> {
    if symbol.is_empty() {
        return Ok(0);
    }

    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation("API keys missing".into()));
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    let list_url = format!(
        "{base_url}/v2/orders?status=open&symbols={}&limit=500",
        symbol
    );
    let resp = state
        .http_client
        .get(&list_url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("cancel_open_orders_for_symbol list: {e}")))?;

    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(TitanError::Broker(format!(
            "cancel_open_orders_for_symbol list: Alpaca API {txt}"
        )));
    }

    let orders: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();

    let mut cancelled: u32 = 0;
    let want_sym = symbol.to_uppercase();
    for order in orders {
        // MERIDIAN auction orders are not ours to cancel (see cancel_all_open_orders).
        if order.get("client_order_id").and_then(|v| v.as_str()).is_some_and(is_meridian_order) {
            continue;
        }
        let broker_id = match order.get("id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };
        let order_sym = order
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        if order_sym != want_sym {
            continue;
        }

        // S16.6 — only target bracket legs (limit + stop) and never our
        // own pending market exit. The original loop iteration cancelled
        // every open order, including the freshly-submitted closing
        // market SELL: each tick the bot re-evaluated FSM Exit, the new
        // pre-submit cleanup found that pending SELL, cancelled it, then
        // submitted yet another one — ~7 attempts inside 5 seconds for a
        // single QBTS close (observed live).
        let order_type = order.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(
            order_type,
            "limit" | "stop" | "stop_limit" | "trailing_stop"
        ) {
            tracing::debug!(
                component = "order",
                symbol = %symbol,
                %broker_id,
                order_type,
                "Skipping non-bracket order during exit cleanup"
            );
            continue;
        }

        let del_url = format!("{base_url}/v2/orders/{broker_id}");
        match state
            .http_client
            .delete(&del_url)
            .header("APCA-API-KEY-ID", &key)
            .header("APCA-API-SECRET-KEY", &secret)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() || r.status() == reqwest::StatusCode::NO_CONTENT => {
                cancelled += 1;
                tracing::debug!(
                    component = "order",
                    symbol = %symbol,
                    %broker_id,
                    "Cancelled bracket/open order before exit"
                );
            }
            Ok(r)
                if r.status() == reqwest::StatusCode::NOT_FOUND
                    || r.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY =>
            {
                // 422 = already cancelled / filled / pending; treat as benign.
                tracing::debug!(
                    component = "order",
                    symbol = %symbol,
                    %broker_id,
                    status = %r.status(),
                    "open order already resolved during cleanup"
                );
            }
            Ok(r) => {
                let status = r.status();
                let txt = r.text().await.unwrap_or_default();
                tracing::warn!(
                    component = "order",
                    symbol = %symbol,
                    %broker_id,
                    %status,
                    "Failed to cancel open order: {txt}"
                );
            }
            Err(e) => {
                tracing::warn!(
                    component = "order",
                    symbol = %symbol,
                    %broker_id,
                    error = %e,
                    "Network error cancelling open order"
                );
            }
        }
    }

    Ok(cancelled)
}

// ═══════════════════════════════════════════════════════════════════════
//  S17.6 — Pre-submit broker check: same-side close already in flight
// ═══════════════════════════════════════════════════════════════════════
//
// Companion to `cancel_open_orders_for_symbol`. That helper cancels
// **bracket legs** (limit / stop / stop_limit / trailing_stop) but
// deliberately skips bare market orders so it does not race with the
// bot's own freshly-submitted exit. The trade-off is that a previously
// submitted market exit that is still working at the broker is invisible
// to the cleanup, so the next exit submission collides with
// `held_for_orders` and Alpaca returns HTTP 403.
//
// Live evidence (2026-05-06 03:02→03:51 UTC, symbol CRE): an exit
// market SELL was held by the broker for ~5 hours; the bot kept
// firing `ADAPTIVE EXIT` every 30 s, the bracket-cleanup correctly
// skipped the market order, `send_order` resubmitted, broker rejected
// 100× consecutively. The position only closed when the orphan
// finally filled at 08:01 UTC.
//
// `has_open_close_order` does a single GET on `/v2/orders?status=open`
// filtered by symbol, then matches on `side`. Returns `false` on any
// error so we **fail open** — a flaky network must never block a
// legitimate exit. The combined defence (S17.6 timeout bump → halves
// retry rate; this pre-check → blocks the duplicate entirely) leaves
// an orphan-market scenario as a no-op for the bot until the orphan
// resolves.

/// Returns `true` iff the broker has at least one **open** order for
/// `symbol` whose `side` matches `close_side` (`"buy"` or `"sell"`).
///
/// Match is case-insensitive on both fields. Network errors, broker
/// 5xx, malformed JSON, missing API keys → `false` (fail open).
pub async fn has_open_close_order(state: &TitanState, symbol: &str, close_side: &str) -> bool {
    if symbol.is_empty() {
        return false;
    }

    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    if key.is_empty() || secret.is_empty() {
        return false;
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    let url = format!("{base_url}/v2/orders?status=open&symbols={symbol}&limit=500");
    let resp = match state
        .http_client
        .get(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(
                component = "order",
                symbol = %symbol,
                error = %e,
                "has_open_close_order: network error, failing open"
            );
            return false;
        }
    };

    if !resp.status().is_success() {
        tracing::debug!(
            component = "order",
            symbol = %symbol,
            status = %resp.status(),
            "has_open_close_order: non-2xx, failing open"
        );
        return false;
    }

    let orders: Vec<serde_json::Value> = match resp.json().await {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(
                component = "order",
                symbol = %symbol,
                error = %e,
                "has_open_close_order: JSON parse failed, failing open"
            );
            return false;
        }
    };

    let want_sym = symbol.to_uppercase();
    let want_side = close_side.to_lowercase();
    orders.iter().any(|o| {
        let osym = o
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        if osym != want_sym {
            return false;
        }
        let oside = o
            .get("side")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        oside == want_side
    })
}

// ═══════════════════════════════════════════════════════════════════════
//  Cancel by client_order_id (S03: partial-fill + orphan cleanup)
// ═══════════════════════════════════════════════════════════════════════

/// Cancel a specific order identified by `client_order_id`. Alpaca does not
/// support `DELETE /v2/orders?client_order_id=X` — we list open orders,
/// find the one matching our CID, and cancel it by broker_id.
/// Returns `Ok(true)` if cancelled, `Ok(false)` if not found (already done).
pub async fn cancel_order_by_cid(state: &TitanState, cid: &str) -> Result<bool, TitanError> {
    if cid.is_empty() {
        return Ok(false);
    }

    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation("API keys missing".into()));
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    let list_url = format!("{base_url}/v2/orders?status=open&limit=500");
    let resp = state
        .http_client
        .get(&list_url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("cancel_order_by_cid list: {e}")))?;

    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(TitanError::Broker(format!(
            "cancel_order_by_cid list: Alpaca API {txt}"
        )));
    }

    let orders: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
    let matching = orders
        .iter()
        .find(|o| o.get("client_order_id").and_then(|v| v.as_str()) == Some(cid));

    let Some(order) = matching else {
        tracing::debug!(
            component = "order",
            cid,
            "cancel_order_by_cid: order not found (already done)"
        );
        return Ok(false);
    };

    let broker_id = order.get("id").and_then(|v| v.as_str()).unwrap_or("");

    if broker_id.is_empty() {
        return Ok(false);
    }

    let del_url = format!("{base_url}/v2/orders/{broker_id}");
    let del_resp = state
        .http_client
        .delete(&del_url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("cancel_order_by_cid delete: {e}")))?;

    if del_resp.status().is_success() || del_resp.status() == reqwest::StatusCode::NO_CONTENT {
        tracing::info!(
            component = "order",
            cid,
            broker_id,
            "Cancelled order by CID"
        );
        Ok(true)
    } else if del_resp.status() == reqwest::StatusCode::NOT_FOUND
        || del_resp.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY
    {
        tracing::debug!(
            component = "order",
            cid,
            "cancel_order_by_cid: already done/filled"
        );
        Ok(false)
    } else {
        let txt = del_resp.text().await.unwrap_or_default();
        Err(TitanError::Broker(format!(
            "cancel_order_by_cid delete: {txt}"
        )))
    }
}

pub async fn cancel_order_by_id(state: &TitanState, broker_id: &str) -> Result<bool, TitanError> {
    if broker_id.is_empty() {
        return Ok(false);
    }
    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };
    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };
    let del_url = format!("{base_url}/v2/orders/{broker_id}");
    let del_resp = state
        .http_client
        .delete(&del_url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("cancel_order_by_id delete: {e}")))?;

    if del_resp.status().is_success() || del_resp.status() == reqwest::StatusCode::NO_CONTENT {
        Ok(true)
    } else {
        Ok(false)
    }
}

async fn fetch_latest_quote(state: &TitanState, symbol: &str) -> Result<(f64, f64), TitanError> {
    let (key, secret) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };
    let base_url = "https://data.alpaca.markets";
    let url = format!("{}/v2/stocks/{}/quotes/latest?feed=sip", base_url, symbol);
    let res = state
        .http_client
        .get(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("Failed to fetch quote: {e}")))?;

    if !res.status().is_success() {
        return Err(TitanError::Broker(format!(
            "Latest quote API status: {}",
            res.status()
        )));
    }

    let json: serde_json::Value = res
        .json()
        .await
        .map_err(|e| TitanError::Broker(format!("Invalid quote JSON: {e}")))?;

    let quote = &json["quote"];
    let bid_price = quote["bp"]
        .as_f64()
        .ok_or_else(|| TitanError::Broker("Missing bp in quote".into()))?;
    let ask_price = quote["ap"]
        .as_f64()
        .ok_or_else(|| TitanError::Broker("Missing ap in quote".into()))?;
    Ok((bid_price, ask_price))
}

async fn fetch_order_filled_qty(
    state: &TitanState,
    broker_id: &str,
) -> Result<(Decimal, String), TitanError> {
    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };
    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };
    let url = format!("{}/v2/orders/{}", base_url, broker_id);
    let res = state
        .http_client
        .get(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("Failed to fetch order status: {e}")))?;

    if !res.status().is_success() {
        return Err(TitanError::Broker(format!(
            "Order status API status: {}",
            res.status()
        )));
    }

    let json: serde_json::Value = res
        .json()
        .await
        .map_err(|e| TitanError::Broker(format!("Invalid order JSON: {e}")))?;

    let filled_qty_str = json["filled_qty"].as_str().unwrap_or("0");
    use std::str::FromStr;
    let filled_qty = Decimal::from_str(filled_qty_str).unwrap_or(Decimal::ZERO);
    let status = json["status"].as_str().unwrap_or("unknown").to_string();
    Ok((filled_qty, status))
}

pub async fn send_order_pegged_ioc(
    state: Arc<TitanState>,
    order: OrderRequest,
    app: AppHandle,
    metrics: &mut ExecutionMetrics,
) -> Result<OrderResult, TitanError> {
    validate_order(&order)?;
    // Alpaca's paper matcher leaves IOC limits in `new` indefinitely
    // (live evidence: LTGO/KRNY 2026-08-18, QBTS 2026-08-21 —
    // `Pegged IOC order failed: last status=new`, 0 fills). Paper
    // accounts use the DAY market/bracket router so AUTO entries can
    // actually fill. Live still tries IOC first.
    let paper = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.market.paper_mode
    };
    if skip_ioc_on_paper(paper) {
        tracing::info!(
            component = "pegged_router",
            symbol = %order.symbol,
            "paper_mode — routing through DAY market/bracket instead of IOC"
        );
        return send_order(state, order, app, metrics).await;
    }

    let symbol = order.symbol.trim().to_uppercase();
    let side = side_str(&order.side);
    let mut remaining_qty = order.qty;
    let mut attempt = 0;
    let mut last_broker_id = String::new();
    let mut last_client_order_id = String::new();
    let mut last_status = "canceled".to_string();

    while remaining_qty > Decimal::ZERO && attempt < 3 {
        attempt += 1;

        let (bid_price, ask_price) = match fetch_latest_quote(&state, &symbol).await {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(component = "pegged_router", symbol = %symbol, "Failed to fetch latest quote: {e}");
                return Err(e);
            }
        };

        // A real IOC must be marketable. The old code parked a buy at
        // bid / sell at ask, then treated Alpaca's transient `new`
        // status after 400ms as a broker failure. That is neither
        // pegged nor IOC: it is a passive DAY order that almost never
        // fills in paper. Cap the immediate order at the opposite
        // quote: buy at ask, sell at bid.
        let limit_price = if side == "buy" {
            Decimal::from_f64_retain(ask_price).unwrap_or(remaining_qty)
        } else {
            Decimal::from_f64_retain(bid_price).unwrap_or(remaining_qty)
        };

        let client_order_id = format!(
            "st-ioc-{}-{}",
            symbol,
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let body =
            build_pegged_ioc_body(&symbol, side, remaining_qty, limit_price, &client_order_id);

        let (key, secret) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
            )
        };
        let base_url = "https://api.alpaca.markets";
        let url = format!("{}/v2/orders", base_url);

        let t0 = Instant::now();
        let send_result = state
            .http_client
            .post(&url)
            .header("APCA-API-KEY-ID", &key)
            .header("APCA-API-SECRET-KEY", &secret)
            .json(&body)
            .send()
            .await;
        let latency_ms = t0.elapsed().as_millis() as u64;
        metrics.record(latency_ms);

        let res = match send_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(component = "pegged_router", symbol = %symbol, "Network error on attempt {attempt}: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };

        if !res.status().is_success() {
            let txt = res.text().await.unwrap_or_default();
            tracing::warn!(component = "pegged_router", symbol = %symbol, "API error on attempt {attempt}: {txt}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        }

        let resp_json: serde_json::Value = res.json().await.unwrap_or(serde_json::json!({}));
        let broker_id = resp_json["id"].as_str().unwrap_or("").to_string();
        if broker_id.is_empty() {
            last_status = "missing_broker_id".to_string();
            tracing::warn!(
                component = "pegged_router",
                symbol = %symbol,
                attempt,
                "Alpaca accepted IOC request without an order id"
            );
            continue;
        }
        last_broker_id = broker_id.clone();
        last_client_order_id = client_order_id.clone();

        // `new` / `accepted` are normal broker lifecycle states, not an
        // execution result. Poll a bounded 1.5s confirmation window;
        // IOC should transition to filled / partially_filled / canceled
        // inside it. We record the real terminal state below.
        let (filled_qty, status) = confirm_pegged_ioc(&state, &broker_id, &symbol).await;
        last_status = status.clone();
        remaining_qty = remaining_qty.saturating_sub(filled_qty);

        if is_ioc_pending(&status) {
            // Defensive cleanup for a broker that has not applied the
            // requested IOC expiry by the confirmation deadline.
            let _ = cancel_order_by_id(&state, &broker_id).await;
        }

        log_order_attempt(&OrderLogEntry {
            symbol: &symbol,
            side,
            qty: remaining_qty.saturating_add(filled_qty),
            payload: &body,
            response: &format!("IOC broker_id={broker_id} status={status} filled_qty={filled_qty}"),
            latency_ms,
            client_order_id: &client_order_id,
            broker_id: Some(&broker_id),
            retry_count: attempt - 1,
        });
        push_history(
            &state,
            OrderHistoryEntry {
                order_id: broker_id.clone(),
                symbol: symbol.clone(),
                side: side.into(),
                qty: filled_qty,
                order_type: "limit_ioc".into(),
                limit_price: Some(limit_price),
                status: status.clone(),
                source: order.source.clone(),
                submitted_at: chrono::Utc::now().timestamp(),
                filled_price: None,
                filled_at: if filled_qty > Decimal::ZERO {
                    Some(chrono::Utc::now().timestamp())
                } else {
                    None
                },
                reject_reason: if filled_qty == Decimal::ZERO {
                    Some(format!("IOC ended {status}"))
                } else {
                    None
                },
                entry_score: order.entry_score,
                entry_timeframe: order.entry_timeframe.clone(),
            },
        );
        let _ = app.emit_all(
            "order-lifecycle",
            serde_json::json!({
                "event": "ioc_confirmed",
                "broker_id": broker_id,
                "client_order_id": client_order_id,
                "symbol": symbol,
                "side": side,
                "filled_qty": filled_qty.to_string(),
                "status": status,
                "latency_ms": latency_ms,
                "attempt": attempt,
            }),
        );
    }

    if remaining_qty < order.qty {
        let filled_qty = order.qty.saturating_sub(remaining_qty);
        Ok(OrderResult {
            broker_id: last_broker_id,
            client_order_id: last_client_order_id,
            status: last_status,
            symbol,
            qty: filled_qty,
            side: side.into(),
        })
    } else {
        tracing::warn!(
            component = "pegged_router",
            symbol = %symbol,
            last_status = %last_status,
            "IOC unfilled after retries — falling back to DAY market/bracket"
        );
        send_order(state, order, app, metrics).await
    }
}

/// Marketable limit IOC body used exclusively by the latency-sensitive
/// router. Bracket legs are intentionally not attached: Alpaca does not
/// support bracket+IOC, and the position exits remain managed by the
/// existing bot / trade-update paths after a confirmed fill.
fn build_pegged_ioc_body(
    symbol: &str,
    side: &str,
    qty: Decimal,
    limit_price: Decimal,
    client_order_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "symbol": symbol,
        "qty": qty.round_dp(6).to_string(),
        "side": side,
        "type": "limit",
        "time_in_force": "ioc",
        // Preserve the project-wide stock-price serialization contract.
        // Alpaca rejects excess price precision for most US equities.
        "limit_price": format!("{:.2}", limit_price),
        "client_order_id": client_order_id,
    })
}

fn is_ioc_pending(status: &str) -> bool {
    matches!(
        status,
        "new" | "accepted" | "pending_new" | "partially_filled"
    )
}

/// Paper matching does not honour IOC (see `send_order_pegged_ioc`).
fn skip_ioc_on_paper(paper: bool) -> bool {
    paper
}

async fn confirm_pegged_ioc(
    state: &TitanState,
    broker_id: &str,
    symbol: &str,
) -> (Decimal, String) {
    const POLLS: u32 = 15;
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    let mut last = (Decimal::ZERO, "unknown".to_string());
    for poll in 0..POLLS {
        if poll > 0 {
            tokio::time::sleep(INTERVAL).await;
        }
        match fetch_order_filled_qty(state, broker_id).await {
            Ok((filled_qty, status)) => {
                let terminal = !is_ioc_pending(&status);
                last = (filled_qty, status);
                if terminal {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(
                    component = "pegged_router",
                    symbol,
                    broker_id,
                    "Failed to confirm IOC status: {e}"
                );
            }
        }
    }
    last
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn market_order(symbol: &str, qty: Decimal) -> OrderRequest {
        OrderRequest {
            conviction: None,
            expected_price: 0.0,
            symbol: symbol.into(),
            side: Direction::Long,
            qty,
            stop_loss: None,
            take_profit: None,
            source: String::new(),
            deducted_amount: Decimal::ZERO,
            order_type: "market".into(),
            limit_price: None,
            bar_time: None,
            notional: None,
            is_exit: false,
            is_swing_intent: false,
            entry_timeframe: String::new(),
            entry_score: None,
        }
    }

    #[test]
    fn validate_empty_symbol_rejected() {
        let req = market_order("", dec!(10));
        assert!(validate_order(&req).is_err());
    }

    #[test]
    fn validate_zero_qty_rejected() {
        let req = market_order("AAPL", Decimal::ZERO);
        assert!(validate_order(&req).is_err());
    }

    #[test]
    fn validate_negative_qty_rejected() {
        let req = market_order("AAPL", dec!(-5));
        assert!(validate_order(&req).is_err());
    }

    #[test]
    fn validate_invalid_stop_loss_rejected() {
        let mut req = market_order("AAPL", dec!(10));
        req.stop_loss = Some(dec!(-1));
        assert!(validate_order(&req).is_err());
    }

    #[test]
    fn validate_invalid_take_profit_rejected() {
        let mut req = market_order("AAPL", dec!(10));
        req.take_profit = Some(Decimal::ZERO);
        assert!(validate_order(&req).is_err());
    }

    #[test]
    fn validate_valid_order_passes() {
        let mut req = market_order("AAPL", dec!(10));
        req.stop_loss = Some(dec!(170));
        req.take_profit = Some(dec!(200));
        assert!(validate_order(&req).is_ok());
    }

    #[test]
    fn validate_limit_order_without_price_rejected() {
        let req = OrderRequest {
            order_type: "limit".into(),
            limit_price: None,
            ..market_order("AAPL", dec!(10))
        };
        assert!(validate_order(&req).is_err());
    }

    #[test]
    fn validate_limit_order_zero_price_rejected() {
        let req = OrderRequest {
            order_type: "limit".into(),
            limit_price: Some(Decimal::ZERO),
            ..market_order("AAPL", dec!(10))
        };
        assert!(validate_order(&req).is_err());
    }

    #[test]
    fn validate_limit_order_valid_passes() {
        let req = OrderRequest {
            order_type: "limit".into(),
            limit_price: Some(dec!(150)),
            ..market_order("AAPL", dec!(10))
        };
        assert!(validate_order(&req).is_ok());
    }

    #[test]
    fn build_simple_order_no_bracket() {
        let body = build_order_body(
            "AAPL",
            "buy",
            dec!(10),
            None,
            None,
            "test-id",
            "market",
            None,
            None,
        );
        assert_eq!(body["type"], "market");
        assert_eq!(body["time_in_force"], "day");
        assert!(body.get("order_class").is_none());
        assert!(body.get("stop_loss").is_none());
        assert!(body.get("take_profit").is_none());
        assert!(body.get("limit_price").is_none());
    }

    #[test]
    fn build_limit_order_no_bracket() {
        let body = build_order_body(
            "AAPL",
            "buy",
            dec!(10),
            None,
            None,
            "test-id",
            "limit",
            Some(dec!(150)),
            None,
        );
        assert_eq!(body["type"], "limit");
        assert_eq!(body["limit_price"], "150.00");
        assert!(body.get("order_class").is_none());
    }

    #[test]
    fn pegged_ioc_body_is_marketable_limit_ioc() {
        let body = build_pegged_ioc_body("AAPL", "buy", dec!(3.5), dec!(201.123456), "ioc-1");
        assert_eq!(body["type"], "limit");
        assert_eq!(body["time_in_force"], "ioc");
        assert_eq!(body["limit_price"], "201.12");
        assert_eq!(body["qty"], "3.5");
        assert!(body.get("order_class").is_none());
    }

    #[test]
    fn ioc_pending_statuses_are_not_execution_failures() {
        for status in ["new", "accepted", "pending_new", "partially_filled"] {
            assert!(is_ioc_pending(status), "{status} is transient");
        }
        for status in ["filled", "canceled", "rejected", "expired"] {
            assert!(!is_ioc_pending(status), "{status} is terminal");
        }
    }

    #[test]
    fn paper_accounts_skip_ioc_and_use_day_router() {
        assert!(skip_ioc_on_paper(true));
        assert!(!skip_ioc_on_paper(false));
    }

    #[test]
    fn build_bracket_order() {
        let body = build_order_body(
            "NVDA",
            "buy",
            dec!(5),
            Some(dec!(170)),
            Some(dec!(200)),
            "test-id",
            "market",
            None,
            None,
        );
        assert_eq!(body["order_class"], "bracket");
        assert_eq!(body["stop_loss"]["stop_price"], "170.00");
        assert_eq!(body["take_profit"]["limit_price"], "200.00");
    }

    #[test]
    fn build_bracket_limit_order() {
        let body = build_order_body(
            "NVDA",
            "buy",
            dec!(5),
            Some(dec!(170)),
            Some(dec!(200)),
            "test-id",
            "limit",
            Some(dec!(180)),
            None,
        );
        assert_eq!(body["order_class"], "bracket");
        assert_eq!(body["type"], "limit");
        assert_eq!(body["limit_price"], "180.00");
        assert_eq!(body["stop_loss"]["stop_price"], "170.00");
        assert_eq!(body["take_profit"]["limit_price"], "200.00");
    }

    /// S15.11: the body builder is intentionally unopinionated — it
    /// emits whatever the (already-validated) caller asks for. The
    /// single-leg-bracket *policy* lives in `validate_order` (see
    /// `validate_bracket_only_stop_loss_rejected` below).
    #[test]
    fn build_bracket_only_stop_loss_builder_passes() {
        let body = build_order_body(
            "AAPL",
            "sell",
            dec!(3),
            Some(dec!(185.50)),
            None,
            "test-id",
            "market",
            None,
            None,
        );
        assert_eq!(body["order_class"], "bracket");
        assert_eq!(body["stop_loss"]["stop_price"], "185.50");
        assert!(body.get("take_profit").is_none());
    }

    // ── S15.11: single-leg bracket rejected at validate_order ──

    #[test]
    fn validate_bracket_only_stop_loss_rejected() {
        let mut req = market_order("AAPL", dec!(10));
        req.stop_loss = Some(dec!(170));
        // intentionally no take_profit
        let err = validate_order(&req).unwrap_err();
        assert!(
            err.to_string().contains("Bracket order requires BOTH"),
            "expected single-leg bracket rejection, got: {err}"
        );
    }

    #[test]
    fn validate_bracket_only_take_profit_rejected() {
        let mut req = market_order("AAPL", dec!(10));
        req.take_profit = Some(dec!(200));
        // intentionally no stop_loss
        let err = validate_order(&req).unwrap_err();
        assert!(
            err.to_string().contains("Bracket order requires BOTH"),
            "expected single-leg bracket rejection, got: {err}"
        );
    }

    // ── S08: Notional API tests ──

    #[test]
    fn notional_market_order_builds_correct_body() {
        let body = build_order_body(
            "AAPL",
            "buy",
            Decimal::ZERO,
            None,
            None,
            "test-id",
            "market",
            None,
            Some(dec!(100)),
        );
        assert_eq!(body["notional"], "100.00");
        assert!(
            body.get("qty").is_none(),
            "notional path must not include qty"
        );
        assert_eq!(body["side"], "buy");
    }

    #[test]
    fn notional_with_bracket_rejected_at_validate() {
        let mut req = market_order("AAPL", Decimal::ZERO);
        req.notional = Some(dec!(100));
        // S15.11: full bracket to bypass the new single-leg guard,
        // so this test still targets the Bracket+notional check.
        req.stop_loss = Some(dec!(170));
        req.take_profit = Some(dec!(200));
        let err = validate_order(&req).unwrap_err();
        assert!(err.to_string().contains("Bracket+notional incompatible"));
    }

    #[test]
    fn notional_with_take_profit_rejected_at_validate() {
        let mut req = market_order("AAPL", Decimal::ZERO);
        req.notional = Some(dec!(100));
        // S15.11: full bracket to bypass the new single-leg guard.
        req.take_profit = Some(dec!(200));
        req.stop_loss = Some(dec!(180));
        let err = validate_order(&req).unwrap_err();
        assert!(err.to_string().contains("Bracket+notional incompatible"));
    }

    #[test]
    fn notional_without_bracket_passes_validate() {
        let mut req = market_order("AAPL", Decimal::ZERO);
        req.notional = Some(dec!(100));
        assert!(validate_order(&req).is_ok());
    }

    #[test]
    fn side_str_mapping() {
        assert_eq!(side_str(&Direction::Long), "buy");
        assert_eq!(side_str(&Direction::Short), "sell");
    }

    #[test]
    fn execution_metrics_avg_latency() {
        let mut m = ExecutionMetrics::default();
        assert!((m.avg_latency_ms()).abs() < f64::EPSILON);
        m.record(100);
        m.record(200);
        assert!((m.avg_latency_ms() - 150.0).abs() < f64::EPSILON);
        assert_eq!(m.last_latency_ms, 200);
        assert_eq!(m.orders_submitted, 2);
    }

    #[test]
    fn client_order_id_deterministic_for_same_signal() {
        fn make_cid(symbol: &str, side: &str, bar_time: i64) -> String {
            let mut hasher = Sha256::new();
            hasher.update(format!("{symbol}|{side}|{bar_time}|smarttrend"));
            let hash = hasher.finalize();
            let hex_str: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
            format!("st-{hex_str}")
        }

        let cid1 = make_cid("AAPL", "buy", 1700000000);
        let cid2 = make_cid("AAPL", "buy", 1700000000);
        assert_eq!(cid1, cid2, "Same signal must produce same CID");

        let cid3 = make_cid("AAPL", "buy", 1700000300);
        assert_ne!(cid1, cid3, "Different bar_time must produce different CID");

        let cid4 = make_cid("MSFT", "buy", 1700000000);
        assert_ne!(cid1, cid4, "Different symbol must produce different CID");

        assert!(cid1.starts_with("st-"), "CID prefix must be st-");
        assert_eq!(cid1.len(), 3 + 16, "CID format: st- + 16 hex chars");
    }
}
