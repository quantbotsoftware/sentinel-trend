//! OMEGA (Sesiunea 3) — Tauri command surface.
//!
//! Read-mostly endpoints serving the OMEGA UI tab plus the manual
//! actions (force retrain, toggle model). All commands short-circuit
//! with a friendly error when [`crate::state::TitanState::dplus`] is
//! `None` (Evolution protocol disabled in config).

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::ml::predictor_loop::build_loaded_registry;
use crate::ml::default_ml_dir;
use crate::ml::regime_transformer::{idx_to_regime, NUM_CLASSES};
use crate::ml::{Prediction, COLD_START_MIN_SAMPLES};
use crate::state::TitanState;

fn dplus_or_err(
    state: &Arc<TitanState>,
) -> Result<&crate::smarttrend::dplus::DPlusHandles, String> {
    state
        .dplus()
        .ok_or_else(|| "OMEGA disabled (set evolution_protocol.enabled = true)".to_string())
}

// ════════════════════════════════════════════════════════════════════
//  get_omega_state
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct ModelStatusRow {
    pub model_id: String,
    pub architecture: String,
    pub version: u32,
    pub last_trained_at: i64,
    pub accuracy: f64,
    pub samples_trained: usize,
    pub is_warm: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OmegaState {
    pub models: Vec<ModelStatusRow>,
    pub cold_start_threshold: usize,
    pub recent_count: usize,
}

#[tauri::command]
pub fn get_omega_state(
    state: State<'_, Arc<TitanState>>,
) -> Result<OmegaState, String> {
    let dplus = dplus_or_err(&state)?;
    let registry = dplus.omega.load();
    let mut models: Vec<ModelStatusRow> = Vec::new();
    for id in registry.ids() {
        if let Some(p) = registry.get(id) {
            let m = p.meta();
            models.push(ModelStatusRow {
                model_id: m.model_id.clone(),
                architecture: m.architecture.clone(),
                version: m.version,
                last_trained_at: m.last_trained_at,
                accuracy: m.accuracy,
                samples_trained: m.samples_trained,
                is_warm: p.is_warm(),
                enabled: dplus.is_omega_enabled(id),
            });
        }
    }
    models.sort_by(|a, b| a.model_id.cmp(&b.model_id));
    Ok(OmegaState {
        models,
        cold_start_threshold: COLD_START_MIN_SAMPLES,
        recent_count: registry.recent(usize::MAX).len(),
    })
}

// ════════════════════════════════════════════════════════════════════
//  get_recent_predictions
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct PredictionRow {
    pub model_id: String,
    pub symbol: String,
    pub direction: f64,
    pub confidence: f64,
    pub expectancy: f64,
    pub created_at: i64,
}

impl From<Prediction> for PredictionRow {
    fn from(p: Prediction) -> Self {
        Self {
            model_id: p.model_id,
            symbol: p.symbol,
            direction: p.direction,
            confidence: p.confidence,
            expectancy: p.expectancy,
            created_at: p.created_at,
        }
    }
}

#[tauri::command]
pub fn get_recent_predictions(
    state: State<'_, Arc<TitanState>>,
    limit: Option<usize>,
) -> Result<Vec<PredictionRow>, String> {
    let dplus = dplus_or_err(&state)?;
    let registry = dplus.omega.load();
    let take = limit.unwrap_or(50).min(crate::ml::PREDICTION_RING_CAP);
    Ok(registry.recent(take).into_iter().map(Into::into).collect())
}

// ════════════════════════════════════════════════════════════════════
//  force_retrain
// ════════════════════════════════════════════════════════════════════

#[tauri::command]
pub async fn force_retrain(
    state: State<'_, Arc<TitanState>>,
    app: AppHandle,
    model_id: String,
) -> Result<bool, String> {
    let _ = dplus_or_err(&state)?;
    let dir = default_ml_dir();
    let memdb = state
        .dplus()
        .ok_or_else(|| "OMEGA disabled".to_string())?
        .memory_db
        .clone();
    let omega_handle = state
        .dplus()
        .ok_or_else(|| "OMEGA disabled".to_string())?
        .omega
        .clone();
    // Spawn blocking trainer; complete asynchronously.
    let model_id_clone = model_id.clone();
    tokio::task::spawn_blocking(move || {
        let reports = crate::ml::predictor_loop::train_models_blocking(&dir, memdb);
        let new_reg = build_loaded_registry();
        omega_handle.store(Arc::new(new_reg));
        
        let trained_report = reports.iter().find(|r| r.model_id == model_id_clone);
        if let Some(r) = trained_report {
            match &r.status {
                crate::ml::TrainStatus::Trained { version, accuracy, samples } => {
                    let _ = app.emit_all(
                        "omega-train-complete",
                        serde_json::json!({
                            "model_id": model_id_clone,
                            "version": version,
                            "accuracy": accuracy,
                            "samples": samples,
                            "manual": true,
                        }),
                    );
                }
                crate::ml::TrainStatus::Failed { message } => {
                    let _ = app.emit_all(
                        "omega-train-failed",
                        serde_json::json!({
                            "model_id": model_id_clone,
                            "message": message,
                            "manual": true,
                        }),
                    );
                }
                _ => {
                    let _ = app.emit_all(
                        "omega-train-complete",
                        serde_json::json!({
                            "model_id": model_id_clone,
                            "manual": true,
                        }),
                    );
                }
            }
        } else {
            let _ = app.emit_all(
                "omega-train-complete",
                serde_json::json!({
                    "model_id": model_id_clone,
                    "manual": true,
                }),
            );
        }
    });
    tracing::info!(
        component = "omega",
        model_id = %model_id,
        "force_retrain triggered"
    );
    Ok(true)
}

// ════════════════════════════════════════════════════════════════════
//  toggle_omega_enabled
// ════════════════════════════════════════════════════════════════════

#[tauri::command]
pub fn toggle_omega_enabled(
    state: State<'_, Arc<TitanState>>,
    model_id: String,
    enabled: bool,
) -> Result<bool, String> {
    let dplus = dplus_or_err(&state)?;
    dplus.set_omega_enabled(&model_id, enabled);
    tracing::info!(
        component = "omega",
        model_id = %model_id,
        enabled,
        "OMEGA model toggle"
    );
    Ok(true)
}

// ════════════════════════════════════════════════════════════════════
//  get_regime_forecast
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct RegimeForecast {
    pub probabilities: [f32; NUM_CLASSES],
    pub labels: [&'static str; NUM_CLASSES],
    pub argmax_index: usize,
    pub argmax_label: &'static str,
    pub argmax_probability: f32,
    pub horizon_minutes: u32,
    pub last_updated_at: i64,
    pub is_warm: bool,
}

#[tauri::command]
pub fn get_regime_forecast(
    state: State<'_, Arc<TitanState>>,
) -> Result<RegimeForecast, String> {
    let dplus = dplus_or_err(&state)?;
    let registry = dplus.omega.load();
    let xfmr = registry
        .get("regime_xfmr")
        .ok_or_else(|| "regime transformer not registered".to_string())?;
    let is_warm = xfmr.is_warm();
    let labels = ["calm", "trending", "volatile", "panic"];
    let mut probs = [0.25f32; NUM_CLASSES];
    let mut argmax_index = 0usize;
    let mut argmax_probability = -1.0f32;
    if is_warm {
        if let Some(p) = xfmr.last_probabilities() {
            probs = p;
        }
    }
    for (i, p) in probs.iter().enumerate() {
        if *p > argmax_probability {
            argmax_probability = *p;
            argmax_index = i;
        }
    }
    let argmax_label = idx_to_regime(argmax_index)
        .map(|_| labels[argmax_index])
        .unwrap_or("unknown");
    Ok(RegimeForecast {
        probabilities: probs,
        labels,
        argmax_index,
        argmax_label,
        argmax_probability,
        horizon_minutes: 90,
        last_updated_at: xfmr.meta().last_trained_at,
        is_warm,
    })
}
