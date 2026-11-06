//! D-PLUS (Sesiunea 2) — Causal Lead-Lag background loop.
//!
//! Polls every active symbol's last price from `MarketDataState` once
//! per second, feeding the [`CausalEngine`] ring buffers. Every 60s
//! the loop calls `recompute_graph` and emits a `causal-graph-update`
//! event so the frontend D-PLUS panel can refresh.
//!
//! ## Symbol selection
//!
//! - All pinned watchlist symbols, plus
//! - The current `auto_pool` symbols (limit 50 most active by RVOL),
//!   plus
//! - Always-on probes: SPY, QQQ, IWM (regime / lead-lag anchors)
//!
//! ## Tunables
//!
//! - Tick cadence: 1 sec (matches the buffer's 1-sec spacing).
//! - Recompute cadence: 60 sec (cheap O(N²·L) where N≤50, L≤300).
//! - Buffer cap: from `EvolutionProtocolConfig` if present, else
//!   the engine default.

use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::market::market_state::MarketDataState;
use crate::smarttrend::causal::CausalEngine;
use crate::state::TitanState;

const LOOP_NAME: &str = "causal";
const TICK_INTERVAL_SECS: u64 = 1;
const RECOMPUTE_INTERVAL_SECS: u64 = 60;
const MAX_TRACKED_SYMBOLS: usize = 50;
const ANCHORS: &[&str] = &["SPY", "QQQ", "IWM"];

pub type CausalHandle = Arc<Mutex<CausalEngine>>;

/// Build a fresh handle suitable for `tauri::Builder::manage`.
pub fn new_handle() -> CausalHandle {
    Arc::new(Mutex::new(CausalEngine::new(
        crate::smarttrend::causal::DEFAULT_BUFFER_CAP,
    )))
}

pub async fn run_causal_loop(
    state: Arc<TitanState>,
    market: Arc<MarketDataState>,
    engine: CausalHandle,
    app: AppHandle,
) {
    let enabled = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.evolution_protocol.enabled
    };
    if !enabled {
        tracing::info!(
            component = LOOP_NAME,
            "Evolution protocol disabled — causal loop skipped"
        );
        return;
    }
    tracing::info!(
        component = LOOP_NAME,
        tick_interval_secs = TICK_INTERVAL_SECS,
        recompute_secs = RECOMPUTE_INTERVAL_SECS,
        "Causal lead-lag loop starting"
    );
    let cancel = state.shutdown.clone();
    let mut tick = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
    let mut recompute = tokio::time::interval(Duration::from_secs(RECOMPUTE_INTERVAL_SECS));
    // First tick fires immediately; skip it
    tick.tick().await;
    recompute.tick().await;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(component = LOOP_NAME, "shutdown requested");
                break;
            }
            _ = tick.tick() => {
                ingest_once(&state, &market, &engine);
            }
            _ = recompute.tick() => {
                let now = chrono::Utc::now().timestamp();
                {
                    let eng = engine.lock().unwrap_or_else(|p| p.into_inner());
                    eng.recompute_graph(now);
                }
                let snap = {
                    let eng = engine.lock().unwrap_or_else(|p| p.into_inner());
                    eng.snapshot()
                };
                let _ = app.emit_all(
                    "causal-graph-update",
                    serde_json::json!({
                        "edges": snap.edges,
                        "recomputed_at": snap.recomputed_at,
                        "samples_seen": snap.samples_seen,
                    }),
                );
            }
        }
    }
}

fn ingest_once(state: &Arc<TitanState>, market: &MarketDataState, engine: &CausalHandle) {
    let symbols = collect_symbols(state);
    let prices = {
        let p = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
        let mut out: Vec<(String, f64)> = Vec::with_capacity(symbols.len());
        for s in &symbols {
            if let Some(price) = p.get(s).copied() {
                out.push((s.clone(), price));
            }
        }
        out
    };
    if prices.is_empty() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let mut eng = engine.lock().unwrap_or_else(|p| p.into_inner());
    for (sym, price) in prices {
        eng.ingest_tick(&sym, price, now);
    }
}

fn collect_symbols(state: &Arc<TitanState>) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<String> = BTreeSet::new();
    for a in ANCHORS {
        set.insert((*a).to_string());
    }
    if let Ok(cfg) = state.config.lock() {
        for s in cfg.ui.favorites.iter() {
            set.insert(s.to_uppercase());
            if set.len() >= MAX_TRACKED_SYMBOLS {
                break;
            }
        }
    }
    // Auto-universe pool lives under `state.market` (S18).
    {
        let mk = state.market.lock().unwrap_or_else(|p| p.into_inner());
        for s in mk.auto_universe.pinned.iter() {
            set.insert(s.to_uppercase());
            if set.len() >= MAX_TRACKED_SYMBOLS {
                break;
            }
        }
        for sym in mk.auto_universe.auto_pool.keys() {
            set.insert(sym.to_uppercase());
            if set.len() >= MAX_TRACKED_SYMBOLS {
                break;
            }
        }
    }
    set.into_iter().take(MAX_TRACKED_SYMBOLS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_name_is_stable() {
        assert_eq!(LOOP_NAME, "causal");
    }

    #[test]
    fn anchors_include_index_etfs() {
        assert!(ANCHORS.contains(&"SPY"));
        assert!(ANCHORS.contains(&"QQQ"));
        assert!(ANCHORS.contains(&"IWM"));
    }

    #[test]
    fn new_handle_returns_unique_arc() {
        let a = new_handle();
        let b = new_handle();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn cadence_constants_are_reasonable() {
        // Recompute should never be smaller than the tick (avoids
        // double work), and tick should be ≥1 sec to keep the buffer
        // semantics correct (1 sample per second).
        assert!(TICK_INTERVAL_SECS >= 1);
        assert!(RECOMPUTE_INTERVAL_SECS >= TICK_INTERVAL_SECS);
        assert!(RECOMPUTE_INTERVAL_SECS <= 300);
    }
}
