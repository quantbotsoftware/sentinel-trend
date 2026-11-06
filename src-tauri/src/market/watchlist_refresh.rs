//! S14 — background task that refreshes per-symbol watchlist metrics
//! every 30s and writes them to `MarketDataState.watchlist_metrics`
//! (Tier B6).
//!
//! ## Why a dedicated loop
//! The discovery scanner runs every 5–30 minutes (depending on
//! `scan_interval_minutes`). Between scans, the watchlist UI was
//! reading from the cached `scan_results` snapshot — so a symbol
//! that wasn't in the most recent scan returned `0.0` on every metric
//! ("watchlist orb between scan-uri", SCANNER_ROADMAP §Probleme țintă #3).
//!
//! ## Lock ordering (S05 / S14)
//! Each tick proceeds strictly in this order:
//!
//!   1. Tier-A `config` — clone the snapshot inputs (favorites, API
//!      keys, scan params, catalyst weights) and DROP.
//!   2. Network I/O — `fetch_snapshots_batch` + `build_news_map`.
//!      No locks held during `await`.
//!   3. Tier-B5 `halts` — `halts_snapshot()` clones and drops B5.
//!   4. Pure compute — per-symbol `extract_bar_metrics` →
//!      `RvolMethod` dispatch → `CatalystFlags::compute` →
//!      `pills_owned`.
//!   5. Tier-B6 `watchlist_metrics` — `replace_watchlist_metrics`
//!      swaps the entire map under the lock and drops.
//!   6. **Emit-after-drop** — `app.emit_all("watchlist-update", ...)`
//!      runs only after every state guard has been dropped.
//!   7. Tier-A `system` — quick `loops["watchlist_refresh"].tick()`
//!      for the watchdog. Acquired alone, never with another lock.
//!
//! Tier-A and Tier-B are NEVER held simultaneously, satisfying the
//! `state.rs` boundary rule.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tauri::Manager;

use crate::market::alpaca_news::{build_news_map, NewsItem};
use crate::market::catalyst::{
    is_halted_combined, CatalystFlags, CatalystInput, CatalystWeights,
};
use crate::market::market_state::{HaltState, MarketDataState, WatchlistMetrics};
use crate::market::scanner::{
    extract_bar_metrics, fetch_snapshots_batch, time_adjusted_rvol, RvolMethod, ScanParams,
    SnapshotData,
};
use crate::state::TitanState;

/// 30s cadence — fast enough for "live" watchlist feel, slow enough
/// that the snapshot batch (≤200 symbols) fits one HTTP call without
/// flirting with Alpaca's 200 req/min REST limit.
const REFRESH_INTERVAL_SECS: u64 = 30;

/// Public entry point spawned from `main.rs`. Honors `state.shutdown`
/// gracefully and ticks the watchdog once per successful loop body.
pub async fn run_watchlist_refresh_loop(
    state: Arc<TitanState>,
    market_data: Arc<MarketDataState>,
    app: tauri::AppHandle,
) {
    tracing::info!(
        component = "watchlist_refresh",
        interval_secs = REFRESH_INTERVAL_SECS,
        "Watchlist refresh loop started"
    );

    loop {
        if state.shutdown.is_cancelled() {
            tracing::info!(component = "watchlist_refresh", "shutting down");
            break;
        }

        // ─── 1. Tier-A: snapshot config + universe ───────────────────
        // S05/S18: Tier-A acquire ascending — config(1) → market(2),
        // both released before the network I/O step.
        let (api_key, secret_key, favorites, scan_params, catalyst_weights, au_enabled) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
                cfg.ui.favorites.clone(),
                cfg.market.scan_params.clone(),
                cfg.catalyst.weights,
                cfg.auto_universe.enabled,
            )
        };
        // S18: when AutoUniverse is enabled, the refresh loop pulls
        // metrics for the *managed* universe (pinned ∪ auto-pool)
        // instead of the static favorites list. This is what feeds
        // the watchlist UI rows for auto-promoted candidates.
        let mut symbols: Vec<String> = if au_enabled {
            let mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
            mkt.auto_universe.active_set().into_iter().collect()
        } else {
            favorites.into_iter().map(|s| s.to_uppercase()).collect()
        };
        symbols.sort();
        symbols.dedup();
        let favorites = symbols; // rebind to keep downstream variable name stable

        if favorites.is_empty() || api_key.is_empty() || secret_key.is_empty() {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(REFRESH_INTERVAL_SECS)) => {}
                _ = state.shutdown.cancelled() => { break; }
            }
            continue;
        }

        // ─── 2. Network I/O (no locks held) ──────────────────────────
        let snapshots = fetch_snapshots_batch(
            &state.http_client,
            &api_key,
            &secret_key,
            &favorites,
            None, // silent refresh — no progress reporter
        )
        .await;
        let news_map = build_news_map(&favorites).await;

        // ─── 3. Tier-B5: halts snapshot (clone + drop) ───────────────
        let halts = market_data.halts_snapshot();

        // ─── 4. Pure compute ─────────────────────────────────────────
        let now_et = Utc::now().with_timezone(&chrono_tz::US::Eastern);
        let metrics = compute_watchlist_metrics(
            &favorites,
            &snapshots,
            &halts,
            &news_map,
            &scan_params,
            &catalyst_weights,
            now_et,
        );

        // ─── 5. Tier-B6: replace map (acquire + swap + drop) ─────────
        let map: HashMap<String, WatchlistMetrics> = metrics
            .iter()
            .map(|m| (m.symbol.clone(), m.clone()))
            .collect();
        market_data.replace_watchlist_metrics(map);

        // ─── 6. S05 invariant — emit AFTER all guards dropped ────────
        if let Err(e) = app.emit_all("watchlist-update", &metrics) {
            tracing::warn!(
                component = "watchlist_refresh",
                "emit watchlist-update failed: {e}"
            );
        }

        // ─── 7. Watchdog tick (Tier-A system, acquired alone) ────────
        {
            let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = sys.loops.get_mut("watchlist_refresh") {
                h.tick();
            }
        }

        tracing::debug!(
            component = "watchlist_refresh",
            requested = favorites.len(),
            produced = metrics.len(),
            "watchlist refresh complete"
        );

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(REFRESH_INTERVAL_SECS)) => {}
            _ = state.shutdown.cancelled() => { break; }
        }
    }
}

/// Pure per-symbol compute. Extracted so unit tests can exercise the
/// algorithm without spawning a runtime, hitting the network, or
/// holding any lock.
///
/// `favorites` ordering is preserved in the output `Vec`, so the
/// frontend can rely on the emit payload matching the user's chosen
/// list order.
pub(crate) fn compute_watchlist_metrics(
    favorites: &[String],
    snapshots: &HashMap<String, SnapshotData>,
    halts: &HashMap<String, HaltState>,
    news_map: &HashMap<String, Vec<NewsItem>>,
    scan_params: &ScanParams,
    catalyst_weights: &CatalystWeights,
    now_et: chrono::DateTime<chrono_tz::Tz>,
) -> Vec<WatchlistMetrics> {
    let mut out = Vec::with_capacity(favorites.len());
    for sym in favorites {
        let key = sym.to_uppercase();
        let Some(snap) = snapshots.get(&key).or_else(|| snapshots.get(sym)) else {
            continue;
        };
        let m = extract_bar_metrics(snap, now_et);
        if !m.current_price.is_finite() || m.current_price <= 0.0 {
            continue;
        }

        let rvol = match scan_params.rvol_method {
            RvolMethod::Naive => {
                if m.prev_volume > 0.0 {
                    m.volume_today / m.prev_volume
                } else {
                    1.0
                }
            }
            RvolMethod::TimeAdjusted | RvolMethod::Historical => {
                time_adjusted_rvol(m.volume_today, m.prev_volume, now_et)
            }
        };
        let gap_pct = if m.prev_close > 0.0 {
            ((m.open_price - m.prev_close) / m.prev_close) * 100.0
        } else {
            0.0
        };

        let is_halted = is_halted_combined(halts, &key, m.last_trade_age_seconds, now_et);
        let has_recent_news = news_map.contains_key(&key);
        // PRIME F0.6 — headline polarity for catalyst-direction plays.
        let news_sentiment = news_map.get(&key).and_then(|items| {
            let headlines: Vec<&str> = items.iter().map(|n| n.headline.as_str()).collect();
            crate::market::catalyst::headline_sentiment(&headlines)
        });

        // `has_earnings` is unknown at this layer (the calendar lives in
        // Tier-A `MarketState.earnings_today`). `commands::discovery::
        // get_watchlist` re-evaluates the catalyst with the correct
        // earnings flag when serving the UI; the live emit uses
        // earnings=false so an out-of-band watchlist refresh never
        // mis-attributes an EARN pill.
        let catalyst_flags = CatalystFlags::compute(
            &CatalystInput {
                symbol: &key,
                change_pct: m.change_pct,
                gap_pct,
                rvol,
                has_earnings: false,
                has_recent_news,
                is_halted,
                has_offering: crate::market::edgar::has_recent_offering(&key),
            },
            catalyst_weights,
        );
        let catalyst_pills = catalyst_flags.pills_owned();

        out.push(WatchlistMetrics {
            symbol: key,
            price: m.current_price,
            bid: m.bid,
            ask: m.ask,
            spread_bps: m.spread_bps,
            change_percent: m.change_pct,
            volume: m.volume_today,
            rvol,
            gap_percent: gap_pct,
            last_trade_age_seconds: m.last_trade_age_seconds,
            // SC11: no same-ET-date print ⇒ treat as non-actionable/stale.
            is_stale: m.is_stale || !m.has_today_trade,
            catalyst_flags,
            catalyst_pills,
            news_sentiment,
            updated_at: Utc::now(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::market_state::HaltSource;
    use crate::market::scanner::{LatestTrade, SnapshotDailyBar};
    use chrono::TimeZone;

    fn rth_now() -> chrono::DateTime<chrono_tz::Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 10, 30, 0)
            .unwrap()
    }

    fn fresh_snapshot(close: f64, open: f64, vol: f64, prev_close: f64, prev_vol: f64) -> SnapshotData {
        SnapshotData {
            latest_trade: Some(LatestTrade {
                p: close,
                t: "2026-04-29T14:30:00Z".to_string(), // matches rth_now()
                s: None,
            }),
            latest_quote: None,
            minute_bar: None,
            daily_bar: Some(SnapshotDailyBar { c: close, o: open, v: vol }),
            prev_daily_bar: Some(SnapshotDailyBar { c: prev_close, o: prev_close, v: prev_vol }),
        }
    }

    #[test]
    fn compute_metrics_for_halted_symbol_returns_score_zero() {
        let mut snaps = HashMap::new();
        snaps.insert(
            "HLT".to_string(),
            fresh_snapshot(150.0, 145.0, 5_000_000.0, 140.0, 1_000_000.0),
        );
        let mut halts: HashMap<String, HaltState> = HashMap::new();
        halts.insert(
            "HLT".to_string(),
            HaltState {
                halted_since: Utc::now(),
                reason: Some("Trading Halt".into()),
                source: HaltSource::StatusSub,
                halt_code: Some("H".into()),
                pre_halt_price: None,
                resumed_at: None,
                resume_prints: Vec::new(),
            },
        );
        let metrics = compute_watchlist_metrics(
            &["HLT".into()],
            &snaps,
            &halts,
            &HashMap::new(),
            &ScanParams::default(),
            &CatalystWeights::default(),
            rth_now(),
        );
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].catalyst_flags.halted);
        assert_eq!(metrics[0].catalyst_flags.score, 0.0);
        assert_eq!(metrics[0].catalyst_pills, vec!["HALT".to_string()]);
    }

    #[test]
    fn compute_metrics_for_stale_symbol_marks_is_stale() {
        // Trade 6 minutes old in RTH → STALE per the S11/S12 rule.
        let mut snap = fresh_snapshot(150.0, 145.0, 5_000_000.0, 140.0, 1_000_000.0);
        snap.latest_trade = Some(LatestTrade {
            p: 150.0,
            t: "2026-04-29T14:24:00Z".into(),
            s: None,
        });
        let mut snaps = HashMap::new();
        snaps.insert("OLD".to_string(), snap);

        let metrics = compute_watchlist_metrics(
            &["OLD".into()],
            &snaps,
            &HashMap::new(),
            &HashMap::new(),
            &ScanParams::default(),
            &CatalystWeights::default(),
            rth_now(),
        );
        assert_eq!(metrics.len(), 1, "stale symbols still surface in watchlist");
        assert!(metrics[0].is_stale);
        // Heuristic age >120s in RTH should also flag halted.
        assert!(metrics[0].catalyst_flags.halted);
    }

    #[test]
    fn rvol_uses_params_method() {
        let mut snaps = HashMap::new();
        snaps.insert(
            "SYM".to_string(),
            fresh_snapshot(150.0, 145.0, 10_000_000.0, 140.0, 10_000_000.0),
        );
        let halts = HashMap::new();
        let news = HashMap::new();
        let weights = CatalystWeights::default();

        let mut params = ScanParams::default();
        params.rvol_method = RvolMethod::Naive;
        let m_naive =
            compute_watchlist_metrics(&["SYM".into()], &snaps, &halts, &news, &params, &weights, rth_now());
        assert_eq!(m_naive.len(), 1);
        assert!(
            (m_naive[0].rvol - 1.0).abs() < 1e-9,
            "Naive rvol = vol/prev = 1.0 (got {})",
            m_naive[0].rvol
        );

        params.rvol_method = RvolMethod::TimeAdjusted;
        let m_ta =
            compute_watchlist_metrics(&["SYM".into()], &snaps, &halts, &news, &params, &weights, rth_now());
        assert!(
            m_ta[0].rvol > 4.0,
            "TimeAdjusted rvol must be >> 1 at 10:30 ET (got {})",
            m_ta[0].rvol
        );
    }

    #[test]
    fn compute_metrics_preserves_favorites_order() {
        let mut snaps = HashMap::new();
        for s in ["AAA", "BBB", "CCC"] {
            snaps.insert(
                s.to_string(),
                fresh_snapshot(150.0, 145.0, 2_000_000.0, 140.0, 1_000_000.0),
            );
        }
        let metrics = compute_watchlist_metrics(
            &["BBB".into(), "AAA".into(), "CCC".into()],
            &snaps,
            &HashMap::new(),
            &HashMap::new(),
            &ScanParams::default(),
            &CatalystWeights::default(),
            rth_now(),
        );
        let order: Vec<&str> = metrics.iter().map(|m| m.symbol.as_str()).collect();
        assert_eq!(order, vec!["BBB", "AAA", "CCC"]);
    }

    #[test]
    fn missing_snapshot_skipped_silently() {
        // Snapshot present for AAA, missing for ZZZ — output has only AAA.
        let mut snaps = HashMap::new();
        snaps.insert(
            "AAA".to_string(),
            fresh_snapshot(150.0, 145.0, 2_000_000.0, 140.0, 1_000_000.0),
        );
        let metrics = compute_watchlist_metrics(
            &["AAA".into(), "ZZZ".into()],
            &snaps,
            &HashMap::new(),
            &HashMap::new(),
            &ScanParams::default(),
            &CatalystWeights::default(),
            rth_now(),
        );
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].symbol, "AAA");
    }
}
