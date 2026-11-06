use std::sync::Arc;

use tauri::Manager;

use crate::market::market_state::MarketDataState;
use crate::market::scanner::{
    self, in_actionable_scan_window, now_et, session_date_et_str, timestamp_is_today_et, ScanParams,
    ScanResult,
};
use crate::ops;
use crate::state::TitanState;

// ─── Commands ───────────────────────────────────────────────────────

fn make_progress_reporter(app: &tauri::AppHandle) -> scanner::ProgressReporter {
    let app = app.clone();
    Arc::new(move |progress| {
        let _ = app.emit_all("scan-progress", &progress);
    })
}

fn progress_event(phase: &str, label: impl Into<String>, percent: u8) -> scanner::ScanProgressEvent {
    scanner::ScanProgressEvent {
        phase: phase.to_string(),
        label: label.into(),
        percent,
        current: None,
        total: None,
    }
}

/// SC11 — drop process-local scan cache when it is not from today's ET session.
fn purge_stale_scan_cache(market: &mut crate::state::MarketState, now_et: chrono::DateTime<chrono_tz::Tz>) -> bool {
    let today = session_date_et_str(now_et);
    let ts_ok = market
        .scan_status
        .last_scan_ts
        .map(|ts| timestamp_is_today_et(ts, now_et))
        .unwrap_or(false);
    let rows_ok = !market.scan_results.is_empty()
        && market
            .scan_results
            .iter()
            .all(|r| r.session_date_et == today);
    if ts_ok && rows_ok {
        return false;
    }
    market.scan_results.clear();
    market.scan_status.results_count = 0;
    true
}

#[tauri::command]
pub async fn run_market_scan(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<TitanState>>,
    market_data: tauri::State<'_, Arc<MarketDataState>>,
    params: ScanParams,
) -> Result<Vec<ScanResult>, String> {
    {
        let mut cfg = state.config.lock().map_err(|e| format!("lock: {e}"))?;
        cfg.market.scan_params = params;
    }

    let state_arc = state.inner().clone();
    let market_data_arc = market_data.inner().clone();

    {
        let mut market = state_arc.market.lock().map_err(|e| format!("lock: {e}"))?;
        market.scan_status.running = true;
    }
    let status = {
        state_arc.market.lock().map_err(|e| format!("lock: {e}"))?.scan_status.clone()
    };
    let _ = app.emit_all("scan-status", &status);

    let start = std::time::Instant::now();
    let reporter = make_progress_reporter(&app);
    let results = match scanner::run_scan_with_progress(
        &state_arc,
        &market_data_arc,
        Some(reporter),
    )
    .await
    {
        Ok(results) => results,
        Err(e) => {
            {
                let mut market = state_arc.market.lock().map_err(|lock| format!("lock: {lock}"))?;
                market.scan_status.running = false;
            }
            let status = {
                state_arc.market.lock().map_err(|lock| format!("lock: {lock}"))?.scan_status.clone()
            };
            let _ = app.emit_all("scan-status", &status);
            let _ = app.emit_all(
                "scan-progress",
                &progress_event("error", format!("Scan failed: {e}"), 100),
            );
            return Err(e.to_string());
        }
    };
    let elapsed = start.elapsed().as_millis() as u64;
    let et = now_et();

    {
        let mut market = state_arc.market.lock().map_err(|e| format!("lock: {e}"))?;
        // Outside session returns empty — clear any yesterday leftovers.
        market.scan_results = results.clone();
        market.scan_status.running = false;
        market.scan_status.last_scan_ts = if results.is_empty() && !in_actionable_scan_window(et) {
            None
        } else {
            Some(chrono::Utc::now().timestamp())
        };
        market.scan_status.last_scan_duration_ms = elapsed;
        market.scan_status.results_count = results.len() as u32;
    }

    let _ = app.emit_all("scan-results", &results);
    let status = {
        state_arc.market.lock().map_err(|e| format!("lock: {e}"))?.scan_status.clone()
    };
    let _ = app.emit_all("scan-status", &status);
    let label = if results.is_empty() && !in_actionable_scan_window(et) {
        "Outside session — no today movers".to_string()
    } else {
        format!("Scan complete: {} results", results.len())
    };
    let _ = app.emit_all(
        "scan-progress",
        &progress_event("complete", label, 100),
    );

    Ok(results)
}

#[tauri::command]
pub fn get_scan_results(
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<Vec<ScanResult>, String> {
    let mut market = state.market.lock().map_err(|e| format!("lock: {e}"))?;
    let et = now_et();
    if purge_stale_scan_cache(&mut market, et) {
        return Ok(Vec::new());
    }
    let today = session_date_et_str(et);
    Ok(market
        .scan_results
        .iter()
        .filter(|r| r.session_date_et == today)
        .cloned()
        .collect())
}

// ─── Manual "Apply to Watchlist" (S18.1) ───────────────────────────
//
// Companion to `run_market_scan`. The manual scan command is read-only
// against AutoUniverse — it just shows the operator what's hot. This
// command lets the operator promote/demote the watchlist on demand,
// using the *last* scan results stored in `market.scan_results`.
//
// Same pipeline as the auto-scan loop: identical `process_scan_results`
// call, same protected-symbols set, same `auto-universe-update` event.
// Idempotent: calling twice in a row with no fresh scan is a no-op.
#[tauri::command]
pub async fn apply_scan_to_watchlist(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<TitanState>>,
    market_data: tauri::State<'_, Arc<MarketDataState>>,
) -> Result<crate::market::auto_universe::PromotionReport, String> {
    let et = now_et();
    let today = session_date_et_str(et);
    let results: Vec<ScanResult> = {
        let mut market = state.market.lock().map_err(|e| format!("lock: {e}"))?;
        if purge_stale_scan_cache(&mut market, et) {
            return Err(
                "Scan results are from a prior session. Run a fresh scan today first.".into(),
            );
        }
        market
            .scan_results
            .iter()
            .filter(|r| r.session_date_et == today)
            .cloned()
            .collect()
    };
    if results.is_empty() {
        return Err(
            "No today scan results to apply. Run a scan first, then click \"Apply to Watchlist\"."
                .into(),
        );
    }

    let state_arc = state.inner().clone();
    let market_data_arc = market_data.inner().clone();
    let report = crate::market::auto_scan::apply_scan_to_universe(&state_arc, &market_data_arc, &app, &results);

    match report {
        Some(r) => {
            tracing::info!(
                component = "scanner",
                promoted = r.promoted.len(),
                demoted = r.demoted.len(),
                "Manual apply_scan_to_watchlist completed"
            );
            Ok(r)
        }
        None => Err(
            "AutoUniverse is disabled. Enable it in CONFIG → AUTO-UNIVERSE first.".into(),
        ),
    }
}

#[tauri::command]
pub fn set_scan_params(
    state: tauri::State<'_, Arc<TitanState>>,
    params: ScanParams,
) -> Result<String, String> {
    {
        let mut cfg = state.config.lock().map_err(|e| format!("lock: {e}"))?;
        cfg.market.scan_params = params;
    }

    if let Err(e) = ops::persistence::save_config(&state) {
        tracing::error!("Failed to persist scan params: {e}");
        return Err(e.to_string());
    }

    tracing::info!(component = "scanner", "Scan params updated and saved");
    Ok("Scan params saved".into())
}

#[tauri::command]
pub fn toggle_auto_scan(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<String, String> {
    let new_state;
    {
        let mut cfg = state.config.lock().map_err(|e| format!("lock: {e}"))?;
        cfg.market.auto_scan_active = !cfg.market.auto_scan_active;
        new_state = cfg.market.auto_scan_active;
    }

    if let Err(e) = ops::persistence::save_config(&state) {
        tracing::error!("Failed to persist auto_scan toggle: {e}");
        return Err(e.to_string());
    }

    {
        let mut market = state.market.lock().map_err(|e| format!("lock: {e}"))?;
        market.scan_status.auto_active = new_state;
    }

    let status = {
        state.market.lock().map_err(|e| format!("lock: {e}"))?.scan_status.clone()
    };
    let _ = app.emit_all("scan-status", &status);

    let label = if new_state { "ON" } else { "OFF" };
    tracing::info!(component = "scanner", auto_scan = label, "Auto-scan toggled");
    Ok(format!("Auto-scan: {label}"))
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::auto_scan::ScanStatus;
    use chrono::TimeZone;

    #[test]
    fn scan_status_serializes() {
        let s = ScanStatus {
            running: true,
            last_scan_ts: Some(1700000000),
            last_scan_duration_ms: 1234,
            results_count: 42,
            auto_active: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("1234"));
        assert!(json.contains("42"));
    }

    #[test]
    fn scan_params_deserialize() {
        let json = r#"{"price_min":1,"price_max":20,"volume_min":1000000,"change_min":10,"rvol_min":5,"gap_min":0,"float_max":20000000,"cap_max":2000000000,"exclude_earnings":false,"max_candidates":100,"scan_interval_minutes":30,"score_preset":"balanced"}"#;
        let p: ScanParams = serde_json::from_str(json).unwrap();
        assert!((p.price_min - 1.0).abs() < 0.01);
        assert!((p.price_max - 20.0).abs() < 0.01);
        assert_eq!(p.score_preset, "balanced");
    }

    #[test]
    fn purge_stale_scan_cache_clears_prior_session() {
        let mut market = crate::state::MarketState::default();
        market.scan_results = vec![ScanResult {
            symbol: "OLD".into(),
            session_date_et: "2026-04-28".into(),
            ..Default::default()
        }];
        market.scan_status.last_scan_ts = Some(1_714_348_800); // prior day-ish
        let now = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 10, 0, 0)
            .unwrap();
        assert!(purge_stale_scan_cache(&mut market, now));
        assert!(market.scan_results.is_empty());
        assert_eq!(market.scan_status.results_count, 0);
    }
}
