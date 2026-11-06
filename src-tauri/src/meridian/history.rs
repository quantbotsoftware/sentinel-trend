//! Incremental 1-min history cache for the MERIDIAN symbols.
//!
//! One JSONL file per symbol under `<data_dir>/meridian/live/<SYM>.jsonl`,
//! same Alpaca bar shape the research archive uses, so the live replay
//! reads its data through the very same loader as the backtest
//! (`titan_meridian::data::load_jsonl_days_live`) — twin by construction.
//!
//! `sync` appends only bars newer than the last cached timestamp; the
//! first run backfills `history_days`.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

const BARS_URL: &str = "https://data.alpaca.markets/v2/stocks/bars";

pub fn cache_dir() -> PathBuf {
    crate::paths::data_dir().join("meridian").join("live")
}

pub fn cache_path(symbol: &str) -> PathBuf {
    cache_dir().join(format!("{}.jsonl", symbol.to_uppercase()))
}

#[derive(Debug, Deserialize)]
struct RawBar {
    t: String,
}

/// Timestamp of the last bar in the cache file (scans the tail only).
pub fn last_cached_ts(path: &Path) -> Option<DateTime<Utc>> {
    let data = std::fs::read(path).ok()?;
    let tail_start = data.len().saturating_sub(4096);
    let tail = std::str::from_utf8(&data[tail_start..]).ok()?;
    tail.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<RawBar>(l).ok())
        .filter_map(|b| DateTime::parse_from_rfc3339(&b.t).ok())
        .map(|t| t.with_timezone(&Utc))
        .next()
}

#[derive(Debug, Deserialize)]
struct BarsPage {
    #[serde(default)]
    bars: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// Append bars in `(start, now]` for `symbol` to its cache. Returns the
/// number of bars appended. Network errors abort the sync (partial pages
/// already written are fine: the next sync resumes from the last bar).
pub async fn sync(client: &reqwest::Client, key: &str, secret: &str, symbol: &str, history_days: u32) -> Result<usize, String> {
    let path = cache_path(symbol);
    std::fs::create_dir_all(cache_dir()).map_err(|e| e.to_string())?;
    let start = match last_cached_ts(&path) {
        Some(t) => t + Duration::minutes(1),
        None => Utc::now() - Duration::days(history_days as i64),
    };
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    let mut token: Option<String> = None;
    let mut appended = 0usize;
    loop {
        let mut req = client
            .get(BARS_URL)
            .header("APCA-API-KEY-ID", key)
            .header("APCA-API-SECRET-KEY", secret)
            .query(&[
                ("symbols", symbol),
                ("timeframe", "1Min"),
                ("start", &start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                ("limit", "10000"),
                ("feed", "sip"),
                ("adjustment", "split"),
                ("sort", "asc"),
            ]);
        if let Some(t) = &token {
            req = req.query(&[("page_token", t)]);
        }
        let resp = req.send().await.map_err(|e| format!("bars fetch: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("bars fetch: HTTP {}", resp.status()));
        }
        let page: serde_json::Value = resp.json().await.map_err(|e| format!("bars parse: {e}"))?;
        // multi-symbol shape: {"bars": {"SPY": [...]}, "next_page_token": ..}
        let next = page.get("next_page_token").and_then(|v| v.as_str()).map(|s| s.to_string());
        let bars = page.get("bars").and_then(|b| b.get(symbol)).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for b in &bars {
            writeln!(file, "{b}").map_err(|e| e.to_string())?;
            appended += 1;
        }
        let _ = BarsPage { bars: None, next_page_token: None }; // shape documentation only
        match next {
            Some(t) if !bars.is_empty() => token = Some(t),
            _ => break,
        }
    }
    Ok(appended)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_cached_ts_reads_tail() {
        let p = std::env::temp_dir().join(format!("meridian_hist_{}.jsonl", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap_or_else(|e| panic!("{e}"));
        for m in 0..500 {
            writeln!(f, r#"{{"t":"2026-09-03T13:{:02}:00Z","o":1,"h":1,"l":1,"c":1,"v":1}}"#, 30 + m % 30).unwrap_or_else(|e| panic!("{e}"));
        }
        writeln!(f, r#"{{"t":"2026-09-03T19:59:00Z","o":1,"h":1,"l":1,"c":1,"v":1}}"#).unwrap_or_else(|e| panic!("{e}"));
        writeln!(f, "garbage").unwrap_or_else(|e| panic!("{e}"));
        let t = last_cached_ts(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(t.map(|t| t.to_rfc3339()), Some("2026-09-03T19:59:00+00:00".to_string()));
        assert!(last_cached_ts(Path::new("/nonexistent/x.jsonl")).is_none());
    }

    #[test]
    fn cache_path_is_uppercase_under_data_dir() {
        let p = cache_path("spy");
        assert!(p.ends_with("meridian/live/SPY.jsonl"), "{p:?}");
    }
}
