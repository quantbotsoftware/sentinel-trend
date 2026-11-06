use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::Mutex as TokioMutex;

use super::run::report_progress;
use super::types::{AlpacaAsset, ProgressReporter, SnapshotData};
use crate::error::TitanError;

// ─── Asset Cache (4h TTL, in-memory only) ───────────────────────────

type AssetCacheInner = Option<(Instant, Vec<AlpacaAsset>)>;

fn asset_cache() -> &'static TokioMutex<AssetCacheInner> {
    static CACHE: OnceLock<TokioMutex<AssetCacheInner>> = OnceLock::new();
    CACHE.get_or_init(|| TokioMutex::new(None))
}

const ASSET_CACHE_TTL: Duration = Duration::from_secs(4 * 3600);

/// Single-flight guard for the asset download: bootstrap and auto-scan
/// both call `fetch_active_assets` on cold start; without this they race
/// two ~10MB downloads in parallel, halving throughput on slow links.
fn fetch_in_flight() -> &'static TokioMutex<()> {
    static LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| TokioMutex::new(()))
}

/// Look up cached asset metadata by symbol (case-insensitive — symbols are
/// stored uppercase). Returns `None` on cache miss; callers MUST treat this
/// as a conservative reject (no trade without validation flags).
///
/// Used by `bot.rs` Gate 0 (short shortable/ETB) and `order_exec.rs`
/// (fractionable check) to avoid trading symbols whose Alpaca metadata
/// hasn't been populated yet.
pub async fn lookup_asset(symbol: &str) -> Option<AlpacaAsset> {
    let needle = symbol.trim().to_uppercase();
    if needle.is_empty() {
        return None;
    }
    let cache = asset_cache().lock().await;
    cache.as_ref().and_then(|(_, assets)| {
        assets.iter().find(|a| a.symbol == needle).cloned()
    })
}

// ─── Inactive-asset blacklist (S17.0) ────────────────────────────────
//
// `fetch_active_assets` filters Alpaca's asset list to `status=active`.
// Delisted/halted/acquired tickers (e.g. TRUE — TrueCar acquired
// Q4 2024) never appear there, so `lookup_asset` always returns `None`
// for them. That used to make `bot/exit.rs` happily resubmit market
// SELLs every 30 s, which the broker then rejects with HTTP 422
// `asset X is not active` — observed 631 such rejections in a single
// session.
//
// Solution: lazy-populated session blacklist. The first time
// `is_tradable_now()` is asked about a symbol that's missing from the
// active cache, we ping `/v2/assets/{symbol}` directly. If the broker
// confirms inactive/non-tradable we cache the result for the rest of
// the session — subsequent checks return `false` instantly. Any
// process restart re-validates (cheap one-shot per delisted holding).

fn inactive_blacklist() -> &'static TokioMutex<HashSet<String>> {
    static BL: OnceLock<TokioMutex<HashSet<String>>> = OnceLock::new();
    BL.get_or_init(|| TokioMutex::new(HashSet::new()))
}

/// Check whether a symbol is currently tradable on Alpaca, with a
/// session-scoped negative cache for delisted/halted assets. Used by
/// `bot/exit.rs` to suppress max-hold loops on dead tickers.
///
/// Performs at most one extra HTTP call per unknown symbol per process
/// lifetime; subsequent calls are O(1).
pub async fn is_tradable_now(
    client: &reqwest::Client,
    api_key: &str,
    secret_key: &str,
    symbol: &str,
) -> bool {
    let needle = symbol.trim().to_uppercase();
    if needle.is_empty() {
        return false;
    }

    if inactive_blacklist().lock().await.contains(&needle) {
        return false;
    }

    if let Some(asset) = lookup_asset(&needle).await {
        if !asset.tradable {
            inactive_blacklist().lock().await.insert(needle);
            return false;
        }
        return true;
    }

    let url = format!("https://paper-api.alpaca.markets/v2/assets/{needle}");
    match client
        .get(&url)
        .header("APCA-API-KEY-ID", api_key)
        .header("APCA-API-SECRET-KEY", secret_key)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let v: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(_) => return true,
            };
            let tradable = v.get("tradable").and_then(|x| x.as_bool()).unwrap_or(true);
            let status = v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !tradable || status.eq_ignore_ascii_case("inactive") {
                tracing::warn!(
                    component = "scanner",
                    symbol = %needle,
                    status = %status,
                    tradable,
                    "Asset confirmed non-tradable; adding to session blacklist"
                );
                inactive_blacklist().lock().await.insert(needle);
                false
            } else {
                true
            }
        }
        Ok(resp) => {
            tracing::debug!(
                component = "scanner",
                symbol = %needle,
                status = %resp.status(),
                "is_tradable_now: broker non-success"
            );
            true
        }
        Err(e) => {
            tracing::debug!(
                component = "scanner",
                symbol = %needle,
                error = %e,
                "is_tradable_now: HTTP failure"
            );
            true
        }
    }
}

/// One-shot bootstrap fetch: populates the asset cache (if cold) so Gate 0
/// has metadata available before the first scan runs. Spawned at startup
/// from `main.rs`. Returns the count of assets cached or an error.
pub async fn refresh_asset_metadata(
    client: &reqwest::Client,
    api_key: &str,
    secret_key: &str,
) -> Result<usize, TitanError> {
    let assets = fetch_active_assets(client, api_key, secret_key).await?;
    Ok(assets.len())
}

pub async fn fetch_active_assets(
    client: &reqwest::Client,
    api_key: &str,
    secret_key: &str,
) -> Result<Vec<AlpacaAsset>, TitanError> {
    // Serialize downloads; whoever waited here re-checks the cache first,
    // so a second concurrent caller reuses the winner's result.
    let _flight = fetch_in_flight().lock().await;
    {
        let cache = asset_cache().lock().await;
        if let Some((ts, assets)) = cache.as_ref() {
            if ts.elapsed() < ASSET_CACHE_TTL {
                tracing::debug!(component = "scanner", cached = assets.len(), "Using cached asset list");
                return Ok(assets.clone());
            }
        }
    }

    let url = "https://paper-api.alpaca.markets/v2/assets?status=active&asset_class=us_equity";

    let do_fetch = || async {
        let resp = client
            .get(url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", secret_key)
            // The full asset list is the largest Alpaca payload we pull;
            // override the client-wide 30s so slow links still finish
            // (observed: ~770KB gzipped at ~10KB/s ≈ 80s on hotspot).
            .timeout(Duration::from_secs(180))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let req_id = resp
                .headers()
                .get("x-request-id")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("?")
                .to_string();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(
                status = status.as_u16(),
                request_id = %req_id,
                body = %body.chars().take(200).collect::<String>(),
                "Alpaca API error (fetch_assets)"
            );
            return Err(TitanError::Broker(format!(
                "fetch_assets: HTTP {} — request_id={req_id}",
                status.as_u16(),
            )));
        }

        let assets: Vec<AlpacaAsset> = resp.json().await?;
        Ok(assets)
    };

    let result = match do_fetch().await {
        Ok(assets) => Ok(assets),
        Err(e) => {
            tracing::warn!(component = "scanner", "fetch_active_assets attempt 1 failed: {e}");
            tokio::time::sleep(Duration::from_millis(1500)).await;
            do_fetch().await
        }
    };

    match result {
        Ok(assets) => {
            tracing::info!(component = "scanner", count = assets.len(), "Fetched active assets");
            let mut cache = asset_cache().lock().await;
            *cache = Some((Instant::now(), assets.clone()));
            Ok(assets)
        }
        Err(e) => {
            let cache = asset_cache().lock().await;
            if let Some((_, assets)) = cache.as_ref() {
                tracing::warn!(component = "scanner", stale = assets.len(), "Using stale asset cache after failure");
                return Ok(assets.clone());
            }
            Err(e)
        }
    }
}

const SNAPSHOT_BATCH_SIZE: usize = 200;

pub async fn fetch_snapshots_batch(
    client: &reqwest::Client,
    api_key: &str,
    secret_key: &str,
    symbols: &[String],
    reporter: Option<&ProgressReporter>,
) -> HashMap<String, SnapshotData> {
    let mut all: HashMap<String, SnapshotData> = HashMap::new();
    let total_chunks = symbols.len().div_ceil(SNAPSHOT_BATCH_SIZE);

    for (i, chunk) in symbols.chunks(SNAPSHOT_BATCH_SIZE).enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let sym_list = chunk.join(",");
        let url = format!(
            "https://data.alpaca.markets/v2/stocks/snapshots?symbols={sym_list}&feed=sip"
        );
        match client
            .get(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", secret_key)
            .send()
            .await
        {
            Ok(resp) => match resp.json::<HashMap<String, SnapshotData>>().await {
                Ok(snaps) => {
                    tracing::debug!(
                        component = "scanner",
                        chunk = i + 1,
                        total = total_chunks,
                        count = snaps.len(),
                        "Snapshot chunk OK"
                    );
                    all.extend(snaps);
                }
                Err(e) => {
                    tracing::warn!(component = "scanner", chunk = i + 1, "Snapshot parse error: {e}");
                }
            },
            Err(e) => {
                tracing::warn!(component = "scanner", chunk = i + 1, "Snapshot fetch error: {e}");
            }
        }

        let current = (i + 1) as u32;
        let total = total_chunks as u32;
        let percent = if total_chunks == 0 {
            80
        } else {
            45 + (((i + 1) * 35) / total_chunks) as u8
        };
        report_progress(
            reporter,
            "snapshots",
            format!("Loading snapshots {}/{}", i + 1, total_chunks),
            percent,
            Some(current),
            Some(total),
        );
    }

    tracing::info!(
        component = "scanner",
        total_snapshots = all.len(),
        total_symbols = symbols.len(),
        chunks = total_chunks,
        "Snapshot fetch complete"
    );
    all
}
