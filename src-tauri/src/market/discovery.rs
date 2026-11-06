use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::market::market_state::MarketDataState;
use crate::market::scanner::{
    in_actionable_scan_window, is_current_session_trade, now_et, session_date_et_str,
};
use crate::state::{Mover, TitanState};

#[derive(Debug, Deserialize)]
struct AlpacaMoverItem {
    symbol: String,
    price: Option<f64>,
    change: Option<f64>,
    #[serde(alias = "trade_count")]
    volume: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct AlpacaMoversResponse {
    #[serde(alias = "most_actives")]
    movers: Option<Vec<AlpacaMoverItem>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AlpacaSnapshot {
    #[serde(alias = "latestTrade")]
    latest_trade: Option<AlpacaSnapshotTrade>,
    #[serde(alias = "dailyBar")]
    daily_bar: Option<AlpacaSnapshotBar>,
    #[serde(alias = "minuteBar")]
    minute_bar: Option<AlpacaSnapshotBar>,
    #[serde(alias = "prevDailyBar")]
    prev_daily_bar: Option<AlpacaSnapshotBar>,
}

#[derive(Debug, Deserialize)]
struct AlpacaSnapshotTrade {
    p: Option<f64>,
    #[serde(default)]
    t: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlpacaSnapshotBar {
    v: Option<f64>,
    o: Option<f64>,
    c: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeSpikeEvent {
    pub symbol: String,
    pub volume: f64,
    pub price: f64,
    pub change_pct: f64,
}

/// SC11 — movers refresh only inside the actionable ET scan window
/// (NYSE day, 04:05–15:30). Replaces the old fixed UTC−4 hack.
fn is_movers_active_hours() -> bool {
    in_actionable_scan_window(now_et())
}

async fn fetch_movers_screener(
    client: &reqwest::Client,
    api_key: &str,
    api_secret: &str,
) -> Result<Vec<Mover>, crate::error::TitanError> {
    let url = "https://data.alpaca.markets/v1beta1/screener/stocks/most-actives?by=volume&top=20";

    let resp = client
        .get(url)
        .header("APCA-API-KEY-ID", api_key)
        .header("APCA-API-SECRET-KEY", api_secret)
        .send()
        .await
        .map_err(|e| crate::error::TitanError::Network(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        let req_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("?")
            .to_string();
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(status = %status, request_id = %req_id, body = %body, "Alpaca API error (screener)");
        return Err(crate::error::TitanError::Broker(format!(
            "Screener API {status}: request_id={req_id}"
        )));
    }

    let data: AlpacaMoversResponse = resp
        .json()
        .await
        .map_err(|e| crate::error::TitanError::Internal(e.to_string()))?;

    let now_ts = Utc::now().timestamp();
    let session = session_date_et_str(now_et());
    let movers = data
        .movers
        .unwrap_or_default()
        .into_iter()
        .map(|m| Mover {
            symbol: m.symbol,
            price: m.price.unwrap_or(0.0),
            change_pct: m.change.unwrap_or(0.0),
            volume: m.volume.unwrap_or(0.0),
            last_update_ts: now_ts,
            session_date_et: session.clone(),
        })
        .collect();

    Ok(movers)
}

async fn fetch_movers_snapshots(
    client: &reqwest::Client,
    api_key: &str,
    api_secret: &str,
    symbols: &[&str],
) -> Result<Vec<Mover>, crate::error::TitanError> {
    let syms = symbols.join(",");
    let url = format!(
        "https://data.alpaca.markets/v2/stocks/snapshots?symbols={syms}&feed=sip"
    );

    let resp = client
        .get(&url)
        .header("APCA-API-KEY-ID", api_key)
        .header("APCA-API-SECRET-KEY", api_secret)
        .send()
        .await
        .map_err(|e| crate::error::TitanError::Network(e.to_string()))?;

    let snap_status = resp.status();
    if !snap_status.is_success() {
        let req_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("?")
            .to_string();
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(status = %snap_status, request_id = %req_id, body = %body, "Alpaca API error (snapshots)");
        return Err(crate::error::TitanError::Broker(format!(
            "Snapshots API {snap_status}: request_id={req_id}"
        )));
    }

    let data: HashMap<String, AlpacaSnapshot> = resp
        .json()
        .await
        .map_err(|e| crate::error::TitanError::Internal(e.to_string()))?;

    let now_ts = Utc::now().timestamp();
    let et = now_et();
    let session = session_date_et_str(et);
    let mut movers: Vec<Mover> = data
        .into_iter()
        .filter_map(|(sym, snap)| {
            let trade = snap.latest_trade?;
            let price = trade.p.filter(|p| *p > 0.0)?;
            // SC11: require a same-ET-date print when timestamp is present.
            if let Some(ref t) = trade.t {
                if !is_current_session_trade(et, t) {
                    return None;
                }
            }
            let prev_close = snap
                .prev_daily_bar
                .as_ref()
                .and_then(|b| b.c)
                .filter(|c| *c > 0.0);
            let vol = snap.daily_bar.as_ref().and_then(|b| b.v).unwrap_or(0.0);
            let change_pct = match prev_close {
                Some(pc) => ((price - pc) / pc) * 100.0,
                None => 0.0,
            };
            Some(Mover {
                symbol: sym,
                price,
                change_pct,
                volume: vol,
                last_update_ts: now_ts,
                session_date_et: session.clone(),
            })
        })
        .collect();

    // Sort by absolute move first (actual movers), then volume.
    movers.sort_by(|a, b| {
        b.change_pct
            .abs()
            .partial_cmp(&a.change_pct.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.volume.partial_cmp(&a.volume).unwrap_or(std::cmp::Ordering::Equal))
    });
    movers.truncate(20);
    Ok(movers)
}

use std::collections::HashMap;

const FALLBACK_SYMBOLS: &[&str] = &[
    "AAPL", "MSFT", "NVDA", "TSLA", "AMZN", "META", "GOOG", "AMD", "SPY", "QQQ",
    "NFLX", "COIN", "PLTR", "SOFI", "BABA", "DIS", "NIO", "BA", "JPM", "V",
];

pub async fn run_discovery_loop(
    state: Arc<TitanState>,
    _market_data: Arc<MarketDataState>,
    app: AppHandle,
) {
    tracing::info!("Discovery loop started");

    let poll_secs = {
        let cfg = state.config.lock().unwrap();
        cfg.market.discovery_poll_secs.max(30)
    };

    loop {
        if state.shutdown.is_cancelled() {
            tracing::info!("Discovery loop shutting down");
            break;
        }

        if is_movers_active_hours() {
            if let Err(e) = discovery_tick(&state, &app).await {
                tracing::warn!("Discovery tick failed: {e}");
            }
        } else {
            // SC11: clear retained overnight movers so UI never shows yesterday.
            let today = session_date_et_str(now_et());
            let cleared = {
                let mut market = state.market.lock().unwrap();
                let dirty = market
                    .movers
                    .iter()
                    .any(|m| m.session_date_et != today || m.session_date_et.is_empty());
                if dirty && !market.movers.is_empty() {
                    market.movers.clear();
                    true
                } else {
                    false
                }
            };
            if cleared {
                let _ = tauri::Manager::emit_all(
                    &app,
                    "discovery-movers",
                    &Vec::<Mover>::new(),
                );
            }
        }

        {
            let mut sys = state.system.lock().unwrap();
            if let Some(h) = sys.loops.get_mut("discovery") {
                h.tick();
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(poll_secs)) => {}
            _ = state.shutdown.cancelled() => { break; }
        }
    }
}

async fn discovery_tick(
    state: &TitanState,
    app: &AppHandle,
) -> Result<(), crate::error::TitanError> {
    let (api_key, api_secret) = {
        let cfg = state.config.lock().unwrap();
        (cfg.market.alpaca_key_id.clone(), cfg.market.alpaca_secret.clone())
    };

    if api_key.is_empty() || api_secret.is_empty() {
        return Ok(());
    }

    let movers = match fetch_movers_screener(&state.http_client, &api_key, &api_secret).await {
        Ok(m) if !m.is_empty() => m,
        Ok(_) | Err(_) => {
            tracing::debug!("Screener unavailable, falling back to snapshots");
            fetch_movers_snapshots(&state.http_client, &api_key, &api_secret, FALLBACK_SYMBOLS)
                .await?
        }
    };

    {
        let mut market = state.market.lock().unwrap();
        market.movers = movers.clone();
    }

    let _ = tauri::Manager::emit_all(app, "discovery-movers", &movers);

    for m in &movers {
        if m.change_pct.abs() > 5.0 && m.volume > 1_000_000.0 {
            let spike = VolumeSpikeEvent {
                symbol: m.symbol.clone(),
                volume: m.volume,
                price: m.price,
                change_pct: m.change_pct,
            };
            let _ = tauri::Manager::emit_all(app, "volume-spike", &spike);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mover_serde_roundtrip() {
        let m = Mover {
            symbol: "AAPL".to_string(),
            price: 185.50,
            change_pct: 2.3,
            volume: 50_000_000.0,
            last_update_ts: 1712000000,
            session_date_et: "2026-04-29".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let loaded: Mover = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.symbol, "AAPL");
        assert!((loaded.change_pct - 2.3).abs() < f64::EPSILON);
    }

    #[test]
    fn volume_spike_event_serialize() {
        let spike = VolumeSpikeEvent {
            symbol: "TSLA".to_string(),
            volume: 80_000_000.0,
            price: 250.0,
            change_pct: -6.5,
        };
        let json = serde_json::to_string(&spike).unwrap();
        assert!(json.contains("TSLA"));
    }
}
