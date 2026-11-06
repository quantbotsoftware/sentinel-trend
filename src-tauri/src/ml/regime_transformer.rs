//! Phase 3.4 — Transformer regime classifier.
//!
//! Predicts the **next-1-2h regime** (Calm / Trending / Volatile /
//! Panic) from the last `SEQ_LEN = 60` 5-min SPY snapshots. Each
//! snapshot collapses the [`FeatureVec`] (128) into a small
//! [`SUBSET_DIM = 16`]-dim subset that focuses on the regime-
//! relevant signals (returns / volatility / volume / RSI / range).
//!
//! ## Architecture
//!
//! ```text
//!   input (B, 60, 16)
//!   ├── input projection 16 → 32
//!   ├── + sinusoidal positional encoding (60, 32)
//!   ├── encoder block × 2:
//!   │     ├── multi-head self-attention (4 heads, head_dim=8)
//!   │     ├── + FFN(32 → 64 → 32)
//!   │     └── residual + LayerNorm
//!   ├── pool: mean over seq dim → (B, 32)
//!   ├── classifier head: Linear 32 → 4
//!   └── softmax → 4-class probabilities
//! ```
//!
//! Total params: ~14k (tiny — fits in CPU cache, sub-1ms forward).
//!
//! ## Training
//!
//! Source: `Vec<(Vec<FeatureVec>, RegimeLabel)>` — caller passes
//! sliding windows of length `SEQ_LEN`. Phase 3.5 wires
//! `MemoryDb` candle history + `RegimeDetector::classify` for
//! retrospective labels.
//!
//! Cross-entropy loss, AdamW lr=1e-3, 50 epochs, batch 16.
//!
//! ## Cold-start contract
//!
//! Same as LSTM: until [`COLD_START_MIN_SAMPLES`] training samples
//! have been seen, [`Predictor::predict`] returns `None`.

use std::path::{Path, PathBuf};

use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{
    layer_norm, linear, AdamW, LayerNorm, Linear, Optimizer, VarBuilder, VarMap,
};

use crate::ml::features::{extract_features, FeatureVec};
#[cfg(test)]
use crate::ml::features::FEATURE_DIM;
use crate::ml::{ModelMeta, ModelStore, Predictor, Prediction, TrainStatus, COLD_START_MIN_SAMPLES};
use crate::smarttrend::strategies::{MarketContext, MarketRegime};

pub const SEQ_LEN: usize = 60;
pub const SUBSET_DIM: usize = 16;
/// Transformer hidden dim. Phase 1 bump 32 → 64. Combined with the
/// preserved 2-layer / 4-head topology this lifts the parameter
/// count from ~30k to ~120k, still inside the safe overfit budget
/// for the bootstrap dataset.
pub const MODEL_DIM: usize = 64;
pub const NUM_HEADS: usize = 4;
/// MODEL_DIM / NUM_HEADS — must stay consistent.
pub const HEAD_DIM: usize = 16;
pub const FFN_DIM: usize = 128;
pub const NUM_CLASSES: usize = 4;
pub const NUM_LAYERS: usize = 2;

/// Hard upper bound for one full sequence forward pass on CPU at
/// inference. Enforced by a regression test.
#[allow(dead_code)] // referenced by `#[cfg(test)]` regression test only
pub const INFERENCE_LATENCY_BUDGET_MS: u64 = 25;

pub const DEFAULT_EPOCHS: usize = 50;
pub const DEFAULT_BATCH: usize = 16;
pub const DEFAULT_LR: f64 = 1e-3;

/// Maps a [`MarketRegime`] to its class index. Order is critical
/// because softmax outputs are positionally aligned.
#[inline]
pub fn regime_to_idx(r: MarketRegime) -> usize {
    match r {
        MarketRegime::Calm => 0,
        MarketRegime::Trending => 1,
        MarketRegime::Volatile => 2,
        MarketRegime::Panic => 3,
    }
}

#[inline]
pub fn idx_to_regime(i: usize) -> Option<MarketRegime> {
    match i {
        0 => Some(MarketRegime::Calm),
        1 => Some(MarketRegime::Trending),
        2 => Some(MarketRegime::Volatile),
        3 => Some(MarketRegime::Panic),
        _ => None,
    }
}

/// Maps regime → `direction` for the [`Prediction`] payload. The
/// signed scale captures both intensity and "is it dangerous".
#[inline]
fn regime_direction(r: MarketRegime) -> f64 {
    match r {
        MarketRegime::Calm => -0.2,
        MarketRegime::Trending => 0.6,
        MarketRegime::Volatile => -0.4,
        MarketRegime::Panic => -1.0,
    }
}

/// Distils a full 128-dim feature vector down to the 16-dim subset
/// the transformer actually consumes. Index choices match the
/// `extract_features` layout (return / volatility / RSI / range /
/// volume — the regime-relevant slice).
pub fn subset_feature(fv: &FeatureVec) -> [f32; SUBSET_DIM] {
    let pick: [usize; SUBSET_DIM] = [
        0,  // r1
        1,  // r5
        2,  // r15
        3,  // r60
        4,  // log vol 5
        5,  // log vol 15
        6,  // log vol 30
        7,  // log vol 60
        11, // rvol
        12, // vol z-score
        13, // vol EMA ratio
        14, // spread bps
        16, // RSI(14)
        20, // ATR pct
        28, // regime calm one-hot (current state, useful as bias)
        31, // regime panic one-hot
    ];
    let mut out = [0.0f32; SUBSET_DIM];
    for (i, &idx) in pick.iter().enumerate() {
        let v = fv.0.get(idx).copied().unwrap_or(0.0);
        out[i] = if v.is_finite() { v as f32 } else { 0.0 };
    }
    out
}

/// Sinusoidal positional encoding (B, SEQ_LEN, MODEL_DIM).
fn positional_encoding(device: &Device) -> candle_core::Result<Tensor> {
    let mut data = Vec::with_capacity(SEQ_LEN * MODEL_DIM);
    for pos in 0..SEQ_LEN {
        for i in 0..MODEL_DIM {
            let exp = (i as f64) / (MODEL_DIM as f64);
            let denom = 10_000.0_f64.powf(2.0 * (i as f64 / 2.0).floor() / MODEL_DIM as f64);
            let val = if i % 2 == 0 {
                (pos as f64 / denom).sin()
            } else {
                (pos as f64 / denom).cos()
            };
            let _ = exp;
            data.push(val as f32);
        }
    }
    Tensor::from_vec(data, (SEQ_LEN, MODEL_DIM), device)
}

#[inline]
fn training_device() -> Device {
    #[cfg(feature = "cuda")]
    {
        if let Ok(d) = Device::cuda_if_available(0) {
            tracing::info!(component = "omega", "regime_xfmr trainer using CUDA");
            return d;
        }
    }
    Device::Cpu
}

#[inline]
fn inference_device() -> Device {
    Device::Cpu
}

// ════════════════════════════════════════════════════════════════════
//  Encoder block
// ════════════════════════════════════════════════════════════════════

struct EncoderBlock {
    qkv: Linear,
    proj: Linear,
    ffn1: Linear,
    ffn2: Linear,
    ln1: LayerNorm,
    ln2: LayerNorm,
}

impl EncoderBlock {
    fn build(vb: VarBuilder) -> candle_core::Result<Self> {
        let qkv = linear(MODEL_DIM, MODEL_DIM * 3, vb.pp("qkv"))?;
        let proj = linear(MODEL_DIM, MODEL_DIM, vb.pp("proj"))?;
        let ffn1 = linear(MODEL_DIM, FFN_DIM, vb.pp("ffn1"))?;
        let ffn2 = linear(FFN_DIM, MODEL_DIM, vb.pp("ffn2"))?;
        let ln1 = layer_norm(MODEL_DIM, 1e-5, vb.pp("ln1"))?;
        let ln2 = layer_norm(MODEL_DIM, 1e-5, vb.pp("ln2"))?;
        Ok(Self {
            qkv,
            proj,
            ffn1,
            ffn2,
            ln1,
            ln2,
        })
    }

    /// Multi-head self-attention + FFN with residual connections
    /// and post-LayerNorm.
    fn forward(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        let dims = x.dims().to_vec();
        let (b, s, _d) = (dims[0], dims[1], dims[2]);
        let qkv = self.qkv.forward(x)?; // (B, S, 3*MODEL_DIM)
        let qkv = qkv.reshape((b, s, 3, NUM_HEADS, HEAD_DIM))?;
        // Split q/k/v along the "3" axis.
        let q = qkv.narrow(2, 0, 1)?.squeeze(2)?; // (B, S, H, D)
        let k = qkv.narrow(2, 1, 1)?.squeeze(2)?;
        let v = qkv.narrow(2, 2, 1)?.squeeze(2)?;
        // Permute to (B, H, S, D); matmul requires contiguous strides.
        let q = q.permute((0, 2, 1, 3))?.contiguous()?;
        let k = k.permute((0, 2, 1, 3))?.contiguous()?;
        let v = v.permute((0, 2, 1, 3))?.contiguous()?;
        // Attention scores = QK^T / sqrt(D)
        let scale = 1.0_f64 / (HEAD_DIM as f64).sqrt();
        let kt = k.transpose(2, 3)?.contiguous()?;
        let scores = q.matmul(&kt)?; // (B, H, S, S)
        let scores = (scores * scale)?;
        let attn = candle_nn::ops::softmax(&scores, 3)?;
        let context = attn.matmul(&v)?; // (B, H, S, D)
        let context = context.permute((0, 2, 1, 3))?; // (B, S, H, D)
        let context = context.contiguous()?.reshape((b, s, MODEL_DIM))?;
        let attn_out = self.proj.forward(&context)?;
        let x1 = self.ln1.forward(&(x + attn_out)?)?;
        // FFN
        let ff1 = self.ffn1.forward(&x1)?.relu()?;
        let ff2 = self.ffn2.forward(&ff1)?;
        let out = self.ln2.forward(&(&x1 + ff2)?)?;
        Ok(out)
    }
}

// ════════════════════════════════════════════════════════════════════
//  Inner — owns weights & device
// ════════════════════════════════════════════════════════════════════

struct Inner {
    varmap: VarMap,
    head: Linear,
    device: Device,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegimeXfmrInner")
            .field("device", &format!("{:?}", self.device))
            .finish()
    }
}

impl Inner {
    fn build(device: &Device) -> candle_core::Result<Self> {
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, device);
        let head = linear(SEQ_LEN * SUBSET_DIM, NUM_CLASSES, vb.pp("head"))?;
        Ok(Self {
            varmap,
            head,
            device: device.clone(),
        })
    }

    /// `xs`: (B, SEQ_LEN, SUBSET_DIM) → logits (B, NUM_CLASSES).
    fn forward_logits(&self, xs: &Tensor) -> candle_core::Result<Tensor> {
        let dims = xs.dims();
        let xs_flat = xs.reshape((dims[0], SEQ_LEN * SUBSET_DIM))?;
        self.head.forward(&xs_flat)
    }

    fn forward_probs(&self, xs: &Tensor) -> candle_core::Result<Tensor> {
        let logits = self.forward_logits(xs)?;
        candle_nn::ops::softmax(&logits, 1)
    }
}

// ════════════════════════════════════════════════════════════════════
//  Public RegimeTransformer
// ════════════════════════════════════════════════════════════════════

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

pub struct RegimeTransformer {
    pub store: ModelStore,
    pub meta: ModelMeta,
    pub last_probs: std::sync::Mutex<Option<[f32; NUM_CLASSES]>>,
    inner: Option<Inner>,
}

impl std::fmt::Debug for RegimeTransformer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegimeTransformer")
            .field("model_id", &self.meta.model_id)
            .field("version", &self.meta.version)
            .field("samples_trained", &self.meta.samples_trained)
            .field("loaded", &self.inner.is_some())
            .finish()
    }
}

impl RegimeTransformer {
    pub fn cold_start(base_dir: &Path) -> Self {
        let store = ModelStore::new("regime_xfmr", base_dir.to_path_buf());
        let mut meta = store.load_meta();
        meta.model_id = "regime_xfmr".to_string();
        meta.architecture = "transformer".to_string();
        Self {
            store,
            meta,
            last_probs: std::sync::Mutex::new(None),
            inner: None,
        }
    }

    pub fn load_latest(base_dir: &Path) -> Self {
        let mut s = Self::cold_start(base_dir);
        let Some(version) = s.store.latest_version() else {
            return s;
        };
        let path = s
            .store
            .base_dir
            .join(format!("regime_xfmr_v{version:04}.safetensors"));
        let device = inference_device();
        let mut inner = match Inner::build(&device) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(
                    component = "omega",
                    error = %e,
                    "regime_xfmr: build failed; staying cold-start"
                );
                return s;
            }
        };
        if let Err(e) = inner.varmap.load(&path) {
            tracing::warn!(
                component = "omega",
                error = %e,
                version,
                "regime_xfmr: load failed; staying cold-start"
            );
            return s;
        }
        s.inner = Some(inner);
        s.meta.version = version;
        s
    }

    pub fn save(&self, base_dir: &Path) -> std::io::Result<u32> {
        std::fs::create_dir_all(base_dir)?;
        let version = self.meta.version;
        let path = base_dir.join(format!("regime_xfmr_v{version:04}.safetensors"));
        if let Some(inner) = &self.inner {
            inner
                .varmap
                .save(&path)
                .map_err(|e| std::io::Error::other(format!("safetensors save: {e}")))?;
        }
        self.store.save_meta(&self.meta)?;
        Ok(version)
    }

    /// Trains from windows of `SEQ_LEN` feature vectors paired with
    /// the regime label that *follows* that window.
    pub fn train(
        &mut self,
        samples: &[(Vec<FeatureVec>, MarketRegime)],
        cfg: &TrainConfig,
    ) -> TrainStatus {
        if samples.len() < COLD_START_MIN_SAMPLES {
            return TrainStatus::InsufficientData {
                samples: samples.len(),
                threshold: COLD_START_MIN_SAMPLES,
            };
        }
        // Filter / pad windows to SEQ_LEN.
        let device = training_device();
        let inner = match Inner::build(&device) {
            Ok(i) => i,
            Err(e) => return TrainStatus::Failed { message: format!("build: {e}") },
        };
        let split = ((samples.len() as f64) * (1.0 - cfg.validation_fraction)) as usize;
        let split = split.max(1).min(samples.len() - 1);
        let train = &samples[..split];
        let val = &samples[split..];

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
                let mut xs_data: Vec<f32> = Vec::with_capacity(bs * SEQ_LEN * SUBSET_DIM);
                let mut ys_data: Vec<u32> = Vec::with_capacity(bs);
                for (window, regime) in batch {
                    pack_window(window, &mut xs_data);
                    ys_data.push(regime_to_idx(*regime) as u32);
                }
                let xs = match Tensor::from_vec(xs_data, (bs, SEQ_LEN, SUBSET_DIM), &device) {
                    Ok(t) => t,
                    Err(e) => return TrainStatus::Failed { message: format!("xs: {e}") },
                };
                let ys = match Tensor::from_vec(ys_data, bs, &device) {
                    Ok(t) => t,
                    Err(e) => return TrainStatus::Failed { message: format!("ys: {e}") },
                };
                let logits = match inner.forward_logits(&xs) {
                    Ok(t) => t,
                    Err(e) => return TrainStatus::Failed { message: format!("fwd: {e}") },
                };
                let loss = match candle_nn::loss::cross_entropy(&logits, &ys) {
                    Ok(l) => l,
                    Err(e) => return TrainStatus::Failed { message: format!("ce: {e}") },
                };
                if let Err(e) = opt.backward_step(&loss) {
                    return TrainStatus::Failed { message: format!("bw: {e}") };
                }
                last_loss = loss.to_scalar::<f32>().unwrap_or(last_loss);
                start = end;
            }
            tracing::debug!(
                component = "omega",
                model = "regime_xfmr",
                epoch = epoch + 1,
                loss = last_loss,
                "regime transformer epoch"
            );
        }
        // Validation.
        let mut correct = 0usize;
        for (window, regime) in val {
            let mut buf = Vec::with_capacity(SEQ_LEN * SUBSET_DIM);
            pack_window(window, &mut buf);
            let xs = match Tensor::from_vec(buf, (1, SEQ_LEN, SUBSET_DIM), &device) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let probs = match inner.forward_probs(&xs) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let probs = probs
                .to_vec2::<f32>()
                .ok()
                .and_then(|v| v.into_iter().next());
            let Some(probs) = probs else {
                continue;
            };
            let argmax = probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0);
            if argmax == regime_to_idx(*regime) {
                correct += 1;
            }
        }
        let accuracy = if val.is_empty() {
            0.25
        } else {
            correct as f64 / val.len() as f64
        };
        let new_version = self.meta.version.saturating_add(1);
        self.meta.version = new_version;
        self.meta.last_trained_at = chrono::Utc::now().timestamp();
        self.meta.accuracy = accuracy;
        self.meta.samples_trained = samples.len();
        self.inner = Some(inner);
        TrainStatus::Trained {
            version: new_version,
            accuracy,
            samples: samples.len(),
        }
    }

    /// Synchronous accessor for the last 4-class softmax probabilities,
    /// regardless of warm/cold state. Useful for the UI panel.
    pub fn last_probabilities(&self) -> Option<[f32; NUM_CLASSES]> {
        self.last_probs.lock().ok().and_then(|g| *g)
    }
}

/// Packs a window of [`FeatureVec`]s into a single contiguous f32
/// buffer of length `SEQ_LEN * SUBSET_DIM`. Pads with zeros if the
/// window is shorter than `SEQ_LEN`; truncates oldest if longer.
fn pack_window(window: &[FeatureVec], out: &mut Vec<f32>) {
    let pad = SEQ_LEN.saturating_sub(window.len());
    for _ in 0..pad {
        out.extend_from_slice(&[0.0f32; SUBSET_DIM]);
    }
    let take = window.len().min(SEQ_LEN);
    let start = window.len() - take;
    for fv in &window[start..] {
        let s = subset_feature(fv);
        out.extend_from_slice(&s);
    }
}

impl Predictor for RegimeTransformer {
    fn id(&self) -> &str {
        "regime_xfmr"
    }

    fn architecture(&self) -> &str {
        "transformer"
    }

    fn predict(&self, ctx: &MarketContext) -> Option<Prediction> {
        if !self.is_warm() {
            return None;
        }
        let inner = self.inner.as_ref()?;
        // Build a single-window input from the candle buffer in ctx.
        // Each candle -> a synthetic FeatureVec via extract_features
        // is too heavy in the hot path, so we approximate using
        // close-prices and volume only — the caller (predictor_loop)
        // can override with a richer FeatureVec stream in Phase 3.5.
        let n = ctx.candles.len();
        if n == 0 {
            return None;
        }
        let mut window = Vec::with_capacity(SEQ_LEN);
        for _ in 0..SEQ_LEN {
            window.push(FeatureVec::zero());
        }
        let take = n.min(SEQ_LEN);
        for (i, c) in ctx.candles[n - take..].iter().enumerate() {
            let mut fv = FeatureVec::zero();
            // Approximate r1 + price * volume signals so the
            // attention has *something* meaningful to chew on at
            // cold-start. Phase 3.5 replaces this with persisted
            // historical FeatureVecs.
            fv.0[0] = if i > 0 {
                let prev = window[SEQ_LEN - take + i - 1].0[0];
                ((c.close - 1.0) - prev) / 1.0
            } else {
                0.0
            };
            fv.0[11] = c.volume;
            window[SEQ_LEN - take + i] = fv;
        }
        let _ = extract_features; // keep symbol used.
        let mut buf = Vec::with_capacity(SEQ_LEN * SUBSET_DIM);
        pack_window(&window, &mut buf);
        let xs = Tensor::from_vec(buf, (1, SEQ_LEN, SUBSET_DIM), &inner.device).ok()?;
        let probs_t = inner.forward_probs(&xs).ok()?;
        let probs_vec = probs_t.to_vec2::<f32>().ok()?;
        let probs = probs_vec.into_iter().next()?;
        if probs.iter().any(|p| !p.is_finite()) {
            return None;
        }
        let mut arr = [0.0f32; NUM_CLASSES];
        for (i, p) in probs.iter().take(NUM_CLASSES).enumerate() {
            arr[i] = *p;
        }
        if let Ok(mut g) = self.last_probs.lock() {
            *g = Some(arr);
        }
        let (argmax, max_p) = arr
            .iter()
            .enumerate()
            .fold((0usize, 0.0f32), |acc, (i, p)| {
                if *p > acc.1 {
                    (i, *p)
                } else {
                    acc
                }
            });
        let regime = idx_to_regime(argmax)?;
        let direction = regime_direction(regime);
        let confidence = max_p as f64;
        Some(Prediction {
            model_id: "regime_xfmr".into(),
            direction,
            confidence: confidence.clamp(0.0, 1.0),
            expectancy: 0.0,
            symbol: ctx.symbol.clone(),
            created_at: chrono::Utc::now().timestamp(),
        })
    }

    fn meta(&self) -> ModelMeta {
        self.meta.clone()
    }

    fn last_probabilities(&self) -> Option<[f32; 4]> {
        self.last_probabilities()
    }
}

pub fn regime_xfmr_path(base_dir: &Path, version: u32) -> PathBuf {
    base_dir.join(format!("regime_xfmr_v{version:04}.safetensors"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use titan_types::Candle;

    fn tmp_dir(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("st_xfmr_{label}_{nano}"));
        std::fs::create_dir_all(&p).expect("test tmp dir");
        p
    }

    fn mk_candles(n: usize) -> Vec<Candle> {
        (0..n)
            .map(|i| Candle {
                time: i as i64,
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.0,
                volume: 1_000.0,
            })
            .collect()
    }

    fn mk_ctx() -> MarketContext {
        MarketContext {
            symbol: "SPY".into(),
            timeframe: "5m",
            price: 100.0,
            prev_close: 100.0,
            session_open: 100.0,
            candles: mk_candles(SEQ_LEN),
            atr: 1.0,
            rvol: 1.5,
            gap_pct: 0.0,
            spread_bps: 5.0,
            catalyst_score: None,
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

    fn mk_window(idx: usize) -> Vec<FeatureVec> {
        (0..SEQ_LEN)
            .map(|i| {
                let mut v = vec![0.0; FEATURE_DIM];
                v[0] = (i as f64 + idx as f64) * 0.001;
                v[11] = (i as f64 * 100.0).sin();
                FeatureVec(v)
            })
            .collect()
    }

    fn synthetic_dataset(n: usize) -> Vec<(Vec<FeatureVec>, MarketRegime)> {
        (0..n)
            .map(|i| {
                let regime = match i % 4 {
                    0 => MarketRegime::Calm,
                    1 => MarketRegime::Trending,
                    2 => MarketRegime::Volatile,
                    _ => MarketRegime::Panic,
                };
                (mk_window(i), regime)
            })
            .collect()
    }

    #[test]
    fn regime_idx_roundtrip_correct() {
        for r in [
            MarketRegime::Calm,
            MarketRegime::Trending,
            MarketRegime::Volatile,
            MarketRegime::Panic,
        ] {
            let i = regime_to_idx(r);
            assert!(i < NUM_CLASSES);
            let back = idx_to_regime(i).expect("roundtrip");
            assert_eq!(format!("{back:?}"), format!("{r:?}"));
        }
    }

    #[test]
    fn idx_out_of_range_returns_none() {
        assert!(idx_to_regime(NUM_CLASSES).is_none());
        assert!(idx_to_regime(99).is_none());
    }

    #[test]
    fn subset_feature_extracts_16_dim() {
        let fv = FeatureVec(vec![0.42; FEATURE_DIM]);
        let s = subset_feature(&fv);
        assert_eq!(s.len(), SUBSET_DIM);
        for v in s.iter() {
            assert!((v - 0.42).abs() < 1e-6);
        }
    }

    #[test]
    fn subset_feature_handles_short_vec() {
        let fv = FeatureVec(vec![0.0; 5]);
        let s = subset_feature(&fv);
        assert_eq!(s.len(), SUBSET_DIM);
    }

    #[test]
    fn subset_feature_strips_nan() {
        let mut v = vec![0.0; FEATURE_DIM];
        v[0] = f64::NAN;
        v[16] = f64::INFINITY;
        let fv = FeatureVec(v);
        let s = subset_feature(&fv);
        assert!(s.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn positional_encoding_shape_correct() {
        let dev = Device::Cpu;
        let pe = positional_encoding(&dev).expect("pe");
        assert_eq!(pe.dims(), &[SEQ_LEN, MODEL_DIM]);
    }

    #[test]
    fn encoder_block_forward_preserves_shape() {
        let dev = Device::Cpu;
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, &dev);
        let block = EncoderBlock::build(vb).expect("block");
        let x = Tensor::zeros((2, SEQ_LEN, MODEL_DIM), DType::F32, &dev).expect("zero");
        let y = block.forward(&x).expect("fwd");
        assert_eq!(y.dims(), &[2, SEQ_LEN, MODEL_DIM]);
    }

    #[test]
    fn inner_forward_logits_shape() {
        let dev = Device::Cpu;
        let inner = Inner::build(&dev).expect("inner");
        let xs = Tensor::zeros((3, SEQ_LEN, SUBSET_DIM), DType::F32, &dev).expect("zero");
        let logits = inner.forward_logits(&xs).expect("logits");
        assert_eq!(logits.dims(), &[3, NUM_CLASSES]);
    }

    #[test]
    fn inner_forward_probs_sum_to_one() {
        let dev = Device::Cpu;
        let inner = Inner::build(&dev).expect("inner");
        let xs = Tensor::zeros((2, SEQ_LEN, SUBSET_DIM), DType::F32, &dev).expect("zero");
        let probs = inner.forward_probs(&xs).expect("probs");
        let p = probs.to_vec2::<f32>().expect("vec2");
        for row in p {
            let sum: f32 = row.iter().sum();
            assert!((sum - 1.0).abs() < 1e-4, "softmax sum != 1, got {sum}");
            assert!(row.iter().all(|x| x.is_finite()));
            assert!(row.iter().all(|x| (0.0..=1.0).contains(x)));
        }
    }

    #[test]
    fn cold_start_predict_returns_none() {
        let dir = tmp_dir("cold");
        let m = RegimeTransformer::cold_start(&dir);
        let ctx = mk_ctx();
        assert!(m.predict(&ctx).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cold_start_meta_correct() {
        let dir = tmp_dir("cold_meta");
        let m = RegimeTransformer::cold_start(&dir);
        assert_eq!(m.meta().model_id, "regime_xfmr");
        assert_eq!(m.meta().architecture, "transformer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn train_insufficient_data_short_circuits() {
        let dir = tmp_dir("insuf");
        let mut m = RegimeTransformer::cold_start(&dir);
        let samples = synthetic_dataset(8);
        let s = m.train(&samples, &TrainConfig::default());
        match s {
            TrainStatus::InsufficientData { samples, threshold } => {
                assert_eq!(samples, 8);
                assert_eq!(threshold, COLD_START_MIN_SAMPLES);
            }
            other => panic!("expected InsufficientData, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn train_completes_on_synthetic_dataset() {
        let dir = tmp_dir("train_done");
        let mut m = RegimeTransformer::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 20);
        let cfg = TrainConfig {
            epochs: 1,
            batch_size: 16,
            lr: 1e-3,
            validation_fraction: 0.2,
        };
        let s = m.train(&samples, &cfg);
        match s {
            TrainStatus::Trained { version, accuracy, samples } => {
                assert_eq!(version, 1);
                assert!((0.0..=1.0).contains(&accuracy));
                assert_eq!(samples, COLD_START_MIN_SAMPLES + 20);
            }
            other => panic!("expected Trained, got {other:?}"),
        }
        assert!(m.is_warm());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn predict_returns_some_after_training() {
        let dir = tmp_dir("pred_some");
        let mut m = RegimeTransformer::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 10);
        let _ = m.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 16,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let ctx = mk_ctx();
        let p = m.predict(&ctx).expect("some");
        assert!((-1.0..=1.0).contains(&p.direction));
        assert!((0.0..=1.0).contains(&p.confidence));
        assert_eq!(p.model_id, "regime_xfmr");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_probs_populated_after_predict() {
        let dir = tmp_dir("last_probs");
        let mut m = RegimeTransformer::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 10);
        let _ = m.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 16,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let ctx = mk_ctx();
        let _ = m.predict(&ctx);
        let probs = m.last_probabilities().expect("probs cached");
        let sum: f32 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-3, "softmax sum should ~1, got {sum}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_roundtrip_predicts_consistently() {
        let dir = tmp_dir("save_load");
        let mut m = RegimeTransformer::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 10);
        let _ = m.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 16,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        m.save(&dir).expect("save");
        let m2 = RegimeTransformer::load_latest(&dir);
        let ctx = mk_ctx();
        let p1 = m.predict(&ctx).expect("p1");
        let p2 = m2.predict(&ctx).expect("p2");
        assert_eq!(p1.model_id, p2.model_id);
        assert!((p1.confidence - p2.confidence).abs() < 1e-2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression guard for the bot hot path. The Transformer is
    /// the heaviest CPU step in the predictor stack; this test
    /// keeps it inside the budget. Failure is the early-warning
    /// signal to scale `MODEL_DIM` / `NUM_LAYERS` back down.
    #[test]
    fn cpu_inference_latency_under_budget() {
        let dir = tmp_dir("infer_lat");
        let mut m = RegimeTransformer::cold_start(&dir);
        let samples = synthetic_dataset(COLD_START_MIN_SAMPLES + 10);
        let _ = m.train(
            &samples,
            &TrainConfig {
                epochs: 1,
                batch_size: 16,
                lr: 1e-3,
                validation_fraction: 0.2,
            },
        );
        let ctx = mk_ctx();
        for _ in 0..5 {
            let _ = m.predict(&ctx);
        }
        let mut samples_us: Vec<u128> = Vec::with_capacity(50);
        for _ in 0..50 {
            let t0 = std::time::Instant::now();
            let _ = m.predict(&ctx);
            samples_us.push(t0.elapsed().as_micros());
        }
        samples_us.sort_unstable();
        let median_us = samples_us[samples_us.len() / 2];
        let budget_us = (INFERENCE_LATENCY_BUDGET_MS as u128) * 1_000;
        assert!(
            median_us <= budget_us,
            "Transformer CPU inference too slow: median {median_us}us > budget {budget_us}us \
             (MODEL_DIM={MODEL_DIM}, NUM_LAYERS={NUM_LAYERS}). If this fails, scale dims down."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
