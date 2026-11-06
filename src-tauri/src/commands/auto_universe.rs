//! S18 — Tauri commands for the Self-Curating Watchlist (AutoUniverse).
//!
//! Each command emits an `auto-universe-state` event after a successful
//! mutation so the frontend can re-render without polling. The state
//! is also persisted via the regular snapshot loop, but mutations
//! happen immediately in memory and are visible in `active_set()` on
//! the very next streamer / watchlist tick.

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::market::auto_universe::{AutoUniverse, AutoUniverseConfig};
use crate::state::TitanState;

/// Snapshot payload returned by `get_auto_universe_state` and emitted
/// on `auto-universe-state` after every mutation.
#[derive(Debug, Clone, Serialize)]
pub struct AutoUniverseSnapshot {
    pub enabled: bool,
    pub pinned: Vec<String>,
    pub auto_pool: Vec<crate::market::auto_universe::AutoEntry>,
    pub blacklist: Vec<crate::market::auto_universe::BlacklistEntry>,
    pub max_total_slots: usize,
    pub max_pinned_slots: usize,
}

fn snapshot_from(universe: &AutoUniverse, cfg: &AutoUniverseConfig) -> AutoUniverseSnapshot {
    let mut auto_pool: Vec<_> = universe.auto_pool.values().cloned().collect();
    auto_pool.sort_by(|a, b| {
        b.last_score
            .partial_cmp(&a.last_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut blacklist: Vec<_> = universe.blacklist.values().cloned().collect();
    blacklist.sort_by_key(|b| b.banned_at);
    AutoUniverseSnapshot {
        enabled: cfg.enabled,
        pinned: universe.pinned.clone(),
        auto_pool,
        blacklist,
        max_total_slots: cfg.max_total_slots,
        max_pinned_slots: cfg.max_pinned_slots,
    }
}

fn read_snapshot(state: &TitanState) -> AutoUniverseSnapshot {
    let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
    let au_cfg = cfg.auto_universe.clone();
    drop(cfg);
    let market = state.market.lock().unwrap_or_else(|p| p.into_inner());
    snapshot_from(&market.auto_universe, &au_cfg)
}

fn emit_state(app: &AppHandle, state: &TitanState) {
    let snap = read_snapshot(state);
    let _ = app.emit_all("auto-universe-state", &snap);
}

// ─── Read ───────────────────────────────────────────────────────────

#[tauri::command]
pub fn get_auto_universe_state(
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<AutoUniverseSnapshot, String> {
    Ok(read_snapshot(&state))
}

// ─── Master enable / disable ────────────────────────────────────────

#[tauri::command]
pub fn set_auto_universe_enabled(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
    enabled: bool,
) -> Result<bool, String> {
    {
        let mut cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.auto_universe.enabled = enabled;
    }
    if let Err(e) = crate::ops::persistence::save_config(&state) {
        tracing::error!(component = "auto_universe", "save_config failed: {e}");
    }
    tracing::info!(
        component = "auto_universe",
        enabled,
        "AutoUniverse master switch toggled"
    );
    emit_state(&app, &state);
    Ok(enabled)
}

// ─── Pin / Unpin (user-driven) ──────────────────────────────────────

#[tauri::command]
pub fn pin_symbol(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
    symbol: String,
) -> Result<String, String> {
    let cfg = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.auto_universe.clone()
    };
    {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        market.auto_universe.pin(&symbol, &cfg)?;
    }
    emit_state(&app, &state);
    Ok(format!("{} pinned", symbol.trim().to_uppercase()))
}

#[tauri::command]
pub fn unpin_symbol(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
    symbol: String,
) -> Result<String, String> {
    {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        market.auto_universe.unpin(&symbol)?;
    }
    emit_state(&app, &state);
    Ok(format!("{} unpinned", symbol.trim().to_uppercase()))
}

#[tauri::command]
pub fn promote_auto_to_pinned(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
    symbol: String,
) -> Result<String, String> {
    let cfg = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.auto_universe.clone()
    };
    {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        market.auto_universe.promote_auto_to_pinned(&symbol, &cfg)?;
    }
    emit_state(&app, &state);
    Ok(format!(
        "{} promoted to pinned",
        symbol.trim().to_uppercase()
    ))
}

#[tauri::command]
pub fn force_remove_universe_symbol(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
    symbol: String,
) -> Result<String, String> {
    {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        market.auto_universe.force_remove(&symbol)?;
    }
    emit_state(&app, &state);
    Ok(format!(
        "{} removed from universe",
        symbol.trim().to_uppercase()
    ))
}

// ─── Blacklist (manual override) ────────────────────────────────────

#[tauri::command]
pub fn manual_blacklist_symbol(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
    symbol: String,
    ttl_hours: Option<u32>,
) -> Result<String, String> {
    let cfg = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.auto_universe.clone()
    };
    {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        market
            .auto_universe
            .manual_blacklist(&symbol, &cfg, ttl_hours, chrono::Utc::now());
    }
    emit_state(&app, &state);
    Ok(format!(
        "{} manually blacklisted",
        symbol.trim().to_uppercase()
    ))
}

#[tauri::command]
pub fn clear_blacklist_entry(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
    symbol: String,
) -> Result<String, String> {
    {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        market.auto_universe.clear_blacklist_entry(&symbol);
    }
    emit_state(&app, &state);
    Ok(format!(
        "{} blacklist entry cleared",
        symbol.trim().to_uppercase()
    ))
}

#[tauri::command]
pub fn clear_all_blacklist(
    state: tauri::State<'_, Arc<TitanState>>,
    app: AppHandle,
) -> Result<String, String> {
    let cleared = {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        let n = market.auto_universe.blacklist.len();
        market.auto_universe.clear_all_blacklist();
        n
    };
    emit_state(&app, &state);
    Ok(format!("Cleared {cleared} blacklist entr(y/ies)"))
}
