//! OMEGA (Sesiunea 3) — predictive ML scaffolding.
//!
//! Hosts the [`Predictor`] trait, [`Prediction`] payload type, the
//! [`ModelStore`] versioned weight loader, and the [`PredictorRegistry`]
//! container that the bot loop consults at signal time.
//!
//! ## Why a trait, not concrete types
//!
//! Different models have different state (LSTM keeps recurrent
//! weights; Transformer keeps attention matrices) but they all answer
//! the same question: "what's your forecast for this `MarketContext`?".
//! A trait keeps the bot loop polymorphic — the same `predict()` call
//! routes to whatever model registry has loaded — so we can hot-swap
//! architectures without touching `bot/entry.rs`.
//!
//! ## Cold-start contract
//!
//! Every predictor MUST gracefully refuse to score until it has
//! observed enough training samples. This is enforced two ways:
//!
//! 1. The [`Predictor::predict`] return type is `Option<Prediction>` —
//!    `None` means "I have nothing to say". The bot loop already
//!    short-circuits on `None`, preserving v2.1 behaviour.
//! 2. The [`COLD_START_MIN_SAMPLES`] constant pins the minimum
//!    training sample count below which `predict` MUST return `None`.
//!
//! ## Versioning
//!
//! Weight files are named `<model_id>_v<N>.safetensors`. A sibling
//! `<model_id>_meta.json` records `last_trained_at`, `accuracy`,
//! `samples_trained`, and the canonical `version` to load. The
//! versioning policy is "monotonically increasing"; bumping is the
//! responsibility of the trainer (see Phase 3.5 `predictor_loop.rs`).
//!
//! ## Failure semantics
//!
//! - File I/O errors during `ModelStore::load` → returns `None`,
//!   trace warns once. Predictor stays in cold-start.
//! - File corruption / version skew on load → same: warn + cold-start.
//! - Forward pass panic / NaN / Inf → caller wraps in
//!   `std::panic::catch_unwind` (Phase 3.5); `predict` returns `None`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::smarttrend::strategies::MarketContext;

pub mod bandit;
pub mod features;
pub mod feature_pruning;
pub mod hawkes_core;
pub mod lstm;
pub mod ml_logger;
pub mod monte_carlo;
pub mod predictor_loop;
pub mod regime_transformer;

#[cfg(test)]
mod tests;

// ════════════════════════════════════════════════════════════════════
//  Constants
// ════════════════════════════════════════════════════════════════════

/// Minimum number of training samples below which any predictor MUST
/// return `None` from [`Predictor::predict`]. Pins the cold-start
/// contract — bot behaviour identical to v2.1 until enough closed
/// setups exist.
pub const COLD_START_MIN_SAMPLES: usize = 30;

/// Highest version number we'll auto-load. Files numbered above this
/// are ignored to keep boot startup bounded even if the trainer ever
/// runs amok and writes a million versions.
pub const MAX_VERSION_LOOKUP: u32 = 9_999;

/// Default ring-buffer size for [`PredictorRegistry::recent`] — bot
/// records the last N predictions for the UI panel without bloating
/// memory.
pub const PREDICTION_RING_CAP: usize = 1_000;

// ════════════════════════════════════════════════════════════════════
//  Public types
// ════════════════════════════════════════════════════════════════════

/// One model's verdict on a single market context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    /// Identifies the model that produced this prediction. Useful for
    /// downstream attribution and A/B comparisons.
    pub model_id: String,
    /// Direction in `[-1.0, +1.0]` — positive = expected up move,
    /// negative = expected down move. Magnitude is informative
    /// (closer to ±1 = stronger signal).
    pub direction: f64,
    /// Calibrated confidence in `[0.0, 1.0]`. Used by the bot to
    /// scale position size (`size *= confidence.clamp(0.5, 1.5)`)
    /// and to gate vetoes (`confidence > 0.7` adverse → block).
    pub confidence: f64,
    /// Estimated dollar expectancy per trade for this setup. Falls
    /// back to `0.0` when the model has no calibration history.
    pub expectancy: f64,
    /// Symbol the prediction applies to (mirrors `ctx.symbol`).
    pub symbol: String,
    /// Epoch seconds when the prediction was produced.
    pub created_at: i64,
}

impl Prediction {
    /// Returns `true` iff direction × intended_side is negative AND
    /// confidence exceeds `veto_threshold`. Used by the bot's entry
    /// path as a hard veto gate.
    pub fn vetoes(&self, intended_side: f64, veto_threshold: f64) -> bool {
        self.confidence > veto_threshold
            && (self.direction * intended_side).is_sign_negative()
            && self.direction.abs() > 0.1
    }

    /// Position-size multiplier. Maps `confidence ∈ [0,1]` to
    /// `[0.5, 1.5]` linearly so a no-signal prediction (0.5 conf)
    /// maps to 1.0× (no change) and full-confidence either direction
    /// scales by ±0.5×.
    pub fn size_multiplier(&self) -> f64 {
        let m = 0.5 + self.confidence;
        m.clamp(0.5, 1.5)
    }
}

/// Snapshot of a model's metadata. Persisted alongside weights so
/// the bot can read accuracy + sample count without loading the
/// tensor itself (UI summary only).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelMeta {
    pub model_id: String,
    pub version: u32,
    pub last_trained_at: i64,
    /// Out-of-sample accuracy on the validation split last training
    /// cycle (0.0..=1.0).
    pub accuracy: f64,
    /// Total samples seen across all training cycles.
    pub samples_trained: usize,
    /// Architecture identifier — `"lstm"`, `"transformer"`, etc.
    pub architecture: String,
}

/// Outcome of a training cycle. Used by [`predictor_loop`] to decide
/// whether to hot-swap the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrainStatus {
    /// Trained successfully; new version is `version`.
    Trained {
        version: u32,
        accuracy: f64,
        samples: usize,
    },
    /// Skipped because samples < threshold.
    InsufficientData {
        samples: usize,
        threshold: usize,
    },
    /// Skipped because the data hasn't changed enough since last
    /// training (used for retrain cooldown).
    NoChange {
        samples: usize,
    },
    /// Trainer hit a hard error — message is for ops dashboards.
    Failed { message: String },
}

// ════════════════════════════════════════════════════════════════════
//  Predictor trait
// ════════════════════════════════════════════════════════════════════

/// Polymorphic interface every OMEGA model implements.
///
/// Implementations MUST be `Send + Sync` so the bot loop can store
/// boxes inside `ArcSwap<Box<dyn Predictor>>` and the trainer can
/// hot-swap from another thread without locks.
pub trait Predictor: Send + Sync {
    fn id(&self) -> &str;

    /// Architecture tag — `"lstm"`, `"transformer"`, `"baseline"`.
    fn architecture(&self) -> &str;

    /// Score the supplied context. `None` means cold-start or any
    /// other "decline to score" condition. MUST never panic; the
    /// caller in `bot/entry.rs` does NOT wrap calls in
    /// `catch_unwind` because Rust's `panic = "abort"` profile would
    /// take down the bot.
    fn predict(&self, ctx: &MarketContext) -> Option<Prediction>;

    fn meta(&self) -> ModelMeta;

    /// Convenience: predictor is "warm" iff it can produce a
    /// prediction for at least one well-formed context. Default
    /// implementation: `samples_trained >= COLD_START_MIN_SAMPLES`.
    fn is_warm(&self) -> bool {
        self.meta().samples_trained >= COLD_START_MIN_SAMPLES
    }

    /// Returns the last softmax probabilities if this is a classifier.
    /// Default implementation returns `None`.
    fn last_probabilities(&self) -> Option<[f32; 4]> {
        None
    }
}

// ════════════════════════════════════════════════════════════════════
//  ModelStore — versioned safetensors loader
// ════════════════════════════════════════════════════════════════════

/// Filesystem-backed weight loader. One instance per model id.
#[derive(Debug, Clone)]
pub struct ModelStore {
    pub model_id: String,
    pub base_dir: PathBuf,
}

impl ModelStore {
    pub fn new(model_id: impl Into<String>, base_dir: impl Into<PathBuf>) -> Self {
        Self {
            model_id: model_id.into(),
            base_dir: base_dir.into(),
        }
    }

    fn weights_path(&self, version: u32) -> PathBuf {
        self.base_dir
            .join(format!("{}_v{:04}.safetensors", self.model_id, version))
    }

    fn meta_path(&self) -> PathBuf {
        self.base_dir.join(format!("{}_meta.json", self.model_id))
    }

    /// Returns the highest-numbered version currently on disk, or
    /// `None` if no weights are present.
    pub fn latest_version(&self) -> Option<u32> {
        let dir = std::fs::read_dir(&self.base_dir).ok()?;
        let prefix = format!("{}_v", self.model_id);
        let mut best: Option<u32> = None;
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stripped) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Some(stripped) = stripped.strip_suffix(".safetensors") else {
                continue;
            };
            let Ok(v) = stripped.parse::<u32>() else {
                continue;
            };
            if v > MAX_VERSION_LOOKUP {
                continue;
            }
            best = Some(best.map_or(v, |b| b.max(v)));
        }
        best
    }

    /// Loads metadata; returns `Default` if the file is missing.
    pub fn load_meta(&self) -> ModelMeta {
        let path = self.meta_path();
        match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str::<ModelMeta>(&body).unwrap_or_else(|e| {
                tracing::warn!(
                    component = "omega",
                    model_id = %self.model_id,
                    error = %e,
                    "Failed to parse meta file; using cold-start default"
                );
                ModelMeta {
                    model_id: self.model_id.clone(),
                    ..Default::default()
                }
            }),
            Err(_) => ModelMeta {
                model_id: self.model_id.clone(),
                ..Default::default()
            },
        }
    }

    pub fn save_meta(&self, meta: &ModelMeta) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.base_dir)?;
        let body = serde_json::to_string_pretty(meta).map_err(std::io::Error::other)?;
        let tmp = self.meta_path().with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, self.meta_path())?;
        Ok(())
    }

    /// Reads the safetensors at `version` into a `Vec<u8>`. Returns
    /// `None` if the file is missing or corrupt. Caller is
    /// responsible for parsing into the actual model.
    pub fn load_weights(&self, version: u32) -> Option<Vec<u8>> {
        let path = self.weights_path(version);
        std::fs::read(&path).ok()
    }

    pub fn save_weights(&self, version: u32, body: &[u8]) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.base_dir)?;
        let path = self.weights_path(version);
        let tmp = path.with_extension("safetensors.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Convenience: drop versions older than `keep_last_n`. Returns
    /// the count removed. Best-effort — IO errors are logged warn.
    pub fn prune_old_versions(&self, keep_last_n: usize) -> usize {
        let Ok(dir) = std::fs::read_dir(&self.base_dir) else {
            return 0;
        };
        let prefix = format!("{}_v", self.model_id);
        let mut versions: Vec<(u32, PathBuf)> = Vec::new();
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stripped) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Some(stripped) = stripped.strip_suffix(".safetensors") else {
                continue;
            };
            let Ok(v) = stripped.parse::<u32>() else {
                continue;
            };
            versions.push((v, entry.path()));
        }
        versions.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
        let mut removed = 0usize;
        for (_, path) in versions.into_iter().skip(keep_last_n) {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// Default base path for ML weights:
/// `~/.local/share/sentinel-trend/ml`.
pub fn default_ml_dir() -> PathBuf {
    let mut p = if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h)
    } else {
        PathBuf::from(".")
    };
    p.push(".local/share/sentinel-trend/ml");
    p
}

// ════════════════════════════════════════════════════════════════════
//  PredictorRegistry — runtime container
// ════════════════════════════════════════════════════════════════════

/// Container of predictors keyed by model id. The bot loop holds an
/// `Arc<arc_swap::ArcSwap<PredictorRegistry>>`; the trainer
/// hot-swaps the entire registry atomically when new weights land.
pub struct PredictorRegistry {
    inner: HashMap<String, Box<dyn Predictor>>,
    /// Recent predictions ring buffer (capacity [`PREDICTION_RING_CAP`]).
    /// Wrapped in a Mutex because the registry itself is held behind
    /// `ArcSwap` (immutable from the consumer's perspective).
    recent: std::sync::Mutex<std::collections::VecDeque<Prediction>>,
}

impl Default for PredictorRegistry {
    fn default() -> Self {
        Self {
            inner: HashMap::new(),
            recent: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(
                PREDICTION_RING_CAP,
            )),
        }
    }
}

impl std::fmt::Debug for PredictorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PredictorRegistry")
            .field("models", &self.inner.keys().collect::<Vec<_>>())
            .field("recent_count", &self.recent.lock().map(|q| q.len()).unwrap_or(0))
            .finish()
    }
}

impl PredictorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, p: Box<dyn Predictor>) {
        let key = p.id().to_string();
        self.inner.insert(key, p);
    }

    pub fn get(&self, id: &str) -> Option<&dyn Predictor> {
        self.inner.get(id).map(|b| &**b)
    }

    pub fn ids(&self) -> Vec<&str> {
        self.inner.keys().map(|s| s.as_str()).collect()
    }

    pub fn metas(&self) -> Vec<ModelMeta> {
        let mut out: Vec<_> = self.inner.values().map(|p| p.meta()).collect();
        out.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        out
    }

    /// Records a prediction in the ring buffer. Drops the oldest
    /// entry when capacity is reached. Best-effort under poison.
    pub fn record(&self, prediction: Prediction) {
        let Ok(mut q) = self.recent.lock() else {
            return;
        };
        if q.len() >= PREDICTION_RING_CAP {
            q.pop_front();
        }
        q.push_back(prediction);
    }

    pub fn recent(&self, limit: usize) -> Vec<Prediction> {
        let Ok(q) = self.recent.lock() else {
            return Vec::new();
        };
        q.iter().rev().take(limit).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns `true` if any predictor in the registry has
    /// `is_warm() == true`. Cheap fast-path used by the bot's
    /// entry hook to skip the (heavier) MarketContext build when
    /// no model can score yet.
    pub fn any_warm(&self) -> bool {
        self.inner.values().any(|p| p.is_warm())
    }
}

/// Convenience constructor used by `main.rs` at boot to populate the
/// registry with cold-start instances of every model. Models are
/// intentionally created in cold-start mode — the trainer will
/// produce real weights asynchronously when MemoryDb has data.
pub fn cold_start_registry(base_dir: &Path) -> PredictorRegistry {
    let mut reg = PredictorRegistry::new();
    reg.insert(Box::new(lstm::LstmForecaster::cold_start(base_dir)));
    reg.insert(Box::new(regime_transformer::RegimeTransformer::cold_start(
        base_dir,
    )));
    reg
}
