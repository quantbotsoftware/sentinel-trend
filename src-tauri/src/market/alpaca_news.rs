// ═══════════════════════════════════════════════════════════════════
//  S13 — Alpaca News API integration
//
//  Endpoint:  GET https://data.alpaca.markets/v1beta1/news
//  Auth:      APCA-API-KEY-ID / APCA-API-SECRET-KEY headers (S01)
//  Per docs:  limit max 50 per call; rate limit 100 req/min.
//
//  Cache contract:
//    - TTL = 5 min per symbol; cap 500 entries (oldest-first eviction).
//    - Tier B (own TokioMutex). Critical sections never cross await.
//    - Global singleton via OnceLock<NewsCache> (mirrors `asset_cache`
//      pattern in scanner/snapshots.rs).
//
//  Retry policy:
//    - 429 → exponential backoff 1s, 2s, 4s (max 3 retries).
//    - 500 → 1 retry.
//    - 4xx other (400/401/403) → no retry, propagate Err.
//    - Network err → 1 retry after 1.5s.
//
//  Lock ordering: this module never touches TitanState; the refresh
//  loop reads `state.config` (Tier A) → drops → does HTTP I/O → writes
//  the cache (own mutex). No Tier-A and Tier-B held simultaneously.
// ═══════════════════════════════════════════════════════════════════

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

use crate::state::TitanState;

const NEWS_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const NEWS_CACHE_MAX: usize = 500;
const NEWS_CHUNK_SIZE: usize = 50;
const NEWS_API_URL: &str = "https://data.alpaca.markets/v1beta1/news";
const REFRESH_INTERVAL_SECS: u64 = 300; // 5 min — Alpaca rate-limit constraint
/// S15.5 Fix #4 — watchdog tick cadence for slow-refresh loops.
/// `STALE_WARN_SECS=60` in `ops::watchdog`; ticking every 30s keeps
/// the news loop healthy even though its real refresh fires only at
/// `REFRESH_INTERVAL_SECS`. Aligned with `watchlist_refresh` cadence.
const TICK_CHUNK_SECS: u64 = 30;

// ─── Item ───────────────────────────────────────────────────────────

/// One news article as returned by Alpaca's `/v1beta1/news`.
///
/// `id` is `i64` per the official OpenAPI schema (`int64` is signed).
/// The S13 prompt suggested `u64`; in practice IDs are positive but
/// keeping the docs-correct type avoids serde failures on edge cases.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NewsItem {
    pub id: i64,
    pub headline: String,
    #[serde(default)]
    pub author: String,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub source: String,
    /// v2.4.3 — canonical article URL returned by Alpaca's
    /// `/v1beta1/news` (Benzinga / wire-service link).
    /// `#[serde(default)]` so previously-cached items (no `url`)
    /// deserialize cleanly; the UI falls back to text-only rendering
    /// when this is empty.
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct NewsResponse {
    #[serde(default)]
    news: Vec<NewsItem>,
    #[serde(default)]
    #[allow(dead_code)] // pagination not consumed in S13 (lookback=4h fits in one page)
    next_page_token: Option<String>,
}

// ─── Cache ──────────────────────────────────────────────────────────

type CacheInner = HashMap<String, (Instant, Vec<NewsItem>)>;

#[derive(Default)]
pub struct NewsCache {
    inner: Arc<TokioMutex<CacheInner>>,
}

impl NewsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch a fresh entry (within TTL) for `symbol`; None on miss.
    pub async fn get(&self, symbol: &str) -> Option<Vec<NewsItem>> {
        let key = symbol.to_uppercase();
        let guard = self.inner.lock().await;
        guard.get(&key).and_then(|(at, items)| {
            if at.elapsed() < NEWS_CACHE_TTL {
                Some(items.clone())
            } else {
                None
            }
        })
    }

    /// Insert (or overwrite) an entry. Evicts the oldest when at cap.
    pub async fn set(&self, symbol: &str, items: Vec<NewsItem>) {
        let key = symbol.to_uppercase();
        let mut guard = self.inner.lock().await;
        if guard.len() >= NEWS_CACHE_MAX && !guard.contains_key(&key) {
            // Linear-scan eviction of oldest. n=500 → cheap.
            if let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest_key);
            }
        }
        guard.insert(key, (Instant::now(), items));
    }

    /// Bulk merge — used by the refresh loop after a fan-out call.
    pub async fn set_many(&self, map: HashMap<String, Vec<NewsItem>>) {
        for (sym, items) in map {
            self.set(&sym, items).await;
        }
    }

    /// Real-time upsert of a single item (news websocket / EDGAR).
    /// Prepends, dedupes by `id`, refreshes the TTL instant, caps the
    /// per-symbol list at 25 items (S06 — bounded).
    pub async fn push_item(&self, symbol: &str, item: NewsItem) {
        let key = symbol.to_uppercase();
        let mut guard = self.inner.lock().await;
        if guard.len() >= NEWS_CACHE_MAX && !guard.contains_key(&key) {
            if let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest_key);
            }
        }
        let entry = guard.entry(key).or_insert_with(|| (Instant::now(), Vec::new()));
        entry.0 = Instant::now();
        entry.1.retain(|n| n.id != item.id);
        entry.1.insert(0, item);
        entry.1.truncate(25);
    }

    #[allow(dead_code)] // exposed for daily_reset hook (S06 invariant), wired in S14+
    pub async fn clear(&self) {
        self.inner.lock().await.clear();
    }

    #[allow(dead_code)]
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

/// Process-wide singleton. The scanner / watchlist / refresh loop all
/// share this instance.
pub fn global_cache() -> &'static NewsCache {
    static CACHE: OnceLock<NewsCache> = OnceLock::new();
    CACHE.get_or_init(NewsCache::new)
}

/// Build a `news_map` view of cached items for a candidate set. Keys
/// are uppercase symbols. Missing-from-cache symbols are simply absent.
pub async fn build_news_map(symbols: &[String]) -> HashMap<String, Vec<NewsItem>> {
    let cache = global_cache();
    let mut out = HashMap::with_capacity(symbols.len());
    for sym in symbols {
        if let Some(items) = cache.get(sym).await {
            out.insert(sym.to_uppercase(), items);
        }
    }
    out
}

// ─── Fetch ─────────────────────────────────────────────────────────

/// Split symbols into chunks of NEWS_CHUNK_SIZE (50). Pure helper —
/// extracted so the chunk logic is unit-testable without a live API.
pub fn chunk_symbols(symbols: &[String]) -> Vec<&[String]> {
    symbols.chunks(NEWS_CHUNK_SIZE).collect()
}

/// Fetch news for the given symbols since `since` (inclusive). Items
/// are fanned out by `symbols` field — one news article mentioning two
/// tickers will surface in both vec entries.
///
/// Returns Err on hard failure (final retry exhausted, 4xx other).
/// Returns an empty map (not Err) when no symbols provided or when
/// the API response contains zero items.
pub async fn fetch_news_for_symbols(
    client: &reqwest::Client,
    api_key_id: &str,
    api_secret: &str,
    symbols: &[String],
    since: DateTime<Utc>,
) -> Result<HashMap<String, Vec<NewsItem>>, String> {
    if symbols.is_empty() {
        return Ok(HashMap::new());
    }
    if api_key_id.is_empty() || api_secret.is_empty() {
        return Err("Alpaca API keys not configured".to_string());
    }

    let mut out: HashMap<String, Vec<NewsItem>> = HashMap::new();
    // S17.6 — Alpaca News query parameter parsing.
    //
    // `chrono::DateTime<Utc>::to_rfc3339()` emits the offset suffix as
    // `+00:00`, which becomes URL-decoded as `<space>00:00` once the
    // unencoded `+` is interpreted as a space. Alpaca then rejects
    // the request with HTTP 400:
    //   "Invalid format for parameter start: error parsing
    //    '2026-05-07T06:09:12 00:00' as RFC3339"
    //
    // Live evidence: every catalyst-news refresh between 2026-05-05
    // 23:00 UTC and 2026-05-07 10:08 UTC failed with 400 (~one
    // request_id every 5 min) → news-recent flag never armed → the
    // 25-point news contribution was missing from every catalyst score
    // → only candidates with gap+vol or vol-spike alone could clear
    // a `catalyst_min_score=25` Quality Gate.
    //
    // Use the explicit `Z` formatter; it sidesteps both the literal-
    // `+` issue and any sub-second precision Alpaca may not expect.
    let start_iso = since.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    for chunk in chunk_symbols(symbols) {
        let sym_list = chunk.join(",");
        let resp = fetch_one_chunk(
            client,
            api_key_id,
            api_secret,
            &sym_list,
            &start_iso,
        )
        .await?;

        for item in resp.news {
            // Fan out by mentioned symbols, scoped to the requested set.
            let chunk_set: std::collections::HashSet<String> =
                chunk.iter().map(|s| s.to_uppercase()).collect();
            let item_arc_symbols: Vec<String> = item
                .symbols
                .iter()
                .map(|s| s.to_uppercase())
                .filter(|s| chunk_set.contains(s))
                .collect();
            for sym in item_arc_symbols {
                out.entry(sym).or_default().push(item.clone());
            }
        }
    }

    Ok(out)
}

async fn fetch_one_chunk(
    client: &reqwest::Client,
    api_key_id: &str,
    api_secret: &str,
    sym_list: &str,
    start_iso: &str,
) -> Result<NewsResponse, String> {
    let url = format!(
        "{NEWS_API_URL}?symbols={sym_list}&start={start_iso}\
         &limit=50&sort=desc&include_content=false&exclude_contentless=false"
    );

    // 429: 1s, 2s, 4s exponential backoff; max 3 retries.
    let backoffs_429 = [1_u64, 2, 4];
    // 500: single retry after 1s.
    let mut server_err_attempted = false;
    // Network err: single retry after 1.5s.
    let mut net_err_attempted = false;
    // 429 attempt cursor (0..3).
    let mut rate_attempt = 0_usize;

    loop {
        let send = client
            .get(&url)
            .header("APCA-API-KEY-ID", api_key_id)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await;

        match send {
            Err(e) => {
                if !net_err_attempted {
                    net_err_attempted = true;
                    tracing::warn!(component = "news", "network error: {e}; retrying in 1.5s");
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    continue;
                }
                return Err(format!("news request failed: {e}"));
            }
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return resp
                        .json::<NewsResponse>()
                        .await
                        .map_err(|e| format!("news parse error: {e}"));
                }
                if status.as_u16() == 429 {
                    if rate_attempt >= backoffs_429.len() {
                        let req_id = req_id_from(&resp);
                        return Err(format!(
                            "news 429 rate-limited after {} retries (request_id={req_id})",
                            backoffs_429.len()
                        ));
                    }
                    let delay = backoffs_429[rate_attempt];
                    rate_attempt += 1;
                    tracing::warn!(
                        component = "news",
                        attempt = rate_attempt,
                        delay_secs = delay,
                        "news 429 rate-limit; backing off"
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    continue;
                }
                if status.as_u16() == 500 || status.as_u16() == 502 || status.as_u16() == 503 {
                    if !server_err_attempted {
                        server_err_attempted = true;
                        tracing::warn!(
                            component = "news",
                            status = status.as_u16(),
                            "news server error; retrying once"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    let req_id = req_id_from(&resp);
                    return Err(format!(
                        "news server error {} after retry (request_id={req_id})",
                        status.as_u16()
                    ));
                }
                let req_id = req_id_from(&resp);
                return Err(format!(
                    "news API error {} (request_id={req_id})",
                    status.as_u16()
                ));
            }
        }
    }
}

fn req_id_from(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("x-request-id")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("?")
        .to_string()
}

// ─── Background loop ───────────────────────────────────────────────

/// Single watchdog tick on the named loop. Tier-A `system` lock is
/// acquired alone (never held with Tier-B), satisfying the boundary
/// rule. Poison-tolerant per S05 hot-path policy.
fn tick_loop_watchdog(state: &TitanState, loop_name: &str) {
    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(h) = sys.loops.get_mut(loop_name) {
        h.tick();
    }
}

/// S15.5 Fix #4 — chunked sleep with periodic watchdog ticks.
///
/// Splits a long sleep (`total_secs`) into `chunk_secs`-sized slices
/// and ticks the watchdog after each slice. Returns `false` if the
/// shutdown token cancelled mid-sleep (so the caller can `break`),
/// `true` on a normal completion.
///
/// Reusable pattern for any loop whose true refresh cadence exceeds
/// `STALE_WARN_SECS=60` (currently only `catalyst_news`; future slow
/// loops should adopt this pattern instead of touching the watchdog).
async fn chunked_tick_sleep(
    state: &TitanState,
    loop_name: &str,
    total_secs: u64,
    chunk_secs: u64,
) -> bool {
    if chunk_secs == 0 || total_secs == 0 {
        return true;
    }
    let chunks = total_secs.div_ceil(chunk_secs);
    for _ in 0..chunks {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(chunk_secs)) => {}
            _ = state.shutdown.cancelled() => { return false; }
        }
        tick_loop_watchdog(state, loop_name);
    }
    true
}

/// 5-min refresh of news for watchlist symbols + active symbol.
/// Reads `state.config` (Tier A only); drops; calls Alpaca News;
/// writes to the global cache (own Tier-B mutex). No Tier-A while
/// holding Tier-B and vice-versa.
///
/// S15.5 — uses [`chunked_tick_sleep`] so the watchdog (60s threshold)
/// stays green between the 5-min refreshes. The original spec assumed
/// the Err arm of the `fetch` match wasn't ticking, but reading the
/// pre-S15.5 code shows the tick was already after the match: the real
/// cause was `REFRESH_INTERVAL_SECS=300 > STALE_WARN_SECS=60`.
pub async fn run_catalyst_refresh_loop(state: Arc<TitanState>) {
    tracing::info!(component = "catalyst_news", "Catalyst news refresh loop started");

    loop {
        if state.shutdown.is_cancelled() {
            tracing::info!(component = "catalyst_news", "shutting down");
            break;
        }

        let (api_key, secret, news_enabled, lookback_minutes, symbols, active_symbol) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            let mut symbols: Vec<String> = cfg
                .ui
                .favorites
                .iter()
                .map(|s| s.to_uppercase())
                .collect();
            let active = cfg.ui.active_symbol.to_uppercase();
            if !active.is_empty() && !symbols.contains(&active) {
                symbols.push(active.clone());
            }
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
                cfg.catalyst.news_enabled,
                cfg.catalyst.news_lookback_minutes.max(15),
                symbols,
                active,
            )
        };
        let _ = active_symbol;

        if !news_enabled || api_key.is_empty() || secret.is_empty() || symbols.is_empty() {
            // Quiet path — still tick the watchdog at 30s cadence so
            // operators aren't spammed with stale warnings while they
            // configure their Alpaca key.
            if !chunked_tick_sleep(
                &state,
                "catalyst_news",
                REFRESH_INTERVAL_SECS,
                TICK_CHUNK_SECS,
            )
            .await
            {
                break;
            }
            continue;
        }

        let since = Utc::now() - chrono::Duration::minutes(lookback_minutes as i64);
        match fetch_news_for_symbols(&state.http_client, &api_key, &secret, &symbols, since).await
        {
            Ok(map) => {
                let n = map.len();
                global_cache().set_many(map).await;
                tracing::info!(
                    component = "catalyst_news",
                    symbols_with_news = n,
                    requested = symbols.len(),
                    "news refresh complete"
                );
            }
            Err(e) => {
                tracing::warn!(component = "catalyst_news", "news refresh failed: {e}");
            }
        }

        // Tick once for the work just completed, then chunked sleep
        // (which ticks again every 30s) until the next refresh.
        tick_loop_watchdog(&state, "catalyst_news");
        if !chunked_tick_sleep(
            &state,
            "catalyst_news",
            REFRESH_INTERVAL_SECS,
            TICK_CHUNK_SECS,
        )
        .await
        {
            break;
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, headline: &str, sym: &str) -> NewsItem {
        NewsItem {
            id,
            headline: headline.to_string(),
            author: "test".to_string(),
            created_at: "2026-04-29T10:00:00Z".to_string(),
            updated_at: "2026-04-29T10:00:00Z".to_string(),
            symbols: vec![sym.to_string()],
            source: "benzinga".to_string(),
            url: String::new(),
        }
    }

    #[test]
    fn news_item_serde_roundtrip() {
        let n = item(123, "Earnings beat", "AAPL");
        let json = serde_json::to_string(&n).unwrap();
        let loaded: NewsItem = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, n);
    }

    /// Test 12: cache TTL miss on stale entry.
    #[test]
    fn news_cache_ttl_expiry() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cache = NewsCache::new();

            // Manually insert with a stale timestamp to simulate >5min old.
            {
                let mut guard = cache.inner.lock().await;
                let stale = Instant::now() - Duration::from_secs(360);
                guard.insert("AAPL".to_string(), (stale, vec![item(1, "old", "AAPL")]));
            }

            assert!(
                cache.get("AAPL").await.is_none(),
                "stale entry must be reported as miss"
            );
        });
    }

    /// Test 13: set + get roundtrip preserves data.
    #[test]
    fn news_cache_set_get_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cache = NewsCache::new();
            let items = vec![item(1, "a", "AAPL"), item(2, "b", "AAPL")];
            cache.set("AAPL", items.clone()).await;

            let got = cache.get("AAPL").await.expect("must hit");
            assert_eq!(got.len(), 2);
            assert_eq!(got[0].id, 1);
            assert_eq!(got[1].id, 2);

            // Case-insensitive: lowercase query hits uppercase storage.
            let lower = cache.get("aapl").await.expect("case-insensitive must hit");
            assert_eq!(lower.len(), 2);
        });
    }

    /// Test 14: chunking 120 symbols yields 3 chunks of size 50/50/20.
    #[test]
    fn chunk_symbols_50_yields_three_calls_for_120() {
        let symbols: Vec<String> = (0..120).map(|i| format!("S{i:03}")).collect();
        let chunks = chunk_symbols(&symbols);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 50);
        assert_eq!(chunks[1].len(), 50);
        assert_eq!(chunks[2].len(), 20);
    }

    #[test]
    fn chunk_symbols_50_exact_yields_one() {
        let symbols: Vec<String> = (0..50).map(|i| format!("S{i:02}")).collect();
        let chunks = chunk_symbols(&symbols);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 50);
    }

    #[test]
    fn chunk_symbols_empty_yields_none() {
        let symbols: Vec<String> = vec![];
        assert!(chunk_symbols(&symbols).is_empty());
    }

    #[test]
    fn news_cache_eviction_at_cap() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cache = NewsCache::new();
            // Pre-fill manually past cap (skip set's gating to hit eviction path).
            {
                let mut guard = cache.inner.lock().await;
                for i in 0..NEWS_CACHE_MAX {
                    let key = format!("S{i:04}");
                    let stale = Instant::now() - Duration::from_secs((NEWS_CACHE_MAX - i) as u64);
                    guard.insert(key, (stale, vec![item(i as i64, "x", "X")]));
                }
            }

            // Insert one more — oldest (S0000, with the largest negative offset)
            // must be evicted.
            cache.set("NEW", vec![item(9999, "fresh", "NEW")]).await;

            let len = cache.len().await;
            assert_eq!(len, NEWS_CACHE_MAX, "cache must not grow past cap");

            // S0000 was the oldest insert (largest negative offset);
            // S0001 was second-oldest, etc. The eviction picks the
            // single oldest, so S0000 is gone.
            assert!(cache.get("S0000").await.is_none(), "oldest must be evicted");
            assert!(cache.get("NEW").await.is_some(), "new entry must be present");
        });
    }

    #[test]
    fn fetch_returns_err_on_empty_keys() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = reqwest::Client::new();
            let res = fetch_news_for_symbols(
                &client,
                "",
                "",
                &["AAPL".to_string()],
                Utc::now(),
            )
            .await;
            assert!(res.is_err());
        });
    }

    // ─── S15.5 — chunked watchdog tick ─────────────────────────────
    /// Verifies that `chunked_tick_sleep` ticks the watchdog at the
    /// chunk cadence (every 30s) over a 300s window — keeping the news
    /// loop healthy under the 60s `STALE_WARN_SECS` threshold.
    #[tokio::test(start_paused = true)]
    async fn chunked_tick_sleep_ticks_at_30s_cadence() {
        use crate::config::RuntimeConfig;

        let state = Arc::new(TitanState::new(RuntimeConfig::default()));
        {
            let mut sys = state.system.lock().unwrap();
            sys.register_loop("catalyst_news");
        }

        // Spawn the chunked sleep with the production parameters.
        let st = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            chunked_tick_sleep(&st, "catalyst_news", 300, 30).await
        });

        // Advance 30s × 10 = 300s, awaiting between advances so the
        // sleeps fire deterministically.
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
        }
        let completed = handle.await.unwrap();
        assert!(completed, "chunked_tick_sleep should complete normally");

        let sys = state.system.lock().unwrap();
        let h = sys.loops.get("catalyst_news").expect("loop registered");
        assert_eq!(
            h.tick_count, 10,
            "10 chunks × 30s should produce 10 watchdog ticks (got {})",
            h.tick_count
        );
    }

    /// Verifies that shutdown cancellation mid-sleep returns false
    /// promptly (without waiting for the rest of the chunks).
    #[tokio::test(start_paused = true)]
    async fn chunked_tick_sleep_exits_promptly_on_shutdown() {
        use crate::config::RuntimeConfig;

        let state = Arc::new(TitanState::new(RuntimeConfig::default()));
        {
            let mut sys = state.system.lock().unwrap();
            sys.register_loop("catalyst_news");
        }

        let st = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            chunked_tick_sleep(&st, "catalyst_news", 300, 30).await
        });

        // Advance a tiny bit so the first sleep is in flight.
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;

        // Trigger shutdown.
        state.shutdown.cancel();
        let completed = handle.await.unwrap();
        assert!(!completed, "shutdown must return false from chunked_tick_sleep");

        let sys = state.system.lock().unwrap();
        let h = sys.loops.get("catalyst_news").expect("loop registered");
        assert_eq!(
            h.tick_count, 0,
            "no chunk completed → no ticks (got {})",
            h.tick_count
        );
    }

    #[test]
    fn fetch_returns_empty_for_empty_input() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = reqwest::Client::new();
            let res =
                fetch_news_for_symbols(&client, "k", "s", &[], Utc::now())
                    .await
                    .expect("empty input should be Ok");
            assert!(res.is_empty());
        });
    }

    // ── S17.6 regression — Alpaca News `start` parameter URL format ──
    //
    // chrono's `to_rfc3339()` emits the offset suffix as literal
    // `+00:00`. When that string is interpolated into a URL query
    // without percent-encoding, the receiving server sees `+` as a
    // space and Alpaca rejects with HTTP 400. The fix uses an explicit
    // `Z` formatter; this test pins that contract so a future change
    // back to `to_rfc3339()` will fail loudly here instead of silently
    // breaking the catalyst pipeline at runtime.
    #[test]
    fn s17_6_start_iso_uses_z_suffix_not_plus_offset() {
        let naive = chrono::NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2026, 5, 7).unwrap(),
            chrono::NaiveTime::from_hms_opt(10, 9, 12).unwrap(),
        );
        let dt: chrono::DateTime<Utc> =
            chrono::DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
        let formatted = dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        assert_eq!(formatted, "2026-05-07T10:09:12Z");
        assert!(
            !formatted.contains('+'),
            "URL-unsafe '+' must not appear in start_iso (Alpaca decodes \
             unencoded '+' as space and returns HTTP 400)"
        );
        assert!(
            !formatted.contains(' '),
            "no whitespace in start_iso — Alpaca would 400 on it"
        );
        assert!(
            formatted.ends_with('Z'),
            "Z suffix is the contract; switching to to_rfc3339() reverts the bug"
        );
    }
}
