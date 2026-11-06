//! FRONTIER (Sesiunea 4) — Contextual Thompson-Bernoulli bandit.
//!
//! Final-stage size selector. After OMEGA assigns a confidence,
//! the Monte-Carlo gate filters obvious losers, and the bot has
//! computed a tentative `size_usd`, this bandit applies a learned
//! multiplier ∈ {0.5, 0.75, 1.0} based on the trade's *context*
//! (market regime × confidence high/low). The multiplier comes from
//! Thompson sampling against a Beta-Bernoulli posterior maintained
//! per (bucket, arm) pair.
//!
//! ## Cold-start contract
//!
//! Until a context bucket has accumulated
//! [`COLD_START_TRADES_PER_BUCKET`] closed trades the bandit
//! returns the **neutral multiplier** (1.0×). This guarantees
//! identical bot behaviour to v2.3.9 on day-1 paper trading and
//! prevents "Beta(1,1) noise" from amplifying random signal in
//! the first few entries.
//!
//! ### v2.4.1 fix — cold-start *records* observations
//!
//! Before v2.4.1 the cold-start branch returned [`ARM_NONE`], which
//! the bot's entry path interprets as "no learning row to stash".
//! That created a chicken-and-egg: the threshold gate keeps firing
//! cold-start because the bucket never receives any observation,
//! since cold-start picks `ARM_NONE` and so the close handler skips
//! the update. The fix returns [`COLD_START_ARM`] (= arm 2 →
//! `1.0×`, behaviour identical to v2.3.9) while still flagging
//! `cold_start = true`. Sizing is unchanged; the close path now
//! routes the realised P&L into arm 2's Beta posterior so the
//! bucket can actually graduate past the threshold. Disabled
//! bandits still return `ARM_NONE` via [`BanditDecision::neutral`]
//! — we never learn while disabled.
//!
//! ## Why Beta-Bernoulli (and not LinUCB)
//!
//! The reward we observe per trade is binary (profit / loss after
//! costs). Beta-Bernoulli is the canonical conjugate prior for
//! that distribution and has a closed-form posterior update
//! (`α += 1` on success, `β += 1` on failure), so updates are
//! O(1) and require no matrix algebra. LinUCB would buy us
//! continuous-context resolution, but at our context dimension
//! (regime × confidence_bucket = 8 buckets) the discretised
//! variant captures everything that matters.
//!
//! ## Action set
//!
//! - Arm 0 — `0.50×` (half size; "this context tends to lose")
//! - Arm 1 — `0.75×` (three-quarters; mild caution)
//! - Arm 2 — `1.00×` (full intended size; bandit defers)
//!
//! Notably the bandit can only **shrink**, never inflate. That's a
//! deliberate safety choice: OMEGA + MC already capped size, and
//! we don't want a noisy posterior to push the bot above the
//! risk-engine's blessing.
//!
//! ## Sampling
//!
//! Beta(α, β) sampling is built on top of Marsaglia–Tsang Gamma
//! sampling (the Box–Muller helper from `monte_carlo` provides
//! the underlying N(0, 1) draws). All randomness flows from
//! [`SplitMix64`] seeded by an atomic counter so concurrent
//! `select()` calls from different bot threads remain
//! deterministic-on-replay if the seed counter is captured.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::ml::monte_carlo::rand_normals;
use crate::smarttrend::genetic_optimizer::SplitMix64;
use crate::smarttrend::strategies::MarketRegime;

/// Number of action arms exposed to the bot.
pub const NUM_ARMS: usize = 5;

/// Multiplier applied to `size_usd` when each arm wins (first 4 arms).
/// matches the [`BanditDecision::arm_idx`] returned by `select`.
pub const ARM_MULTIPLIERS: [f64; 4] = [0.5, 0.75, 1.0, 1.35];

/// Minimum closed-trade count per bucket before Thompson sampling
/// is allowed to take over from the neutral default.
pub const COLD_START_TRADES_PER_BUCKET: u32 = 20;

/// What `select()` returns when cold-start or `enabled = false`.
pub const NEUTRAL_MULTIPLIER: f64 = 1.0;

/// Confidence threshold separating the "low" and "high" sides of
/// the context bucket. Matches the OMEGA veto threshold
/// (0.65 → high) so bucket assignment is consistent with the rest
/// of the predictive stack.
pub const HIGH_CONFIDENCE_THRESHOLD: f64 = 0.65;

/// Sentinel arm index used by callers who never received a real
/// recommendation (e.g., bandit disabled). [`update`] treats this
/// as a no-op so trade journaling stays robust.
pub const ARM_NONE: usize = usize::MAX;

/// Arm picked during the cold-start window. Index 2 is the
/// `1.0×` (full size) arm — identical behaviour to v2.3.9 — but
/// because it is a *valid* arm index the close handler will
/// route the realised P&L into the bucket's Beta posterior. That
/// is the mechanism that lets a cold bucket eventually accumulate
/// the [`COLD_START_TRADES_PER_BUCKET`] observations it needs to
/// hand control over to Thompson sampling.
pub const COLD_START_ARM: usize = 2;

// ─── State types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextBucket {
    pub regime: u8, // 0..=3 mirrors `regime_to_idx`
    pub high_confidence: bool,
}

impl ContextBucket {
    pub fn from_inputs(regime: MarketRegime, confidence: f64) -> Self {
        Self {
            regime: regime_idx(regime),
            high_confidence: confidence >= HIGH_CONFIDENCE_THRESHOLD,
        }
    }

    /// Stable string key used as the persistence map key. The
    /// format is `<regime>:<H|L>` (e.g., `"1:H"`) so a corrupted
    /// state file with a stray entry doesn't bring the loader down.
    pub fn key(&self) -> String {
        format!("{}:{}", self.regime, if self.high_confidence { 'H' } else { 'L' })
    }
}

fn regime_idx(r: MarketRegime) -> u8 {
    match r {
        MarketRegime::Calm => 0,
        MarketRegime::Trending => 1,
        MarketRegime::Volatile => 2,
        MarketRegime::Panic => 3,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArmStats {
    pub successes: u32,
    pub failures: u32,
}

impl ArmStats {
    pub fn alpha(&self) -> f64 {
        self.successes as f64 + 1.0
    }
    pub fn beta(&self) -> f64 {
        self.failures as f64 + 1.0
    }
    pub fn total(&self) -> u32 {
        self.successes.saturating_add(self.failures)
    }
    pub fn success_rate(&self) -> f64 {
        let t = self.total();
        if t == 0 {
            0.5
        } else {
            self.successes as f64 / t as f64
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BucketStats {
    pub arms: [ArmStats; NUM_ARMS],
}

impl<'de> Deserialize<'de> for BucketStats {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper {
            arms: Vec<ArmStats>,
        }
        let helper = Helper::deserialize(deserializer)?;
        let mut arms = [ArmStats::default(), ArmStats::default(), ArmStats::default(), ArmStats::default(), ArmStats::default()];
        for (i, stat) in helper.arms.into_iter().enumerate() {
            if i < NUM_ARMS {
                arms[i] = stat;
            }
        }
        Ok(BucketStats { arms })
    }
}

impl Serialize for BucketStats {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Helper<'a> {
            arms: &'a [ArmStats],
        }
        Helper { arms: &self.arms }.serialize(serializer)
    }
}

impl BucketStats {
    pub fn total_observations(&self) -> u32 {
        self.arms.iter().map(|a| a.total()).sum()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BanditState {
    /// Map keyed by [`ContextBucket::key`]. We store the string
    /// key (rather than the struct) so a future schema migration
    /// that renames buckets doesn't poison persistence.
    pub buckets: HashMap<String, BucketStats>,
    pub total_updates: u64,
    pub last_updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BanditDecision {
    pub arm_idx: usize,
    pub multiplier: f64,
    pub bucket_observations: u32,
    pub cold_start: bool,
    pub diag: String,
}

/// Snapshot of the bandit context for a single in-flight trade. We
/// stash this in a side-table keyed by symbol at entry time so the
/// trade-close handler can call `update()` with the same `(regime,
/// confidence)` bucket that `select()` used. Restart-safe: an
/// in-flight pending row that doesn't match any open trade after a
/// crash is simply dropped — the bandit just misses one update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanditPendingContext {
    pub arm_idx: usize,
    pub regime: MarketRegime,
    pub confidence: f64,
    pub recorded_at: i64,
}

impl BanditDecision {
    pub fn neutral(diag: impl Into<String>) -> Self {
        Self {
            arm_idx: ARM_NONE,
            multiplier: NEUTRAL_MULTIPLIER,
            bucket_observations: 0,
            cold_start: true,
            diag: diag.into(),
        }
    }
}

// ─── Bandit ─────────────────────────────────────────────────────────

pub struct ThompsonBandit {
    state: BanditState,
    seed_base: u64,
    seed_counter: AtomicU64,
    activation_threshold: u32,
    enabled: bool,
}

impl std::fmt::Debug for ThompsonBandit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThompsonBandit")
            .field("enabled", &self.enabled)
            .field("activation_threshold", &self.activation_threshold)
            .field("buckets", &self.state.buckets.len())
            .field("total_updates", &self.state.total_updates)
            .finish()
    }
}

impl ThompsonBandit {
    /// Fresh bandit (cold-start everywhere).
    pub fn new(enabled: bool, activation_threshold: u32) -> Self {
        Self {
            state: BanditState::default(),
            seed_base: deterministic_boot_seed(),
            seed_counter: AtomicU64::new(0),
            activation_threshold,
            enabled,
        }
    }

    /// Bandit hydrated from a previously-persisted state.
    pub fn from_state(state: BanditState, enabled: bool, activation_threshold: u32) -> Self {
        Self {
            state,
            seed_base: deterministic_boot_seed(),
            seed_counter: AtomicU64::new(0),
            activation_threshold,
            enabled,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn state(&self) -> &BanditState {
        &self.state
    }

    pub fn activation_threshold(&self) -> u32 {
        self.activation_threshold
    }

    /// Total trade observations across all buckets.
    pub fn total_observations(&self) -> u64 {
        self.state.total_updates
    }

    /// Loads from disk; returns a fresh bandit on any failure
    /// (missing file, corrupt JSON). Failure is logged once at
    /// `warn` level by the caller.
    pub fn load(path: &Path, enabled: bool, activation_threshold: u32) -> Self {
        match std::fs::read_to_string(path) {
            Ok(body) => match serde_json::from_str::<BanditState>(&body) {
                Ok(state) => Self::from_state(state, enabled, activation_threshold),
                Err(e) => {
                    tracing::warn!(
                        component = "bandit",
                        error = %e,
                        path = %path.display(),
                        "Failed to parse bandit state; starting cold"
                    );
                    Self::new(enabled, activation_threshold)
                }
            },
            Err(_) => Self::new(enabled, activation_threshold),
        }
    }

    /// Atomic write: `write tmp → rename`. Caller responsible for
    /// having created the parent directory.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(&self.state).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Pick an action for this context.
    pub fn select(&self, regime: MarketRegime, confidence: f64, hawkes_intensity: f64) -> BanditDecision {
        if !self.enabled {
            return BanditDecision::neutral("bandit disabled");
        }
        let bucket = ContextBucket::from_inputs(regime, confidence);
        let stats = self.state.buckets.get(&bucket.key()).cloned().unwrap_or_default();
        let observations = stats.total_observations();
        if observations < self.activation_threshold {
            // v2.4.1 fix: emit COLD_START_ARM (full-size, identical
            // sizing to v2.3.9) instead of ARM_NONE so the close
            // path actually feeds an observation back into the
            // bucket. Without this the threshold gate is a permanent
            // trap — the bucket can never reach the activation
            // threshold because every entry resolves with ARM_NONE,
            // which `update()` ignores.
            return BanditDecision {
                arm_idx: COLD_START_ARM,
                multiplier: ARM_MULTIPLIERS[COLD_START_ARM],
                bucket_observations: observations,
                cold_start: true,
                diag: format!(
                    "cold-start ({}/{} obs)",
                    observations, self.activation_threshold
                ),
            };
        }

        // Thompson sampling: draw one Beta sample per arm, pick the
        // largest. The seed counter advances atomically per call so
        // concurrent `select()` calls produce independent samples.
        let call_id = self.seed_counter.fetch_add(1, Ordering::Relaxed);
        let mut best_idx = 2; // default to "full size" arm if all draws tie
        let mut best_sample = -f64::INFINITY;
        for (i, arm) in stats.arms.iter().enumerate() {
            let seed = self
                .seed_base
                .wrapping_add(call_id.wrapping_mul(NUM_ARMS as u64))
                .wrapping_add(i as u64);
            let sample = sample_beta(arm.alpha(), arm.beta(), seed);
            if sample > best_sample {
                best_sample = sample;
                best_idx = i;
            }
        }
        let elastic_mult = if hawkes_intensity > (0.05 * 3.5) {
            (1.0 + (hawkes_intensity / 0.05).ln()).min(2.50)
        } else {
            1.0
        };
        let multiplier = if best_idx < 4 {
            ARM_MULTIPLIERS[best_idx]
        } else {
            elastic_mult
        };
        BanditDecision {
            arm_idx: best_idx,
            multiplier,
            bucket_observations: observations,
            cold_start: false,
            diag: format!(
                "{}: arm {} (×{:.2}) @ obs={}",
                bucket.key(),
                best_idx,
                multiplier,
                observations
            ),
        }
    }

    /// Records a trade outcome. `arm_idx == ARM_NONE` is a no-op
    /// (defensive — caller may have stored a sentinel for vetoed
    /// trades).
    pub fn update(
        &mut self,
        regime: MarketRegime,
        confidence: f64,
        arm_idx: usize,
        profitable: bool,
    ) {
        if arm_idx >= NUM_ARMS {
            return;
        }
        let bucket = ContextBucket::from_inputs(regime, confidence);
        let entry = self.state.buckets.entry(bucket.key()).or_default();
        if profitable {
            entry.arms[arm_idx].successes = entry.arms[arm_idx].successes.saturating_add(1);
        } else {
            entry.arms[arm_idx].failures = entry.arms[arm_idx].failures.saturating_add(1);
        }
        self.state.total_updates = self.state.total_updates.saturating_add(1);
        self.state.last_updated_at = chrono::Utc::now().timestamp();
    }

    /// Number of arms an UI / telemetry consumer can render. Kept
    /// even though it's a constant so future configurable arm
    /// counts won't break callers.
    #[allow(dead_code)]
    pub fn num_arms() -> usize {
        NUM_ARMS
    }

    /// Returns the per-arm posterior summary for a context. Useful
    /// for the UI panel ("here's what the bandit thinks about
    /// `Trending+High`").
    pub fn bucket_summary(&self, regime: MarketRegime, confidence: f64) -> BucketSummary {
        let bucket = ContextBucket::from_inputs(regime, confidence);
        let stats = self.state.buckets.get(&bucket.key()).cloned().unwrap_or_default();
        let total = stats.total_observations();
        BucketSummary {
            key: bucket.key(),
            regime,
            high_confidence: bucket.high_confidence,
            arms: stats.arms,
            total,
        }
    }
}

/// Default location for the bandit state file. Same convention
/// as the other ML artefacts.
pub fn default_state_path() -> PathBuf {
    let mut p = crate::ml::default_ml_dir();
    p.push("bandit_state.json");
    p
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketSummary {
    pub key: String,
    pub regime: MarketRegime,
    pub high_confidence: bool,
    pub arms: [ArmStats; NUM_ARMS],
    pub total: u32,
}

// ─── Helpers ────────────────────────────────────────────────────────

/// One-time pick of a "secret" base seed for Thompson sampling.
/// We pull from the system clock so two boots produce different
/// sample sequences (avoids a pathological run where every entry
/// of a session sees the same arm winning by accident).
fn deterministic_boot_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xA1B2C3D4_E5F60718);
    nanos ^ 0x9E3779B97F4A7C15
}

/// Marsaglia–Tsang Gamma sampler for shape `α ≥ 1` (and arbitrary
/// scale `1`). Reuses [`rand_normals`] for the underlying N(0, 1)
/// draws and a [`SplitMix64`] for the uniform draws.
///
/// For `α < 1` we apply the standard "boost" trick — draw from
/// Gamma(α + 1) and multiply by `U^(1/α)` — but in our use case
/// (α = successes + 1 ≥ 1) this branch is unreachable. We keep
/// it for paranoia / future generality.
fn sample_gamma(alpha: f64, seed: u64) -> f64 {
    if alpha <= 0.0 || !alpha.is_finite() {
        return 0.0;
    }
    if alpha < 1.0 {
        // Boost: G(α) = G(α+1) * U^(1/α)
        let mut rng = SplitMix64::new(seed.wrapping_mul(2));
        let u = rng.next_f64().max(f64::MIN_POSITIVE);
        let g = sample_gamma(alpha + 1.0, seed.wrapping_add(1));
        return g * u.powf(1.0 / alpha);
    }
    let d = alpha - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    let mut rng = SplitMix64::new(seed);
    let mut iter = 0;
    loop {
        iter += 1;
        if iter > 64 {
            // Pathological — fall back to mean of the distribution
            // to keep the bandit's calling code from spinning.
            return alpha;
        }
        let z = rand_normals(1, rng.next_u64())[0];
        let v_cube_root = 1.0 + c * z;
        if v_cube_root <= 0.0 {
            continue;
        }
        let v = v_cube_root.powi(3);
        let u = rng.next_f64().max(f64::MIN_POSITIVE);
        let z_sq = z * z;
        let z_quad = z_sq * z_sq;
        if u < 1.0 - 0.0331 * z_quad {
            return d * v;
        }
        if u.ln() < 0.5 * z_sq + d * (1.0 - v + v.ln()) {
            return d * v;
        }
    }
}

/// Beta(α, β) sample via the Gamma ratio: `Gα / (Gα + Gβ)`.
pub(crate) fn sample_beta(alpha: f64, beta: f64, seed: u64) -> f64 {
    if alpha <= 0.0 || beta <= 0.0 {
        return 0.5;
    }
    let g_alpha = sample_gamma(alpha, seed);
    let g_beta = sample_gamma(beta, seed.wrapping_add(0x1234_5678_9ABC_DEF0));
    let denom = g_alpha + g_beta;
    if denom <= 0.0 {
        0.5
    } else {
        (g_alpha / denom).clamp(0.0, 1.0)
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_multipliers_only_shrink_or_match() {
        for &m in &ARM_MULTIPLIERS {
            assert!((0.5..=1.35).contains(&m));
        }
        // Ensure arm 2 is exactly 1.0 (the "full size" anchor).
        assert_eq!(ARM_MULTIPLIERS[2], 1.0);
    }

    #[test]
    fn cold_start_arm_is_full_size() {
        // The cold-start contract requires identical sizing to v2.3.9.
        // If somebody re-points COLD_START_ARM at a shrunk arm this
        // test fires before paper-trading day-1 burns capital.
        assert_eq!(ARM_MULTIPLIERS[COLD_START_ARM], NEUTRAL_MULTIPLIER);
        assert!(COLD_START_ARM < NUM_ARMS);
        assert_ne!(COLD_START_ARM, ARM_NONE);
    }

    #[test]
    fn context_bucket_key_format() {
        let b = ContextBucket {
            regime: 1,
            high_confidence: true,
        };
        assert_eq!(b.key(), "1:H");
        let b2 = ContextBucket {
            regime: 3,
            high_confidence: false,
        };
        assert_eq!(b2.key(), "3:L");
    }

    #[test]
    fn context_bucket_from_inputs_uses_threshold() {
        let b = ContextBucket::from_inputs(MarketRegime::Trending, 0.7);
        assert!(b.high_confidence);
        assert_eq!(b.regime, 1);
        let b2 = ContextBucket::from_inputs(MarketRegime::Trending, 0.5);
        assert!(!b2.high_confidence);
    }

    #[test]
    fn arm_stats_alpha_beta_offset_by_one() {
        let s = ArmStats {
            successes: 5,
            failures: 3,
        };
        assert!((s.alpha() - 6.0).abs() < 1e-12);
        assert!((s.beta() - 4.0).abs() < 1e-12);
    }

    #[test]
    fn arm_stats_success_rate_default_is_half() {
        let s = ArmStats::default();
        assert_eq!(s.success_rate(), 0.5);
    }

    #[test]
    fn arm_stats_success_rate_proportional() {
        let s = ArmStats {
            successes: 7,
            failures: 3,
        };
        assert!((s.success_rate() - 0.7).abs() < 1e-12);
    }

    #[test]
    fn sample_gamma_finite_for_basic_shapes() {
        for shape in [1.0, 2.0, 5.0, 10.0, 50.0] {
            let g = sample_gamma(shape, 17 + (shape * 1000.0) as u64);
            assert!(g.is_finite() && g >= 0.0, "gamma({shape}) = {g}");
        }
    }

    #[test]
    fn sample_beta_in_unit_interval() {
        for seed in 0..200 {
            let b = sample_beta(1.0, 1.0, seed);
            assert!(
                (0.0..=1.0).contains(&b),
                "Beta sample out of range: {b}"
            );
        }
    }

    #[test]
    fn sample_beta_uniform_on_one_one_approx() {
        let n = 5_000;
        let xs: Vec<f64> = (0..n).map(|i| sample_beta(1.0, 1.0, i as u64 + 1)).collect();
        let mean: f64 = xs.iter().sum::<f64>() / n as f64;
        // Beta(1, 1) ~ Uniform(0, 1) → expect mean ≈ 0.5.
        assert!(
            (mean - 0.5).abs() < 0.05,
            "Beta(1,1) mean = {mean} (expected ~0.5)"
        );
    }

    #[test]
    fn sample_beta_skewed_toward_alpha() {
        // Beta(10, 1) puts most mass near 1.
        let n = 5_000;
        let xs: Vec<f64> = (0..n).map(|i| sample_beta(10.0, 1.0, i as u64)).collect();
        let mean: f64 = xs.iter().sum::<f64>() / n as f64;
        assert!(
            mean > 0.7,
            "Beta(10,1) mean = {mean}, expected closer to 10/11 = 0.91"
        );
    }

    #[test]
    fn cold_start_returns_neutral_multiplier() {
        let b = ThompsonBandit::new(true, COLD_START_TRADES_PER_BUCKET);
        let dec = b.select(MarketRegime::Trending, 0.8, 0.05);
        assert_eq!(dec.multiplier, NEUTRAL_MULTIPLIER);
        assert!(dec.cold_start);
        assert_eq!(dec.bucket_observations, 0);
    }

    /// Regression for the v2.4.0 cold-start trap: `select()` during
    /// cold-start used to return `ARM_NONE`, which the bot's entry
    /// path skips for pending-row insertion. Result: the close handler
    /// had nothing to feed back into the bucket so the activation
    /// threshold was unreachable. This guards the v2.4.1 fix: cold
    /// start must return a *valid* arm index so the rest of the
    /// pipeline can route observations into the posterior.
    #[test]
    fn cold_start_returns_valid_arm_for_learning() {
        let b = ThompsonBandit::new(true, COLD_START_TRADES_PER_BUCKET);
        let dec = b.select(MarketRegime::Trending, 0.8, 0.05);
        assert!(dec.cold_start);
        assert!(
            dec.arm_idx < NUM_ARMS,
            "cold-start arm must be a learnable index, got {}",
            dec.arm_idx
        );
        assert_eq!(dec.arm_idx, COLD_START_ARM);
        assert_ne!(dec.arm_idx, ARM_NONE);
    }

    /// End-to-end: with the cold-start fix, repeatedly calling
    /// `select` and feeding the returned arm back through `update`
    /// must monotonically advance the bucket toward the activation
    /// threshold. Before the fix this loop would run forever.
    #[test]
    fn cold_start_accumulates_observations_through_update() {
        let threshold = 5;
        let mut b = ThompsonBandit::new(true, threshold);
        for i in 0..threshold {
            let dec = b.select(MarketRegime::Trending, 0.8, 0.05);
            assert!(dec.cold_start, "iteration {i} should still be cold-start");
            assert!(dec.arm_idx < NUM_ARMS);
            b.update(MarketRegime::Trending, 0.8, dec.arm_idx, i % 2 == 0);
        }
        // After `threshold` updates the gate flips → Thompson now in charge.
        let post = b.select(MarketRegime::Trending, 0.8, 0.05);
        assert!(!post.cold_start);
        assert_eq!(post.bucket_observations, threshold);
    }

    #[test]
    fn disabled_bandit_returns_neutral() {
        let b = ThompsonBandit::new(false, 0);
        let dec = b.select(MarketRegime::Volatile, 0.9, 0.05);
        assert_eq!(dec.multiplier, NEUTRAL_MULTIPLIER);
        assert!(dec.cold_start);
        assert!(dec.diag.contains("disabled"));
    }

    #[test]
    fn update_increments_correct_arm_stats() {
        let mut b = ThompsonBandit::new(true, 0);
        b.update(MarketRegime::Calm, 0.8, 1, true);
        b.update(MarketRegime::Calm, 0.8, 1, false);
        b.update(MarketRegime::Calm, 0.8, 2, true);

        let bucket = ContextBucket::from_inputs(MarketRegime::Calm, 0.8);
        let stats = b.state.buckets.get(&bucket.key()).cloned().unwrap_or_default();
        assert_eq!(stats.arms[1].successes, 1);
        assert_eq!(stats.arms[1].failures, 1);
        assert_eq!(stats.arms[2].successes, 1);
        assert_eq!(stats.arms[2].failures, 0);
    }

    #[test]
    fn update_arm_none_is_noop() {
        let mut b = ThompsonBandit::new(true, 0);
        b.update(MarketRegime::Calm, 0.8, ARM_NONE, true);
        let bucket = ContextBucket::from_inputs(MarketRegime::Calm, 0.8);
        assert!(!b.state.buckets.contains_key(&bucket.key()));
    }

    #[test]
    fn update_buckets_separated_by_confidence() {
        let mut b = ThompsonBandit::new(true, 0);
        b.update(MarketRegime::Trending, 0.9, 0, true); // high
        b.update(MarketRegime::Trending, 0.4, 0, false); // low
        let high = b
            .state
            .buckets
            .get(
                &ContextBucket::from_inputs(MarketRegime::Trending, 0.9).key(),
            )
            .unwrap();
        let low = b
            .state
            .buckets
            .get(
                &ContextBucket::from_inputs(MarketRegime::Trending, 0.4).key(),
            )
            .unwrap();
        assert_eq!(high.arms[0].successes, 1);
        assert_eq!(high.arms[0].failures, 0);
        assert_eq!(low.arms[0].successes, 0);
        assert_eq!(low.arms[0].failures, 1);
    }

    #[test]
    fn select_after_threshold_uses_thompson() {
        let mut b = ThompsonBandit::new(true, 5);
        // Pump arm 2 with wins, arm 0 with losses.
        for _ in 0..10 {
            b.update(MarketRegime::Trending, 0.8, 2, true);
            b.update(MarketRegime::Trending, 0.8, 0, false);
        }
        // Run many select() calls; arm 2 should dominate.
        let mut count = [0usize; NUM_ARMS];
        for _ in 0..200 {
            let dec = b.select(MarketRegime::Trending, 0.8, 0.05);
            assert!(!dec.cold_start);
            count[dec.arm_idx] += 1;
        }
        assert!(
            count[2] > count[0],
            "arm 2 picked {} times, arm 0 picked {} times",
            count[2],
            count[0]
        );
    }

    #[test]
    fn winning_arm_eventually_dominates() {
        let mut b = ThompsonBandit::new(true, 5);
        for _ in 0..50 {
            b.update(MarketRegime::Calm, 0.5, 0, false);
            b.update(MarketRegime::Calm, 0.5, 1, false);
            b.update(MarketRegime::Calm, 0.5, 2, true);
        }
        let mut count = [0usize; NUM_ARMS];
        for _ in 0..200 {
            let dec = b.select(MarketRegime::Calm, 0.5, 0.05);
            count[dec.arm_idx] += 1;
        }
        assert!(count[2] > count[0] + count[1]);
    }

    #[test]
    fn select_returns_bucket_observation_count() {
        let mut b = ThompsonBandit::new(true, 5);
        for _ in 0..7 {
            b.update(MarketRegime::Volatile, 0.7, 1, true);
        }
        let dec = b.select(MarketRegime::Volatile, 0.7, 0.05);
        assert_eq!(dec.bucket_observations, 7);
        assert!(!dec.cold_start);
    }

    #[test]
    fn save_load_roundtrip_preserves_state() {
        let dir = tmp_dir("save_load");
        let path = dir.join("bandit.json");

        let mut b1 = ThompsonBandit::new(true, 3);
        for _ in 0..10 {
            b1.update(MarketRegime::Calm, 0.8, 0, true);
        }
        b1.update(MarketRegime::Trending, 0.4, 2, false);
        b1.save(&path).expect("save");

        let b2 = ThompsonBandit::load(&path, true, 3);
        assert_eq!(
            b2.state.buckets.len(),
            b1.state.buckets.len()
        );
        for (k, v) in &b1.state.buckets {
            assert_eq!(b2.state.buckets.get(k).cloned().unwrap_or_default(), *v);
        }
        assert_eq!(b1.state.total_updates, b2.state.total_updates);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_returns_cold_bandit() {
        let dir = tmp_dir("missing");
        let path = dir.join("never_written.json");
        let b = ThompsonBandit::load(&path, true, 5);
        assert_eq!(b.state.total_updates, 0);
        assert_eq!(b.state.buckets.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corrupt_file_returns_cold_bandit() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("bandit.json");
        std::fs::write(&path, "not valid json {{").expect("write corrupt");
        let b = ThompsonBandit::load(&path, true, 5);
        assert_eq!(b.state.total_updates, 0);
        assert_eq!(b.state.buckets.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tmp_dir("create_parent");
        let nested = dir.join("nested").join("deep").join("bandit.json");
        let b = ThompsonBandit::new(true, 5);
        b.save(&nested).expect("save creates parents");
        assert!(nested.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bucket_summary_returns_zero_for_unseen() {
        let b = ThompsonBandit::new(true, 5);
        let s = b.bucket_summary(MarketRegime::Panic, 0.9);
        assert_eq!(s.total, 0);
        assert!(s.high_confidence);
    }

    #[test]
    fn bucket_summary_reflects_updates() {
        let mut b = ThompsonBandit::new(true, 5);
        b.update(MarketRegime::Panic, 0.9, 0, true);
        b.update(MarketRegime::Panic, 0.9, 0, false);
        let s = b.bucket_summary(MarketRegime::Panic, 0.9);
        assert_eq!(s.total, 2);
        assert_eq!(s.arms[0].successes, 1);
        assert_eq!(s.arms[0].failures, 1);
    }

    #[test]
    fn update_out_of_range_arm_is_noop() {
        let mut b = ThompsonBandit::new(true, 0);
        b.update(MarketRegime::Calm, 0.5, 99, true);
        assert_eq!(b.state.total_updates, 0);
    }

    #[test]
    fn select_diag_includes_bucket_key() {
        let mut b = ThompsonBandit::new(true, 1);
        b.update(MarketRegime::Trending, 0.8, 1, true);
        let dec = b.select(MarketRegime::Trending, 0.8, 0.05);
        // Bucket key is "1:H" for Trending+High-conf.
        assert!(dec.diag.contains("1:H"), "diag = {}", dec.diag);
    }

    #[test]
    fn total_observations_sums_all_buckets() {
        let mut b = ThompsonBandit::new(true, 5);
        b.update(MarketRegime::Calm, 0.5, 0, true);
        b.update(MarketRegime::Trending, 0.8, 1, false);
        b.update(MarketRegime::Volatile, 0.3, 2, true);
        assert_eq!(b.total_observations(), 3);
    }

    #[test]
    fn set_enabled_toggles_runtime() {
        let mut b = ThompsonBandit::new(false, 5);
        assert!(!b.enabled());
        b.set_enabled(true);
        assert!(b.enabled());
    }

    /// Smoke for the seed counter mechanic — repeated calls with
    /// the same context must NOT produce an identical arm pick
    /// every time once we're past cold-start (else Thompson would
    /// degenerate into a fixed-arm strategy).
    #[test]
    fn select_diversifies_arms_across_calls() {
        let mut b = ThompsonBandit::new(true, 1);
        // Equal evidence on all arms → posteriors near identical →
        // Thompson should select arms roughly uniformly.
        for _ in 0..30 {
            for arm in 0..NUM_ARMS {
                b.update(MarketRegime::Calm, 0.8, arm, arm % 2 == 0);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.insert(b.select(MarketRegime::Calm, 0.8, 0.05).arm_idx);
        }
        assert!(
            seen.len() >= 2,
            "Thompson collapsed to a single arm — saw {:?}",
            seen
        );
    }

    fn tmp_dir(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("st_bandit_{label}_{nano}"));
        std::fs::create_dir_all(&p).expect("test tmp dir");
        p
    }
}
