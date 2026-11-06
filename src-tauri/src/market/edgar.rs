//! SEC EDGAR current-filings poller (free, primary source).
//!
//! Two atom feeds every 30s (well under SEC's 10 req/s cap, with the
//! mandatory identifying User-Agent):
//!   * `8-K`  — material events → pushed into the shared `NewsCache`
//!     as headlines, lighting `news_recent` (and the PREMARKET_IGNITION
//!     catalyst gate) with no consumer changes.
//!   * `424B` — offering / pricing supplements → recorded in a bounded
//!     offering cache surfaced as `CatalystFlags::offering_risk`
//!     (the classic gapper-killer: dilution priced during the run).
//!
//! CIK→ticker resolution uses SEC's `company_tickers.json`, refreshed
//! daily. Filings whose CIK has no listed ticker are ignored.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::state::TitanState;

const LOOP_NAME: &str = "edgar";
const POLL_SECS: u64 = 30;
/// SEC's WAF requires the documented `Name contact@email` UA format —
/// anything else gets 403 + a temporary IP throttle.
const USER_AGENT: &str = "SentinelTrend admin@sentineltrend.local";
const TICKER_MAP_URL: &str = "https://www.sec.gov/files/company_tickers.json";
const CURRENT_URL: &str = "https://www.sec.gov/cgi-bin/browse-edgar?action=getcurrent&type={FORM}&company=&dateb=&owner=include&count=40&output=atom";
/// Offering flag stays hot this long (dilution risk lingers all day).
const OFFERING_TTL_SECS: i64 = 24 * 3600;
const SEEN_CAP: usize = 4_000;
const OFFERING_CAP: usize = 500;

// ─── Offering cache (S06: capped + TTL prune + daily-reset hook) ────

#[derive(Default)]
pub struct OfferingCache {
    /// symbol → unix ts of the latest offering-type filing.
    inner: Mutex<HashMap<String, i64>>,
}

fn offering_cache() -> &'static OfferingCache {
    static CACHE: OnceLock<OfferingCache> = OnceLock::new();
    CACHE.get_or_init(OfferingCache::default)
}

fn record_offering(symbol: &str, ts: i64) {
    let mut g = offering_cache().inner.lock().unwrap_or_else(|p| p.into_inner());
    let now = chrono::Utc::now().timestamp();
    g.retain(|_, t| now - *t < OFFERING_TTL_SECS);
    if g.len() >= OFFERING_CAP {
        if let Some(oldest) = g.iter().min_by_key(|(_, t)| **t).map(|(k, _)| k.clone()) {
            g.remove(&oldest);
        }
    }
    let e = g.entry(symbol.to_uppercase()).or_insert(ts);
    *e = (*e).max(ts);
}

/// `true` when an offering-type filing landed for `symbol` within the
/// last 24h. Sync + lock-scoped — callable from any consumer.
pub fn has_recent_offering(symbol: &str) -> bool {
    let g = offering_cache().inner.lock().unwrap_or_else(|p| p.into_inner());
    let now = chrono::Utc::now().timestamp();
    g.get(&symbol.to_uppercase())
        .map(|t| now - *t < OFFERING_TTL_SECS)
        .unwrap_or(false)
}

/// Daily-reset hook (S06). Offerings expire by TTL anyway; this keeps
/// the map from carrying weekend leftovers into Monday.
pub fn daily_reset() {
    offering_cache()
        .inner
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

// ─── Atom parsing (pure, unit-testable) ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Filing {
    pub form: String,
    pub cik: u64,
    pub title: String,
    pub entry_id: String,
    pub updated: String,
    pub url: String,
}

fn tag_text<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = block.find(&open)?;
    let gt = block[start..].find('>')? + start + 1;
    let end = block[gt..].find(&format!("</{tag}>"))? + gt;
    Some(block[gt..end].trim())
}

fn attr_of<'a>(block: &'a str, tag: &str, attr: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = block.find(&open)?;
    let end = block[start..].find('>')? + start;
    let inside = &block[start..end];
    let key = format!("{attr}=\"");
    let a = inside.find(&key)? + key.len();
    let b = inside[a..].find('"')? + a;
    Some(&inside[a..b])
}

/// Parse an EDGAR `getcurrent` atom feed. Titles look like
/// `8-K - ACME CORP (0001234567) (Filer)`; the 10-digit number in
/// parentheses is the CIK.
pub fn parse_atom(xml: &str) -> Vec<Filing> {
    let mut out = Vec::new();
    for block in xml.split("<entry>").skip(1) {
        let block = block.split("</entry>").next().unwrap_or("");
        let Some(title) = tag_text(block, "title") else { continue };
        let Some((form, rest)) = title.split_once(" - ") else { continue };
        let Some(cik) = rest
            .rfind('(')
            .and_then(|_| {
                rest.split('(')
                    .filter_map(|s| {
                        let digits: String =
                            s.chars().take_while(|c| c.is_ascii_digit()).collect();
                        if digits.len() == 10 {
                            digits.parse::<u64>().ok()
                        } else {
                            None
                        }
                    })
                    .next()
            })
        else {
            continue;
        };
        out.push(Filing {
            form: form.trim().to_string(),
            cik,
            title: title.to_string(),
            entry_id: tag_text(block, "id").unwrap_or(title).to_string(),
            updated: tag_text(block, "updated").unwrap_or_default().to_string(),
            url: attr_of(block, "link", "href").unwrap_or_default().to_string(),
        });
    }
    out
}

/// Offering / dilution form families (prefix match on the form type).
pub fn is_offering_form(form: &str) -> bool {
    const PREFIXES: &[&str] = &["424B", "S-3", "S-1", "F-1", "F-3"];
    PREFIXES.iter().any(|p| form.starts_with(p))
}

// ─── Ticker map ─────────────────────────────────────────────────────

async fn load_ticker_map(client: &reqwest::Client) -> Option<HashMap<u64, String>> {
    let resp = client
        .get(TICKER_MAP_URL)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let mut map = HashMap::new();
    if let Some(obj) = json.as_object() {
        for v in obj.values() {
            if let (Some(cik), Some(ticker)) = (
                v.get("cik_str").and_then(|c| c.as_u64()),
                v.get("ticker").and_then(|t| t.as_str()),
            ) {
                map.insert(cik, ticker.to_uppercase());
            }
        }
    }
    (!map.is_empty()).then_some(map)
}

// ─── Poll loop ──────────────────────────────────────────────────────

pub async fn run_edgar_loop(state: Arc<TitanState>) {
    tracing::info!(component = LOOP_NAME, "EDGAR poller started");
    // Dedicated client: SEC throttles unfamiliar IPs to a trickle
    // (~3KB/s observed), so the ~1MB ticker map needs gzip (10x —
    // fits in ~30s even throttled) plus a generous timeout.
    let client = reqwest::Client::builder()
        .gzip(true)
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| state.http_client.clone());

    let mut tickers: HashMap<u64, String> = HashMap::new();
    let mut tickers_day: Option<chrono::NaiveDate> = None;
    let mut seen: HashSet<String> = HashSet::new();
    let mut first_pass = true;
    let mut next_map_attempt = tokio::time::Instant::now();

    loop {
        if state.shutdown.is_cancelled() {
            break;
        }

        let today = chrono::Utc::now()
            .with_timezone(&chrono_tz::US::Eastern)
            .date_naive();
        if tickers_day != Some(today) && tokio::time::Instant::now() >= next_map_attempt {
            if let Some(map) = load_ticker_map(&client).await {
                tracing::info!(component = LOOP_NAME, entries = map.len(), "CIK→ticker map refreshed");
                tickers = map;
                tickers_day = Some(today);
                seen.clear(); // day rollover — yesterday's atom ids are stale
            } else {
                // Back off 5 min: hammering the map endpoint is what
                // keeps SEC's per-IP throttle alive in the first place.
                next_map_attempt = tokio::time::Instant::now() + Duration::from_secs(300);
                if tickers.is_empty() {
                    tracing::warn!(component = LOOP_NAME, "ticker map unavailable; next attempt in 5m");
                }
            }
        }

        if !tickers.is_empty() {
            for form in ["8-K", "424B"] {
                let url = CURRENT_URL.replace("{FORM}", form);
                match client
                    .get(&url)
                    .header("User-Agent", USER_AGENT)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        let body = resp.text().await.unwrap_or_default();
                        // First pass seeds `seen` silently: everything
                        // already on the feed predates our boot.
                        consume_filings(&parse_atom(&body), &tickers, &mut seen, first_pass)
                            .await;
                    }
                    Ok(resp) => {
                        tracing::warn!(component = LOOP_NAME, status = %resp.status(), form, "EDGAR feed non-200");
                    }
                    Err(e) => {
                        tracing::warn!(component = LOOP_NAME, error = %e, form, "EDGAR fetch failed");
                    }
                }
                if seen.len() > SEEN_CAP {
                    seen.clear();
                }
            }
            first_pass = false;
        }

        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(POLL_SECS)) => {}
        }
    }
    tracing::info!(component = LOOP_NAME, "EDGAR poller exiting");
}

async fn consume_filings(
    filings: &[Filing],
    tickers: &HashMap<u64, String>,
    seen: &mut HashSet<String>,
    seed_only: bool,
) {
    for f in filings {
        if !seen.insert(f.entry_id.clone()) || seed_only {
            continue;
        }
        let Some(ticker) = tickers.get(&f.cik) else {
            continue;
        };
        let ts = chrono::DateTime::parse_from_rfc3339(&f.updated)
            .map(|d| d.timestamp())
            .unwrap_or_else(|_| chrono::Utc::now().timestamp());

        if is_offering_form(&f.form) {
            record_offering(ticker, ts);
            tracing::warn!(
                component = LOOP_NAME,
                symbol = %ticker,
                form = %f.form,
                "OFFERING filing detected — dilution risk flagged"
            );
        }

        // Every filing (8-K and offerings alike) is also a headline —
        // negative catalysts are still catalysts.
        let item = super::alpaca_news::NewsItem {
            // Deterministic pseudo-id from the entry id (EDGAR has no
            // numeric id); negative range avoids Alpaca id collisions.
            id: -(f
                .entry_id
                .bytes()
                .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
                as i64)
                .abs()
                .wrapping_neg(),
            headline: format!("SEC {}: {}", f.form, f.title),
            author: "SEC EDGAR".into(),
            created_at: f.updated.clone(),
            updated_at: f.updated.clone(),
            symbols: vec![ticker.clone()],
            source: "sec-edgar".into(),
            url: f.url.clone(),
        };
        tracing::info!(
            component = LOOP_NAME,
            symbol = %ticker,
            form = %f.form,
            "new EDGAR filing → news cache"
        );
        super::alpaca_news::global_cache().push_item(ticker, item).await;
    }
}

#[cfg(test)]
#[path = "edgar_tests.rs"]
mod tests;
