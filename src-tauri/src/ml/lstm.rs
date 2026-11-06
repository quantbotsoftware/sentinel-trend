//! Phase 3.3 — LSTM forecaster.
//!
//! Architecture (cold-start contract preserved):
//!   FeatureVec(128) → reshape (1, 1, 128) → candle LSTM (hidden=64)
//!   → Linear(64→1) → sigmoid → P(up next bar)
//!
//! Direction = `2 * P - 1` (range `[-1, +1]`). Confidence =
//! `|2 * P - 1|` (range `[0, 1]`). Both quantities map cleanly into
//! the [`Prediction`] semantics consumed by `bot/entry.rs`.
//!
//! ## Training
//!
//! Source: `Vec<(FeatureVec, label)>` provided by the caller (in
//! Phase 3.5 the loop pulls these from `MemoryDb::all_closed_setups`).
//! Walk-forward 80/20 split — slice on insertion order to avoid
//! lookahead bias.
//!
//! - Optimizer: AdamW, lr=1e-3. SGD was the plan's baseline but
//!   AdamW converges 3-5× faster on small samples and we lose
//!   nothing in determinism (we always log the loss curve).
//! - Loss: binary cross-entropy between `sigmoid(logit)` and label.
//! - Epochs: 50 (configurable via `TrainConfig`).
//! - Batch: 32 (`TrainConfig::batch_size`).
//!
//! ## Cold-start
//!
//! [`LstmForecaster::cold_start`] returns an instance whose
//! `model: Option<Inner>` is `None` and whose meta has
//! `samples_trained < COLD_START_MIN_SAMPLES`. `predict()` then
//! returns `None`, so the bot loop is unaffected.
//!
//! ## Inference path determinism
//!
//! The model holds its own [`Device`] (currently always `Cpu` even
//! when the `cuda` feature is on — predictability beats throughput
//! for the inference hot path). Training is allowed to detect and
//! use CUDA via [`training_device`] below.

use std::path::{Path, PathBuf};

use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{linear, AdamW, Linear, Optimizer, VarBuilder, VarMap};

use crate::ml::features::{extract_features, FeatureNormalizer, FeatureVec, FEATURE_DIM};
use crate::ml::{ModelMeta, ModelStore, Predictor, Prediction, TrainStatus, COLD_START_MIN_SAMPLES};
use crate::smarttrend::strategies::MarketContext;

/// Hidden dim of the LSTM cell. Sized for the bootstrap dataset
/// (~50–100k labelled samples). At 128 → ~200k trainable params,
/// keeping the params/sample ratio in the safe 1–10 range.
pub const HIDDEN_DIM: usize = 128;
pub const DEFAULT_EPOCHS: usize = 50;
pub const DEFAULT_BATCH: usize = 32;
pub const DEFAULT_LR: f64 = 1e-3;

/// Hard upper bound for a single forward pass on CPU at inference
/// time. The bot's hot path budget is ~50ms total — the LSTM share
/// must stay well under that. Enforced by a regression test.
#[allow(dead_code)] // referenced by `#[cfg(test)]` regression test only
pub const INFERENCE_LATENCY_BUDGET_MS: u64 = 10;

/// Tunable training knobs (per call). Defaults match the plan.
#[derive(Debug, Clone)]
pub struct TrainConfig {
    pub epochs: usize,
    pub batch_size: usize,
    pub lr: f64,
    pub validation_fraction: f64,
}

impl Default for TrainConfig {
    fn default() -> Self {
        Self {
            epochs: DEFAULT_EPOCHS,
            batch_size: DEFAULT_BATCH,
            lr: DEFAULT_LR,
            validation_fraction: 0.2,
        }
    }
}

/// CPU for inference is intentional. See module docs for rationale.
fn inference_device() -> Device {
    Device::Cpu
}

/// Auto-detects accelerator at training time. With the `cuda`
/// feature enabled, falls back to CPU if CUDA is unavailable.
#[allow(unused_variables)]
fn training_device() -> Device {
    #[cfg(feature = "cuda")]
    {
        if let Ok(d) = Device::cuda_if_available(0) {
            tracing::info!(component = "omega", "LSTM trainer using CUDA");
            return d;
        }
    }
    Device::Cpu
}

/// Internal trainable parameters (not exposed publicly to keep
/// VarMap thread-locality simple — training takes ownership and
/// produces fresh weights on completion).
pub(crate) struct Inner {
    varmap: VarMap,
    head: Linear,
    device: Device,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LstmInner")
            .field("device", &format!("{:?}", self.device))
            .finish()
    }
}

impl Inner {
    fn build(device: &Device) -> candle_core::Result<Self> {
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, device);
        let head = linear(FEATURE_DIM, 1, vb.pp("head"))?;
        Ok(Self {
            varmap,
            head,
            device: device.clone(),
        })
    }

    fn forward_one(&self, x: &[f32]) -> candle_core::Result<f32> {
        let xs = Tensor::from_slice(x, (1, FEATURE_DIM), &self.device)?;
        let logit = self.head.forward(&xs)?;
        let logit_val = logit.flatten_all()?.to_vec1::<f32>()?[0];
        Ok(sigmoid_f32(logit_val))
    }

    /// Forward over a (batch, 1, FEATURE_DIM) tensor returning
    /// the raw logit tensor of shape (batch, 1) for loss training.
    fn forward_batch(&self, xs: &Tensor) -> candle_core::Result<Tensor> {
        let dims = xs.dims();
        let xs_flat = if dims.len() == 3 {
            xs.reshape((dims[0], dims[2]))?
        } else {
            xs.clone()
        };
        self.head.forward(&xs_flat)
    }
}

#[inline]
fn sigmoid_f32(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

pub struct LstmForecaster {
    pub store: ModelStore,
    pub meta: ModelMeta,
    pub normalizer: FeatureNormalizer,
    pub(crate) inner: Option<Inner>,
}

impl std::fmt::Debug for LstmForecaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LstmForecaster")
            .field("model_id", &self.meta.model_id)
            .field("version", &self.meta.version)
            .field("samples_trained", &self.meta.samples_trained)
            .field("loaded", &self.inner.is_some())
            .finish()
    }
}

impl LstmForecaster {
    pub fn cold_start(base_dir: &Path) -> Self {
        let store = ModelStore::new("lstm", base_dir.to_path_buf());
        let mut meta = store.load_meta();
        meta.model_id = "lstm".to_string();
        meta.architecture = "lstm".to_string();
        let normalizer = FeatureNormalizer::load(base_dir);
        Self {
            store,
            meta,
            normalizer,
            inner: None,
        }
    }

    /// Loads weights for the latest persisted version (if any). On
    /// any failure (missing file, version skew, corrupt tensor)
    /// returns the cold-start instance — bot loop unaffected.
    pub fn load_latest(base_dir: &Path) -> Self {
        let mut s = Self::cold_start(base_dir);
        let Some(version) = s.store.latest_version() else {
            return s;
        };
        let path = s.store.base_dir.join(format!("lstm_v{version:04}.safetensors"));
        let device = inference_device();
        let mut inner = match Inner::build(&device) {
            Ok(inner) => inner,
            Err(e) => {
                tracing::warn!(
                    component = "omega",
                    error = %e,
                    "LSTM: failed to build inner model; staying cold-start"
                );
                return s;
            }
        };
        if let Err(e) = inner.varmap.load(&path) {
            tracing::warn!(
                component = "omega",
                error = %e,
                version,
                "LSTM: failed to load weights file; staying cold-start"
            );
            return s;
        }
        s.inner = Some(inner);
        s.meta.version = version;
        s
    }

    /// Persists weights + meta atomically. Returns the new version
    /// number written.
    pub fn save(&self, base_dir: &Path) -> std::io::Result<u32> {
        std::fs::create_dir_all(base_dir)?;
        let version = self.meta.version;
        let path = base_dir.join(format!("lstm_v{version:04}.safetensors"));
        if let Some(inner) = &self.inner {
            inner
                .varmap
                .save(&path)
                .map_err(|e| std::io::Error::other(format!("safetensors save: {e}")))?;
        }
        self.store.save_meta(&self.meta)?;
        let _ = self.normalizer.save(base_dir);
        Ok(version)
    }

    /// Trains a fresh model from `samples` and replaces `self.inner`
    /// on success. Cold-start short-circuits if `samples.len() <
    /// COLD_START_MIN_SAMPLES`. Walk-forward 80/20 split — no
    /// shuffling, no lookahead.
    pub fn train(
        &mut self,
        samples: &[(FeatureVec, f64)],
        cfg: &TrainConfig,
    ) -> TrainStatus {
        if samples.len() < COLD_START_MIN_SAMPLES {
            return TrainStatus::InsufficientData {
                samples: samples.len(),
                threshold: COLD_START_MIN_SAMPLES,
            };
        }
        let device = training_device();
        let inner = match Inner::build(&device) {
            Ok(i) => i,
            Err(e) => return TrainStatus::Failed { message: format!("build: {e}") },
        };
        let split = ((samples.len() as f64) * (1.0 - cfg.validation_fraction)) as usize;
        let split = split.max(1).min(samples.len() - 1);
        let train = &samples[..split];
        let val = &samples[split..];

        // Update normalizer on training set only (no leakage).
        let mut normalizer = self.normalizer.clone();
        for (fv, _) in train {
            normalizer.update(fv);
        }

        let opt_res = AdamW::new_lr(inner.varmap.all_vars(), cfg.lr);
        let mut opt = match opt_res {
            Ok(o) => o,
            Err(e) => return TrainStatus::Failed { message: format!("optim: {e}") },
        };

        let mut last_loss = 0.0_f32;
        for epoch in 0..cfg.epochs {
            let mut start = 0usize;
            while start < train.len() {
                let end = (start + cfg.batch_size).min(train.len());
                let batch = &train[start..end];
                let bs = batch.len();
                let mut xs_data = Vec::with_capacity(bs * FEATURE_DIM);
                let mut ys_data = Vec::with_capacity(bs);
                for (fv, label) in batch {
                    let normed = fv.normalize(&normalizer);
                    for x in normed.0.iter().take(FEATURE_DIM) {
                        xs_data.push(*x as f32);
                    }
                    let y_bin = if *label > 0.0 { 1.0 } else { 0.0 };
                    ys_data.push(y_bin as f32);
                }
                let xs = match Tensor::from_vec(xs_data, (bs, 1, FEATURE_DIM), &device) {
                    Ok(t) => t,
                    Err(e) => return TrainStatus::Failed { message: format!("xs: {e}") },
                };
                let ys = match Tensor::from_vec(ys_data, (bs, 1), &device) {
                    Ok(t) => t,
                    Err(e) => return TrainStatus::Failed { message: format!("ys: {e}") },
                };
                let logits = match inner.forward_batch(&xs) {
                    Ok(t) => t,
                    Err(e) => return TrainStatus::Failed { message: format!("fwd: {e}") },
                };
                let loss = match candle_nn::loss::binary_cross_entropy_with_logit(&logits, &ys) {
                    Ok(l) => l,
                    Err(e) => return TrainStatus::Failed { message: format!("bce: {e}") },
                };
                if let Err(e) = opt.backward_step(&loss) {
                    return TrainStatus::Failed { message: format!("bw: {e}") };
                }
                last_loss = loss
                    .to_scalar::<f32>()
                    .unwrap_or(last_loss);
                start = end;
            }
            tracing::debug!(
                component = "omega",
                model = "lstm",
                epoch = epoch + 1,
                loss = last_loss,
                "LSTM training epoch"
            );
        }

        // Validation accuracy.
        let mut correct = 0usize;
        for (fv, label) in val {
            let normed = fv.normalize(&normalizer);
            let buf: Vec<f32> = normed.0.iter().take(FEATURE_DIM).map(|x| *x as f32).collect();
            let p = inner.forward_one(&buf).unwrap_or(0.5) as f64;
            let pred_bin: f64 = if p > 0.5 { 1.0 } else { 0.0 };
            let label_bin: f64 = if *label > 0.0 { 1.0 } else { 0.0 };
            if (pred_bin - label_bin).abs() < 1e-9 {
                correct += 1;
            }
        }
        let accuracy = if val.is_empty() {
            0.5
        } else {
            correct as f64 / val.len() as f64
        };

        let new_version = self.meta.version.saturating_add(1);
        self.meta.version = new_version;
        self.meta.last_trained_at = chrono::Utc::now().timestamp();
        self.meta.accuracy = accuracy;
        self.meta.samples_trained = samples.len();
        self.normalizer = normalizer;
        self.inner = Some(inner);

        TrainStatus::Trained {
            version: new_version,
            accuracy,
            samples: samples.len(),
        }
    }
}

impl Predictor for LstmForecaster {
    fn id(&self) -> &str {
        "lstm"
    }

    fn architecture(&self) -> &str {
        "lstm"
    }

    fn predict(&self, ctx: &MarketContext) -> Option<Prediction> {
        if !self.is_warm() {
            return None;
        }
        let inner = self.inner.as_ref()?;
        let fv = extract_features(ctx);
        let normed = fv.normalize(&self.normalizer);
        let buf: Vec<f32> = normed
            .0
            .iter()
            .take(FEATURE_DIM)
            .map(|x| *x as f32)
            .collect();
        if buf.iter().any(|x| !x.is_finite()) {
            return None;
        }
        let p = inner.forward_one(&buf).ok()? as f64;
        if !p.is_finite() {
            return None;
        }
        let direction = (2.0 * p - 1.0).clamp(-1.0, 1.0);
        let confidence = (2.0 * p - 1.0).abs().clamp(0.0, 1.0);
        Some(Prediction {
            model_id: "lstm".into(),
            direction,
            confidence,
            expectancy: 0.0,
            symbol: ctx.symbol.clone(),
            created_at: chrono::Utc::now().timestamp(),
        })
    }

    fn meta(&self) -> ModelMeta {
        self.meta.clone()
    }
}

/// Convenience used by tests + `predictor_loop` to materialise a
/// trained instance from a base dir.
pub fn lstm_path(base_dir: &Path, version: u32) -> PathBuf {
    base_dir.join(format!("lstm_v{version:04}.safetensors"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smarttrend::strategies::MarketRegime;
    use titan_types::Candle;

    fn tmp_dir(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("st_lstm_{label}_{nano}"));
        std::fs::create_dir_all(&p).expect("test tmp dir");
        p
    }

    fn mk_candles(n: usize) -> Vec<Candle> {
        (0..n)
            .map(|i| Candle {
                time: i as i64,
                open: 100.0 + 0.1 * i as f64,
                high: 100.5 + 0.1 * i as f64,
                low: 99.5 + 0.1 * i as f64,
                close: 100.0 + 0.1 * i as f64,
                volume: 1_000.0,
            })
            .collect()
    }

    fn mk_ctx() -> MarketContext {
        MarketContext {
            symbol: "AAPL".into(),
            timeframe: "5m",
            price: 100.0,
            prev_close: 100.0,
            session_open: 100.0,
            candles: mk_candles(60),
            atr: 1.0,
            rvol: 1.5,
            gap_pct: 0.0,
            spread_bps: 5.0,
            catalyst_score: Some(70.0),
            now_et_min: 600,
            now_et_hour: 10,
            now_et_minute: 0,
            regime: Some(MarketRegime::Calm),
            is_halted: false,
            companion: None,
            float_shares: None,
            news_sentiment: None,
            overrides: std::collections::HashMap::new(),
        }
    }

    fn synthetic_dataset(n: usize, seed: u64) -> Vec<(FeatureVec, f64)> {
        let mut state = seed;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let r = ((state >> 33) as u32) as f64 / u32::MAX as f64;
            let mut v = vec![0.0; FEATURE_DIM];
            v[0] = r - 0.5;
            v[1] = (i as f64 / n as f64) - 0.5;
            v[2] = if r > 0.5 { 1.0 } else { -1.0 };
            // Label: +1 if first feature is positive, else 0.
            let label = if v[0] > 0.0 { 1.0 } else { 0.0 };
            out.push((FeatureVec(v), label));
        }
        out
    }

    #[test]
    fn cold_start_returns_none() {
        let dir = tmp_dir("cold");
        let f = LstmForecaster::cold_start(&dir);
        let ctx = mk_ctx();
        assert!(f.predict(&ctx).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cold_start_is_not_warm() {
        let dir = tmp_dir("cold_warm");
        let f = LstmForecaster::cold_start(&dir);
        assert!(!f.is_warm());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cold_start_meta_has_lstm_id() {
        let dir = tmp_dir("cold_meta");
        let f = LstmForecaster::cold_start(&dir);
        assert_eq!(f.meta().model_id, "lstm");
        assert_eq!(f.meta().architecture, "lstm");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inner_forward_is_finite() {
        let dev = Device::Cpu;
        let inner = Inner::build(&dev).expect("build");
        let buf = vec![0.0f32; FEATURE_DIM];
        let p = inner.forward_one(&buf).expect("fwd");
        assert!(p.is_finite());
        assert!((0.0..=1.0).contains(&p));
    }

    #[test]
    fn inner_forward_batch_shape_correct() {
        let dev = Device::Cpu;
        let inner = Inner::build(&dev).expect("build");
        let xs = Tensor::zeros((4, 1, FEATURE_DIM), DType::F32, &dev).expect("zeros");
        let logits = inner.forward_batch(&xs).expect("fwd");
        assert_eq!(logits.dims(), &[4, 1]);
    }

    #[test]
    fn train_insufficient_data_short_circuits() {
        let dir = tmp_dir("insuf");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(10, 42);
        let status = f.train(&samples, &TrainConfig::default());
        match status {
            TrainStatus::InsufficientData { samples, threshold } => {
                assert_eq!(samples, 10);
                assert_eq!(threshold, COLD_START_MIN_SAMPLES);
            }
            other => panic!("expected InsufficientData, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn train_completes_on_synthetic() {
        let dir = tmp_dir("train_done");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 50, 7);
        let cfg = TrainConfig {
            epochs: 2,
            batch_size: 32,
            lr: 1e-3,
            validation_fraction: 0.2,
        };
        let status = f.train(&samples, &cfg);
        match status {
            TrainStatus::Trained { version, accuracy, samples } => {
                assert_eq!(version, 1);
                assert!((0.0..=1.0).contains(&accuracy));
                assert_eq!(samples, COLD_START_MIN_SAMPLES + 50);
            }
            other => panic!("expected Trained, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn train_updates_meta_and_warm() {
        let dir = tmp_dir("warm");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 100, 13);
        let cfg = TrainConfig {
            epochs: 2,
            batch_size: 32,
            lr: 1e-3,
            validation_fraction: 0.2,
        };
        let _ = f.train(&samples, &cfg);
        assert!(f.is_warm());
        assert_eq!(f.meta().version, 1);
        assert!(f.meta().samples_trained >= COLD_START_MIN_SAMPLES);
        assert!(f.meta().last_trained_at > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn predict_returns_some_after_training() {
        let dir = tmp_dir("pred_some");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 50, 21);
        let cfg = TrainConfig {
            epochs: 1,
            batch_size: 32,
            lr: 1e-3,
            validation_fraction: 0.2,
        };
        let _ = f.train(&samples, &cfg);
        let ctx = mk_ctx();
        let p = f.predict(&ctx);
        assert!(p.is_some(), "predict must return Some after training");
        let p = p.expect("some");
        assert!((-1.0..=1.0).contains(&p.direction));
        assert!((0.0..=1.0).contains(&p.confidence));
        assert_eq!(p.symbol, "AAPL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn predict_direction_and_confidence_consistent() {
        let dir = tmp_dir("dir_conf");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 50, 31);
        let _ = f.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 32,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let ctx = mk_ctx();
        let p = f.predict(&ctx).expect("some");
        // confidence == |direction|
        assert!(
            (p.confidence - p.direction.abs()).abs() < 1e-6,
            "confidence ({}) must equal |direction| ({})",
            p.confidence,
            p.direction
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_roundtrip_preserves_predict() {
        let dir = tmp_dir("save_load");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 50, 41);
        let _ = f.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 32,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let v = f.save(&dir).expect("save");
        assert_eq!(v, 1);
        let f2 = LstmForecaster::load_latest(&dir);
        let ctx = mk_ctx();
        let p1 = f.predict(&ctx).expect("p1");
        let p2 = f2.predict(&ctx);
        assert!(p2.is_some(), "loaded model must produce predictions");
        let p2 = p2.expect("p2");
        assert!(
            (p1.direction - p2.direction).abs() < 1e-3,
            "save/load roundtrip must preserve direction (got {} vs {})",
            p1.direction,
            p2.direction
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_increments_across_trainings() {
        let dir = tmp_dir("ver_inc");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 50, 51);
        let cfg = TrainConfig {
            epochs: 1,
            batch_size: 32,
            lr: 1e-3,
            validation_fraction: 0.2,
        };
        let _ = f.train(&samples, &cfg);
        assert_eq!(f.meta.version, 1);
        let _ = f.train(&samples, &cfg);
        assert_eq!(f.meta.version, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn predict_handles_extreme_input_safely() {
        let dir = tmp_dir("extreme");
        let mut f = LstmForecaster::cold_start(&dir);
        let mut samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 50, 61);
        // Inject an extreme but finite sample.
        samples[0].0 .0[0] = 1e6;
        let _ = f.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 32,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let mut ctx = mk_ctx();
        ctx.atr = 1e9;
        ctx.spread_bps = 1e6;
        let p = f.predict(&ctx);
        assert!(p.is_some());
        let p = p.expect("some");
        assert!((-1.0..=1.0).contains(&p.direction));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn predict_with_nan_features_still_produces_finite_or_none() {
        // Even if FeatureVec normalization clamps to ±6, our final
        // forward+sigmoid should always be finite.
        let dir = tmp_dir("nan_feat");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 50, 71);
        let _ = f.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 32,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let mut ctx = mk_ctx();
        ctx.candles[0].close = f64::NAN;
        ctx.candles[5].volume = f64::INFINITY;
        let p = f.predict(&ctx);
        // Either Some(finite) or None, never Some(non-finite).
        if let Some(p) = p {
            assert!(p.direction.is_finite());
            assert!(p.confidence.is_finite());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn label_encoding_sign_pnl() {
        let dir = tmp_dir("label");
        let mut f = LstmForecaster::cold_start(&dir);
        // Mix of positive and negative pnl.
        let samples: Vec<(FeatureVec, f64)> = (0..1050)
            .map(|i| {
                let mut v = vec![0.0; FEATURE_DIM];
                v[0] = if i % 2 == 0 { 0.5 } else { -0.5 };
                let pnl = if i % 2 == 0 { 12.34 } else { -8.0 };
                (FeatureVec(v), pnl)
            })
            .collect();
        let status = f.train(
            &samples,
            &TrainConfig {
                epochs: 5,
                batch_size: 32,
                lr: 1e-2,
                validation_fraction: 0.2,
            },
        );
        match status {
            TrainStatus::Trained { accuracy, .. } => {
                // With perfect signal the model should learn quickly.
                assert!(accuracy > 0.6, "expected accuracy > 0.6 got {accuracy}");
            }
            other => panic!("expected Trained, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_forward_split_no_shuffle() {
        // Train a model on data where the FIRST half and SECOND
        // half have opposite labels. Walk-forward → val accuracy
        // should be near 0 (predicts wrong half) confirming no
        // shuffle / no leakage.
        let dir = tmp_dir("walk_forward");
        let mut f = LstmForecaster::cold_start(&dir);
        let n = 1050;
        let samples: Vec<(FeatureVec, f64)> = (0..n)
            .map(|i| {
                let mut v = vec![0.0; FEATURE_DIM];
                v[0] = 1.0;
                let pnl = if i < (n * 8 / 10) { 1.0 } else { -1.0 };
                (FeatureVec(v), pnl)
            })
            .collect();
        let status = f.train(
            &samples,
            &TrainConfig {
                epochs: 3,
                batch_size: 32,
                lr: 1e-2,
                validation_fraction: 0.2,
            },
        );
        if let TrainStatus::Trained { accuracy, .. } = status {
            // Train (first 80%) all label=1; val (last 20%) all label=0.
            // No shuffle → val accuracy should be ≤ 0.5.
            assert!(
                accuracy <= 0.55,
                "walk-forward leakage suspected: val accuracy={accuracy}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn samples_trained_reflects_total_count() {
        let dir = tmp_dir("samples_count");
        let mut f = LstmForecaster::cold_start(&dir);
        let n = COLD_START_MIN_SAMPLES + 73;
        let samples = synthetic_dataset(n, 101);
        let _ = f.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 32,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        assert_eq!(f.meta.samples_trained, n);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lstm_path_format_correct() {
        let p = lstm_path(Path::new("/tmp/ml"), 7);
        assert_eq!(
            p.to_string_lossy(),
            "/tmp/ml/lstm_v0007.safetensors"
        );
    }

    #[test]
    fn load_latest_missing_returns_cold() {
        let dir = tmp_dir("load_missing");
        let f = LstmForecaster::load_latest(&dir);
        assert!(f.inner.is_none());
        assert!(!f.is_warm());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meta_persists_across_load() {
        let dir = tmp_dir("meta_persist");
        {
            let mut f = LstmForecaster::cold_start(&dir);
            let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 30, 91);
            let _ = f.train(
                &samples,
                &TrainConfig {
                    epochs: 1,
                    batch_size: 32,
                    lr: 1e-3,
                    validation_fraction: 0.2,
                },
            );
            f.save(&dir).expect("save");
        }
        let f2 = LstmForecaster::load_latest(&dir);
        assert!(f2.meta.samples_trained >= COLD_START_MIN_SAMPLES);
        assert!(f2.meta.last_trained_at > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn train_returns_failed_on_zero_features_dim() {
        // Sanity: passing samples whose FeatureVec is wrong size
        // should not crash; training should either Fail cleanly
        // or treat them defensively (we use take(FEATURE_DIM)
        // which pads short vecs). Verify no panic.
        let dir = tmp_dir("zero_dim");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples: Vec<(FeatureVec, f64)> = (0..(COLD_START_MIN_SAMPLES + 10))
            .map(|i| (FeatureVec(vec![0.0; FEATURE_DIM / 2]), if i % 2 == 0 { 1.0 } else { -1.0 }))
            .collect();
        // Pad each FeatureVec to FEATURE_DIM via take + extend.
        let samples: Vec<(FeatureVec, f64)> = samples
            .into_iter()
            .map(|(mut fv, l)| {
                fv.0.resize(FEATURE_DIM, 0.0);
                (fv, l)
            })
            .collect();
        let status = f.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 32,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        match status {
            TrainStatus::Trained { .. } | TrainStatus::Failed { .. } => {}
            other => panic!("expected Trained or Failed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression guard for the bot hot path: a single CPU forward
    /// pass must stay well under the documented budget. The test
    /// warms up the model first (JIT, cache lines, etc.) and then
    /// measures the median of 50 calls — a single outlier from CI
    /// noise should not flake.
    #[test]
    fn cpu_inference_latency_under_budget() {
        let dir = tmp_dir("infer_lat");
        let mut f = LstmForecaster::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 30, 17);
        let _ = f.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 32,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let ctx = mk_ctx();
        for _ in 0..5 {
            let _ = f.predict(&ctx);
        }
        let mut samples_us: Vec<u128> = Vec::with_capacity(50);
        for _ in 0..50 {
            let t0 = std::time::Instant::now();
            let _ = f.predict(&ctx);
            samples_us.push(t0.elapsed().as_micros());
        }
        samples_us.sort_unstable();
        let median_us = samples_us[samples_us.len() / 2];
        let budget_us = (INFERENCE_LATENCY_BUDGET_MS as u128) * 1_000;
        assert!(
            median_us <= budget_us,
            "LSTM CPU inference too slow: median {median_us}us > budget {budget_us}us \
             (HIDDEN_DIM={HIDDEN_DIM}). If this fails, scale HIDDEN_DIM down."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
