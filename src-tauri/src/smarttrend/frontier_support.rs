//! F3 helpers — fail-closed FrontierContext builder + marker/shadow I/O.

use std::collections::{HashMap, HashSet};

use chrono::{Timelike, Utc};
use tauri::{AppHandle, Manager};

use crate::market::market_state::{MarketDataState, ResumeEvent};
use crate::smarttrend::markers::create_marker;
use crate::smarttrend::strategies::frontier::{
    FrontierCandidate, FrontierContext, ResumeSnapshot,
};
use crate::state::{MarkerKind, TitanState};

pub(crate) fn build_frontier_context(
    _state: &TitanState,
    market: &MarketDataState,
    symbol: &str,
    resume_ev: Option<&ResumeEvent>,
    entered_today: &HashMap<String, HashSet<&'static str>>,
) -> Option<FrontierContext> {
    let key = symbol.to_uppercase();
    let price = {
        let lp = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
        *lp.get(&key)?
    };
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let candles_1m = {
        let buffers = market.candle_buffers.lock().unwrap_or_else(|p| p.into_inner());
        buffers.get(&format!("{key}:1m"))?.snapshot()
    };
    if candles_1m.len() < 5 {
        return None;
    }
    let halt_row = {
        let halts = market.halts.lock().unwrap_or_else(|p| p.into_inner());
        halts.get(&key).cloned()
    };
    if halt_row
        .as_ref()
        .map(|h| h.resumed_at.is_none())
        .unwrap_or(false)
    {
        return None;
    }
    let micro = {
        let book = market.micro.lock().unwrap_or_else(|p| p.into_inner());
        book.get(&key)?.clone()
    };
    if !micro.fresh || micro.spread_bps <= 0.0 {
        return None;
    }

    let atr = {
        let mut trs = Vec::new();
        for i in 1..candles_1m.len() {
            let h = candles_1m[i].high;
            let l = candles_1m[i].low;
            let pc = candles_1m[i - 1].close;
            trs.push((h - l).max((h - pc).abs()).max((l - pc).abs()));
        }
        let n = 14.min(trs.len()).max(1);
        trs[trs.len() - n..].iter().sum::<f64>() / n as f64
    };

    let now_et = Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let now_et_min = now_et.hour() * 60 + now_et.minute();
    let prev_close = if candles_1m.len() >= 2 {
        candles_1m[candles_1m.len() - 2].close
    } else {
        price
    };
    let session_open = candles_1m
        .iter()
        .find(|c| {
            let Some(dt) = chrono::DateTime::<Utc>::from_timestamp(c.time, 0) else {
                return false;
            };
            let et = dt.with_timezone(&chrono_tz::US::Eastern);
            et.format("%Y-%j").to_string() == now_et.format("%Y-%j").to_string()
                && (et.hour() * 60 + et.minute()) >= 9 * 60 + 30
        })
        .map(|c| c.open)
        .unwrap_or(prev_close);

    let resume = if let Some(ev) = resume_ev {
        Some(ResumeSnapshot {
            resumed_at: ev.resumed_at,
            halt_code: ev.halt_code.clone(),
            pre_halt_price: ev.pre_halt_price,
            halted_since: ev.halted_since,
            first_prints: halt_row
                .as_ref()
                .map(|h| {
                    h.resume_prints
                        .iter()
                        .map(|p| (p.price, p.size, p.is_buy))
                        .collect()
                })
                .unwrap_or_default(),
        })
    } else {
        halt_row
            .as_ref()
            .filter(|h| h.resumed_at.is_some())
            .map(|h| ResumeSnapshot {
                resumed_at: h.resumed_at.unwrap(),
                halt_code: h.halt_code.clone(),
                pre_halt_price: h.pre_halt_price,
                halted_since: h.halted_since,
                first_prints: h
                    .resume_prints
                    .iter()
                    .map(|p| (p.price, p.size, p.is_buy))
                    .collect(),
            })
    };

    let entered_today_set = entered_today.get(&key).cloned().unwrap_or_default();
    // Catalyst context from watchlist metrics (Tier-B B6, own scope —
    // no other guard is held here). Absent ⇒ no catalyst, not an error:
    // burst-promoted symbols may not be on the scanner watchlist yet.
    let (catalyst_news, catalyst_gap_pct) = {
        let wm = market
            .watchlist_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        wm.get(&key)
            .map(|m| (m.catalyst_flags.news_recent, m.gap_percent))
            .unwrap_or((false, 0.0))
    };
    let micro_age_secs = micro.update_count.min(600);
    let baseline_pps_5m = {
        let vols: Vec<f64> = candles_1m.iter().rev().take(5).map(|c| c.volume).collect();
        if vols.is_empty() {
            0.0
        } else {
            let mean_vol = vols.iter().sum::<f64>() / vols.len() as f64;
            (mean_vol / 100.0) / 60.0
        }
    };

    Some(FrontierContext {
        symbol: key,
        price,
        atr,
        candles_1m,
        micro,
        now_et_min,
        now_utc: Utc::now(),
        micro_age_secs,
        resume,
        prev_close,
        session_open,
        baseline_pps_5m,
        entered_today: entered_today_set,
        catalyst_news,
        catalyst_gap_pct,
    })
}

pub(crate) fn resolve_shadows(state: &TitanState, market: &MarketDataState) {
    let prices: HashMap<String, f64> = {
        let lp = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
        lp.clone()
    };
    let Some(dp) = state.dplus() else { return };
    let Ok(mut book) = dp.shadow.lock() else { return };
    let ts = Utc::now().timestamp();
    for (sym, px) in prices {
        let _ = book.on_price(&sym, px, ts);
    }
}

pub(crate) fn emit_frontier_marker(
    state: &TitanState,
    app: &AppHandle,
    symbol: &str,
    price: f64,
    cand: &FrontierCandidate,
    shadow: bool,
) {
    let mut marker = create_marker(
        MarkerKind::Entry,
        cand.signal.side,
        Utc::now().timestamp(),
        price,
        cand.signal.score,
        "1m",
        symbol,
    );
    marker.source = Some("frontier".into());
    marker.setup = Some(cand.setup.as_str().into());
    marker.conviction = Some(cand.conviction);
    marker.verdict = Some(if shadow { "SHADOW".into() } else { "GO".into() });
    marker.display_only = shadow;

    let payload = {
        let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.markers.push(marker.clone());
        const CAP: usize = 2_000;
        if st.markers.len() > CAP {
            let drain = st.markers.len() - CAP;
            st.markers.drain(0..drain);
        }
        serde_json::json!({
            "symbol": symbol,
            "phase": "Frontier",
            "prev_phase": "Idle",
            "score": cand.signal.score,
            "direction": format!("{:?}", cand.signal.side),
            "bar_time": marker.bar_time,
            "timeframe": "any",
            "markers": [{
                "kind": "Entry",
                "direction": format!("{:?}", cand.signal.side),
                "price": marker.price,
                "score": marker.score,
                "bar_time": marker.bar_time,
                "display_only": marker.display_only,
                "source": marker.source,
                "setup": marker.setup,
                "conviction": marker.conviction,
                "verdict": marker.verdict,
            }]
        })
    };
    let _ = app.emit_all("smarttrend-alert", &payload);
}
