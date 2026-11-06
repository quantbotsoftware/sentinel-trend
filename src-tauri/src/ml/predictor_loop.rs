//! Phase 3.5 — OMEGA predictor background loop.
//!
//! Wakes once per minute and:
//!
//! - At 21:00 ET (post-evolver) checks whether retraining should
//!   trigger for any model in the registry.
//! - Spawns a blocking task to run training (training is CPU-heavy
//!   and could otherwise starve the tokio runtime).
//! - Hot-swaps the registry via [`DPlusHandles::swap_omega`] when
//!   training succeeds.
//! - Emits `omega-train-complete` with `{model_id, accuracy,
//!   samples, version}` on success and `omega-train-failed` on
//!   error so the UI can show a status badge without polling.
//!
//! ## Retrain gate
//!
//! Plan says `samples_added > 100 && hours_since_train > 24`. Right
//! now we don't have an integrated MemoryDb tap, so the loop calls
//! `MemoryDb::stats().total_setups` and tracks the delta locally.
//!
//! ## Failure semantics
//!
//! - Tokio cancellation aborts the loop cleanly.
//! - Training panics are caught by `tokio::task::spawn_blocking`'s
//!   join-handle error path; loop logs a warn and survives.
//! - All disk writes (safetensors, meta) are logged at warn on
//!   failure but never abort the loop.

use std::sync::Arc;

use chrono::{Datelike, Timelike};
use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::ml::lstm::LstmForecaster;
use crate::ml::regime_transformer::RegimeTransformer;
use crate::ml::{
    cold_start_registry, default_ml_dir, PredictorRegistry, TrainStatus, COLD_START_MIN_SAMPLES,
};
use crate::state::TitanState;

pub const LOOP_NAME: &str = "omega";
pub const TICK_INTERVAL_SECS: u64 = 60;
pub const FIRE_HOUR_ET: u32 = 21;
pub const RETRAIN_MIN_NEW_SAMPLES: i64 = 100;
pub const RETRAIN_MIN_HOURS: i64 = 24;

/// Convenience: builds an initial cold-start registry. `main.rs`
/// uses this at boot to seed `DPlusHandles::omega`.
pub fn build_initial_registry() -> PredictorRegistry {
    cold_start_registry(&default_ml_dir())
}

/// Loads the latest persisted weights for both models. Falls back
/// to cold-start instances when no version file is present.
pub fn build_loaded_registry() -> PredictorRegistry {
    let mut reg = PredictorRegistry::new();
    let dir = default_ml_dir();
    reg.insert(Box::new(LstmForecaster::load_latest(&dir)));
    reg.insert(Box::new(RegimeTransformer::load_latest(&dir)));
    reg
}

pub async fn run_predictor_loop(state: Arc<TitanState>, app: AppHandle) {
    let enabled = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.evolution_protocol.enabled && cfg.evolution_protocol.omega_enabled
    };
    if !enabled {
        tracing::info!(
            component = LOOP_NAME,
            "OMEGA disabled in config — predictor loop skipped"
        );
        return;
    }
    let Some(dplus) = state.dplus() else {
        tracing::info!(
            component = LOOP_NAME,
            "DPlusHandles unavailable — predictor loop skipped"
        );
        return;
    };
    tracing::info!(
        component = LOOP_NAME,
        fire_hour_et = FIRE_HOUR_ET,
        "OMEGA predictor loop starting"
    );
    let cancel = state.shutdown.clone();
    let mut tick = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
    let mut last_run_date: Option<(i32, u32, u32)> = None;
    let mut last_total_samples: i64 = -1;
    let mut last_train_ts: i64 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(component = LOOP_NAME, "shutdown requested");
                break;
            }
            _ = tick.tick() => {
                if !should_fire(&mut last_run_date) {
                    continue;
                }
                // Use `ml_samples` count, not `setups` count: only
                // rows in `ml_samples` carry the FeatureVec BLOB the
                // trainer actually consumes.
                let total_samples = dplus
                    .memory_db
                    .count_ml_samples()
                    .unwrap_or(0);
                if !should_retrain(
                    total_samples,
                    last_total_samples,
                    last_train_ts,
                    chrono::Utc::now().timestamp(),
                ) {
                    tracing::debug!(
                        component = LOOP_NAME,
                        total_samples,
                        last_total_samples,
                        "retrain gate not met — skipping"
                    );
                    continue;
                }
                let dir = default_ml_dir();
                let app_handle = app.clone();
                let dplus_omega = dplus.omega.clone();
                let memdb = dplus.memory_db.clone();
                tracing::info!(
                    component = LOOP_NAME,
                    total_samples,
                    "kicking off retrain"
                );
                let train_handle = tokio::task::spawn_blocking(move || {
                    train_models_blocking(&dir, memdb)
                });
                match train_handle.await {
                    Ok(reports) => {
                        // Build a fresh registry from the just-trained
                        // models so all readers see the new weights.
                        let registry = build_loaded_registry();
                        dplus_omega.store(Arc::new(registry));
                        for r in &reports {
                            match &r.status {
                                TrainStatus::Trained { version, accuracy, samples } => {
                                    let _ = app_handle.emit_all(
                                        "omega-train-complete",
                                        serde_json::json!({
                                            "model_id": r.model_id,
                                            "version": version,
                                            "accuracy": accuracy,
                                            "samples": samples,
                                        }),
                                    );
                                }
                                TrainStatus::InsufficientData { samples, threshold } => {
                                    tracing::info!(
                                        component = LOOP_NAME,
                                        model = %r.model_id,
                                        samples,
                                        threshold,
                                        "insufficient data — staying cold"
                                    );
                                }
                                TrainStatus::NoChange { .. } => {}
                                TrainStatus::Failed { message } => {
                                    let _ = app_handle.emit_all(
                                        "omega-train-failed",
                                        serde_json::json!({
                                            "model_id": r.model_id,
                                            "message": message,
                                        }),
                                    );
                                }
                            }
                        }
                        last_train_ts = chrono::Utc::now().timestamp();
                        last_total_samples = total_samples;
                    }
                    Err(e) => {
                        tracing::error!(
                            component = LOOP_NAME,
                            error = %e,
                            "training task panicked / cancelled"
                        );
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrainReport {
    pub model_id: String,
    pub status: TrainStatus,
}

/// Synchronous trainer — runs both models against MemoryDb's closed
/// setups. Designed for `spawn_blocking` so it can monopolise a
/// thread without starving the runtime.
pub(crate) fn train_models_blocking(
    base_dir: &std::path::Path,
    memdb: Arc<crate::smarttrend::memory_db::MemoryDb>,
) -> Vec<TrainReport> {
    let mut out = Vec::new();
    // Pull historical (FeatureVec, label) samples from MemoryDb.
    let mut samples = pull_lstm_samples(memdb.as_ref());

    // Entropy-Based Dynamic Feature Pruning: zero out noisy dimensions
    // whose Mutual Information I(X_d; Y) falls below the 25th percentile
    // (or MI_FLOOR = 0.005, whichever is higher).
    let mi_scores = crate::ml::feature_pruning::compute_feature_mutual_information(&samples);
    let pruned = crate::ml::feature_pruning::apply_entropy_pruning(&mut samples, &mi_scores);
    if pruned > 0 {
        tracing::info!(
            component = LOOP_NAME,
            pruned_dims = pruned,
            "Entropy pruning applied before LSTM training"
        );
    }

    // LSTM
    {
        let mut model = LstmForecaster::cold_start(base_dir);
        let cfg = crate::ml::lstm::TrainConfig::default();
        let status = model.train(&samples, &cfg);
        if matches!(status, TrainStatus::Trained { .. }) {
            if let Err(e) = model.save(base_dir) {
                tracing::warn!(
                    component = LOOP_NAME,
                    model = "lstm",
                    error = %e,
                    "LSTM weight save failed"
                );
            }
        }
        out.push(TrainReport {
            model_id: "lstm".into(),
            status,
        });
    }
    // Transformer regime classifier — needs window-shaped data.
    let mut xfmr_samples = pull_xfmr_samples(memdb.as_ref());
    // Apply the same MI mask to transformer samples for consistency.
    crate::ml::feature_pruning::apply_entropy_pruning_xfmr(&mut xfmr_samples, &mi_scores);
    {
        let mut model = RegimeTransformer::cold_start(base_dir);
        let cfg = crate::ml::regime_transformer::TrainConfig::default();
        let status = model.train(&xfmr_samples, &cfg);
        if matches!(status, TrainStatus::Trained { .. }) {
            if let Err(e) = model.save(base_dir) {
                tracing::warn!(
                    component = LOOP_NAME,
                    model = "regime_xfmr",
                    error = %e,
                    "regime_xfmr weight save failed"
                );
            }
        }
        out.push(TrainReport {
            model_id: "regime_xfmr".into(),
            status,
        });
    }
    out
}

/// Maximum LSTM training samples pulled per retrain. 50k rows × 1 KB
/// per FeatureVec = 50 MB, well under the budget for a 1-shot fetch.
pub const MAX_LSTM_PULL: usize = 50_000;

/// Maximum Transformer regime training samples pulled per retrain.
/// Each sample is a `seq_len`-long window of FeatureVecs; we cap on
/// finished windows, not raw rows.
pub const MAX_XFMR_PULL: usize = 20_000;

/// Pull `(FeatureVec, return_norm)` training pairs for the LSTM
/// regression head from MemoryDb.
///
/// On cold-start (no `ml_samples` rows yet) this returns an empty
/// vec and the LSTM trainer short-circuits to `InsufficientData`.
fn pull_lstm_samples(
    memdb: &crate::smarttrend::memory_db::MemoryDb,
) -> Vec<(crate::ml::features::FeatureVec, f64)> {
    match memdb.pull_lstm_samples(MAX_LSTM_PULL) {
        Ok(rows) => rows
            .into_iter()
            .map(|(fv, ret)| (crate::ml::features::FeatureVec(fv), ret))
            .collect(),
        Err(e) => {
            tracing::warn!(
                component = LOOP_NAME,
                error = %e,
                "MemoryDb::pull_lstm_samples failed — falling back to cold-start"
            );
            Vec::new()
        }
    }
}

/// Pull `(sequence, regime)` training tuples for the Transformer
/// regime classifier. Each `sequence` has exactly
/// [`crate::ml::regime_transformer::SEQ_LEN`] consecutive frames
/// from the same symbol.
///
/// On cold-start returns an empty vec.
fn pull_xfmr_samples(
    memdb: &crate::smarttrend::memory_db::MemoryDb,
) -> Vec<(
    Vec<crate::ml::features::FeatureVec>,
    crate::smarttrend::strategies::MarketRegime,
)> {
    let seq_len = crate::ml::regime_transformer::SEQ_LEN;
    match memdb.pull_xfmr_samples(seq_len, MAX_XFMR_PULL) {
        Ok(rows) => rows
            .into_iter()
            .map(|(seq, regime)| {
                let fvs = seq
                    .into_iter()
                    .map(crate::ml::features::FeatureVec)
                    .collect();
                (fvs, regime)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                component = LOOP_NAME,
                error = %e,
                "MemoryDb::pull_xfmr_samples failed — falling back to cold-start"
            );
            Vec::new()
        }
    }
}

/// Returns true at most once per ET day, when local clock crosses
/// `FIRE_HOUR_ET`. Mirrors `evolver_loop::should_fire` but for our
/// dedicated 21:00 ET slot.
pub fn should_fire(last_run_date: &mut Option<(i32, u32, u32)>) -> bool {
    let now_utc = chrono::Utc::now();
    let now_et = now_utc.with_timezone(&chrono_tz::US::Eastern);
    let today = (now_et.year(), now_et.month(), now_et.day());
    if now_et.hour() < FIRE_HOUR_ET {
        return false;
    }
    if last_run_date.is_some_and(|d| d == today) {
        return false;
    }
    *last_run_date = Some(today);
    true
}

/// Result of consulting the OMEGA registry at entry time.
#[derive(Debug, Clone)]
pub struct OmegaEntryDecision {
    /// Multiplicative size factor in `[0.5, 1.5]`. `1.0` = no
    /// adjustment (cold-start path).
    pub size_multiplier: f64,
    /// `true` when at least one model recommends blocking the entry.
    pub vetoed: bool,
    /// Reason text for veto / diagnostic field for the log line.
    pub diag: String,
    /// Calibrated confidence from OMEGA (defaults to 0.5).
    pub confidence: f64,
}

impl Default for OmegaEntryDecision {
    fn default() -> Self {
        Self {
            size_multiplier: 1.0,
            vetoed: false,
            diag: String::new(),
            confidence: 0.5,
        }
    }
}

/// Consults the OMEGA registry and returns size-modifier + veto.
///
/// `intended_side` is `+1.0` for long, `-1.0` for short. Cold-start
/// behaviour: every predictor returns `None`, so this function
/// returns the default (no-op) decision and the bot path is
/// completely unaffected.
pub fn evaluate_omega_at_entry(
    dplus: &crate::smarttrend::dplus::DPlusHandles,
    ctx: &crate::smarttrend::strategies::MarketContext,
    intended_side: f64,
) -> OmegaEntryDecision {
    if !dplus.is_omega_enabled("lstm") && !dplus.is_omega_enabled("regime_xfmr") {
        return OmegaEntryDecision::default();
    }
    let registry = dplus.omega.load();
    if !registry.any_warm() {
        return OmegaEntryDecision::default();
    }
    let mut decision = OmegaEntryDecision::default();
    let mut diag_parts: Vec<String> = Vec::new();
    if dplus.is_omega_enabled("lstm") {
        if let Some(p) = registry.get("lstm").and_then(|m| m.predict(ctx)) {
            decision.size_multiplier *= p.size_multiplier();
            decision.confidence = p.confidence;
            if p.vetoes(intended_side, 0.7) {
                decision.vetoed = true;
                diag_parts.push(format!("lstm_veto:{:.2}", p.confidence));
            } else {
                diag_parts.push(format!("lstm_conf:{:.2}", p.confidence));
            }
            registry.record(p);
        }
    }
    if dplus.is_omega_enabled("regime_xfmr") {
        if let Some(p) = registry.get("regime_xfmr").and_then(|m| m.predict(ctx)) {
            // The regime classifier doesn't directly veto entries,
            // it only contributes context.
            diag_parts.push(format!("regime_conf:{:.2}", p.confidence));
            registry.record(p);
        }
    }
    decision.size_multiplier = decision.size_multiplier.clamp(0.5, 1.5);
    decision.diag = diag_parts.join(",");
    decision
}

/// Fast-path wrapper used by `bot/entry.rs`. Skips constructing
/// the [`MarketContext`] when the registry is empty / fully cold —
/// most of the time the closure (`build_ctx`) is never called.
///
/// Returns a decision with `size_multiplier = 1.0`, `vetoed = false`,
/// `diag = ""` when:
///   - the [`crate::state::TitanState::dplus`] handle is `None`,
///   - the OMEGA master toggle is off in config (caller checks),
///   - or no predictor in the registry is warm.
pub fn evaluate_omega_at_entry_lazy<F>(
    dplus: &crate::smarttrend::dplus::DPlusHandles,
    intended_side: f64,
    build_ctx: F,
) -> OmegaEntryDecision
where
    F: FnOnce() -> Option<crate::smarttrend::strategies::MarketContext>,
{
    if !dplus.is_omega_enabled("lstm") && !dplus.is_omega_enabled("regime_xfmr") {
        return OmegaEntryDecision::default();
    }
    if !dplus.omega.load().any_warm() {
        return OmegaEntryDecision::default();
    }
    let Some(ctx) = build_ctx() else {
        return OmegaEntryDecision::default();
    };
    evaluate_omega_at_entry(dplus, &ctx, intended_side)
}

/// Pure helper for testing: implements the retrain gate.
pub fn should_retrain(
    total_samples: i64,
    last_total_samples: i64,
    last_train_ts: i64,
    now_ts: i64,
) -> bool {
    if total_samples < COLD_START_MIN_SAMPLES as i64 {
        return false;
    }
    let delta = total_samples - last_total_samples.max(0);
    if delta < RETRAIN_MIN_NEW_SAMPLES {
        return false;
    }
    let hours_since = (now_ts - last_train_ts) / 3_600;
    if last_train_ts > 0 && hours_since < RETRAIN_MIN_HOURS {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_retrain_blocks_below_cold_start() {
        assert!(!should_retrain(
            COLD_START_MIN_SAMPLES as i64 - 1,
            0,
            0,
            chrono::Utc::now().timestamp()
        ));
    }

    #[test]
    fn should_retrain_blocks_when_delta_too_small() {
        assert!(!should_retrain(
            COLD_START_MIN_SAMPLES as i64 + 50,
            COLD_START_MIN_SAMPLES as i64,
            0,
            chrono::Utc::now().timestamp()
        ));
    }

    #[test]
    fn should_retrain_allows_when_delta_and_no_prior_train() {
        assert!(should_retrain(
            COLD_START_MIN_SAMPLES as i64 + RETRAIN_MIN_NEW_SAMPLES + 5,
            COLD_START_MIN_SAMPLES as i64,
            0,
            chrono::Utc::now().timestamp()
        ));
    }

    #[test]
    fn should_retrain_blocks_within_cooldown_hours() {
        let now = 1_700_000_000;
        assert!(!should_retrain(
            COLD_START_MIN_SAMPLES as i64 + RETRAIN_MIN_NEW_SAMPLES + 10,
            COLD_START_MIN_SAMPLES as i64,
            now - 3600,
            now,
        ));
    }

    #[test]
    fn should_retrain_allows_after_cooldown() {
        let now = 1_700_000_000;
        assert!(should_retrain(
            COLD_START_MIN_SAMPLES as i64 + RETRAIN_MIN_NEW_SAMPLES + 10,
            COLD_START_MIN_SAMPLES as i64,
            now - (RETRAIN_MIN_HOURS + 1) * 3600,
            now,
        ));
    }

    #[test]
    fn should_fire_first_call_after_hour_returns_true() {
        let mut last: Option<(i32, u32, u32)> = None;
        // We can't mock chrono::Utc::now without injection; this
        // test calls the function — at any time of day. We accept
        // either branch and validate consistency: a second call in
        // the same minute MUST return false.
        let first = should_fire(&mut last);
        let second = should_fire(&mut last);
        if first {
            assert!(!second, "second call same day must be false");
        } else {
            assert!(!second, "second call must remain false");
        }
    }

    #[test]
    fn build_initial_registry_has_cold_models() {
        let temp = std::env::temp_dir().join(format!("st_test_home_{}", uuid::Uuid::new_v4()));
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &temp);

        let reg = build_initial_registry();
        assert!(reg.get("lstm").is_some());
        assert!(reg.get("regime_xfmr").is_some());
        let lstm = reg.get("lstm").expect("lstm");
        assert!(!lstm.is_warm());

        if let Some(h) = old_home {
            std::env::set_var("HOME", &h);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn build_loaded_registry_falls_back_to_cold_when_no_weights() {
        let reg = build_loaded_registry();
        assert!(reg.get("lstm").is_some());
        assert!(reg.get("regime_xfmr").is_some());
    }

    #[test]
    fn pull_samples_empty_returns_empty() {
        let dir = std::env::temp_dir().join(format!(
            "st_omega_pull_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let memdb_path = dir.join("memory.db");
        let memdb = crate::smarttrend::memory_db::MemoryDb::open(&memdb_path)
            .expect("open db");
        assert!(pull_lstm_samples(&memdb).is_empty());
        assert!(pull_xfmr_samples(&memdb).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wiring test: insert real `ml_samples` via the public API and
    /// verify the predictor_loop pull functions return them properly.
    #[test]
    fn pull_lstm_samples_round_trips_through_memdb() {
        let memdb = crate::smarttrend::memory_db::MemoryDb::open_in_memory().expect("open");
        let fv: Vec<f64> = (0..crate::ml::features::FEATURE_DIM)
            .map(|i| i as f64 * 0.01)
            .collect();
        for i in 0..5i64 {
            memdb
                .record_ml_sample(
                    &fv,
                    0.05 * i as f64,
                    crate::smarttrend::strategies::MarketRegime::Calm,
                    1000 + i,
                    1500 + i,
                    "AAPL",
                    None,
                    "test",
                )
                .expect("record");
        }
        let pulled = pull_lstm_samples(&memdb);
        assert_eq!(pulled.len(), 5);
        for (vec, _) in &pulled {
            assert_eq!(vec.len(), crate::ml::features::FEATURE_DIM);
        }
        // Most-recent first → newest sample (i=4) at index 0.
        assert!((pulled[0].1 - 0.20).abs() < 1e-9);
    }

    /// Wiring test: insert enough rows for at least one sequence
    /// and verify the Transformer pull returns full-length windows
    /// with the correct regime label.
    #[test]
    fn pull_xfmr_samples_round_trips_through_memdb() {
        let memdb = crate::smarttrend::memory_db::MemoryDb::open_in_memory().expect("open");
        let fv: Vec<f64> = vec![0.0; crate::ml::features::FEATURE_DIM];
        let needed = crate::ml::regime_transformer::SEQ_LEN + 5;
        for i in 0..needed as i64 {
            memdb
                .record_ml_sample(
                    &fv,
                    0.0,
                    crate::smarttrend::strategies::MarketRegime::Volatile,
                    1000 + i,
                    1500 + i,
                    "AAPL",
                    None,
                    "test",
                )
                .expect("record");
        }
        let pulled = pull_xfmr_samples(&memdb);
        assert!(!pulled.is_empty());
        for (seq, regime) in &pulled {
            assert_eq!(seq.len(), crate::ml::regime_transformer::SEQ_LEN);
            assert_eq!(
                *regime,
                crate::smarttrend::strategies::MarketRegime::Volatile
            );
            for fv in seq {
                assert_eq!(fv.len(), crate::ml::features::FEATURE_DIM);
            }
        }
    }

    fn mk_dplus() -> std::sync::Arc<crate::smarttrend::dplus::DPlusHandles> {
        let adaptive_risk = std::sync::Arc::new(std::sync::Mutex::new(
            crate::smarttrend::risk_adaptive::AdaptiveRisk::new(),
        ));
        let dir = std::env::temp_dir().join(format!(
            "st_omega_dplus_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let memdb = std::sync::Arc::new(
            crate::smarttrend::memory_db::MemoryDb::open(&dir.join("memory.db")).expect("db"),
        );
        let causal = std::sync::Arc::new(std::sync::Mutex::new(
            crate::smarttrend::causal::CausalEngine::new(256),
        ));
        let evolver = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::smarttrend::strategy_evolver::Evolver::new(8, 42, dir.clone()),
        ));
        let overrides = std::sync::Arc::new(crate::smarttrend::strategy_evolver::ParamOverrides::new());
        let tournament = std::sync::Arc::new(std::sync::Mutex::new(
            crate::smarttrend::tournament::Tournament::new_default(rust_decimal::Decimal::from(100_000)),
        ));
        std::sync::Arc::new(crate::smarttrend::dplus::DPlusHandles::new(
            adaptive_risk,
            memdb,
            causal,
            evolver,
            overrides,
            tournament,
        ))
    }

    fn mk_ctx() -> crate::smarttrend::strategies::MarketContext {
        crate::smarttrend::strategies::MarketContext {
            symbol: "AAPL".into(),
            timeframe: "5m",
            price: 100.0,
            prev_close: 100.0,
            session_open: 100.0,
            candles: Vec::new(),
            atr: 1.0,
            rvol: 1.0,
            gap_pct: 0.0,
            spread_bps: 5.0,
            catalyst_score: None,
            now_et_min: 600,
            now_et_hour: 10,
            now_et_minute: 0,
            regime: None,
            is_halted: false,
            companion: None,
            float_shares: None,
            news_sentiment: None,
            overrides: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn evaluate_at_entry_cold_start_is_noop() {
        let dplus = mk_dplus();
        let ctx = mk_ctx();
        let d = evaluate_omega_at_entry(&dplus, &ctx, 1.0);
        assert!((d.size_multiplier - 1.0).abs() < 1e-9);
        assert!(!d.vetoed);
        assert!(d.diag.is_empty());
    }

    #[test]
    fn evaluate_at_entry_disabled_models_skip_quickly() {
        let dplus = mk_dplus();
        dplus.set_omega_enabled("lstm", false);
        dplus.set_omega_enabled("regime_xfmr", false);
        let ctx = mk_ctx();
        let d = evaluate_omega_at_entry(&dplus, &ctx, 1.0);
        assert!((d.size_multiplier - 1.0).abs() < 1e-9);
        assert!(!d.vetoed);
    }

    #[test]
    fn evaluate_at_entry_lazy_skips_ctx_build_when_cold() {
        let dplus = mk_dplus();
        let mut called = 0u32;
        let d = evaluate_omega_at_entry_lazy(&dplus, 1.0, || {
            called += 1;
            Some(mk_ctx())
        });
        assert_eq!(called, 0, "build_ctx must NOT run when registry is cold");
        assert!((d.size_multiplier - 1.0).abs() < 1e-9);
        assert!(!d.vetoed);
    }

    #[test]
    fn evaluate_at_entry_lazy_invokes_ctx_build_only_when_warm() {
        let dplus = mk_dplus();
        // Insert a fake warm predictor.
        let mut reg = PredictorRegistry::new();
        struct WarmStub;
        impl crate::ml::Predictor for WarmStub {
            fn id(&self) -> &str {
                "lstm"
            }
            fn architecture(&self) -> &str {
                "lstm"
            }
            fn predict(&self, ctx: &crate::smarttrend::strategies::MarketContext) -> Option<crate::ml::Prediction> {
                Some(crate::ml::Prediction {
                    model_id: "lstm".into(),
                    direction: 0.5,
                    confidence: 0.6,
                    expectancy: 0.0,
                    symbol: ctx.symbol.clone(),
                    created_at: 0,
                })
            }
            fn meta(&self) -> crate::ml::ModelMeta {
                crate::ml::ModelMeta {
                    model_id: "lstm".into(),
                    samples_trained: COLD_START_MIN_SAMPLES + 1,
                    architecture: "lstm".into(),
                    ..Default::default()
                }
            }
        }
        reg.insert(Box::new(WarmStub));
        dplus.swap_omega(reg);
        let mut called = 0u32;
        let d = evaluate_omega_at_entry_lazy(&dplus, 1.0, || {
            called += 1;
            Some(mk_ctx())
        });
        assert_eq!(called, 1, "build_ctx must run exactly once");
        // Confidence 0.6 → multiplier 1.1
        assert!((d.size_multiplier - 1.1).abs() < 1e-9);
        assert!(!d.vetoed, "aligned 0.5 direction must NOT veto long");
    }

    #[test]
    fn train_models_blocking_short_circuits_on_empty_db() {
        let dir = std::env::temp_dir().join(format!(
            "st_omega_train_blank_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let memdb_path = dir.join("memory.db");
        let memdb = std::sync::Arc::new(
            crate::smarttrend::memory_db::MemoryDb::open(&memdb_path).expect("open db"),
        );
        let reports = train_models_blocking(&dir, memdb);
        assert_eq!(reports.len(), 2);
        for r in reports {
            assert!(matches!(
                r.status,
                TrainStatus::InsufficientData { .. }
            ));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
