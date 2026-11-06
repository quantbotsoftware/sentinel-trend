//! Premarket / session hot-mover news prefetch for the scanner.
//!
//! The background `run_catalyst_refresh_loop` only warms news for the
//! watchlist + active symbol. A full-universe scan therefore never saw
//! NEWS pills on fresh gappers. This module selects the hottest movers
//! from the snapshot set, fetches Alpaca news for them, and attaches
//! the top headline/URL onto each `ScanResult` for one-click reading.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use super::scoring::extract_bar_metrics;
use super::sessions::in_premarket;
use super::types::{ScanResult, SnapshotData};
use crate::market::alpaca_news::{
    build_news_map, fetch_news_for_symbols, global_cache, NewsItem,
};

/// Max symbols to request news for per scan (3 chunks × 50 = rate-safe).
const HOT_NEWS_LIMIT: usize = 150;
/// Absolute move (change% or gap%) required to qualify as a news target.
const PREMARKET_HEAT_MIN: f64 = 3.0;
const RTH_HEAT_MIN: f64 = 5.0;

/// Pick the hottest movers from a snapshot map for a news fetch.
///
/// Ranking key = max(|change%|, |gap%|). Stale / invalid rows are skipped.
pub(super) fn select_hot_news_symbols(
    snapshots: &HashMap<String, SnapshotData>,
    now_et: DateTime<Tz>,
    limit: usize,
) -> Vec<String> {
    let heat_min = if in_premarket(now_et) {
        PREMARKET_HEAT_MIN
    } else {
        RTH_HEAT_MIN
    };
    let mut scored: Vec<(f64, String)> = Vec::new();

    for (sym, snap) in snapshots {
        let m = extract_bar_metrics(snap, now_et);
        if m.is_stale
            || !m.has_today_trade
            || !m.current_price.is_finite()
            || m.current_price <= 0.0
            || m.prev_close <= 0.0
        {
            continue;
        }
        let gap_pct = snap
            .daily_bar
            .as_ref()
            .map(|db| ((db.o - m.prev_close) / m.prev_close) * 100.0)
            .filter(|g| g.is_finite())
            .unwrap_or(0.0);
        let heat = m.change_pct.abs().max(gap_pct.abs());
        if heat < heat_min {
            continue;
        }
        scored.push((heat, sym.to_uppercase()));
    }

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(_, s)| s).collect()
}

/// Fetch + cache news for hot movers, then return a news_map view for
/// the full candidate symbol set (cache hits only).
#[allow(clippy::too_many_arguments)]
pub(super) async fn prefetch_hot_mover_news(
    client: &reqwest::Client,
    api_key: &str,
    api_secret: &str,
    snapshots: &HashMap<String, SnapshotData>,
    candidate_symbols: &[String],
    now_et: DateTime<Tz>,
    lookback_minutes: u32,
    news_enabled: bool,
) -> HashMap<String, Vec<NewsItem>> {
    if !news_enabled || api_key.is_empty() || api_secret.is_empty() {
        return build_news_map(candidate_symbols).await;
    }

    let hot = select_hot_news_symbols(snapshots, now_et, HOT_NEWS_LIMIT);
    if hot.is_empty() {
        return build_news_map(candidate_symbols).await;
    }

    // Premarket catalysts often print overnight — stretch lookback to
    // at least 12h so 04:05 ET still sees last evening's wire.
    let lookback = if in_premarket(now_et) {
        lookback_minutes.max(720)
    } else {
        lookback_minutes.max(15)
    };
    let since = Utc::now() - chrono::Duration::minutes(lookback as i64);

    match fetch_news_for_symbols(client, api_key, api_secret, &hot, since).await {
        Ok(map) => {
            let n = map.len();
            global_cache().set_many(map).await;
            tracing::info!(
                component = "scanner",
                hot_requested = hot.len(),
                symbols_with_news = n,
                lookback_min = lookback,
                premarket = in_premarket(now_et),
                "hot-mover news prefetch complete"
            );
        }
        Err(e) => {
            tracing::warn!(
                component = "scanner",
                "hot-mover news prefetch failed: {e}"
            );
        }
    }

    build_news_map(candidate_symbols).await
}

/// Copy the newest headline + URL onto each scan row that has news.
pub(super) fn attach_top_news(
    results: &mut [ScanResult],
    news_map: &HashMap<String, Vec<NewsItem>>,
) {
    for r in results.iter_mut() {
        let key = r.symbol.to_uppercase();
        let Some(items) = news_map.get(&key) else {
            continue;
        };
        let Some(top) = items.first() else {
            continue;
        };
        r.news_headline = top.headline.clone();
        r.news_url = top.url.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::scanner::types::{LatestTrade, SnapshotDailyBar};

    fn et(h: u32, m: u32) -> DateTime<Tz> {
        // Wednesday 2026-04-29
        chrono::NaiveDate::from_ymd_opt(2026, 4, 29)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
            .and_local_timezone(chrono_tz::US::Eastern)
            .unwrap()
    }

    fn snap(price: f64, open: f64, prev: f64, trade_t: &str) -> SnapshotData {
        SnapshotData {
            latest_trade: Some(LatestTrade {
                p: price,
                t: trade_t.to_string(),
                s: Some(100),
            }),
            latest_quote: None,
            minute_bar: None,
            daily_bar: Some(SnapshotDailyBar {
                c: price,
                o: open,
                v: 200_000.0,
            }),
            prev_daily_bar: Some(SnapshotDailyBar {
                c: prev,
                o: prev,
                v: 1_000_000.0,
            }),
        }
    }

    #[test]
    fn select_hot_includes_premarket_gapper() {
        // 07:30 ET premarket — trade age ~60s under 300s threshold.
        let now = et(7, 30);
        let mut map = HashMap::new();
        map.insert(
            "HOT".into(),
            snap(11.0, 10.5, 10.0, "2026-04-29T11:29:00Z"),
        ); // +10% change, +5% gap
        map.insert(
            "COLD".into(),
            snap(10.1, 10.05, 10.0, "2026-04-29T11:29:00Z"),
        ); // +1%
        let hot = select_hot_news_symbols(&map, now, 10);
        assert!(hot.contains(&"HOT".to_string()));
        assert!(!hot.contains(&"COLD".to_string()));
    }

    #[test]
    fn attach_top_news_fills_headline_and_url() {
        let mut results = vec![ScanResult {
            symbol: "ABC".into(),
            ..Default::default()
        }];
        let mut news = HashMap::new();
        news.insert(
            "ABC".into(),
            vec![NewsItem {
                id: 1,
                headline: "ABC wins FDA approval".into(),
                author: "wire".into(),
                created_at: "2026-04-29T10:00:00Z".into(),
                updated_at: String::new(),
                symbols: vec!["ABC".into()],
                source: "benzinga".into(),
                url: "https://example.com/abc".into(),
            }],
        );
        attach_top_news(&mut results, &news);
        assert_eq!(results[0].news_headline, "ABC wins FDA approval");
        assert_eq!(results[0].news_url, "https://example.com/abc");
    }
}
