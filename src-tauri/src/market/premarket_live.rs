//! MERIDIAN C-PM live loop — headline → armed tracker → premarket bars →
//! second-level alerts (plan §3.5-PM, phase 4a: alerts unconditional,
//! execution gated behind the phase-3 backtest verdict).
//!
//! Wiring:
//!   * `news_stream::subscribe_headlines()` — Benzinga headlines within
//!     seconds of publication, classified with `titan_meridian::catalyst`.
//!   * `MarketDataState.premarket_tape` (B9) — 1-min bars built from ALL
//!     SIP prints; drained once per second for armed symbols only.
//!   * previous close — Alpaca snapshot REST, fetched per armed headline
//!     with no lock held.
//!
//! Lock discipline: Tier-A `config` is read alone; Tier-B9 is acquired
//! alone and dropped before any `emit_all` (emit-after-drop, S05). The
//! book itself is task-local — no shared mutable state.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tauri::Manager;
use titan_meridian::catalyst::classify;
use titan_meridian::session::{ext_index, ext_idx_at};
use titan_meridian::sleeves::PmParams;

use super::alpaca_news::NewsItem;
use super::market_state::MarketDataState;
use super::news_stream::subscribe_headlines;
use super::premarket_book::{Headline, PmLiveBook, PmSignal, SignalKind};
use super::scanner::{fetch_snapshots_batch, IgnitionAlert};
use crate::state::TitanState;

const LOOP_NAME: &str = "premarket_live";
/// Sizing base for the suggested share count in alerts until phase 4b
/// binds C-PM to the paper account equity.
const SIZING_EQUITY: f64 = 100_000.0;

/// US common-stock ticker heuristic shared with the research loader.
fn is_us_symbol(s: &str) -> bool {
    !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// Translate an entry signal into the alert payload the frontend already
/// renders (`scanner-ignition` toast + chirp). RVOL is not known on the
/// tape path; the LIVE pill tells the UI which sensor fired.
pub fn to_ignition_alert(s: &PmSignal) -> IgnitionAlert {
    IgnitionAlert {
        symbol: s.symbol.clone(),
        price: s.price,
        change_percent: s.change_pct,
        rvol: 0.0,
        dollar_volume: s.cum_dollar,
        score: 100.0,
        news_headline: s.headline.clone(),
        news_url: s.url.clone(),
        catalyst_pills: vec!["LIVE".into(), s.class.as_str().into(), format!("{:.0} sh", s.shares)],
    }
}

async fn prev_close(state: &TitanState, symbol: &str) -> Option<f64> {
    let (key, secret) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (cfg.market.alpaca_key_id.clone(), cfg.market.alpaca_secret.clone())
    };
    if key.is_empty() {
        return None;
    }
    let snaps = fetch_snapshots_batch(&state.http_client, &key, &secret, &[symbol.to_string()], None).await;
    let snap = snaps.get(symbol)?;
    // During premarket `dailyBar` may already be today's partial bar;
    // the previous *session* close is always `prevDailyBar`.
    snap.prev_daily_bar.as_ref().map(|b| b.c).filter(|c| *c > 0.0)
}

async fn arm_from_headline(book: &mut PmLiveBook, state: &TitanState, item: &NewsItem) {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(&item.created_at) else { return };
    let now = Utc::now();
    let (date, idx) = match ext_index(t.with_timezone(&Utc)) {
        Some(x) => x,
        None => return, // outside 04:00–20:00 ET
    };
    if idx > book.params.entry_last_idx || ext_index(now).map(|x| x.0) != Some(date) {
        return; // stale or after the entry window
    }
    let class = classify(&item.headline);
    if !class.is_tradeable_long() && !class.is_excluded() {
        return;
    }
    for sym in item.symbols.iter().filter(|s| is_us_symbol(s)) {
        // Excluded classes need no price; tradeable ones need prev close.
        let pc = if class.is_excluded() { 1.0 } else { prev_close(state, sym).await.unwrap_or(0.0) };
        let h = Headline { date, idx, symbol: sym.clone(), class, headline: item.headline.clone(), url: item.url.clone(), prev_close: pc };
        let outcome = book.on_headline(&h);
        tracing::info!(component = LOOP_NAME, symbol = %sym, class = class.as_str(), idx, outcome, headline = %item.headline, "headline");
    }
}

fn emit_signals(app: &tauri::AppHandle, signals: &[PmSignal]) {
    if signals.is_empty() {
        return;
    }
    let entries: Vec<IgnitionAlert> = signals.iter().filter(|s| s.kind == SignalKind::Entry).map(to_ignition_alert).collect();
    for s in signals {
        tracing::info!(
            component = LOOP_NAME,
            symbol = %s.symbol,
            kind = ?s.kind,
            bar_idx = s.bar_idx,
            price = s.price,
            shares = s.shares,
            change_pct = s.change_pct,
            "C-PM signal"
        );
    }
    if !entries.is_empty() {
        let _ = app.emit_all("scanner-ignition", &entries);
    }
    let _ = app.emit_all("meridian-pm-signal", signals);
}

pub async fn run_premarket_live(state: Arc<TitanState>, market_data: Arc<MarketDataState>, app: tauri::AppHandle) {
    tracing::info!(component = LOOP_NAME, "premarket live loop started");
    let mut rx = subscribe_headlines();
    let mut book = PmLiveBook::new(PmParams::default(), SIZING_EQUITY);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let flat_idx = ext_idx_at(10, 30);
    let mut flattened_today = None;

    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            item = rx.recv() => {
                match item {
                    Ok(item) => arm_from_headline(&mut book, &state, &item).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(component = LOOP_NAME, dropped = n, "headline bus lagged");
                    }
                    Err(_) => break,
                }
            }
            _ = tick.tick() => {
                let now = Utc::now();
                let armed = book.armed_symbols();
                // ─── Tier-B9 alone: close stale bars, drain armed symbols ───
                let (bars, last_px) = {
                    let mut tape = market_data.premarket_tape.lock().unwrap_or_else(|p| p.into_inner());
                    tape.close_stale(now);
                    let mut bars = Vec::with_capacity(armed.len());
                    let mut last_px = HashMap::new();
                    for s in &armed {
                        let b = tape.drain_bars(s);
                        if let Some(snap) = tape.snapshot(s) {
                            last_px.insert(s.clone(), snap.last);
                        }
                        bars.push((s.clone(), b));
                    }
                    (bars, last_px)
                };
                // guard dropped — pure compute, then emit
                let mut signals = Vec::new();
                for (s, b) in bars {
                    if !b.is_empty() {
                        signals.extend(book.on_bars(&s, &b));
                    }
                }
                if let Some((date, idx)) = ext_index(now) {
                    if idx >= flat_idx && flattened_today != Some(date) && !armed.is_empty() {
                        signals.extend(book.flatten_all(flat_idx, &last_px));
                        flattened_today = Some(date);
                    }
                }
                emit_signals(&app, &signals);
                {
                    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
                        h.tick();
                    }
                }
            }
        }
    }
    tracing::info!(component = LOOP_NAME, "premarket live loop exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use titan_meridian::catalyst::CatalystClass;

    #[test]
    fn ignition_alert_carries_live_pill_and_size() {
        let s = PmSignal {
            symbol: "DLTH".into(),
            class: CatalystClass::EarningsBeat,
            headline: "beat".into(),
            url: "u".into(),
            kind: SignalKind::Entry,
            bar_idx: 109,
            event_idx: 105,
            price: 3.48,
            shares: 1000.0,
            prev_close: 3.0,
            change_pct: 15.0,
            cum_dollar: 600_000.0,
        };
        let a = to_ignition_alert(&s);
        assert_eq!(a.symbol, "DLTH");
        assert_eq!(a.catalyst_pills, vec!["LIVE", "EarningsBeat", "1000 sh"]);
        assert_eq!(a.dollar_volume, 600_000.0);
        assert!(is_us_symbol("DLTH") && !is_us_symbol("TSX:ABC"));
    }
}
