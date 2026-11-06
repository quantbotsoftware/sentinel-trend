//! Phase 3.1 — 15 unit tests for the ML core scaffolding.
//!
//! Coverage (in order):
//!  1. Prediction::vetoes detects adverse direction
//!  2. Prediction::vetoes ignores low confidence
//!  3. Prediction::vetoes ignores aligned direction
//!  4. Prediction::vetoes ignores tiny direction magnitude
//!  5. Prediction::size_multiplier maps confidence linearly
//!  6. Prediction::size_multiplier clamps to [0.5, 1.5]
//!  7. ModelStore weights file round-trip
//!  8. ModelStore latest_version finds highest version
//!  9. ModelStore latest_version returns None on empty dir
//! 10. ModelStore meta load returns default on missing file
//! 11. ModelStore prune_old_versions retains the newest N
//! 12. PredictorRegistry insert / get / ids
//! 13. PredictorRegistry record + recent ring-buffer ordering
//! 14. PredictorRegistry record drops oldest at capacity
//! 15. cold_start_registry returns LSTM + RegimeTransformer (both cold)

use std::path::PathBuf;

use super::{
    cold_start_registry, ModelMeta, ModelStore, Predictor, Prediction, PredictorRegistry,
    COLD_START_MIN_SAMPLES, PREDICTION_RING_CAP,
};

fn tmp_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let nano = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    p.push(format!("st_omega_test_{label}_{nano}"));
    std::fs::create_dir_all(&p).expect("test tmp dir");
    p
}

fn mk_pred(direction: f64, confidence: f64) -> Prediction {
    Prediction {
        model_id: "lstm".into(),
        direction,
        confidence,
        expectancy: 0.0,
        symbol: "AAPL".into(),
        created_at: 0,
    }
}

#[test]
fn prediction_vetoes_when_adverse_high_confidence() {
    let p = mk_pred(-0.8, 0.85);
    assert!(p.vetoes(1.0, 0.7), "adverse + high conf must veto");
}

#[test]
fn prediction_vetoes_ignores_low_confidence() {
    let p = mk_pred(-0.8, 0.50);
    assert!(!p.vetoes(1.0, 0.7), "low confidence must NOT veto");
}

#[test]
fn prediction_vetoes_ignores_aligned_direction() {
    let p = mk_pred(0.8, 0.85);
    assert!(!p.vetoes(1.0, 0.7), "aligned direction must NOT veto");
}

#[test]
fn prediction_vetoes_ignores_tiny_direction() {
    let p = mk_pred(-0.05, 0.95);
    assert!(
        !p.vetoes(1.0, 0.7),
        "magnitude under 0.1 must NOT veto even at high conf"
    );
}

#[test]
fn prediction_size_multiplier_linear_mapping() {
    assert!((mk_pred(0.0, 0.0).size_multiplier() - 0.5).abs() < 1e-9);
    assert!((mk_pred(0.0, 0.5).size_multiplier() - 1.0).abs() < 1e-9);
    assert!((mk_pred(0.0, 1.0).size_multiplier() - 1.5).abs() < 1e-9);
}

#[test]
fn prediction_size_multiplier_clamps() {
    assert!((mk_pred(0.0, -3.0).size_multiplier() - 0.5).abs() < 1e-9);
    assert!((mk_pred(0.0, 999.0).size_multiplier() - 1.5).abs() < 1e-9);
}

#[test]
fn model_store_weights_roundtrip() {
    let dir = tmp_dir("roundtrip");
    let store = ModelStore::new("lstm", dir.clone());
    let body = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
    let p = store.save_weights(7, &body).expect("save");
    assert!(p.exists());
    let loaded = store.load_weights(7).expect("load");
    assert_eq!(loaded, body);
    assert_eq!(store.latest_version(), Some(7));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_store_latest_version_picks_highest() {
    let dir = tmp_dir("latest");
    let store = ModelStore::new("lstm", dir.clone());
    for v in [1u32, 5, 12, 3] {
        store.save_weights(v, &[v as u8]).expect("save");
    }
    assert_eq!(store.latest_version(), Some(12));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_store_latest_version_empty_dir_is_none() {
    let dir = tmp_dir("empty");
    let store = ModelStore::new("lstm", dir.clone());
    assert!(store.latest_version().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_store_meta_default_when_missing() {
    let dir = tmp_dir("meta_missing");
    let store = ModelStore::new("lstm", dir.clone());
    let m = store.load_meta();
    assert_eq!(m.model_id, "lstm");
    assert_eq!(m.version, 0);
    assert_eq!(m.samples_trained, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_store_prune_keeps_newest_n() {
    let dir = tmp_dir("prune");
    let store = ModelStore::new("lstm", dir.clone());
    for v in 1u32..=10 {
        store.save_weights(v, &[v as u8]).expect("save");
    }
    let removed = store.prune_old_versions(3);
    assert_eq!(removed, 7, "should remove 10-3=7");
    assert!(store.load_weights(10).is_some(), "newest preserved");
    assert!(store.load_weights(9).is_some());
    assert!(store.load_weights(8).is_some());
    assert!(store.load_weights(7).is_none(), "older pruned");
    let _ = std::fs::remove_dir_all(&dir);
}

// ──────────────────────────────────────────────────────────────────
//  PredictorRegistry tests use a tiny mock predictor.
// ──────────────────────────────────────────────────────────────────

struct MockPredictor {
    id: String,
    samples: usize,
}

impl Predictor for MockPredictor {
    fn id(&self) -> &str {
        &self.id
    }
    fn architecture(&self) -> &str {
        "mock"
    }
    fn predict(&self, _ctx: &crate::smarttrend::strategies::MarketContext) -> Option<Prediction> {
        if self.samples < COLD_START_MIN_SAMPLES {
            return None;
        }
        Some(mk_pred(0.5, 0.6))
    }
    fn meta(&self) -> ModelMeta {
        ModelMeta {
            model_id: self.id.clone(),
            architecture: "mock".into(),
            samples_trained: self.samples,
            ..Default::default()
        }
    }
}

#[test]
fn registry_insert_and_get() {
    let mut reg = PredictorRegistry::new();
    reg.insert(Box::new(MockPredictor {
        id: "lstm".into(),
        samples: 0,
    }));
    reg.insert(Box::new(MockPredictor {
        id: "regime_xfmr".into(),
        samples: 0,
    }));
    assert_eq!(reg.len(), 2);
    assert!(reg.get("lstm").is_some());
    assert!(reg.get("regime_xfmr").is_some());
    assert!(reg.get("missing").is_none());
    let mut ids = reg.ids();
    ids.sort();
    assert_eq!(ids, vec!["lstm", "regime_xfmr"]);
}

#[test]
fn registry_recent_returns_newest_first() {
    let reg = PredictorRegistry::new();
    for i in 0..5 {
        let mut p = mk_pred(0.0, 0.5);
        p.created_at = i as i64;
        reg.record(p);
    }
    let r = reg.recent(3);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].created_at, 4, "newest first");
    assert_eq!(r[1].created_at, 3);
    assert_eq!(r[2].created_at, 2);
}

#[test]
fn registry_recent_drops_oldest_at_capacity() {
    let reg = PredictorRegistry::new();
    for i in 0..(PREDICTION_RING_CAP + 50) {
        let mut p = mk_pred(0.0, 0.5);
        p.created_at = i as i64;
        reg.record(p);
    }
    let r = reg.recent(PREDICTION_RING_CAP + 100);
    assert_eq!(r.len(), PREDICTION_RING_CAP, "capped at PREDICTION_RING_CAP");
    let oldest = r.last().expect("non-empty").created_at;
    assert_eq!(oldest, 50, "first 50 entries dropped (added=cap+50)");
}

#[test]
fn cold_start_registry_has_lstm_and_transformer() {
    let dir = tmp_dir("coldreg");
    let reg = cold_start_registry(&dir);
    assert!(reg.get("lstm").is_some(), "LSTM must be registered");
    assert!(
        reg.get("regime_xfmr").is_some(),
        "RegimeTransformer must be registered"
    );
    // Both should be cold (samples_trained == 0 → not warm).
    let lstm = reg.get("lstm").expect("lstm");
    assert!(!lstm.is_warm());
    let xfmr = reg.get("regime_xfmr").expect("xfmr");
    assert!(!xfmr.is_warm());
    let _ = std::fs::remove_dir_all(&dir);
}
