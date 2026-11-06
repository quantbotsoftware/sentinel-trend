use std::collections::HashMap;
use std::fs;
use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};

const TTL_SECS: i64 = 7 * 24 * 3600; // 7 days

// ─── Types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatEntry {
    pub float_shares: f64,
    pub fetched_at: i64,
}

pub struct FloatCache {
    entries: HashMap<String, FloatEntry>,
    dirty: bool,
}

// ─── FMP shares-float parse helper ─────────────────────────────────────

/// Response item from /stable/shares-float
#[derive(Debug, Deserialize)]
struct FmpSharesFloatItem {
    #[serde(rename = "floatShares")]
    float_shares: Option<f64>,
}

// ─── FloatCache impl ─────────────────────────────────────────────────

impl FloatCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            dirty: false,
        }
    }

    /// Load from JSON file with .bak fallback (mirrors persistence.rs pattern).
    pub fn load(path: &Path) -> Self {
        if let Some(entries) = try_load_entries(path) {
            return Self { entries, dirty: false };
        }

        let bak = path.with_extension("json.bak");
        if bak.exists() {
            tracing::warn!("Primary float cache corrupt/missing, trying backup: {}", bak.display());
            if let Some(entries) = try_load_entries(&bak) {
                return Self { entries, dirty: false };
            }
        }

        tracing::info!("No valid float cache at {}, starting empty", path.display());
        Self::new()
    }

    /// Atomic save: tmp -> backup existing -> rename. Skips if not dirty.
    pub fn save(&mut self, path: &Path) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        let json = serde_json::to_string_pretty(&self.entries)
            .map_err(std::io::Error::other)?;

        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;

        if path.exists() {
            let bak = path.with_extension("json.bak");
            if let Err(e) = fs::copy(path, &bak) {
                tracing::warn!("Failed to create float cache backup {}: {e}", bak.display());
            }
        }

        fs::rename(&tmp, path)?;
        self.dirty = false;
        tracing::debug!("Saved float cache {} ({} bytes)", path.display(), json.len());
        Ok(())
    }

    /// Returns float_shares if present and within TTL (7 days).
    pub fn get(&self, symbol: &str) -> Option<f64> {
        let entry = self.entries.get(symbol)?;
        let age = Utc::now().timestamp() - entry.fetched_at;
        if age < TTL_SECS {
            Some(entry.float_shares)
        } else {
            None
        }
    }

    pub fn insert(&mut self, symbol: String, float_shares: f64) {
        self.entries.insert(
            symbol,
            FloatEntry {
                float_shares,
                fetched_at: Utc::now().timestamp(),
            },
        );
        self.dirty = true;
    }

    /// True if symbol is missing or TTL expired.
    pub fn needs_fetch(&self, symbol: &str) -> bool {
        self.get(symbol).is_none()
    }
}

fn try_load_entries(path: &Path) -> Option<HashMap<String, FloatEntry>> {
    let data = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&data) {
        Ok(val) => Some(val),
        Err(e) => {
            tracing::warn!("Failed to parse float cache {}: {e}", path.display());
            None
        }
    }
}

// ─── Async FMP fetch (standalone) ────────────────────────────────────

pub async fn fetch_float_fmp(
    client: &reqwest::Client,
    api_key: &str,
    symbol: &str,
) -> Option<f64> {
    if api_key.is_empty() {
        return None;
    }

    // SECURITY: FMP requires apikey in query string. Never log this URL directly;
    // use redact_url() if logging is needed for debugging.
    let url = format!(
        "https://financialmodelingprep.com/stable/shares-float?symbol={symbol}&apikey={api_key}"
    );

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<Vec<FmpSharesFloatItem>>().await {
                Ok(items) => {
                    let float = items.first().and_then(|i| i.float_shares);
                    if float.is_none() {
                        tracing::warn!(symbol, "FMP shares-float returned no floatShares");
                    } else {
                        tracing::debug!(symbol, shares = ?float, "FMP shares-float OK");
                    }
                    float
                }
                Err(e) => {
                    tracing::warn!(symbol, "FMP shares-float parse error: {e}");
                    None
                }
            }
        }
        Ok(resp) => {
            tracing::warn!(symbol, status = %resp.status(), "FMP shares-float request failed");
            None
        }
        Err(e) => {
            tracing::warn!(symbol, "FMP shares-float request error: {e}");
            None
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .join("sentinel_trend_test")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn new_cache_is_empty() {
        let cache = FloatCache::new();
        assert!(cache.get("AAPL").is_none());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn insert_and_get() {
        let mut cache = FloatCache::new();
        cache.insert("AAPL".into(), 5_000_000.0);
        assert_eq!(cache.get("AAPL"), Some(5_000_000.0));
    }

    #[test]
    fn get_expired_returns_none() {
        let mut cache = FloatCache::new();
        cache.entries.insert(
            "OLD".into(),
            FloatEntry {
                float_shares: 1_000_000.0,
                fetched_at: Utc::now().timestamp() - TTL_SECS - 1,
            },
        );
        assert!(cache.get("OLD").is_none());
    }

    #[test]
    fn needs_fetch_missing_symbol() {
        let cache = FloatCache::new();
        assert!(cache.needs_fetch("MISSING"));
    }

    #[test]
    fn needs_fetch_fresh_symbol() {
        let mut cache = FloatCache::new();
        cache.insert("FRESH".into(), 2_000_000.0);
        assert!(!cache.needs_fetch("FRESH"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("float_cache.json");

        let mut cache = FloatCache::new();
        cache.insert("AAPL".into(), 5_000_000.0);
        cache.insert("TSLA".into(), 3_000_000.0);
        cache.save(&path).unwrap();

        let loaded = FloatCache::load(&path);
        assert_eq!(loaded.get("AAPL"), Some(5_000_000.0));
        assert_eq!(loaded.get("TSLA"), Some(3_000_000.0));

        fs::remove_dir_all(&dir).ok();
    }
}
