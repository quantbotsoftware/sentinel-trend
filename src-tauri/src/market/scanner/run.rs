use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use super::hot_news::{attach_top_news, prefetch_hot_mover_news};
use super::prefilter::quant_prefilter;
use super::scoring::is_gap_scanner_profile;
use super::sessions::{in_actionable_scan_window, now_et};
use super::snapshots::{fetch_active_assets, fetch_snapshots_batch};
use super::types::{AlpacaAsset, ProgressReporter, ScanProgressEvent, ScanResult};
use crate::error::TitanError;
use crate::market::market_state::MarketDataState;
use crate::state::TitanState;

pub(super) fn report_progress(
    reporter: Option<&ProgressReporter>,
    phase: &str,
    label: impl Into<String>,
    percent: u8,
    current: Option<u32>,
    total: Option<u32>,
) {
    if let Some(reporter) = reporter {
        reporter(ScanProgressEvent {
            phase: phase.to_string(),
            label: label.into(),
            percent,
            current,
            total,
        });
    }
}

pub async fn run_scan_with_progress(
    state: &Arc<TitanState>,
    market_data: &Arc<MarketDataState>,
    reporter: Option<ProgressReporter>,
) -> Result<Vec<ScanResult>, TitanError> {
    let start = Instant::now();
    report_progress(
        reporter.as_ref(),
        "start",
        "Validating scanner config",
        10,
        None,
        None,
    );

    // S11: read `allow_short` in the same critical section so the
    // compliance gate downstream sees a single coherent config snapshot.
    // S13: also read catalyst_weights here — same Tier-A guard.
    let (api_key, secret_key, params, allow_short, catalyst_weights, excluded_symbols, news_enabled, news_lookback) = {
        let cfg = state.config.lock().map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.scan_params.clone(),
            cfg.execution.allow_short,
            cfg.catalyst.weights,
            cfg.market.excluded_symbols.clone(),
            cfg.catalyst.news_enabled,
            cfg.catalyst.news_lookback_minutes,
        )
    };

    if api_key.is_empty() || secret_key.is_empty() {
        return Err(TitanError::Config("Alpaca API keys not configured".into()));
    }

    // SC11 TODAY-ONLY: refuse to populate actionable results outside
    // the ET scan window (or on NYSE holidays / weekends).
    let now_et_gate = now_et();
    if !in_actionable_scan_window(now_et_gate) {
        report_progress(
            reporter.as_ref(),
            "outside_session",
            "Outside actionable session (04:05–15:30 ET trading day) — no today movers",
            100,
            None,
            None,
        );
        tracing::info!(
            component = "scanner",
            et = %now_et_gate.format("%Y-%m-%d %H:%M"),
            "SC11: scan skipped — outside actionable session"
        );
        return Ok(Vec::new());
    }

    let profile = if is_gap_scanner_profile(&params) {
        "trend_pullback_s6"
    } else {
        "legacy"
    };

    report_progress(
        reporter.as_ref(),
        "assets",
        "Loading active asset universe",
        20,
        None,
        None,
    );
    let assets = fetch_active_assets(&state.http_client, &api_key, &secret_key).await?;
    report_progress(
        reporter.as_ref(),
        "assets",
        format!("Loaded {} active assets", assets.len()),
        30,
        Some(assets.len() as u32),
        None,
    );

    let us_exchanges: HashSet<&str> = ["NYSE", "NASDAQ", "AMEX", "ARCA", "BATS", "NYSEARCA"]
        .iter()
        .copied()
        .collect();
    let tradable_symbols: Vec<String> = assets
        .iter()
        .filter(|a| a.tradable && us_exchanges.contains(a.exchange.as_str()))
        // v2.4.4 — drop permanently excluded tickers from the scan input
        // universe so they can never re-enter auto_pool / scan_results.
        .filter(|a| !crate::config::is_symbol_excluded(&excluded_symbols, &a.symbol))
        .map(|a| a.symbol.clone())
        .collect();

    report_progress(
        reporter.as_ref(),
        "assets",
        format!("Tradable US symbols: {}", tradable_symbols.len()),
        40,
        Some(tradable_symbols.len() as u32),
        None,
    );

    tracing::info!(component = "scanner", tradable = tradable_symbols.len(), "Fetching snapshots");

    report_progress(
        reporter.as_ref(),
        "snapshots",
        format!("Loading snapshots for {} symbols", tradable_symbols.len()),
        45,
        Some(0),
        Some(tradable_symbols.len() as u32),
    );
    let snapshots = fetch_snapshots_batch(
        &state.http_client,
        &api_key,
        &secret_key,
        &tradable_symbols,
        reporter.as_ref(),
    )
    .await;

    let earnings = {
        state
            .market
            .lock()
            .map(|m| m.earnings_today.clone())
            .unwrap_or_default()
    };

    // S11: build asset map (uppercase keys, S03 invariant) once per scan
    // so the prefilter can run the compliance gate (Issue #14).
    let asset_map: HashMap<String, AlpacaAsset> = assets
        .iter()
        .map(|a| (a.symbol.to_uppercase(), a.clone()))
        .collect();
    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);

    report_progress(
        reporter.as_ref(),
        "filtering",
        if is_gap_scanner_profile(&params) {
            format!("Applying S6 trend/pullback filters to {} snapshots", snapshots.len())
        } else {
            format!("Applying filters to {} snapshots", snapshots.len())
        },
        85,
        Some(snapshots.len() as u32),
        None,
    );
    // S13: snapshot halts (Tier B5 acquired-then-released — no overlap
    // with Tier A which was dropped at the top). Prefetch Alpaca news
    // for the hottest movers so NEWS pills + headline links work on
    // premarket catalyst explosions (watchlist-only cache is not enough).
    let halts = market_data.halts_snapshot();
    let candidate_symbols: Vec<String> = snapshots.keys().cloned().collect();
    report_progress(
        reporter.as_ref(),
        "news",
        format!("Fetching catalyst news for hot movers ({})", candidate_symbols.len()),
        88,
        None,
        None,
    );
    let news_map = prefetch_hot_mover_news(
        &state.http_client,
        &api_key,
        &secret_key,
        &snapshots,
        &candidate_symbols,
        now_et,
        news_lookback,
        news_enabled,
    )
    .await;

    let mut results = quant_prefilter(
        &snapshots,
        &params,
        &earnings,
        &asset_map,
        allow_short,
        now_et,
        &halts,
        &news_map,
        &catalyst_weights,
    );
    attach_top_news(&mut results, &news_map);
    report_progress(
        reporter.as_ref(),
        "filtering",
        format!("Ranking {} candidates", results.len()),
        95,
        Some(results.len() as u32),
        None,
    );
    let elapsed = start.elapsed().as_millis() as u64;

    tracing::info!(
        component = "scanner",
        results = results.len(),
        elapsed_ms = elapsed,
        preset = %params.score_preset,
        profile,
        "Scan complete"
    );

    Ok(results)
}
