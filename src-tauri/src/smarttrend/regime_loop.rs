//! Background loop that polls SPY 5-min bars from the market data
//! state and feeds them into the regime detector.
//!
//! Emits `regime-change` events to the front-end whenever the
//! detector flips classification. The front-end's `tournament.js`
//! listens on this event and re-renders the leaderboard so the user
//! can correlate strategy P/L with the active regime.
//!
//! ## Why a separate loop instead of piggy-backing on `bot::run_bot_loop`?
//!
//! The bot loop is event-driven (consumes `BotAction` from a
//! channel). Regime classification needs *time-driven* polling
//! independent of trading activity, otherwise we'd miss regime
//! flips during quiet windows. A 30-second tokio interval is
//! cheap and avoids touching the bot loop's hot path.
//!
//! ## Failure modes
//!
//! - Market data missing for SPY → loop logs at debug and waits for
//!   next tick. The detector never publishes, regime stays None.
//! - SPY symbol not in `last_prices` (cold start) → same as above.
//! - Price feed stale (last update >5 min ago) → we still feed the
//!   bar; the detector treats it as a no-change observation. Health
//!   pulse covers stale-feed alerting separately.

use std::sync::Arc;

use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::market::market_state::MarketDataState;
use crate::smarttrend::regime_detector::{RegimeConfig, RegimeMonitor, RegimeSnapshot};
use crate::state::TitanState;

const LOOP_NAME: &str = "regime";

/// Probe symbol used for regime classification. SPY is the canonical
/// index proxy; the detector logic is index-agnostic so this can be
/// swapped for QQQ/IWM in a future config.
const REGIME_SYMBOL: &str = "SPY";

pub async fn run_regime_loop(
    state: Arc<TitanState>,
    market: Arc<MarketDataState>,
    app: AppHandle,
) {
    // Cheap config snapshot — no need to hold the lock across awaits.
    let (enabled, poll_secs) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (cfg.evolution_protocol.enabled, cfg.evolution_protocol.regime_poll_secs)
    };
    if !enabled {
        tracing::info!(
            component = LOOP_NAME,
            "Evolution protocol disabled — regime loop skipped"
        );
        return;
    }
    let monitor = Arc::new(RegimeMonitor::new(RegimeConfig::default()));
    tracing::info!(
        component = LOOP_NAME,
        symbol = REGIME_SYMBOL,
        poll_secs = poll_secs,
        "Regime detector loop starting"
    );
    let cancel = state.shutdown.clone();
    let mut tick = tokio::time::interval(Duration::from_secs(poll_secs.max(60)));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(component = LOOP_NAME, "shutdown requested");
                break;
            }
            _ = tick.tick() => {
                tick_once(&monitor, &state, &market, &app);
            }
        }
    }
}

fn tick_once(monitor: &RegimeMonitor, state: &Arc<TitanState>, market: &Arc<MarketDataState>, app: &AppHandle) {
    // Snapshot the latest SPY price + a bar timestamp.
    let price = {
        let prices = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
        prices.get(REGIME_SYMBOL).copied().unwrap_or(0.0)
    };
    if price <= 0.0 {
        // Cold start; SPY not yet streamed. Skip silently.
        tracing::debug!(component = LOOP_NAME, "SPY price not available yet");
        return;
    }
    let now = chrono::Utc::now().timestamp();
    if let Some(snap) = monitor.observe(price, now) {
        emit_change(app, &snap);

        let state_clone = Arc::clone(state);
        let market_clone = Arc::clone(market);
        let app_clone = app.clone();
        let new_regime = snap.regime;
        tauri::async_runtime::spawn(async move {
            if let Err(e) = crate::smarttrend::sandbox::run_regime_reallocation(state_clone, market_clone, app_clone, new_regime).await {
                tracing::error!(component = "regime", error = %e, "Regime reallocation failed");
            }
        });
    }
    if let Some(cur) = monitor.current() {
        let mut m = state.market.lock().unwrap_or_else(|p| p.into_inner());
        m.regime = cur.regime;
    }
}

fn emit_change(app: &AppHandle, snap: &RegimeSnapshot) {
    tracing::info!(
        component = LOOP_NAME,
        regime = ?snap.regime,
        realised_vol = snap.realised_vol_annualised_pct,
        slope = snap.trend_slope_pct,
        r2 = snap.trend_r2,
        "regime change"
    );
    let _ = app.emit_all(
        "regime-change",
        serde_json::json!({
            "regime": format!("{:?}", snap.regime).to_lowercase(),
            "realised_vol_annualised_pct": snap.realised_vol_annualised_pct,
            "last_return_pct": snap.last_return_pct,
            "trend_slope_pct": snap.trend_slope_pct,
            "trend_r2": snap.trend_r2,
            "bar_time": snap.bar_time,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_name_is_stable() {
        // The system.register_loop call in main.rs uses this constant —
        // pin it so a typo doesn't silently lose the health metric.
        assert_eq!(LOOP_NAME, "regime");
    }

    #[test]
    fn regime_symbol_is_spy() {
        assert_eq!(REGIME_SYMBOL, "SPY");
    }
}
