use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc::Receiver;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::connect_async;

use crate::market::candle_buffer::{timeframe_to_secs, CandleRingBuffer, FlashCrashDetector};
use crate::market::market_state::{compute_microprice, MarketDataState, OfiState};
use crate::state::{BarClosedMsg, TitanState};

/// **S18** — compute the set of symbols whose stream data we want to
/// keep in `MarketDataState` and process from incoming SIP messages.
///
/// When `cfg.auto_universe.enabled` is `true`, the universe is the
/// AutoUniverse `active_set()` (pinned ∪ auto-pool); otherwise it
/// falls back to legacy `cfg.ui.favorites`. The active UI symbol is
/// always included so chart/quotes work even for ad-hoc lookups.
///
/// Lock order: this helper acquires Tier-A `config(1)` then `market(2)`
/// in that strict order, then drops both before returning. Callers
/// must NOT hold any other Tier-A lock concurrently.
fn compute_watched_symbols(state: &TitanState) -> HashSet<String> {
    let (au_enabled, favorites, tournament_on): (bool, Vec<String>, bool) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.auto_universe.enabled,
            cfg.ui.favorites.clone(),
            cfg.evolution_protocol.enabled,
        )
    };
    let mut set = if au_enabled {
        let mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
        mkt.auto_universe.active_set()
    } else {
        favorites.into_iter().map(|s| s.to_uppercase()).collect()
    };
    {
        let mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
        let active = mkt.active_symbol.to_uppercase();
        if !active.is_empty() {
            set.insert(active);
        }
        // F1b — burst / halt auto-promotions
        for s in &mkt.burst_promoted {
            set.insert(s.clone());
        }
    }
    // PRIME F0.7 — the ETF/pair strategies (and the SPY regime probe)
    // need their fixed universe streamed regardless of the scanner's
    // small-cap hot list; without this the pair/leveraged strategies
    // never receive a single bar (zero trades in the whole journal).
    // Only unioned while the Evolution Protocol is on, so a legacy
    // FSM-only setup keeps its lean subscription set.
    if tournament_on {
        for sym in crate::smarttrend::strategies::PINNED_UNIVERSE {
            set.insert((*sym).to_string());
        }
    }
    set
}

fn prune_unwatched(market: &MarketDataState, watched: &HashSet<String>) {
    market
        .last_prices
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|k, _| watched.contains(k));
    market
        .cvd
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|k, _| watched.contains(k));
    market
        .candle_buffers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|k, _| {
            let sym = k.split(':').next().unwrap_or("");
            watched.contains(sym)
        });
    market
        .micro
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain_watched(watched);
}

const EMA_ALPHA: f64 = 0.3;
/// S34.1 (2026-05-28) — `"10m"` added so the streamer aggregates 10-min
/// candles. Pre-fix the chart UI exposed a `10m` button but no candle
/// buffer existed for it → FSM never received `BarClosedMsg{tf:"10m"}`
/// → zero FSM markers on the 10m chart (operator observed empty arrows
/// vs all other TFs). The streamer aggregates the listed TFs at the
/// same micro-cost (in-memory ring buffer per `symbol:tf`), so adding
/// `10m` is essentially free.
const ALL_TIMEFRAMES: &[&str] = &["1m", "5m", "10m", "15m", "30m", "1h", "4h", "1D"];

#[derive(Debug, Clone)]
pub enum StreamCmd {
    SetSymbol(String),
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // Fields stored for spread regime analysis (Sessions 9+)
struct QuoteTop {
    bid: f64,
    ask: f64,
    bid_size: f64,
    ask_size: f64,
}

// ─── Stream health metrics (BONUS 2) ───────────────────────────────

#[derive(Debug, Clone, Default, Serialize)]
pub struct StreamHealth {
    pub quotes_received: u64,
    pub trades_received: u64,
    pub reconnect_count: u32,
    pub last_quote_ts: Option<i64>,
    pub last_trade_ts: Option<i64>,
}

// ─── Bar-close event payload (BONUS 1) ─────────────────────────────

#[derive(Clone, Serialize)]
struct BarClosedPayload {
    symbol: String,
    timeframe: String,
    time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

/// Main streamer loop. Connects to Alpaca SIP, receives quotes/trades,
/// builds candle buffers, updates OFI, CVD, and emits Tauri events.
///
/// Uses SIP wildcard subscription -- symbol switch is instant (BONUS 3).
pub async fn run_streamer(
    state: Arc<TitanState>,
    market: Arc<MarketDataState>,
    mut rx: Receiver<StreamCmd>,
    bar_tx: tokio::sync::mpsc::Sender<BarClosedMsg>,
    app: AppHandle,
) {
    tracing::info!(component = "streamer", "Streamer task started");

    let mut quote = QuoteTop::default();
    let mut prev_bid_size: f64 = 0.0;
    let mut prev_ask_size: f64 = 0.0;
    let mut flash_detector = FlashCrashDetector::new(30, 5.0, 5);
    let mut stream_health = StreamHealth::default();
    let mut last_trade_price: f64 = 0.0;
    let mut backoff_secs: u64 = 1;

    loop {
        if state.shutdown.is_cancelled() {
            break;
        }

        let (key, secret, paper, sip): (String, String, bool, bool) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
                cfg.market.paper_mode,
                cfg.market.sip_feed,
            )
        };

        if key.is_empty() || secret.is_empty() {
            tracing::warn!(component = "streamer", "API keys missing, retrying in {backoff_secs}s");
            sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(60);
            continue;
        }

        // SIP data feed lives on the live endpoint even with paper keys
        // (SIP is a paid subscription independent of paper/live trading).
        // Only IEX on paper uses the sandbox host.
        let host = if sip || !paper { "stream.data.alpaca.markets" } else { "stream.data.sandbox.alpaca.markets" };
        let feed = if sip { "sip" } else { "iex" };
        let url = format!("wss://{host}/v2/{feed}");
        tracing::info!(component = "streamer", %url, "Connecting...");

        let ws = connect_async(&url).await;
        let Ok((mut socket, _)) = ws else {
            tracing::error!(component = "streamer", "Connection failed, retrying in {backoff_secs}s");
            market.mark_stream_stale();
            sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(60);
            continue;
        };

        // ── Auth ──
        let auth_msg = json!({ "action": "auth", "key": key, "secret": secret });
        if let Err(e) = socket
            .send(tokio_tungstenite::tungstenite::Message::Text(auth_msg.to_string()))
            .await
        {
            tracing::error!(component = "streamer", "Auth send error: {e}");
            sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(60);
            continue;
        }

        let mut authenticated = false;
        let auth_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < auth_deadline {
            match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(msg)))) => {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&msg) {
                        let items = match &parsed {
                            Value::Array(a) => a.iter().collect::<Vec<_>>(),
                            v => vec![v],
                        };
                        for item in &items {
                            match item.get("msg").and_then(|m| m.as_str()) {
                                Some("authenticated") => { authenticated = true; break; }
                                Some("auth failed") => break,
                                _ => {}
                            }
                        }
                    }
                    if authenticated { break; }
                }
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
                _ => {}
            }
        }

        if !authenticated {
            tracing::error!(component = "streamer", "Auth failed, retrying in {backoff_secs}s");
            sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(60);
            continue;
        }

        tracing::info!(component = "streamer", feed, "Authenticated! Subscribing...");
        backoff_secs = 1;
        stream_health.reconnect_count += 1;
        market.mark_stream_reconnected();

        // ── Subscribe (wildcard for instant symbol switch — BONUS 3) ──
        // S13: also subscribe `statuses:["*"]` to surface trading-halt
        // / pause / resume events. Halt parsing lives in
        // `MarketDataState::apply_status_message` (B5 lock only).
        let subscribe = json!({
            "action": "subscribe",
            "trades": ["*"],
            "quotes": ["*"],
            "statuses": ["*"]
        });
        if let Err(e) = socket
            .send(tokio_tungstenite::tungstenite::Message::Text(subscribe.to_string()))
            .await
        {
            tracing::error!(component = "streamer", "Subscribe error: {e}");
            continue;
        }

        {
            let mut mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
            mkt.connected = true;
        }
        let _ = app.emit_all("stream-status", "CONNECTED");

        // ── Update system health ──
        {
            let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = sys.loops.get_mut("streamer") {
                h.tick();
            }
        }

        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        let mut prune_interval = tokio::time::interval(Duration::from_secs(60));
        prune_interval.tick().await; // consume the immediate tick

        // ── Inner message loop ──
        loop {
            if state.shutdown.is_cancelled() {
                break;
            }

            tokio::select! {
                _ = state.shutdown.cancelled() => { break; }

                _ = prune_interval.tick() => {
                    let watched = compute_watched_symbols(&state);
                    prune_unwatched(&market, &watched);
                }

                Some(cmd) = rx.recv() => {
                    match cmd {
                        StreamCmd::SetSymbol(sym) => {
                            tracing::info!(component = "streamer", "Symbol switch to {sym} (no reconnect)");
                            {
                                let mut mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
                                mkt.active_symbol = sym.clone();
                            }
                            quote = QuoteTop::default();
                            prev_bid_size = 0.0;
                            prev_ask_size = 0.0;
                            last_trade_price = 0.0;
                            let _ = app.emit_all("symbol-changed", &sym);
                        }
                    }
                }

                _ = heartbeat.tick() => {
                    market.mark_stream_data_received();
                    {
                        let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(h) = sys.loops.get_mut("streamer") {
                            h.tick();
                        }
                    }
                }

                msg = socket.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            process_message(
                                &text,
                                &state, &market, &app, &bar_tx,
                                &mut quote, &mut prev_bid_size, &mut prev_ask_size,
                                &mut flash_detector, &mut stream_health,
                                &mut last_trade_price,
                            ).await;
                        }
                        Some(Err(e)) => {
                            tracing::error!(component = "streamer", "WebSocket error: {e}");
                            break;
                        }
                        None => {
                            tracing::warn!(component = "streamer", "WebSocket closed");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        // Disconnected — mark stale and loop back for reconnect
        {
            let mut mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
            mkt.connected = false;
        }
        market.mark_stream_stale();
        let _ = app.emit_all("stream-status", "DISCONNECTED");
        tracing::warn!(component = "streamer", "Disconnected, reconnecting in {backoff_secs}s...");
        sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }

    tracing::info!(component = "streamer", "Streamer task exiting");
}

#[allow(clippy::too_many_arguments)]
async fn process_message(
    text: &str,
    state: &TitanState,
    market: &MarketDataState,
    app: &AppHandle,
    bar_tx: &tokio::sync::mpsc::Sender<BarClosedMsg>,
    quote: &mut QuoteTop,
    prev_bid_size: &mut f64,
    prev_ask_size: &mut f64,
    flash_detector: &mut FlashCrashDetector,
    stream_health: &mut StreamHealth,
    last_trade_price: &mut f64,
) {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };

    let items: Vec<&Value> = match &parsed {
        Value::Array(arr) => arr.iter().collect(),
        v => vec![v],
    };

    // S05/S18: Tier A order — config(1) before market(2), never
    // overlapped. The helper resolves the AutoUniverse vs legacy
    // favorites dispatch once per inbound message batch. The active
    // symbol is included unconditionally so chart subscribers work
    // regardless of auto_universe.enabled.
    let watched_symbols = compute_watched_symbols(state);
    let active_symbol = {
        let mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
        mkt.active_symbol.clone()
    };

    for item in items {
        let raw_sym = item.get("S").and_then(|v| v.as_str()).unwrap_or("");
        let sym_upper = raw_sym.to_uppercase();
        let sym = sym_upper.as_str();
        let msg_type = item.get("T").and_then(|v| v.as_str()).unwrap_or("");

        // S13: status messages are dispatched BEFORE the watched-symbols
        // filter — halt state must be tracked globally for the scanner /
        // watchlist, not only for symbols already on the FSM.
        if msg_type == "s" {
            market.apply_status_message(item);
            // F1b — auto-pin halt symbols into watched via burst_promoted
            let sc = item.get("sc").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(sc, "H" | "2" | "P") && !sym.is_empty() {
                promote_burst_symbol(state, sym);
            }
            continue;
        }

        // F1b — tape-wide burst detector runs on ALL trades (pre-filter).
        if msg_type == "t" && !sym.is_empty() {
            let price = item.get("p").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let size = item.get("s").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if price > 0.0 && size > 0.0 {
                // MERIDIAN C-PM — premarket tape (B9, acquired alone; no-op
                // outside 04:00–10:30 ET after one cached minute check).
                {
                    let mut tape = market.premarket_tape.lock().unwrap_or_else(|p| p.into_inner());
                    tape.on_trade(sym, price, size, chrono::Utc::now());
                }
                let promo = {
                    let mut burst = market.burst.lock().unwrap_or_else(|p| p.into_inner());
                    burst.on_trade(sym, price, size, Instant::now())
                };
                if let Some(p) = promo {
                    tracing::info!(
                        component = "streamer",
                        symbol = %p.symbol,
                        nps = p.notional_per_sec,
                        move_pct = p.move_pct_60s,
                        "F1b burst promotion — adding to watched"
                    );
                    promote_burst_symbol(state, &p.symbol);
                }
            }
        }

        let is_active = sym == active_symbol;
        let is_watched = is_active || watched_symbols.contains(sym)
            || {
                let mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
                mkt.burst_promoted.contains(sym)
            };

        if !is_watched {
            continue;
        }

        match msg_type {
            "q" => {
                // F1: quotes for ALL watched symbols feed MicroBook;
                // chart-active path still updates global ofi_state + UI.
                if is_active {
                    handle_quote(
                        item, sym, market, app,
                        quote, prev_bid_size, prev_ask_size, stream_health,
                    );
                } else {
                    feed_micro_quote(item, sym, market);
                }
            }
            "t" => handle_trade(
                item, sym, state, market, app, bar_tx,
                flash_detector, stream_health, last_trade_price,
                is_active,
            ).await,
            _ => {}
        }
    }
}

/// F1b — promote a symbol into `MarketState.burst_promoted` (Tier-A
/// market(2) only; never called while holding Tier-B). Cap 30/session.
fn promote_burst_symbol(state: &TitanState, symbol: &str) {
    const CAP: usize = 30;
    let key = symbol.trim().to_uppercase();
    if key.is_empty() {
        return;
    }
    let mut mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
    if mkt.burst_promoted.contains(&key) {
        return;
    }
    if mkt.burst_promoted.len() >= CAP {
        return;
    }
    mkt.burst_promoted.insert(key);
}

fn feed_micro_quote(item: &Value, symbol: &str, market: &MarketDataState) {
    let bp = item.get("bp").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let ap = item.get("ap").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let bs = item.get("bs").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let az = item.get("as").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if bp <= 0.0 || ap <= 0.0 {
        return;
    }
    market
        .micro
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .on_quote(symbol, bp, ap, bs, az, Instant::now());
}

#[allow(clippy::too_many_arguments)]
fn handle_quote(
    item: &Value,
    symbol: &str,
    market: &MarketDataState,
    app: &AppHandle,
    quote: &mut QuoteTop,
    prev_bid_size: &mut f64,
    prev_ask_size: &mut f64,
    health: &mut StreamHealth,
) {
    let bp = item.get("bp").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let ap = item.get("ap").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let bs = item.get("bs").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let az = item.get("as").and_then(|v| v.as_f64()).unwrap_or(0.0);

    if bp <= 0.0 || ap <= 0.0 {
        return;
    }

    *quote = QuoteTop { bid: bp, ask: ap, bid_size: bs, ask_size: az };
    health.quotes_received += 1;
    if let Some(ts) = parse_timestamp(item) {
        health.last_quote_ts = Some(ts);
    }

    // OFI from NBBO size deltas
    let bid_delta = bs - *prev_bid_size;
    let ask_delta = az - *prev_ask_size;
    let ofi_tick = bid_delta - ask_delta;
    *prev_bid_size = bs;
    *prev_ask_size = az;

    let prev_ofi = market.ofi_state.load();
    let smoothed_ofi = prev_ofi.current_ofi * (1.0 - EMA_ALPHA) + ofi_tick * EMA_ALPHA;

    let total_size = bs + az;
    let imbalance = if total_size > 0.0 { (bs - az) / total_size } else { 0.0 };
    let spread_bps = if bp > 0.0 { ((ap - bp) / bp) * 10_000.0 } else { 0.0 };
    let mp = compute_microprice(bp, ap, bs, az);

    let new_ofi = OfiState {
        current_ofi: smoothed_ofi,
        best_bid: bp,
        best_ask: ap,
        spread_bps,
        bid_depth_total: bs,
        ask_depth_total: az,
        imbalance_ratio: imbalance,
        microprice: mp,
        update_count: prev_ofi.update_count + 1,
        last_update: Some(Instant::now()),
    };
    market.ofi_state.store(Arc::new(new_ofi));

    // F1 — also feed MicroBook for the active symbol (same quote).
    market
        .micro
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .on_quote(symbol, bp, ap, bs, az, Instant::now());

    let _ = app.emit_all("ofi-update", serde_json::json!({
        "symbol": symbol, "ofi": smoothed_ofi, "microprice": mp,
        "spread_bps": spread_bps, "imbalance": imbalance,
    }));
}

#[allow(clippy::too_many_arguments)]
async fn handle_trade(
    item: &Value,
    symbol: &str,
    _state: &TitanState,
    market: &MarketDataState,
    app: &AppHandle,
    bar_tx: &tokio::sync::mpsc::Sender<BarClosedMsg>,
    flash_detector: &mut FlashCrashDetector,
    health: &mut StreamHealth,
    last_trade_price: &mut f64,
    is_active: bool,
) {
    let price = item.get("p").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let size = item.get("s").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let ts = parse_timestamp(item);

    if price <= 0.0 {
        return;
    }

    if is_active {
        health.trades_received += 1;
        if let Some(t) = ts { health.last_trade_ts = Some(t); }
    }

    // F1 — MicroBook trade (B7 alone) before B1/B2/B3 chain.
    market
        .micro
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .on_trade(symbol, price, size, Instant::now());

    // Quote-aligned side once — used by CVD + F2 resume-print capture.
    // B7 is acquired+dropped here before any lower Tier-B lock.
    let is_buy = {
        let book = market.micro.lock().unwrap_or_else(|p| p.into_inner());
        match book.get(symbol) {
            Some(snap) => crate::market::micro::classify_quote_aligned(
                price, snap.bid, snap.ask, *last_trade_price,
            ),
            None => classify_trade(price, *last_trade_price),
        }
    };

    // S05 Tier B order: last_prices(1) → cvd(2) → candle_buffers(3)
    market.last_prices.lock().unwrap_or_else(|p| p.into_inner()).insert(symbol.to_string(), price);

    if is_active {
        *last_trade_price = price;
        let cvd_delta = if is_buy { size } else { -size };
        *market.cvd.lock().unwrap_or_else(|p| p.into_inner()).entry(symbol.to_string()).or_insert(0.0) += cvd_delta;

        if let Some(ev) = flash_detector.check_trade(symbol, price, Instant::now()) {
            tracing::warn!(component = "streamer", symbol, change_pct = ev.change_pct, "Flash crash!");
            let _ = app.emit_all("flash-crash", &ev.symbol);
        }
    }

    for &tf in ALL_TIMEFRAMES {
        let tf_secs = timeframe_to_secs(tf);
        let buf_key = format!("{symbol}:{tf}");

        let last_candle = {
            let mut buffers = market.candle_buffers.lock().unwrap_or_else(|p| p.into_inner());
            let buf = buffers
                .entry(buf_key)
                .or_insert_with(|| CandleRingBuffer::new(tf_secs));
            let closed = buf.update_trade(
                price,
                size,
                ts.unwrap_or_else(|| chrono::Utc::now().timestamp()),
            );
            if closed {
                buf.last_closed().cloned()
            } else {
                None
            }
        };

        if let Some(c) = last_candle {
            if is_active {
                let _ = app.emit_all(
                    "bar-closed",
                    &BarClosedPayload {
                        symbol: symbol.to_string(),
                        timeframe: tf.to_string(),
                        time: c.time,
                        open: c.open,
                        high: c.high,
                        low: c.low,
                        close: c.close,
                        volume: c.volume,
                    },
                );
            }
            let msg = BarClosedMsg {
                symbol: symbol.to_string(),
                timeframe: tf.to_string(),
                bar_time: c.time,
                close_price: c.close,
            };
            match bar_tx.send_timeout(msg, std::time::Duration::from_millis(100)).await {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::SendTimeoutError::Timeout(_)) => {
                    market
                        .bars_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        component = "streamer",
                        symbol = %symbol,
                        tf = %tf,
                        "bar dropped, sentinel channel full after 100ms timeout"
                    );
                }
                Err(tokio::sync::mpsc::error::SendTimeoutError::Closed(_)) => {
                    // Phase 6: the legacy consumer is retired, so a closed
                    // receiver is a configuration state, not an incident.
                    // Counted, logged once per process; never per bar
                    // (2026-09-18: 25 658 ERROR lines from this arm).
                    let n = market.bars_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n == 0 {
                        tracing::warn!(component = "streamer", symbol = %symbol, "bar_tx closed — legacy bar consumer gone; further closed bars are counted, not logged");
                    }
                }
            }
        }
    }

    // F2 — first prints after SIP resume (B5 alone; all other Tier-B dropped).
    market.record_resume_print(symbol, price, size, is_buy);

    let _ = app.emit_all("price-update", serde_json::json!({
        "symbol": symbol, "price": price, "size": size, "ts": ts,
    }));
}

/// Simplified Lee-Ready: compare trade price to last trade price (tick test).
fn classify_trade(price: f64, last_price: f64) -> bool {
    price >= last_price
}

fn parse_timestamp(item: &Value) -> Option<i64> {
    let raw = item.get("t").and_then(|v| v.as_str())?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp())
        .or_else(|| raw.parse::<i64>().ok())
        .or_else(|| {
            tracing::warn!(component = "streamer", raw, "parse_timestamp: unparseable");
            None
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_trade_buy_on_uptick() {
        assert!(classify_trade(100.5, 100.0));
    }

    #[test]
    fn classify_trade_sell_on_downtick() {
        assert!(!classify_trade(99.5, 100.0));
    }

    #[test]
    fn classify_trade_buy_on_zero_tick() {
        assert!(classify_trade(100.0, 100.0));
    }

    #[test]
    fn parse_timestamp_rfc3339() {
        let item = serde_json::json!({"t": "2024-01-15T14:30:00Z"});
        let ts = parse_timestamp(&item);
        assert!(ts.is_some());
        assert!(ts.unwrap() > 0);
    }

    #[test]
    fn parse_timestamp_missing_returns_none() {
        let item = serde_json::json!({"x": "no timestamp"});
        let ts = parse_timestamp(&item);
        assert!(ts.is_none());
    }

    #[test]
    fn parse_timestamp_garbage_returns_none() {
        let item = serde_json::json!({"t": "not-a-date"});
        let ts = parse_timestamp(&item);
        assert!(ts.is_none());
    }

    #[test]
    fn prune_unwatched_retains_watched_only() {
        use crate::market::candle_buffer::CandleRingBuffer;

        let market = MarketDataState::new();
        {
            let mut lp = market.last_prices.lock().unwrap();
            for sym in &["AAPL", "MSFT", "TSLA", "GOOG", "NVDA"] {
                lp.insert(sym.to_string(), 100.0);
            }
        }
        {
            let mut cvd = market.cvd.lock().unwrap();
            for sym in &["AAPL", "MSFT", "TSLA", "GOOG", "NVDA"] {
                cvd.insert(sym.to_string(), 50.0);
            }
        }
        {
            let mut cb = market.candle_buffers.lock().unwrap();
            for sym in &["AAPL", "MSFT", "TSLA", "GOOG", "NVDA"] {
                cb.insert(format!("{sym}:300"), CandleRingBuffer::new(300));
            }
        }

        let watched: HashSet<String> = ["AAPL", "MSFT"].iter().map(|s| s.to_string()).collect();
        prune_unwatched(&market, &watched);

        let lp = market.last_prices.lock().unwrap();
        assert_eq!(lp.len(), 2);
        assert!(lp.contains_key("AAPL"));
        assert!(lp.contains_key("MSFT"));

        let cvd = market.cvd.lock().unwrap();
        assert_eq!(cvd.len(), 2);

        let cb = market.candle_buffers.lock().unwrap();
        assert_eq!(cb.len(), 2);
    }
}
