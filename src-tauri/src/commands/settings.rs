use std::sync::Arc;

use serde::Serialize;
use tauri::Manager;

use crate::config::{
    ExecutionConfig, MarketConfig, RuntimeConfig, SmartTrendConfig, UIConfig,
};
use crate::error::TitanError;
use crate::market::scanner::ScanParams;
use crate::ops;
use crate::state::TitanState;

// ─── Config-changed event payload ───────────────────────────────────

#[derive(Clone, Serialize)]
struct ConfigChangedEvent {
    source: String,
}

// ─── Public (redacted) DTO ──────────────────────────────────────────

fn redact(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        "***".into()
    }
}

#[derive(Clone, Serialize)]
pub struct MarketConfigPublic {
    pub alpaca_key_id: String,
    pub alpaca_secret: String,
    pub paper_mode: bool,
    pub sip_feed: bool,
    pub discovery_poll_secs: u64,
    pub stale_data_secs: u64,
    pub fmp_api_key: String,
    pub scan_params: ScanParams,
    pub auto_scan_active: bool,
}

impl From<&MarketConfig> for MarketConfigPublic {
    fn from(m: &MarketConfig) -> Self {
        Self {
            alpaca_key_id: redact(&m.alpaca_key_id),
            alpaca_secret: redact(&m.alpaca_secret),
            paper_mode: m.paper_mode,
            sip_feed: m.sip_feed,
            discovery_poll_secs: m.discovery_poll_secs,
            stale_data_secs: m.stale_data_secs,
            fmp_api_key: redact(&m.fmp_api_key),
            scan_params: m.scan_params.clone(),
            auto_scan_active: m.auto_scan_active,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct RuntimeConfigPublic {
    pub smarttrend: SmartTrendConfig,
    pub execution: ExecutionConfig,
    pub ui: UIConfig,
    pub market: MarketConfigPublic,
    /// APEX-only mode (2026-07-04). The UI hides the legacy
    /// TOURNAMENT / D-PLUS / OMEGA tabs when this is set — those
    /// subsystems no longer originate live orders.
    pub apex_only: bool,
}

impl From<&RuntimeConfig> for RuntimeConfigPublic {
    fn from(cfg: &RuntimeConfig) -> Self {
        Self {
            smarttrend: cfg.smarttrend.clone(),
            execution: cfg.execution.clone(),
            ui: cfg.ui.clone(),
            market: MarketConfigPublic::from(&cfg.market),
            apex_only: cfg.evolution_protocol.apex_only,
        }
    }
}

// ─── Secret-preserving helper ───────────────────────────────────────

fn preserve_secrets(new_config: &mut RuntimeConfig, prev: &RuntimeConfig) {
    if new_config.market.alpaca_key_id == "***" {
        new_config.market.alpaca_key_id = prev.market.alpaca_key_id.clone();
    }
    if new_config.market.alpaca_secret == "***" {
        new_config.market.alpaca_secret = prev.market.alpaca_secret.clone();
    }
    if new_config.market.fmp_api_key == "***" {
        new_config.market.fmp_api_key = prev.market.fmp_api_key.clone();
    }
    // Preserve the entire evolution protocol configuration since the UI does not expose or modify it.
    new_config.evolution_protocol = prev.evolution_protocol.clone();
}

// ─── Commands ───────────────────────────────────────────────────────

#[tauri::command]
pub fn get_config(state: tauri::State<'_, Arc<TitanState>>) -> Result<RuntimeConfigPublic, String> {
    let cfg = state.config.lock().map_err(|e| format!("lock: {e}"))?;
    Ok(RuntimeConfigPublic::from(&*cfg))
}

#[tauri::command]
pub fn set_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<TitanState>>,
    mut new_config: RuntimeConfig,
) -> Result<(), String> {
    {
        let prev = state.config.lock().map_err(|e| format!("lock: {e}"))?;
        preserve_secrets(&mut new_config, &prev);
    }

    let (errors, warnings) = collect_validation_issues(&new_config);
    if !errors.is_empty() {
        return Err(TitanError::Validation(errors.join("; ")).to_string());
    }
    log_soft_warnings("set_config", &warnings);

    let mut cfg_guard = state.config.lock().map_err(|e| format!("lock: {e}"))?;
    let prev_cfg = cfg_guard.clone();
    *cfg_guard = new_config;
    drop(cfg_guard);

    if let Err(e) = ops::persistence::save_config(&state) {
        tracing::error!("Failed to persist config: {e}");
        *state.config.lock().map_err(|pe| format!("lock: {pe}"))? = prev_cfg;
        return Err(e.to_string());
    }

    let _ = app.emit_all(
        "config-changed",
        ConfigChangedEvent {
            source: "set_config".into(),
        },
    );

    tracing::info!("Config updated and saved");
    Ok(())
}

#[tauri::command]
pub fn reset_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<RuntimeConfigPublic, String> {
    let default = RuntimeConfig::default();
    let public = RuntimeConfigPublic::from(&default);
    *state.config.lock().map_err(|e| format!("lock: {e}"))? = default;

    if let Err(e) = ops::persistence::save_config(&state) {
        tracing::error!("Failed to persist reset config: {e}");
        return Err(e.to_string());
    }

    let _ = app.emit_all(
        "config-changed",
        ConfigChangedEvent {
            source: "reset_config".into(),
        },
    );

    tracing::info!("Config reset to defaults");
    Ok(public)
}

#[tauri::command]
pub fn set_api_keys(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<TitanState>>,
    alpaca_key_id: Option<String>,
    alpaca_secret: Option<String>,
    fmp_api_key: Option<String>,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().map_err(|e| format!("lock: {e}"))?;
        if let Some(v) = alpaca_key_id {
            if v != "***" {
                cfg.market.alpaca_key_id = v;
            }
        }
        if let Some(v) = alpaca_secret {
            if v != "***" {
                cfg.market.alpaca_secret = v;
            }
        }
        if let Some(v) = fmp_api_key {
            if v != "***" {
                cfg.market.fmp_api_key = v;
            }
        }
    }

    if let Err(e) = ops::persistence::save_config(&state) {
        tracing::error!("Failed to persist API keys: {e}");
        return Err(e.to_string());
    }

    let _ = app.emit_all(
        "config-changed",
        ConfigChangedEvent {
            source: "set_api_keys".into(),
        },
    );

    tracing::info!("API keys updated (redacted)");
    Ok(())
}

#[tauri::command]
pub fn validate_config(
    state: tauri::State<'_, Arc<TitanState>>,
    candidate_config: Option<RuntimeConfig>,
) -> Result<Vec<String>, String> {
    let current = state.config.lock().map_err(|e| format!("lock: {e}"))?.clone();
    let target = select_validation_target(current, candidate_config);
    let (errors, warnings) = collect_validation_issues(&target);
    log_soft_warnings("validate_config", &warnings);
    Ok(errors)
}

fn select_validation_target(
    current: RuntimeConfig,
    candidate_config: Option<RuntimeConfig>,
) -> RuntimeConfig {
    candidate_config.unwrap_or(current)
}

fn collect_validation_issues(config: &RuntimeConfig) -> (Vec<String>, Vec<String>) {
    let hard = config.validate_hard_user_facing();
    (hard, Vec::new())
}

fn log_soft_warnings(source: &str, warnings: &[String]) {
    for warning in warnings {
        tracing::warn!(source = source, warning = %warning, "Legacy config warning");
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_changed_event_serializes() {
        let evt = ConfigChangedEvent {
            source: "set_config".into(),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("set_config"));
    }

    #[test]
    fn select_validation_target_prefers_candidate() {
        let mut current = RuntimeConfig::default();
        current.smarttrend.risk_per_trade = 0.01;

        let mut candidate = RuntimeConfig::default();
        candidate.smarttrend.risk_per_trade = 0.02;

        let selected = select_validation_target(current, Some(candidate.clone()));
        assert!((selected.smarttrend.risk_per_trade - 0.02).abs() < f64::EPSILON);
    }

    #[test]
    fn select_validation_target_uses_current_when_candidate_missing() {
        let mut current = RuntimeConfig::default();
        current.smarttrend.risk_per_trade = 0.03;

        let selected = select_validation_target(current.clone(), None);
        assert!((selected.smarttrend.risk_per_trade - 0.03).abs() < f64::EPSILON);
    }

    #[test]
    fn collect_validation_issues_hard_blocks_invalid_user_fields() {
        let mut cfg = RuntimeConfig::default();
        cfg.smarttrend.risk_per_trade = 0.5;

        let (hard, _soft) = collect_validation_issues(&cfg);
        assert!(hard.iter().any(|e| e.contains("risk_per_trade")));
    }

    #[test]
    fn redact_masks_nonempty_and_preserves_empty() {
        assert_eq!(redact("my-secret-key"), "***");
        assert_eq!(redact(""), "");
    }

    #[test]
    fn runtime_config_public_redacts_secrets() {
        let mut cfg = RuntimeConfig::default();
        cfg.market.alpaca_key_id = "AKID123".into();
        cfg.market.alpaca_secret = "SEC456".into();
        cfg.market.fmp_api_key = "FMP789".into();

        let public = RuntimeConfigPublic::from(&cfg);
        assert_eq!(public.market.alpaca_key_id, "***");
        assert_eq!(public.market.alpaca_secret, "***");
        assert_eq!(public.market.fmp_api_key, "***");
        assert_eq!(public.market.paper_mode, cfg.market.paper_mode);
    }

    #[test]
    fn runtime_config_public_empty_keys_stay_empty() {
        let cfg = RuntimeConfig::default();
        let public = RuntimeConfigPublic::from(&cfg);
        assert_eq!(public.market.alpaca_key_id, "");
        assert_eq!(public.market.alpaca_secret, "");
        assert_eq!(public.market.fmp_api_key, "");
    }

    #[test]
    fn preserve_secrets_keeps_real_values_when_placeholder() {
        let mut new_cfg = RuntimeConfig::default();
        new_cfg.market.alpaca_key_id = "***".into();
        new_cfg.market.alpaca_secret = "***".into();
        new_cfg.market.fmp_api_key = "***".into();

        let prev = {
            let mut p = RuntimeConfig::default();
            p.market.alpaca_key_id = "REAL_KEY".into();
            p.market.alpaca_secret = "REAL_SECRET".into();
            p.market.fmp_api_key = "REAL_FMP".into();
            p
        };

        preserve_secrets(&mut new_cfg, &prev);
        assert_eq!(new_cfg.market.alpaca_key_id, "REAL_KEY");
        assert_eq!(new_cfg.market.alpaca_secret, "REAL_SECRET");
        assert_eq!(new_cfg.market.fmp_api_key, "REAL_FMP");
    }

    #[test]
    fn preserve_secrets_allows_new_values() {
        let mut new_cfg = RuntimeConfig::default();
        new_cfg.market.alpaca_key_id = "NEW_KEY".into();
        new_cfg.market.alpaca_secret = "NEW_SECRET".into();
        new_cfg.market.fmp_api_key = "NEW_FMP".into();

        let prev = RuntimeConfig::default();

        preserve_secrets(&mut new_cfg, &prev);
        assert_eq!(new_cfg.market.alpaca_key_id, "NEW_KEY");
        assert_eq!(new_cfg.market.alpaca_secret, "NEW_SECRET");
        assert_eq!(new_cfg.market.fmp_api_key, "NEW_FMP");
    }
}
