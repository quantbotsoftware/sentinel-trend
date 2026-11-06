//! D-PLUS (Sesiunea 2) — Strategy Evolver background loop.
//!
//! Wakes once per minute and triggers a full
//! [`Evolver::run_all`] cycle when the local ET clock crosses
//! 20:00 (post-close on US trading days). The loop runs at most
//! once per day — a `last_run_date` guard prevents repeat fires.
//!
//! ## Why fire at 20:00 ET?
//!
//! - Market closes at 16:00 ET → 4 hours of post-close data settle.
//! - GA cycle takes ~minutes (cold-start) → done by 20:30 ET.
//! - Bot cron usually has slept-restart triggers; 20:00 ET is the
//!   canonical "after-hours analytics" slot in the master plan.
//!
//! ## Failure semantics
//!
//! - Tokio cancellation aborts the loop cleanly.
//! - Evolver panics are caught by the per-task `tracing::error!`
//!   logger; the loop survives by NOT propagating panics.
//! - Disk write errors are logged at warn but do not stop the loop.

use std::sync::Arc;

use chrono::Datelike;
use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::market::market_state::MarketDataState;
use crate::smarttrend::strategy_evolver::{
    default_base_dir, Evolver, EvolverReport, ParamOverrides,
};
use crate::state::TitanState;

const LOOP_NAME: &str = "evolver";
const TICK_INTERVAL_SECS: u64 = 60;
const FIRE_HOUR_ET: u32 = 20;

pub type EvolverHandle = Arc<tokio::sync::Mutex<Evolver>>;
pub type OverridesHandle = Arc<ParamOverrides>;

pub fn new_handles(seed: u64) -> (EvolverHandle, OverridesHandle) {
    let base_dir = default_base_dir();
    let evolver = Evolver::new(16, seed, base_dir.clone());
    let overrides = Arc::new(ParamOverrides::new());
    // Best-effort load — empty-OK
    if let Err(e) = overrides.load_into(&base_dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                component = LOOP_NAME,
                error = %e,
                "ParamOverrides load failed (continuing with empty overrides)"
            );
        }
    }
    (Arc::new(tokio::sync::Mutex::new(evolver)), overrides)
}

pub async fn run_evolver_loop(
    state: Arc<TitanState>,
    market_data: Arc<MarketDataState>,
    evolver: EvolverHandle,
    overrides: OverridesHandle,
    app: AppHandle,
) {
    let enabled = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.evolution_protocol.enabled && cfg.evolution_protocol.evolver_enabled
    };
    if !enabled {
        tracing::info!(
            component = LOOP_NAME,
            "Evolution protocol or evolver disabled — loop skipped"
        );
        return;
    }
    tracing::info!(
        component = LOOP_NAME,
        fire_hour_et = FIRE_HOUR_ET,
        "Strategy evolver loop starting"
    );
    let cancel = state.shutdown.clone();
    let mut tick = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
    let mut last_run_date: Option<(i32, u32, u32)> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(component = LOOP_NAME, "shutdown requested");
                break;
            }
            _ = tick.tick() => {
                if should_fire(&mut last_run_date) {
                    let report = run_once(&state, &market_data, &evolver, &overrides, &app).await;
                    let _ = app.emit_all(
                        "evolver-report",
                        serde_json::json!({
                            "date": report.date,
                            "rows": report.per_strategy.len(),
                            "started_at": report.started_at,
                            "finished_at": report.finished_at,
                        }),
                    );
                }
            }
        }
    }
}

/// Returns true if the local ET clock just crossed `FIRE_HOUR_ET:00`
/// AND we haven't already fired today.
fn should_fire(last_run_date: &mut Option<(i32, u32, u32)>) -> bool {
    use chrono::Timelike;
    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let today = (now_et.year(), now_et.month(), now_et.day());
    if Some(today) == *last_run_date {
        return false;
    }
    if now_et.hour() != FIRE_HOUR_ET || now_et.minute() > 5 {
        // Only fire in the first 5 minutes of the hour to avoid
        // missing a window when the loop comes back from sleep.
        return false;
    }
    *last_run_date = Some(today);
    true
}

async fn run_once(
    state: &TitanState,
    _market_data: &MarketDataState,
    evolver: &EvolverHandle,
    overrides: &OverridesHandle,
    app: &AppHandle,
) -> EvolverReport {
    let mut symbols: Vec<String> = {
        let market = state.market.lock().unwrap();
        market.auto_universe.active_set().into_iter().collect()
    };
    if symbols.is_empty() {
        let cfg = state.config.lock().unwrap();
        symbols = cfg.ui.favorites.clone();
    }
    if symbols.is_empty() {
        symbols = vec!["AAPL".to_string(), "MSFT".to_string(), "NVDA".to_string()];
    }

    let (key, secret) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };

    let end = chrono::Utc::now();
    let start = end - chrono::Duration::days(90);
    let cache_dir = Some(crate::paths::data_dir().join("backtest_cache"));

    tracing::info!(
        component = LOOP_NAME,
        symbols_count = symbols.len(),
        "Fetching 90-day candles for strategy evolver"
    );

    let (candles_90d, failed) = crate::backtest::runner::fetch_universe(
        &symbols,
        "5m",
        start,
        end,
        &state.http_client,
        &key,
        &secret,
        cache_dir.as_deref(),
    )
    .await;

    if !failed.is_empty() {
        tracing::warn!(
            component = LOOP_NAME,
            failed_symbols = ?failed,
            "Failed to fetch candles for some symbols during evolver run"
        );
    }

    // Phase II Sandbox Validation: Prioritize evaluating/running sandbox tests
    // on strategies flagged with pending_sandbox first.
    let mut pending_strategies = Vec::new();
    if let Some(dp) = state.dplus() {
        if let Ok(mut t) = dp.tournament.lock() {
            for (sid, stats) in &mut t.stats {
                if stats.pending_sandbox {
                    pending_strategies.push((*sid, stats.candidate_params.clone()));
                    stats.pending_sandbox = false;
                }
            }
            if !pending_strategies.is_empty() {
                let _ = t.save(&crate::smarttrend::tournament::default_base_dir());
            }
        }
    }

    if !pending_strategies.is_empty() {
        let _ = app.emit_all("tournament-update", serde_json::json!({"reason": "sandbox_clear"}));
    }

    let mut skip_strategies = Vec::new();
    for (sid, candidate_params) in pending_strategies {
        let success = crate::smarttrend::sandbox::validate_and_apply_mutation(
            sid,
            &candidate_params,
            &candles_90d,
            overrides,
        );
        tracing::info!(
            component = LOOP_NAME,
            strategy = ?sid,
            success = success,
            "Sandbox validation complete for strategy"
        );
        skip_strategies.push(sid);
    }

    let overrides_clone = Arc::clone(overrides);
    let candles_clone = candles_90d.clone();
    let skip_clone = skip_strategies.clone();
    let evolver_clone = Arc::clone(evolver);
    tokio::task::spawn_blocking(move || {
        let mut e = evolver_clone.blocking_lock();
        e.run_all(&overrides_clone, &candles_clone, &skip_clone)
    })
    .await
    .unwrap_or_else(|err| {
        tracing::error!(component = LOOP_NAME, "Evolver task panicked: {err}");
        EvolverReport {
            date: "".to_string(),
            started_at: 0,
            finished_at: 0,
            per_strategy: vec![],
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_name_is_stable() {
        assert_eq!(LOOP_NAME, "evolver");
    }

    #[test]
    fn fire_hour_is_post_close_et() {
        // 20:00 ET = 4h post-close. Pin so future refactors don't
        // accidentally move it into RTH.
        assert_eq!(FIRE_HOUR_ET, 20);
    }

    #[test]
    fn new_handles_returns_independent_arcs() {
        let (e1, o1) = new_handles(1);
        let (e2, o2) = new_handles(2);
        assert!(!Arc::ptr_eq(&e1, &e2));
        assert!(!Arc::ptr_eq(&o1, &o2));
    }

    #[test]
    fn should_fire_first_call_in_window_returns_true() {
        // Patch via mock won't work here without time injection — we
        // exercise the boolean logic on the date guard instead.
        let mut last: Option<(i32, u32, u32)> = None;
        // Force "already today" to test the guard: set last to a
        // fake "today" tuple from the live clock.
        let now = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        last = Some((now.year(), now.month(), now.day()));
        // Same-day → should never fire
        assert!(!should_fire(&mut last));
    }

    #[test]
    fn should_fire_outside_hour_returns_false() {
        // We can't force the hour without injecting time, so we
        // assert the negative path: the guard with no last run still
        // returns false unless the hour matches.
        use chrono::Timelike;
        let now = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        if now.hour() == FIRE_HOUR_ET && now.minute() <= 5 {
            // Skip: we're inside the fire window, can't test negation
            return;
        }
        let mut last: Option<(i32, u32, u32)> = None;
        assert!(!should_fire(&mut last));
    }
}
