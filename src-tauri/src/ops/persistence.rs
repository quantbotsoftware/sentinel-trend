use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::time::Duration;

use crate::config::RuntimeConfig;
use crate::error::TitanResult;
use crate::paths;
use crate::state::{SmartTrendMarker, StateSnapshot, SymbolFsm, TitanState};

// ─── Generic atomic save/load ───────────────────────────────────────

/// Atomic write: data -> tmp file -> backup existing -> rename tmp to target.
pub fn save_atomic<T: Serialize>(path: &Path, data: &T) -> TitanResult<()> {
    let json = serde_json::to_string_pretty(data)?;

    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;

    if path.exists() {
        let bak = path.with_extension("json.bak");
        if let Err(e) = fs::copy(path, &bak) {
            tracing::warn!("Failed to create backup {}: {e}", bak.display());
        }
    }

    fs::rename(&tmp, path)?;
    tracing::debug!("Saved {} ({} bytes)", path.display(), json.len());
    Ok(())
}

/// Atomic write for hash sidecar files (write tmp → rename).
pub(crate) fn save_hash_atomic(hash_path: &Path, hash: &str) -> std::io::Result<()> {
    let tmp = hash_path.with_extension("hash.tmp");
    fs::write(&tmp, hash)?;
    fs::rename(&tmp, hash_path)?;
    Ok(())
}

/// Load with .bak fallback. Returns Default if both fail.
pub fn load_with_fallback<T: DeserializeOwned + Default>(path: &Path) -> T {
    if let Some(val) = try_load::<T>(path) {
        return val;
    }

    let bak = path.with_extension("json.bak");
    if bak.exists() {
        tracing::warn!(
            "Primary file corrupt/missing, trying backup: {}",
            bak.display()
        );
        if let Some(val) = try_load::<T>(&bak) {
            return val;
        }
    }

    tracing::info!(
        "No valid file at {}, using defaults",
        path.display()
    );
    T::default()
}

fn try_load<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let data = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&data) {
        Ok(val) => Some(val),
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            None
        }
    }
}

// ─── Config-specific ────────────────────────────────────────────────

fn config_path() -> PathBuf {
    paths::config_dir().join("settings.json")
}

pub fn save_config(state: &TitanState) -> TitanResult<()> {
    let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner()).clone();
    save_atomic(&config_path(), &cfg)?;

    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
    sys.last_persist_ts = Some(chrono::Utc::now());
    Ok(())
}

pub fn load_config() -> RuntimeConfig {
    let mut cfg: RuntimeConfig = load_with_fallback(&config_path());

    // v2.5.0 — defensive scrub: any symbol that appears in
    // `market.excluded_symbols` (operator-banned phantom tickers like
    // Alpaca's stranded `TRUE` asset) must NEVER reach the favorites
    // list, otherwise the streamer keeps a websocket subscription
    // open for it, the auto-universe re-loads it on every startup,
    // and the UI shows a permanent slot for an un-tradeable name.
    // Filter on load + warn so the operator sees the cleanup.
    if !cfg.market.excluded_symbols.is_empty() {
        let before = cfg.ui.favorites.len();
        let excluded = cfg.market.excluded_symbols.clone();
        cfg.ui.favorites.retain(|fav| {
            !crate::config::is_symbol_excluded(&excluded, fav)
        });
        let removed = before - cfg.ui.favorites.len();
        if removed > 0 {
            tracing::warn!(
                component = "config",
                removed_count = removed,
                excluded_list = ?excluded,
                "Stripped excluded symbols from favorites on load"
            );
        }
    }

    let errors = cfg.validate();
    if !errors.is_empty() {
        tracing::warn!("Loaded config has validation warnings:");
        for e in &errors {
            tracing::warn!("  - {e}");
        }
    }

    cfg
}

// ─── Snapshot with hash-skip (Bonus 3) ──────────────────────────────

fn snapshot_path() -> PathBuf {
    paths::snapshots_dir().join("state_snapshot.json")
}

fn snapshot_hash_path() -> PathBuf {
    paths::snapshots_dir().join("state_snapshot.hash")
}

pub(crate) fn content_hash(data: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    h.write(data);
    format!("{:016x}", h.finish())
}

/// Save snapshot, skipping write if content hash is unchanged.
pub fn save_snapshot(state: &TitanState) -> TitanResult<()> {
    let snap = state.snapshot();
    let json = serde_json::to_string_pretty(&snap)?;
    let hash = content_hash(json.as_bytes());

    let hash_path = snapshot_hash_path();
    if let Ok(prev_hash) = fs::read_to_string(&hash_path) {
        if prev_hash.trim() == hash {
            tracing::debug!("Snapshot unchanged, skipping write");
            return Ok(());
        }
    }

    let path = snapshot_path();
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;

    if path.exists() {
        let bak = path.with_extension("json.bak");
        if let Err(e) = fs::copy(&path, &bak) {
            tracing::warn!("Failed to create snapshot backup: {e}");
        }
    }

    fs::rename(&tmp, &path)?;
    save_hash_atomic(&hash_path, &hash)?;

    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
    sys.last_persist_ts = Some(chrono::Utc::now());

    tracing::debug!("Snapshot saved ({} bytes, hash={hash})", json.len());
    Ok(())
}

pub fn load_snapshot() -> Option<StateSnapshot> {
    try_load::<StateSnapshot>(&snapshot_path()).or_else(|| {
        let bak = snapshot_path().with_extension("json.bak");
        if bak.exists() {
            tracing::warn!("Trying snapshot backup");
            try_load::<StateSnapshot>(&bak)
        } else {
            None
        }
    })
}

// ─── FSM state persistence (Session 6) ──────────────────────────────

fn fsm_path() -> PathBuf {
    paths::snapshots_dir().join("fsm_states.json")
}

fn fsm_hash_path() -> PathBuf {
    paths::snapshots_dir().join("fsm_states.hash")
}

/// Lightweight FSM-only save with content-hash skip.
/// Called by sentinel on phase transitions and periodically.
pub fn save_fsm_states(state: &TitanState) -> TitanResult<()> {
    let fsm_map = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.fsm.clone()
    };

    let json = serde_json::to_string_pretty(&fsm_map)?;
    let hash = content_hash(json.as_bytes());

    let hash_file = fsm_hash_path();
    if let Ok(prev) = fs::read_to_string(&hash_file) {
        if prev.trim() == hash {
            tracing::debug!("FSM states unchanged, skipping write");
            return Ok(());
        }
    }

    save_atomic(&fsm_path(), &fsm_map)?;
    save_hash_atomic(&hash_file, &hash)?;
    tracing::debug!("FSM states saved ({} symbols, hash={hash})", fsm_map.len());
    Ok(())
}

/// Load FSM states from disk. Returns empty HashMap if missing/corrupt.
pub fn load_fsm_states() -> HashMap<String, SymbolFsm> {
    load_with_fallback(&fsm_path())
}

// ─── Marker persistence ─────────────────────────────────────────────

fn markers_path() -> PathBuf {
    paths::snapshots_dir().join("markers.json")
}

fn markers_hash_path() -> PathBuf {
    paths::snapshots_dir().join("markers.hash")
}

// S35.6 (2026-05-28 16:50 ET) — retention extended from 7 → 365 days
// and per-symbol cap raised from 200 → 5000. Pre-fix, every bot
// restart silently purged all markers older than 1 week, so on
// slow TFs (4h: latest marker Dec 2025, 1h: latest March 2026) the
// chart appeared completely empty after restart even though the
// FSM had legitimately recorded those transitions on disk. With
// 365-day retention plus the 5000-per-symbol cap (matching the
// in-memory MAX_MARKERS in dispatch.rs), Tony's manual-reference
// chart preserves a full year of FSM signals across all TFs.
//
// Disk impact: ~700 bytes per marker × 5000 × N symbols.
// For QBTS-only setup ≈ 3.5 MB. Acceptable.
const MARKER_RETENTION_SECS: i64 = 365 * 86400;
const MAX_MARKERS_PER_SYMBOL: usize = 5000;

pub fn save_markers(state: &TitanState) -> TitanResult<()> {
    let markers = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.markers.clone()
    };

    let json = serde_json::to_string_pretty(&markers)?;
    let hash = content_hash(json.as_bytes());

    let hash_file = markers_hash_path();
    if let Ok(prev) = fs::read_to_string(&hash_file) {
        if prev.trim() == hash {
            tracing::debug!("Markers unchanged, skipping write");
            return Ok(());
        }
    }

    save_atomic(&markers_path(), &markers)?;
    save_hash_atomic(&hash_file, &hash)?;
    tracing::debug!("Markers saved ({} total, hash={hash})", markers.len());
    Ok(())
}

pub fn load_markers() -> Vec<SmartTrendMarker> {
    let mut markers: Vec<SmartTrendMarker> = load_with_fallback(&markers_path());

    let cutoff = chrono::Utc::now().timestamp() - MARKER_RETENTION_SECS;
    markers.retain(|m| m.bar_time >= cutoff);

    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut kept = Vec::with_capacity(markers.len());
    for m in markers.into_iter().rev() {
        let count = counts.entry(m.symbol.clone()).or_insert(0);
        if *count < MAX_MARKERS_PER_SYMBOL {
            *count += 1;
            kept.push(m);
        }
    }
    kept.reverse();
    kept
}

// ─── Persistence loop ────────────────────────────────────────────────

const PERSIST_INTERVAL_SECS: u64 = 30;

/// Background loop: save snapshot + FSM states every 30s (hash-skip avoids redundant writes).
pub async fn run_persistence_loop(state: Arc<TitanState>) {
    tracing::info!(component = "persistence", "Persistence loop started");

    let mut ticker = tokio::time::interval(Duration::from_secs(PERSIST_INTERVAL_SECS));

    loop {
        ticker.tick().await;

        if state.shutdown.is_cancelled() {
            break;
        }

        {
            let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = sys.loops.get_mut("persistence") {
                h.tick();
            }
        }

        if let Err(e) = save_fsm_states(&state) {
            tracing::error!(component = "persistence", "FSM save failed: {e}");
        }

        if let Err(e) = save_markers(&state) {
            tracing::error!(component = "persistence", "Markers save failed: {e}");
        }

        if let Err(e) = save_snapshot(&state) {
            tracing::error!(component = "persistence", "Snapshot save failed: {e}");
        }

        if let Err(e) = crate::ops::journal::save_journal(&state) {
            tracing::error!(component = "persistence", "Journal save failed: {e}");
        }
    }

    tracing::info!(component = "persistence", "Persistence loop exiting");
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .join("sentinel_trend_test")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("test.json");

        let cfg = RuntimeConfig::default();
        save_atomic(&path, &cfg).unwrap();

        let loaded: RuntimeConfig = load_with_fallback(&path);
        assert!((loaded.smarttrend.risk_per_trade - 0.01).abs() < f64::EPSILON);
        assert_eq!(loaded.ui.active_symbol, "QBTS");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_falls_back_to_bak() {
        let dir = temp_dir();
        let path = dir.join("test.json");

        let cfg = RuntimeConfig::default();
        save_atomic(&path, &cfg).unwrap();

        // Create a second save so .bak exists with valid data
        let mut cfg2 = RuntimeConfig::default();
        cfg2.ui.active_symbol = "TSLA".to_string();
        save_atomic(&path, &cfg2).unwrap();

        // Corrupt the primary file
        fs::write(&path, "NOT VALID JSON {{{{").unwrap();

        let loaded: RuntimeConfig = load_with_fallback(&path);
        // .bak has the first save's data (default chart anchor)
        assert_eq!(loaded.ui.active_symbol, "QBTS");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = temp_dir();
        let path = dir.join("nonexistent.json");

        let loaded: RuntimeConfig = load_with_fallback(&path);
        // S19.1 default for $300 paper account.
        assert_eq!(loaded.execution.max_positions, 40);
        assert!(loaded.market.paper_mode);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bak_created_on_overwrite() {
        let dir = temp_dir();
        let path = dir.join("test.json");

        let cfg = RuntimeConfig::default();
        save_atomic(&path, &cfg).unwrap();

        let mut cfg2 = RuntimeConfig::default();
        cfg2.ui.active_symbol = "GOOG".to_string();
        save_atomic(&path, &cfg2).unwrap();

        let bak = path.with_extension("json.bak");
        assert!(bak.exists(), ".bak file should exist after overwrite");

        let bak_cfg: RuntimeConfig =
            serde_json::from_str(&fs::read_to_string(&bak).unwrap()).unwrap();
        assert_eq!(bak_cfg.ui.active_symbol, "QBTS");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn content_hash_deterministic() {
        let data = b"hello world";
        let h1 = content_hash(data);
        let h2 = content_hash(data);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn fsm_save_load_roundtrip() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            st.fsm.insert("AAPL".to_string(), crate::state::SymbolFsm {
                phase: crate::state::FsmPhase::Stalking,
                score: 58.0,
                confirm_count: 0,
                entry_price: None,
                entry_time: None,
                prev_close: 100.0,
                ..Default::default()
            });
            st.fsm.insert("MSFT".to_string(), crate::state::SymbolFsm {
                phase: crate::state::FsmPhase::Active,
                score: 72.0,
                confirm_count: 3,
                entry_price: Some(350.0),
                entry_time: Some(1700000000),
                prev_close: 349.0,
                ..Default::default()
            });
        }

        let fsm_map = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner()).fsm.clone();
        let json = serde_json::to_string_pretty(&fsm_map).unwrap();
        let loaded: HashMap<String, crate::state::SymbolFsm> =
            serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded.get("AAPL").unwrap().phase,
            crate::state::FsmPhase::Stalking
        );
        assert_eq!(
            loaded.get("MSFT").unwrap().phase,
            crate::state::FsmPhase::Active
        );
    }

    #[test]
    fn snapshot_roundtrip() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
            market.active_symbol = "MSFT".to_string();
        }

        let snap = state.snapshot();
        let json = serde_json::to_string_pretty(&snap).unwrap();
        let loaded: StateSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.active_symbol, "MSFT");
        assert_eq!(loaded.session_id, state.session_id);
    }

    #[test]
    fn save_hash_atomic_uses_tmp_rename() {
        let dir = temp_dir();
        let hash_path = dir.join("test.hash");
        let hash_content = "abc123deadbeef";

        save_hash_atomic(&hash_path, hash_content).unwrap();

        assert!(hash_path.exists(), "hash file must exist after write");
        assert!(
            !dir.join("test.hash.tmp").exists(),
            "tmp file must not linger after atomic rename"
        );
        let read_back = fs::read_to_string(&hash_path).unwrap();
        assert_eq!(read_back, hash_content);

        fs::remove_dir_all(&dir).ok();
    }
}
