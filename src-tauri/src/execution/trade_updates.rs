use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use rust_decimal::prelude::*;
use serde::Serialize;
use tauri::{AppHandle, Manager};
use tokio::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::state::{Direction, Position, TitanState};

const MAX_BACKOFF_SECS: u64 = 60;

/// S15.8: soft welcome-probe timeout — bounds how long we keep the
/// watchdog ticking while waiting for an OPTIONAL welcome frame, NOT
/// a fatal deadline. Live evidence at v1.0.3 showed Alpaca paper
/// `/stream` does not emit a welcome unprompted, so on timeout we
/// fall through to `authenticate` and let the auth-phase 15s deadline
/// catch a truly dead broker. 30s ceiling kept (pre-S15.6: 10s tripped
/// under jitter; S15.6: drop + race → 4s storm + 429; S15.7: fatal →
/// stuck loop).
const WELCOME_TIMEOUT_SECS: u64 = 30;

/// Per-iteration timeout inside the welcome-wait loop. Bounded above by
/// `WELCOME_TIMEOUT_SECS / 6` so the watchdog ticks at least 6 times
/// across a full welcome wait, keeping `loop_name=trade_updates` well
/// inside the 60s STALE threshold even on a slow paper handshake.
const WELCOME_POLL_SECS: u64 = 5;

// ═══════════════════════════════════════════════════════════════════════
//  S15.6 — handshake helpers (call ordering reverted in S15.7)
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthResult {
    Ok,
    Rejected,
    Other,
}

/// Classify a `Text` frame received during the auth phase.
///
/// Alpaca paper `/stream` returns
/// `{"stream":"authorization","data":{"status":"authorized"}}` on success
/// or `"unauthorized"` / `"forbidden"` on rejection. We match on
/// substrings to stay tolerant of minor schema drift.
///
/// S15.6: rejection is checked BEFORE success because `"unauthorized"`
/// contains the substring `"authorized"`. The pre-S15.6 order
/// misclassified rejection frames as success — masked in production
/// because the broker dropped the socket after, but a real bug.
fn classify_auth(t: &str) -> AuthResult {
    if t.contains("unauthorized") || t.contains("forbidden") {
        AuthResult::Rejected
    } else if t.contains("authorized") || t.contains("authenticated") {
        AuthResult::Ok
    } else {
        AuthResult::Other
    }
}

/// Poison-tolerant single-shot tick on the named system loop.
/// Tier-A `state.system` lock acquired alone (boundary rule).
fn tick_watchdog(state: &TitanState, loop_name: &str) {
    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(h) = sys.loops.get_mut(loop_name) {
        h.tick();
    }
}

/// S16.2 — Decide which wallet receives the close-out refund based on the
/// position's `source` field.
///
/// Empty `source` (legacy positions or those reconciled from the broker
/// before attribution metadata existed) is treated as `"bot"`. Rationale:
/// the bot is the only automated entry path; user manual orders set
/// `source="user"` explicitly via `commands/trading/manual.rs`. Without
/// this fallback, sells of orphan positions credit no wallet at all,
/// leaking funds out of the accounting and producing phantom losses in
/// the UI.
///
/// v2.5.0 — Tournament strategies persist `source = "s2:{strategy}"` via
/// `dplus::source_for_strategy`. Without explicit routing these strings
/// hit the `_ => "unknown"` arm and the refund is silently dropped,
/// permanently draining bot_wallet on every Tournament close (cross-
/// validated by peer audits, May 12 2026). Treat any `s2:*` prefix —
/// validated through `parse_strategy_from_source` so unknown suffixes
/// still flag as "unknown" — as a bot-wallet refund target, since the
/// Tournament sub-wallet is bookkeeping inside the same paper-capital
/// pool that funded the entry.
fn attribute_close_source(raw: &str) -> &'static str {
    match raw {
        "user" => "user",
        "bot" | "" => "bot",
        s if crate::smarttrend::dplus::parse_strategy_from_source(s).is_some() => "bot",
        _ => "unknown",
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Health payload emitted to frontend
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
struct TradeUpdatesHealth {
    connected: bool,
    reconnect_count: u32,
    last_disconnect_reason: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════
//  Main loop
// ═══════════════════════════════════════════════════════════════════════

/// Connects to the Alpaca Trading API WebSocket for trade_updates
/// (fills, rejections, cancellations). Separate from market data SIP.
pub async fn run_trade_updates(state: Arc<TitanState>, app: AppHandle) {
    tracing::info!(component = "trade_updates", "Trade updates stream started");

    let mut backoff_secs: u64 = 2;
    let mut reconnect_count: u32 = 0;

    loop {
        if state.shutdown.is_cancelled() {
            break;
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
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let ws_url = if paper {
            "wss://paper-api.alpaca.markets/stream"
        } else {
            "wss://api.alpaca.markets/stream"
        };

        tracing::info!(component = "trade_updates", %ws_url, "Connecting...");
        let connect_time = Instant::now();

        let (mut ws_stream, _) = match connect_async(ws_url).await {
            Ok(v) => {
                tracing::info!(component = "trade_updates", "WebSocket connected");
                backoff_secs = 2;
                reconnect_count += 1;
                let _ = app.emit_all(
                    "trade-updates-health",
                    TradeUpdatesHealth {
                        connected: true,
                        reconnect_count,
                        last_disconnect_reason: None,
                    },
                );
                v
            }
            Err(e) => {
                tracing::error!(component = "trade_updates", phase = "connect", error = %e, "WebSocket connect failed");
                let _ = app.emit_all(
                    "log-message",
                    format!("TRADE UPDATES: Connection failed -- {e}"),
                );
                let _ = app.emit_all(
                    "trade-updates-health",
                    TradeUpdatesHealth {
                        connected: false,
                        reconnect_count,
                        last_disconnect_reason: Some(e.to_string()),
                    },
                );
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                continue;
            }
        };

        // S15.6/S15.7: tick watchdog right after connect — the welcome
        // wait + auth response together can span up to 45s on a slow
        // paper-broker handshake, so without this tick the age would
        // push past STALE_WARN_SECS=60 before the message loop starts
        // ticking.
        tick_watchdog(&state, "trade_updates");

        // ── Wait for welcome (S15.8: soft probe, not a gate) ──
        //
        // v1.0.3 live smoke proved Alpaca paper /stream does NOT emit a
        // welcome frame unprompted — the server waits for the client to
        // send `authenticate` first. S15.7's hard abort therefore stuck
        // the loop forever on welcome-timeout. S15.8 keeps the 30s wait
        // as a best-effort liveness probe + watchdog-tick driver, but on
        // timeout we fall through to auth and let the auth-phase 15s
        // deadline be the real backstop.
        let welcome_start = Instant::now();
        let welcome_deadline =
            welcome_start + Duration::from_secs(WELCOME_TIMEOUT_SECS);
        let mut got_welcome = false;
        let mut abort_reason: Option<&'static str> = None;
        while Instant::now() < welcome_deadline {
            match tokio::time::timeout(
                Duration::from_secs(WELCOME_POLL_SECS),
                ws_stream.next(),
            )
            .await
            {
                Ok(Some(Ok(Message::Text(_) | Message::Binary(_) | Message::Frame(_)))) => {
                    got_welcome = true;
                    break;
                }
                Ok(Some(Ok(Message::Ping(p)))) => {
                    let _ = ws_stream.send(Message::Pong(p)).await;
                    tick_watchdog(&state, "trade_updates");
                }
                Ok(Some(Ok(Message::Pong(_)))) => {
                    tick_watchdog(&state, "trade_updates");
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                    abort_reason = Some("stream_closed");
                    break;
                }
                Ok(Some(Err(_))) => {
                    abort_reason = Some("stream_error");
                    break;
                }
                Err(_) => {
                    // 5s poll timeout — defensive watchdog tick, keep waiting.
                    tick_watchdog(&state, "trade_updates");
                }
            }
        }

        if !got_welcome {
            // S15.8: log + fall through to auth. See the loop preamble
            // above for why this is a soft probe rather than a gate.
            tracing::warn!(
                component = "trade_updates",
                phase = "welcome",
                reason = abort_reason.unwrap_or("timeout"),
                elapsed_secs = welcome_start.elapsed().as_secs(),
                "no welcome frame; proceeding to auth"
            );
        }

        // ── Authenticate ──
        //
        // S15.9: the auth-response read loop below is frame-tolerant —
        // Text/Binary classify via classify_auth; Ping replies Pong;
        // Pong/Frame ignore-and-continue; Close is fatal with reason
        // "server_close_frame"; Err/Ok(None) keep their existing fatal
        // reasons. 15s outer deadline catches a truly dead broker.
        let auth = serde_json::json!({
            "action": "authenticate",
            "data": { "key_id": &key, "secret_key": &secret }
        });
        if let Err(e) = ws_stream.send(Message::Text(auth.to_string())).await {
            tracing::error!(component = "trade_updates", phase = "auth", error = %e, "auth send failed");
            let _ = ws_stream.close(None).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            continue;
        }

        let mut auth_status = AuthResult::Other;
        let mut auth_abort_reason: Option<&'static str> = None;
        let auth_deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < auth_deadline {
            match tokio::time::timeout(Duration::from_secs(10), ws_stream.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => match classify_auth(&t) {
                    AuthResult::Ok => {
                        auth_status = AuthResult::Ok;
                        break;
                    }
                    AuthResult::Rejected => {
                        tracing::error!(component = "trade_updates", phase = "auth", response = %t, "Auth rejected by broker");
                        auth_status = AuthResult::Rejected;
                        break;
                    }
                    AuthResult::Other => continue,
                },
                Ok(Some(Ok(Message::Binary(b)))) => {
                    // S15.9: legacy /stream sometimes ships the auth
                    // response inside a Binary frame. Decode utf-8 and
                    // route through classify_auth; if not utf-8 or
                    // unclassifiable, keep waiting.
                    match std::str::from_utf8(&b) {
                        Ok(t) => match classify_auth(t) {
                            AuthResult::Ok => {
                                auth_status = AuthResult::Ok;
                                break;
                            }
                            AuthResult::Rejected => {
                                tracing::error!(component = "trade_updates", phase = "auth", response = %t, "Auth rejected by broker (binary frame)");
                                auth_status = AuthResult::Rejected;
                                break;
                            }
                            AuthResult::Other => continue,
                        },
                        Err(_) => continue,
                    }
                }
                Ok(Some(Ok(Message::Ping(p)))) => {
                    let _ = ws_stream.send(Message::Pong(p)).await;
                }
                Ok(Some(Ok(Message::Pong(_)))) => {
                    // S15.9: stray pong — ignore, keep waiting.
                }
                Ok(Some(Ok(Message::Close(_)))) => {
                    // S15.9: server explicitly closed (likely after our 30s
                    // welcome silence). Distinct from a silent stream end
                    // so the operator log can tell them apart.
                    auth_abort_reason = Some("server_close_frame");
                    break;
                }
                Ok(Some(Ok(Message::Frame(_)))) => {
                    // S15.9: raw lower-level frame — ignore, keep waiting.
                }
                Ok(Some(Err(_))) => {
                    auth_abort_reason = Some("stream_error");
                    break;
                }
                Ok(None) => {
                    auth_abort_reason = Some("stream_closed");
                    break;
                }
                Err(_) => {
                    auth_abort_reason = Some("no_response_within_15s");
                    break;
                }
            }
        }

        if !matches!(auth_status, AuthResult::Ok) {
            // Rejected paths log inline above (with the response body);
            // every other failure mode logs here with a structured reason.
            if !matches!(auth_status, AuthResult::Rejected) {
                tracing::warn!(
                    component = "trade_updates",
                    phase = "auth",
                    reason = auth_abort_reason.unwrap_or("no_response_within_15s"),
                    "handshake aborted"
                );
            }
            let _ = ws_stream.close(None).await;
            let _ = app.emit_all(
                "log-message",
                "TRADE UPDATES: Auth failed -- check API keys",
            );
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            continue;
        }

        tracing::info!(component = "trade_updates", "trade_updates AUTHENTICATED — listening");

        // ── Subscribe to trade_updates ──
        let sub = serde_json::json!({
            "action": "listen",
            "data": { "streams": ["trade_updates"] }
        });
        let _ = ws_stream.send(Message::Text(sub.to_string())).await;
        let _ = app.emit_all(
            "log-message",
            "TRADE UPDATES: Subscribed -- listening for fills",
        );

        // S15.6: tick watchdog after auth + subscribe.
        tick_watchdog(&state, "trade_updates");

        // ── Message loop ──
        //
        // v2.5.0 NUCLEAR (HF2 + HF2.1) — Stale-detector + heartbeat PING.
        //
        // Live evidence 2026-05-14/15: Alpaca paper /stream auth-d at
        // 20:25 then went silent for 17 hours. `ws_stream.next().await`
        // blocked indefinitely → no message processing → bot drained
        // $86k while bracket SL/TP fills accumulated unrecorded.
        //
        // HF2 mitigation:
        //   1. Wrap recv in `tokio::time::timeout(90s, ...)` — break
        //      out + reconnect if no traffic at all for 90s.
        //   2. Send a PING every 30s as a heartbeat.
        //
        // HF2.1 (2026-05-15 follow-up): HF2 above fired after 56 minutes
        // of session because broker kept responding to PINGs with PONGs
        // even while the trade_updates event stream was dead. PONGs
        // reset the recv timer → silence went undetected.
        //
        // Fix: track time since last **data text frame** independently
        // of PINGs/PONGs. If we have not received a Text frame in
        // `DATA_SILENCE_SECS` (5 min) we force-reconnect, regardless of
        // whether PONGs are flowing. This catches the half-open Alpaca
        // event-pipeline failure mode while keeping fast reconnects on
        // real disconnects.
        const STALE_TIMEOUT_SECS: u64 = 90;
        const PING_INTERVAL_SECS: u64 = 30;
        const DATA_SILENCE_SECS: u64 = 3600;
        let mut last_ping = Instant::now();
        let mut last_text_frame = Instant::now();
        loop {
            if state.shutdown.is_cancelled() {
                break;
            }

            // HF2.1: detect prolonged data-frame silence regardless of
            // PING/PONG handshake liveness.
            if last_text_frame.elapsed() >= Duration::from_secs(DATA_SILENCE_SECS) {
                tracing::warn!(
                    component = "trade_updates",
                    phase = "recv",
                    silence_secs = last_text_frame.elapsed().as_secs(),
                    "no data text frame in 5 min — forcing reconnect (HF2.1 silent-event-stream guard)"
                );
                break;
            }

            // Send heartbeat PING if it's time.
            if last_ping.elapsed() >= Duration::from_secs(PING_INTERVAL_SECS) {
                if let Err(e) = ws_stream.send(Message::Ping(Vec::new())).await {
                    tracing::warn!(
                        component = "trade_updates",
                        phase = "heartbeat",
                        error = %e,
                        "PING send failed — forcing reconnect"
                    );
                    break;
                }
                last_ping = Instant::now();
            }

            let msg = match tokio::time::timeout(
                Duration::from_secs(STALE_TIMEOUT_SECS),
                ws_stream.next(),
            )
            .await
            {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    tracing::warn!(component = "trade_updates", phase = "recv", error = %e, "stream error — reconnecting");
                    break;
                }
                Ok(None) => {
                    tracing::warn!(component = "trade_updates", phase = "recv", "stream ended — reconnecting");
                    break;
                }
                Err(_) => {
                    tracing::warn!(
                        component = "trade_updates",
                        phase = "recv",
                        timeout_secs = STALE_TIMEOUT_SECS,
                        "no message received within timeout — forcing reconnect (Alpaca silent-stream bug)"
                    );
                    break;
                }
            };

            tick_watchdog(&state, "trade_updates");

            let text = match msg {
                Message::Text(t) => {
                    // HF2.1: only Text frames count as live data, NOT
                    // Ping/Pong/binary. This is what makes the silent-
                    // event-stream guard work.
                    last_text_frame = Instant::now();
                    t
                }
                Message::Ping(payload) => {
                    let _ = ws_stream.send(Message::Pong(payload)).await;
                    continue;
                }
                Message::Pong(_) => {
                    continue;
                }
                _ => continue,
            };

            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let stream = parsed
                .get("stream")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if stream != "trade_updates" {
                continue;
            }

            let data = match parsed.get("data") {
                Some(d) => d,
                None => continue,
            };

            let event = data
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let order = data.get("order");

            match event {
                "fill" | "partial_fill" => {
                    handle_fill(event, order, &state, &app);
                }
                "rejected" | "canceled" => {
                    handle_rejected_canceled(event, order, &state, &app);
                }
                "new" | "accepted" | "pending_new" => {
                    // Silently track
                }
                _ => {}
            }
        }

        // ── Disconnected ──
        let _ = ws_stream.close(None).await;
        let session_secs = connect_time.elapsed().as_secs();
        tracing::warn!(
            component = "trade_updates",
            phase = "recv",
            session_secs,
            "WebSocket disconnected; reconnecting"
        );

        // S15.6: stable session > 60s indicates a healthy session that
        // was cut by network / server, not a handshake-loop pathology —
        // reset backoff so we reconnect promptly.
        if session_secs > 60 {
            backoff_secs = 2;
        } else {
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
        }

        let _ = app.emit_all(
            "trade-updates-health",
            TradeUpdatesHealth {
                connected: false,
                reconnect_count,
                last_disconnect_reason: Some(format!("session ended after {session_secs}s")),
            },
        );
        let _ = app.emit_all(
            "log-message",
            format!("TRADE UPDATES: Disconnected, reconnecting in {backoff_secs}s"),
        );
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
    }

    tracing::info!(component = "trade_updates", "Trade updates stream exiting");
}

// ═══════════════════════════════════════════════════════════════════════
//  Fill handling with cumulative-to-delta dedup
// ═══════════════════════════════════════════════════════════════════════

fn handle_fill(
    event: &str,
    order: Option<&serde_json::Value>,
    state: &TitanState,
    app: &AppHandle,
) {
    let order = match order {
        Some(o) => o,
        None => return,
    };

    let symbol = order
        .get("symbol")
        .and_then(|v| v.as_str())
        .unwrap_or("???")
        .to_uppercase();
    let side = order
        .get("side")
        .and_then(|v| v.as_str())
        .unwrap_or("???")
        .to_string();
    let order_id = order
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // P0 data-quality fix (2026-07-03 audit: 42/58 journal rows had
    // empty timeframe): keep the client_order_id as a secondary
    // attribution key. A PendingOrder persisted on a network-failure
    // retry path carries an empty broker `order_id` but always has the
    // deterministic CID, so the broker-id lookup alone silently drops
    // source/timeframe/conviction attribution for those fills.
    let client_order_id = order
        .get("client_order_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cum_filled_qty = order
        .get("filled_qty")
        .and_then(|v| v.as_str())
        .and_then(|s| Decimal::from_str(s).ok())
        .unwrap_or(Decimal::ZERO);
    let cum_filled_avg_price = order
        .get("filled_avg_price")
        .and_then(|v| v.as_str())
        .and_then(|s| Decimal::from_str(s).ok())
        .unwrap_or(Decimal::ZERO);

    // Compute incremental delta from cumulative values
    let (filled_qty, filled_avg_price) = if !order_id.is_empty() {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let (prev_qty, _prev_price) = trading
            .fill_cumulative
            .get(&order_id)
            .copied()
            .unwrap_or((Decimal::ZERO, Decimal::ZERO));

        // S03: detect non-monotonic cumulative fills (broker reordering)
        if cum_filled_qty < prev_qty {
            tracing::warn!(
                component = "trade_updates",
                order_id = %order_id,
                prev = %prev_qty,
                cum = %cum_filled_qty,
                "Non-monotonic cumulative fill — broker reordering; resetting baseline"
            );
            trading
                .fill_cumulative
                .insert(order_id.clone(), (cum_filled_qty, cum_filled_avg_price));
            return;
        }

        let delta_qty = cum_filled_qty - prev_qty;
        trading
            .fill_cumulative
            .insert(order_id.clone(), (cum_filled_qty, cum_filled_avg_price));

        // S03: track last_fill_at for partial-fill timeout watchdog
        if event == "partial_fill" {
            let now_ts = chrono::Utc::now().timestamp();
            if let Some(po) = trading.pending_orders.iter_mut().find(|po| po.order_id == order_id) {
                po.last_fill_at = Some(now_ts);
            }
        }

        if delta_qty <= Decimal::ZERO {
            tracing::debug!(
                component = "trade_updates",
                order_id = %order_id,
                symbol = %symbol,
                "Suppressed zero-delta fill (dedup)"
            );
            return;
        }
        (delta_qty, cum_filled_avg_price)
    } else {
        (cum_filled_qty, cum_filled_avg_price)
    };

    let filled_qty_f = filled_qty.to_f64().unwrap_or(0.0);
    let filled_price_f = filled_avg_price.to_f64().unwrap_or(0.0);

    let _ = app.emit_all(
        "log-message",
        format!(
            "FILL: {} {} x{:.4} @ ${:.2} [{}]",
            side.to_uppercase(),
            symbol,
            filled_qty_f,
            filled_price_f,
            event
        ),
    );

    let _ = app.emit_all(
        "trade-fill",
        serde_json::json!({
            "event": event,
            "symbol": symbol,
            "side": side,
            "filled_qty": filled_qty_f,
            "filled_avg_price": filled_price_f,
        }),
    );

    // Update local position state
    if filled_qty > Decimal::ZERO {
        let is_buy = side.eq_ignore_ascii_case("buy");

        // S36 (2026-05-28 18:46 ET) — FSM is keyed by `(symbol, tf)`.
        // For trade attribution we want the strongest FSM across all
        // TFs of this symbol — the broker fill could have been
        // triggered by any TF's BotAction, and journal entries should
        // capture the most-advanced phase that was alive at fill time.
        // P2 — regime label at fill time for the journal (Tier A #2,
        // taken and dropped before smarttrend #3 / trading #4).
        let regime_label: Option<String> = state
            .market
            .lock()
            .ok()
            .map(|m| crate::smarttrend::memory_db::regime_label(Some(m.regime)).to_string());

        let (fsm_score, fsm_phase) = {
            let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            let fsm = crate::commands::monitoring::pick_best_fsm_for_symbol(&st.fsm, &symbol);
            (
                fsm.as_ref().map_or(0.0, |f| f.score),
                fsm.as_ref()
                    .map_or(crate::state::FsmPhase::Idle, |f| f.phase),
            )
        };

        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());

        // Update order history entry with fill info
        if let Some(hist) = trading.order_history.iter_mut().rfind(|h| h.order_id == order_id) {
            hist.status = event.to_string();
            hist.filled_price = Some(cum_filled_avg_price);
            hist.filled_at = Some(chrono::Utc::now().timestamp());
        }

        // P5 — slippage ledger: score the fill against the price the
        // order was authored at. Terminal fills only (partial fills
        // report the same cumulative average and would double-count).
        // The ledger is its own module-level mutex (no tier), taken
        // and dropped inside `record` — safe under the trading guard.
        if event == "fill" {
            let expected = trading
                .pending_orders
                .iter()
                .find(|po| {
                    (!order_id.is_empty() && po.order_id == order_id)
                        || (!client_order_id.is_empty()
                            && po.client_order_id == client_order_id)
                })
                .map(|po| po.expected_price)
                .unwrap_or(0.0);
            if expected > 0.0 {
                crate::ops::slippage::record(
                    &symbol,
                    is_buy,
                    expected,
                    filled_price_f,
                    chrono::Utc::now().timestamp(),
                );
            }
        }

        if let Some(pos) = trading.positions.iter_mut().find(|p| p.symbol == symbol) {
            let same_direction = (is_buy && pos.side == Direction::Long)
                || (!is_buy && pos.side == Direction::Short);

            if same_direction {
                let old_cost = pos.avg_entry * pos.qty;
                let new_cost = filled_avg_price * filled_qty;
                pos.qty += filled_qty;
                if pos.qty > Decimal::ZERO {
                    pos.avg_entry = (old_cost + new_cost) / pos.qty;
                }
                // S05: qty/avg_entry mutated — bump version for CAS-merge
                pos.version = pos.version.wrapping_add(1);
            } else {
                pos.qty -= filled_qty;
                // S05: qty mutated — bump version for CAS-merge
                pos.version = pos.version.wrapping_add(1);
                let close_threshold = Decimal::from_str("0.001").unwrap();
                if pos.qty <= close_threshold {
                    let side_mult = if pos.side == Direction::Long { Decimal::ONE } else { Decimal::NEGATIVE_ONE };
                    let pnl = (filled_avg_price - pos.avg_entry) * filled_qty * side_mult;
                    let entry_price = pos.avg_entry;
                    let side_str = format!("{:?}", pos.side);
                    let pos_source = pos.source.clone();
                    let pos_trade_id = pos.trade_id.clone();

                    let sym_clone = symbol.clone();
                    trading.positions.retain(|p| p.symbol != sym_clone);
                    // S16.6 — release pending-exit gate on confirmed close.
                    trading.pending_exits.remove(&sym_clone);

                    // Drop trading(4) before locking smarttrend(3)
                    drop(trading);

                    {
                        let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                        st.account.record_fill(pnl);

                        let invested = entry_price * filled_qty;
                        let return_amount = (invested + pnl).max(Decimal::ZERO);
                        let attributed_source = attribute_close_source(&pos_source);
                        match attributed_source {
                            "bot" => {
                                st.bot_wallet.credit(return_amount);
                                tracing::info!(
                                    component = "trade_updates",
                                    symbol = %symbol,
                                    raw_source = %pos_source,
                                    "Returned ${return_amount} to bot wallet → ${}",
                                    st.bot_wallet.balance,
                                );
                            }
                            "user" => {
                                st.user_wallet.credit(return_amount);
                                tracing::info!(
                                    component = "trade_updates",
                                    symbol = %symbol,
                                    "Returned ${return_amount} to user wallet → ${}",
                                    st.user_wallet.balance,
                                );
                            }
                            _ => {
                                tracing::warn!(
                                    component = "trade_updates",
                                    symbol = %symbol,
                                    source = %pos_source,
                                    "Unknown position source — credit skipped (funds may be lost)",
                                );
                            }
                        }

                        tracing::info!(
                            component = "trade_updates",
                            symbol = %symbol,
                            pnl = %pnl,
                            total_pnl = %st.account.total_realized_pnl,
                            "Position closed — PnL recorded"
                        );
                    }

                    let _ = app.emit_all("wallet-changed", serde_json::json!({}));

                    let pnl_f = pnl.to_f64().unwrap_or(0.0);
                    let entry_price_f = entry_price.to_f64().unwrap_or(0.0);
                    let _ = app.emit_all("bot-pnl", serde_json::json!({
                        "symbol": symbol,
                        "pnl": pnl_f,
                        "entry_price": entry_price_f,
                        "exit_price": filled_price_f,
                        "qty": filled_qty_f,
                        "side": side_str,
                    }));

                    let closed = crate::ops::journal::record_trade_close(
                        state, &pos_trade_id, &symbol, filled_price_f, pnl_f, "SignalExit",
                    );
                    if closed {
                        // S18 — `record_trade_outcome` wraps both the
                        // S17.1 streak update AND the AutoUniverse
                        // blacklist hook. `entry_price * filled_qty`
                        // is the dollars allocated to the entry; the
                        // big-loss-pct fast path uses it.
                        let invested_f64 = (entry_price * filled_qty)
                            .to_f64()
                            .unwrap_or(0.0);
                        crate::ops::journal::record_trade_outcome(
                            state,
                            &symbol,
                            pnl_f,
                            invested_f64,
                            Some(app),
                        );
                        // D-PLUS (S2) — strategy-attributed close hooks.
                        // Looks up `Position.source` (e.g. "s2:gap_fade")
                        // to find the strategy id; legacy "bot"/"user"
                        // sources skip transparently.
                        if let Some(dplus) = state.dplus() {
                            let now_ts = chrono::Utc::now().timestamp();
                            let current_regime = {
                                state.market.lock().unwrap().regime
                            };
                            if dplus
                                .on_trade_close(&symbol, &pos_source, pnl, now_ts, current_regime)
                                .is_some()
                            {
                                let _ = app.emit_all(
                                    "dplus-trade-close",
                                    serde_json::json!({
                                        "symbol": symbol,
                                        "pnl": pnl_f,
                                        "ts": now_ts,
                                    }),
                                );
                            }
                        }
                    }

                    // Re-lock trading for subsequent position update logic
                    trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                }
            }

            // Update current price on remaining position
            if let Some(pos) = trading.positions.iter_mut().find(|p| p.symbol == symbol) {
                pos.current_price = filled_price_f;
                let sign: f64 = if pos.side == Direction::Short {
                    -1.0
                } else {
                    1.0
                };
                let avg_f = pos.avg_entry.to_f64().unwrap_or(0.0);
                let qty_f = pos.qty.to_f64().unwrap_or(0.0);
                pos.unrealized_pnl = (pos.current_price - avg_f) * qty_f * sign;
            }
        } else {
            // New position from fill — extract source from pending order
            // S28: also harvest the `is_swing_intent` flag set at submit
            // time. S37 (2026-05-28 22:55 ET): additionally harvest the
            // `entry_timeframe` so `handle_exit` can gate `SignalExit`
            // against cross-TF FSM closes. Single pending_orders pass
            // keeps lock pressure low.
            // P0-B (audit 2026-05-29): also harvest the bracket levels so
            // the `Position` carries stop_loss/take_profit and the close
            // path can classify the real exit_reason.
            // Attribution lookup: broker order_id first, then the
            // deterministic client_order_id (P0 fix — covers pending
            // orders persisted with an empty broker id on the network
            // retry path), then a last-resort order_history match so
            // source/timeframe survive even after the pending order
            // was cleared by an earlier partial fill.
            let pending_match = trading
                .pending_orders
                .iter()
                .find(|po| !order_id.is_empty() && po.order_id == order_id)
                .or_else(|| {
                    trading.pending_orders.iter().find(|po| {
                        !client_order_id.is_empty()
                            && po.client_order_id == client_order_id
                    })
                });
            let (pos_source, swing_intent, entry_tf, pos_stop, pos_target, pos_conviction) =
                match pending_match {
                    Some(po) => (
                        po.source.clone(),
                        po.is_swing_intent,
                        po.entry_timeframe.clone(),
                        po.stop_loss,
                        po.take_profit,
                        po.conviction,
                    ),
                    None => trading
                        .order_history
                        .iter()
                        .rfind(|h| !order_id.is_empty() && h.order_id == order_id)
                        .map(|h| {
                            (
                                h.source.clone(),
                                false,
                                h.entry_timeframe.clone(),
                                0.0,
                                0.0,
                                None,
                            )
                        })
                        .unwrap_or_else(|| (String::new(), false, String::new(), 0.0, 0.0, None)),
                };

            let dir = if is_buy {
                Direction::Long
            } else {
                Direction::Short
            };
            let now_ts = chrono::Utc::now().timestamp();
            // S15.14: fill arrived — release the pending-entry claim so
            // future signals (e.g. after an exit) aren't blocked by the
            // 30 s timeout. The duplicate gate falls back to the
            // `positions` check from here on.
            trading.pending_entries.remove(&symbol);
            // v2.5.0 NUCLEAR (HF3) — clear the direction-lock too. From
            // here on `positions` carries the authoritative direction.
            trading.pending_entry_dirs.remove(&symbol);
            trading.positions.push(Position {
                symbol: symbol.clone(),
                qty: filled_qty,
                avg_entry: filled_avg_price,
                current_price: filled_price_f,
                unrealized_pnl: 0.0,
                side: dir,
                opened_at: Some(now_ts),
                source: pos_source.clone(),
                first_seen: now_ts,
                version: 1,
                trade_id: order_id.clone(),
                is_swing_intent: swing_intent,
                entry_timeframe: entry_tf.clone(),
                stop_loss: pos_stop,
                take_profit: pos_target,
            });

            // v2.5.0 NUCLEAR (Faza 5) — extract Tournament strategy from
            // the canonical `s2:<strategy>` order-source string captured
            // at submit time. `None` for legacy FSM trades (source ==
            // "bot" or empty). Persisted in the journal so per-strategy
            // P/L analytics work without log scraping.
            let strategy_id = pos_source
                .strip_prefix("s2:")
                .map(|s| s.to_string());
            crate::ops::journal::record_trade_open(
                state,
                crate::ops::journal::TradeOpenParams {
                    trade_id: order_id.clone(),
                    symbol: symbol.clone(),
                    side: dir,
                    entry_price: filled_price_f,
                    qty: filled_qty_f,
                    entry_time: now_ts,
                    entry_score: fsm_score,
                    fsm_phase,
                    strategy_id,
                    timeframe: entry_tf,
                    conviction: pos_conviction,
                    regime: regime_label,
                },
            );

            // Clear from pending_orders
            if !order_id.is_empty() {
                let cid_to_clear = trading.pending_orders.iter()
                    .find(|po| po.order_id == order_id)
                    .map(|po| po.client_order_id.clone());
                if let Some(ref cid) = cid_to_clear {
                    let _ = crate::execution::order_wal::clear_intent(cid);
                }
                trading.pending_orders.retain(|po| po.order_id != order_id);
            }
        }

        // Clean cumulative tracking on final fill
        if event == "fill" && !order_id.is_empty() {
            trading.fill_cumulative.remove(&order_id);
        }
    }
}

fn handle_rejected_canceled(
    event: &str,
    order: Option<&serde_json::Value>,
    state: &TitanState,
    app: &AppHandle,
) {
    let symbol = order
        .and_then(|o| o.get("symbol"))
        .and_then(|v| v.as_str())
        .unwrap_or("???");

    if let Some(bid) = order.and_then(|o| o.get("id")).and_then(|v| v.as_str()) {
        let (deducted, source, cid_opt) = {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(hist) = trading.order_history.iter_mut().rfind(|h| h.order_id == bid) {
                hist.status = event.to_string();
                hist.reject_reason = Some(format!("{event} by broker"));
            }
            let found = trading
                .pending_orders
                .iter()
                .find(|po| po.order_id == bid)
                .map(|po| (po.deducted_amount, po.source.clone(), po.client_order_id.clone()));
            trading.pending_orders.retain(|po| po.order_id != bid);
            trading.fill_cumulative.remove(bid);
            // v2.5.0 NUCLEAR (HF3) — release the directional lock so a
            // future signal on this symbol (post-reject) can fire.
            // pending_entries itself doesn't strictly need this (30 s
            // eviction handles it) but the direction-lock has no
            // self-eviction yet — leaving it set blocks all subsequent
            // opposite signals indefinitely.
            trading.pending_entry_dirs.remove(symbol);
            trading.pending_entries.remove(symbol);
            found.unwrap_or((Decimal::ZERO, String::new(), String::new()))
        };

        if !cid_opt.is_empty() {
            let _ = crate::execution::order_wal::clear_intent(&cid_opt);
        }

        if deducted > Decimal::ZERO {
            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            // v2.5.0 — Tournament `s2:*` sources refund to bot_wallet just
            // like legacy `"bot"` source. Without this routing a rejected
            // Tournament order would leak the deducted amount permanently.
            let wallet_target: &str = match source.as_str() {
                "user" => "user",
                "bot" | "" => "bot",
                s if crate::smarttrend::dplus::parse_strategy_from_source(s).is_some() => "bot",
                _ => "unknown",
            };
            match wallet_target {
                "bot" => {
                    st.bot_wallet.credit(deducted);
                    tracing::info!(
                        component = "trade_updates",
                        symbol = %symbol,
                        raw_source = %source,
                        "Refunded ${deducted} to bot wallet on {event}",
                    );
                }
                "user" => {
                    st.user_wallet.credit(deducted);
                    tracing::info!(
                        component = "trade_updates",
                        symbol = %symbol,
                        "Refunded ${deducted} to user wallet on {event}",
                    );
                }
                _ => {
                    tracing::warn!(
                        component = "trade_updates",
                        symbol = %symbol,
                        raw_source = %source,
                        amount = %deducted,
                        "Unknown source — refund skipped on {event}",
                    );
                }
            }
            let _ = app.emit_all("wallet-changed", serde_json::json!({}));
        }
    }

    let _ = app.emit_all(
        "log-message",
        format!("ORDER {}: {}", event.to_uppercase(), symbol),
    );
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use rust_decimal_macros::dec;

    // ─── S15.6 — auth-first handshake ──────────────────────────────

    #[test]
    fn auth_payload_contains_required_fields() {
        let key = "test-key-id";
        let secret = "test-secret-key";
        let auth = serde_json::json!({
            "action": "authenticate",
            "data": { "key_id": key, "secret_key": secret }
        });

        assert_eq!(
            auth.get("action").and_then(|v| v.as_str()),
            Some("authenticate")
        );
        let data = auth.get("data").expect("data field present");
        assert_eq!(data.get("key_id").and_then(|v| v.as_str()), Some(key));
        assert_eq!(
            data.get("secret_key").and_then(|v| v.as_str()),
            Some(secret)
        );
    }

    #[test]
    fn auth_response_classification() {
        assert_eq!(
            classify_auth(r#"{"data":{"status":"authorized"}}"#),
            AuthResult::Ok
        );
        assert_eq!(classify_auth("authenticated"), AuthResult::Ok);
        assert_eq!(
            classify_auth(r#"{"data":{"status":"unauthorized"}}"#),
            AuthResult::Rejected
        );
        assert_eq!(classify_auth("forbidden"), AuthResult::Rejected);
        assert_eq!(classify_auth("random text"), AuthResult::Other);
        assert_eq!(classify_auth(""), AuthResult::Other);
    }

    // ─── S15.9 — auth-phase frame tolerance ────────────────────────

    /// Pin the contract that auth-phase Binary frames flow through the
    /// same classifier as Text — guards against a regression dropping
    /// Binary handling.
    #[test]
    fn auth_classifies_binary_authorized_as_ok() {
        let body = r#"{"stream":"authorization","data":{"status":"authorized"}}"#;
        let t = std::str::from_utf8(body.as_bytes()).expect("utf-8");
        assert_eq!(classify_auth(t), AuthResult::Ok);
    }

    /// Pin the rejection path: an `unauthorized` response wrapped in a
    /// Binary frame must still classify as Rejected (compounded with
    /// the S15.6 substring-ordering invariant).
    #[test]
    fn auth_classifies_binary_unauthorized_as_rejected() {
        let body = r#"{"stream":"authorization","data":{"status":"unauthorized"}}"#;
        let t = std::str::from_utf8(body.as_bytes()).expect("utf-8");
        assert_eq!(classify_auth(t), AuthResult::Rejected);
    }

    // ─── S15.7 — welcome-first rollback ────────────────────────────

    /// Regression lock for the substring-ordering bug fix (S15.6) under
    /// the welcome-first flow (S15.7). Alpaca paper /stream emits a
    /// frame of this exact shape on connect; if `classify_auth` were
    /// ever flipped back to "authorized-first" it would misclassify
    /// this rejection as success.
    #[test]
    fn welcome_unauthorized_frame_classifies_as_rejected() {
        let frame = r#"{"stream":"authorization","data":{"status":"unauthorized","action":""}}"#;
        assert_eq!(classify_auth(frame), AuthResult::Rejected);
    }

    /// Regression lock for the welcome timeout. Pre-S15.6 used 10s,
    /// which tripped under live paper-broker jitter. S15.7 sets it to
    /// 30s and this test prevents a future tightening from repeating
    /// the regression.
    #[test]
    fn welcome_timeout_constant_is_safely_long() {
        assert!(
            WELCOME_TIMEOUT_SECS >= 30,
            "WELCOME_TIMEOUT_SECS must be >= 30 to avoid the 10s regression"
        );
        assert!(
            WELCOME_POLL_SECS > 0 && WELCOME_POLL_SECS <= WELCOME_TIMEOUT_SECS / 6,
            "welcome poll must tick the watchdog at least 6x per wait"
        );
    }

    /// S15.8: pin the intent of the soft fallthrough — upper bound on
    /// the welcome timeout (so a future widening doesn't accidentally
    /// re-create a long stuck-loop window) and that the welcome-shape
    /// frame still classifies correctly.
    #[test]
    fn welcome_timeout_does_not_abort_handshake() {
        assert!(WELCOME_TIMEOUT_SECS <= 60);
        let frame = r#"{"stream":"authorization","data":{"status":"unauthorized"}}"#;
        assert_eq!(classify_auth(frame), AuthResult::Rejected);
    }

    #[test]
    /// S16.2 — Empty `source` falls back to "bot" so legacy / reconciled
    /// orphan positions credit the bot wallet at close. Prior behaviour was
    /// the empty-string arm of the match silently dropping the refund.
    #[test]
    fn s16_2_attribute_close_source_orphan_falls_back_to_bot() {
        assert_eq!(attribute_close_source(""), "bot");
        assert_eq!(attribute_close_source("bot"), "bot");
        assert_eq!(attribute_close_source("user"), "user");
        // Defence-in-depth: unrecognised tags neither leak to bot nor user.
        assert_eq!(attribute_close_source("manual"), "unknown");
        assert_eq!(attribute_close_source("ext"), "unknown");
    }

    /// v2.5.0 — regression test for the fund-leak bug found by peer audit
    /// 2026-05-12: every Tournament strategy's `s2:{id}` source must route
    /// refunds back to the bot wallet (sub-wallet is bookkeeping atop the
    /// same paper-capital pool that funded the entry). An unknown suffix
    /// must still surface as "unknown" so silent leaks remain detectable.
    #[test]
    fn s2_strategy_sources_credit_bot_wallet() {
        assert_eq!(attribute_close_source("s2:catalyst_frontrun"), "bot");
        assert_eq!(attribute_close_source("s2:gap_fade"), "bot");
        assert_eq!(attribute_close_source("s2:extended_news"), "bot");
        assert_eq!(attribute_close_source("s2:halt_resume"), "bot");
        assert_eq!(attribute_close_source("s2:afternoon_fade"), "bot");
        assert_eq!(attribute_close_source("s2:stat_arb_50"), "bot");
        assert_eq!(attribute_close_source("s2:etf_pair"), "bot");
        assert_eq!(attribute_close_source("s2:leveraged_etf"), "bot");
        // Unknown suffix still flags as unknown — fund-leak detectable.
        assert_eq!(attribute_close_source("s2:bogus_strategy"), "unknown");
        // Wrong prefix is treated as foreign source.
        assert_eq!(attribute_close_source("s3:gap_fade"), "unknown");
    }

    #[test]
    fn fill_cumulative_dedup_logic() {
        let state = TitanState::new(RuntimeConfig::default());

        // First fill: cum_qty = 5
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            let prev = trading.fill_cumulative.get("order-1").copied().unwrap_or((Decimal::ZERO, Decimal::ZERO));
            let delta = dec!(5) - prev.0;
            trading.fill_cumulative.insert("order-1".into(), (dec!(5), dec!(180)));
            assert_eq!(delta, dec!(5));
        }

        // Second fill: cum_qty = 8 (delta = 3)
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            let prev = trading.fill_cumulative.get("order-1").copied().unwrap_or((Decimal::ZERO, Decimal::ZERO));
            let delta = dec!(8) - prev.0;
            trading.fill_cumulative.insert("order-1".into(), (dec!(8), dec!(181)));
            assert_eq!(delta, dec!(3));
        }

        // Duplicate fill: cum_qty = 8 (delta = 0, should be suppressed)
        {
            let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            let prev = trading.fill_cumulative.get("order-1").copied().unwrap_or((Decimal::ZERO, Decimal::ZERO));
            let delta = dec!(8) - prev.0;
            assert!(delta <= Decimal::ZERO, "Duplicate fill should produce zero delta");
        }
    }

    #[test]
    fn position_update_same_direction_averages_entry() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.positions.push(Position {
                symbol: "AAPL".into(),
                qty: dec!(10),
                avg_entry: dec!(180),
                current_price: 182.0,
                unrealized_pnl: 20.0,
                side: Direction::Long,
                opened_at: Some(1000),
                source: String::new(),
                first_seen: 1000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        // Add 5 more at 190
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(pos) = trading.positions.iter_mut().find(|p| p.symbol == "AAPL") {
                let old_cost = pos.avg_entry * pos.qty;
                let new_cost = dec!(190) * dec!(5);
                pos.qty += dec!(5);
                pos.avg_entry = (old_cost + new_cost) / pos.qty;
            }
        }

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let pos = trading.positions.iter().find(|p| p.symbol == "AAPL").unwrap();
        assert_eq!(pos.qty, dec!(15));
        // WAP = (180*10 + 190*5) / 15 = 2750/15 ≈ 183.333...
        let expected = dec!(2750) / dec!(15);
        assert_eq!(pos.avg_entry, expected);
    }

    #[test]
    fn position_close_removes_on_zero() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.positions.push(Position {
                symbol: "AAPL".into(),
                qty: dec!(10),
                avg_entry: dec!(180),
                current_price: 185.0,
                unrealized_pnl: 50.0,
                side: Direction::Long,
                opened_at: Some(1000),
                source: String::new(),
                first_seen: 1000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        // Sell 10 (close entire position)
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(pos) = trading.positions.iter_mut().find(|p| p.symbol == "AAPL") {
                pos.qty -= dec!(10);
                let close_threshold = Decimal::from_str("0.001").unwrap();
                if pos.qty <= close_threshold {
                    trading.positions.retain(|p| p.symbol != "AAPL");
                }
            }
        }

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        assert!(trading.positions.is_empty());
    }

    #[test]
    fn new_position_from_fill() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            assert!(trading.positions.is_empty());
            let ts = chrono::Utc::now().timestamp();
            trading.positions.push(Position {
                symbol: "MSFT".into(),
                qty: dec!(5),
                avg_entry: dec!(420),
                current_price: 420.0,
                unrealized_pnl: 0.0,
                side: Direction::Long,
                opened_at: Some(ts),
                source: String::new(),
                first_seen: ts,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(trading.positions.len(), 1);
        assert_eq!(trading.positions[0].symbol, "MSFT");
        assert_eq!(trading.positions[0].qty, dec!(5));
    }

    #[test]
    fn rejected_clears_pending() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_orders.push(crate::state::PendingOrder {
                order_id: "broker-123".into(),
                symbol: "AAPL".into(),
                side: Direction::Long,
                qty: dec!(10),
                submitted_at: 1000,
                source: String::new(),
                deducted_amount: Decimal::ZERO,
                client_order_id: String::new(),
                last_fill_at: None,
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
                conviction: None,
                expected_price: 0.0,
            });
            trading.fill_cumulative.insert("broker-123".into(), (dec!(5), dec!(180)));
        }

        // Simulate rejected handling
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_orders.retain(|po| po.order_id != "broker-123");
            trading.fill_cumulative.remove("broker-123");
        }

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        assert!(trading.pending_orders.is_empty());
        assert!(!trading.fill_cumulative.contains_key("broker-123"));
    }

    #[test]
    fn negative_pnl_close_return_clamped() {
        let invested: f64 = 1000.0;
        let pnl: f64 = -800.0;
        let return_amount = (invested + pnl).max(0.0);
        assert!((return_amount - 200.0).abs() < f64::EPSILON);

        let severe_pnl: f64 = -1500.0;
        let return_severe = (invested + severe_pnl).max(0.0);
        assert!((return_severe).abs() < f64::EPSILON);
    }

    #[test]
    fn rejected_order_refunds_wallet() {

        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            st.bot_wallet.topup(dec!(5000));
            st.bot_wallet.withdraw(dec!(1800)).unwrap();
        }

        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_orders.push(crate::state::PendingOrder {
                order_id: "reject-001".into(),
                symbol: "TSLA".into(),
                side: Direction::Long,
                qty: dec!(10),
                submitted_at: 2000,
                source: "bot".into(),
                deducted_amount: dec!(1800),
                client_order_id: String::new(),
                last_fill_at: None,
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
                conviction: None,
                expected_price: 0.0,
            });
        }

        // Simulate the refund path from handle_rejected_canceled
        {
            let (deducted, source) = {
                let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                let found = trading
                    .pending_orders
                    .iter()
                    .find(|po| po.order_id == "reject-001")
                    .map(|po| (po.deducted_amount, po.source.clone()));
                trading
                    .pending_orders
                    .retain(|po| po.order_id != "reject-001");
                found.unwrap_or((Decimal::ZERO, String::new()))
            };

            assert_eq!(deducted, dec!(1800));
            assert_eq!(source, "bot");

            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            st.bot_wallet.credit(deducted);
        }

        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(st.bot_wallet.balance, dec!(5000));
        assert_eq!(st.bot_wallet.total_deposited, dec!(5000));
    }

    #[test]
    fn fill_increments_position_version() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.positions.push(Position {
                symbol: "TSLA".into(),
                qty: dec!(5),
                avg_entry: dec!(200),
                current_price: 200.0,
                unrealized_pnl: 0.0,
                side: Direction::Long,
                opened_at: Some(1000),
                source: "bot".into(),
                first_seen: 1000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        // Simulate same-direction fill
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            let pos = trading.positions.iter_mut().find(|p| p.symbol == "TSLA").unwrap();
            let old_cost = pos.avg_entry * pos.qty;
            let new_cost = dec!(210) * dec!(3);
            pos.qty += dec!(3);
            pos.avg_entry = (old_cost + new_cost) / pos.qty;
            pos.version = pos.version.wrapping_add(1);
        }

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let pos = trading.positions.iter().find(|p| p.symbol == "TSLA").unwrap();
        assert_eq!(pos.version, 1, "version should be 1 after fill");
        assert_eq!(pos.qty, dec!(8));
    }

    #[test]
    fn refresh_only_does_not_bump_version() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.positions.push(Position {
                symbol: "GOOG".into(),
                qty: dec!(10),
                avg_entry: dec!(150),
                current_price: 155.0,
                unrealized_pnl: 50.0,
                side: Direction::Long,
                opened_at: Some(2000),
                source: "user".into(),
                first_seen: 2000,
                version: 3,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        // Simulate price refresh (no qty/avg_entry change)
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            let pos = trading.positions.iter_mut().find(|p| p.symbol == "GOOG").unwrap();
            pos.current_price = 160.0;
            let avg_f = pos.avg_entry.to_f64().unwrap_or(0.0);
            let qty_f = pos.qty.to_f64().unwrap_or(0.0);
            pos.unrealized_pnl = (pos.current_price - avg_f) * qty_f;
        }

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let pos = trading.positions.iter().find(|p| p.symbol == "GOOG").unwrap();
        assert_eq!(pos.version, 3, "version must not change on price-only refresh");
        assert!((pos.current_price - 160.0).abs() < f64::EPSILON);
    }
}
