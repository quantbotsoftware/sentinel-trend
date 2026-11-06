//! Phase 3.2 — Feature engineering for OMEGA predictors.
//!
//! Extracts a deterministic 128-dim [`FeatureVec`] from a
//! [`MarketContext`] plus the surrounding candle window. Output is
//! always finite (NaN / Inf are scrubbed at the boundary so downstream
//! tensor ops cannot poison gradients).
//!
//! ## Design notes
//!
//! - **Length pin**: the [`FEATURE_DIM`] constant is asserted at the
//!   end of `extract_features` so adding/removing a feature without
//!   updating the dim count fails loud in tests.
//! - **NaN / Inf scrub**: every helper uses [`safe_f64`] before
//!   pushing into the output vec. Empty windows yield zeros.
//! - **No leakage**: features look only at history (slice of
//!   `candles` ending at the current bar). No forward window.
//! - **Welford normalizer**: running mean/variance updated in
//!   constant memory. After ~50 samples the normalizer stabilises
//!   to within ±1% of the population stats.
//! - **Persistence**: normalizer state saves to
//!   `<base_dir>/feature_stats.json`. Atomic write via `rename` to
//!   avoid corrupted stats on crash mid-write.
//!
//! ## Feature layout (indices)
//!
//!  0..3   Returns r1, r5, r15, r60                    (4)
//!  4..7   Log volatility windows 5, 15, 30, 60        (4)
//!  8..10  Momentum ROC10, ROC30, ROC60                (3)
//! 11..13  Volume rvol, vol z-score, vol EMA ratio     (3)
//! 14..15  Microstructure spread_bps, imbalance proxy  (2)
//! 16..20  Technical RSI14, MACD, signal, BB%B, ATR%   (5)
//! 21..27  Time min_of_day_sin/cos + dow_one_hot[5]    (7)
//! 28..31  Regime one-hot calm/trend/vol/panic         (4)
//! 32..35  Catalyst news/earnings/score/halt           (4)
//! 36..127 Rolling stats (mean/std/skew/kurt × 4 wins
//!         × 4 series + cross/lag features)            (92)
//!
//! Total = 128.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use titan_types::Candle;

use crate::smarttrend::strategies::{MarketContext, MarketRegime};

/// Dimensionality of [`FeatureVec`]. Pinned by `assert_eq!` inside
/// `extract_features` — bumping requires touching tests intentionally.
pub const FEATURE_DIM: usize = 128;

/// Path component appended to ml dir for stats persistence.
pub const FEATURE_STATS_FILE: &str = "feature_stats.json";

/// Number of samples below which the normalizer behaves as identity
/// (variance estimates are too noisy with <30 observations).
pub const NORMALIZER_WARMUP_N: u64 = 30;

/// Floor for normalizer std to avoid divide-by-zero.
pub const STD_FLOOR: f64 = 1e-6;

// ════════════════════════════════════════════════════════════════════
//  FeatureVec
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct FeatureVec(pub Vec<f64>);

impl FeatureVec {
    pub fn zero() -> Self {
        Self(vec![0.0; FEATURE_DIM])
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[f64] {
        &self.0
    }

    /// Apply (x - mean) / std using the supplied normalizer. Safe:
    /// std clamped to [`STD_FLOOR`] to avoid divide-by-zero. Below
    /// [`NORMALIZER_WARMUP_N`] samples the normalizer is identity.
    pub fn normalize(&self, n: &FeatureNormalizer) -> Self {
        if n.count < NORMALIZER_WARMUP_N {
            return self.clone();
        }
        let mut out = vec![0.0; FEATURE_DIM];
        for (i, slot) in out.iter_mut().enumerate() {
            let std = n.std[i].max(STD_FLOOR);
            *slot = safe_f64((self.0[i] - n.mean[i]) / std).clamp(-6.0, 6.0);
        }
        Self(out)
    }
}

impl Default for FeatureVec {
    fn default() -> Self {
        Self::zero()
    }
}

// ════════════════════════════════════════════════════════════════════
//  FeatureNormalizer (Welford online mean/variance)
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureNormalizer {
    pub mean: Vec<f64>,
    /// Sum of squared deviations from the running mean (Welford M2).
    /// Variance = m2 / max(count - 1, 1).
    pub m2: Vec<f64>,
    /// Cached std (recomputed on every `update`).
    pub std: Vec<f64>,
    pub count: u64,
}

impl Default for FeatureNormalizer {
    fn default() -> Self {
        Self {
            mean: vec![0.0; FEATURE_DIM],
            m2: vec![0.0; FEATURE_DIM],
            std: vec![1.0; FEATURE_DIM],
            count: 0,
        }
    }
}

impl FeatureNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Welford's online algorithm — one pass, O(d) per sample.
    pub fn update(&mut self, sample: &FeatureVec) {
        if sample.len() != FEATURE_DIM {
            tracing::warn!(
                component = "omega",
                got = sample.len(),
                expected = FEATURE_DIM,
                "FeatureNormalizer::update — sample dim mismatch, skipping"
            );
            return;
        }
        self.count = self.count.saturating_add(1);
        let n = self.count as f64;
        for i in 0..FEATURE_DIM {
            let x = safe_f64(sample.0[i]);
            let delta = x - self.mean[i];
            self.mean[i] += delta / n;
            let delta2 = x - self.mean[i];
            self.m2[i] += delta * delta2;
            // Recompute cached std (lazy: O(d), once per sample).
            let var = if n > 1.0 {
                self.m2[i] / (n - 1.0)
            } else {
                1.0
            };
            self.std[i] = var.max(0.0).sqrt().max(STD_FLOOR);
        }
    }

    pub fn save(&self, base_dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(base_dir)?;
        let path = base_dir.join(FEATURE_STATS_FILE);
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn load(base_dir: &Path) -> Self {
        let path = base_dir.join(FEATURE_STATS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str(&body).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
}

// ════════════════════════════════════════════════════════════════════
//  Helpers
// ════════════════════════════════════════════════════════════════════

/// Strips NaN / Inf at the boundary so downstream tensor math cannot
/// produce poisoned gradients. Replaces with `0.0`.
#[inline]
pub fn safe_f64(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

#[inline]
fn pct_change(prev: f64, cur: f64) -> f64 {
    if prev.abs() < 1e-12 {
        0.0
    } else {
        safe_f64((cur - prev) / prev)
    }
}

fn returns_window(candles: &[Candle], lookback: usize) -> f64 {
    let n = candles.len();
    if n < 2 || lookback == 0 {
        return 0.0;
    }
    let look = lookback.min(n - 1);
    let prev = candles[n - 1 - look].close;
    let cur = candles[n - 1].close;
    pct_change(prev, cur)
}

fn log_volatility(candles: &[Candle], window: usize) -> f64 {
    if candles.len() < 2 || window < 2 {
        return 0.0;
    }
    let n = candles.len();
    let take = window.min(n - 1);
    let mut sum_sq = 0.0;
    for i in (n - take)..n {
        let r = pct_change(candles[i - 1].close, candles[i].close);
        let r1p = (1.0 + r).max(1e-12);
        let lr = r1p.ln();
        sum_sq += lr * lr;
    }
    safe_f64((sum_sq / take as f64).sqrt())
}

fn rsi(candles: &[Candle], period: usize) -> f64 {
    let n = candles.len();
    if n < period + 1 || period == 0 {
        return 50.0;
    }
    let mut gain = 0.0;
    let mut loss = 0.0;
    for i in (n - period)..n {
        let diff = candles[i].close - candles[i - 1].close;
        if diff > 0.0 {
            gain += diff;
        } else {
            loss -= diff;
        }
    }
    if loss < 1e-12 {
        return 100.0;
    }
    let rs = gain / loss;
    safe_f64(100.0 - 100.0 / (1.0 + rs))
}

fn ema(series: &[f64], period: usize) -> Vec<f64> {
    if series.is_empty() || period == 0 {
        return Vec::new();
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(series.len());
    let mut prev = series[0];
    out.push(prev);
    for &x in series.iter().skip(1) {
        prev = alpha * x + (1.0 - alpha) * prev;
        out.push(prev);
    }
    out
}

fn macd(candles: &[Candle]) -> (f64, f64) {
    if candles.len() < 26 {
        return (0.0, 0.0);
    }
    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let ema12 = ema(&closes, 12);
    let ema26 = ema(&closes, 26);
    let macd_line: Vec<f64> = ema12
        .iter()
        .zip(ema26.iter())
        .map(|(a, b)| a - b)
        .collect();
    let signal_line = ema(&macd_line, 9);
    let line = *macd_line.last().unwrap_or(&0.0);
    let sig = *signal_line.last().unwrap_or(&0.0);
    (safe_f64(line), safe_f64(sig))
}

fn bb_percent_b(candles: &[Candle], period: usize, k: f64) -> f64 {
    let n = candles.len();
    if n < period {
        return 0.5;
    }
    let slice = &candles[n - period..];
    let mean: f64 = slice.iter().map(|c| c.close).sum::<f64>() / period as f64;
    let var: f64 = slice
        .iter()
        .map(|c| (c.close - mean).powi(2))
        .sum::<f64>()
        / period as f64;
    let std = var.sqrt().max(STD_FLOOR);
    let upper = mean + k * std;
    let lower = mean - k * std;
    let denom = upper - lower;
    if denom < 1e-12 {
        return 0.5;
    }
    let cur = candles[n - 1].close;
    safe_f64(((cur - lower) / denom).clamp(-1.0, 2.0))
}

fn rolling_stats(candles: &[Candle], window: usize) -> (f64, f64, f64, f64) {
    let n = candles.len();
    if n < 2 || window < 2 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let take = window.min(n - 1);
    let mut returns = Vec::with_capacity(take);
    for i in (n - take)..n {
        returns.push(pct_change(candles[i - 1].close, candles[i].close));
    }
    if returns.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let m = returns.iter().sum::<f64>() / returns.len() as f64;
    let var = returns.iter().map(|r| (r - m).powi(2)).sum::<f64>() / returns.len() as f64;
    let std = var.sqrt().max(STD_FLOOR);
    let skew = returns
        .iter()
        .map(|r| ((r - m) / std).powi(3))
        .sum::<f64>()
        / returns.len() as f64;
    let kurt = returns
        .iter()
        .map(|r| ((r - m) / std).powi(4))
        .sum::<f64>()
        / returns.len() as f64
        - 3.0;
    (
        safe_f64(m),
        safe_f64(std),
        safe_f64(skew),
        safe_f64(kurt),
    )
}

fn volume_rolling_stats(candles: &[Candle], window: usize) -> (f64, f64, f64, f64) {
    let n = candles.len();
    if n < 2 || window < 2 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let take = window.min(n);
    let slice: Vec<f64> = candles[n - take..].iter().map(|c| c.volume).collect();
    let m = slice.iter().sum::<f64>() / slice.len() as f64;
    let var = slice.iter().map(|v| (v - m).powi(2)).sum::<f64>() / slice.len() as f64;
    let std = var.sqrt().max(STD_FLOOR);
    let skew = slice
        .iter()
        .map(|v| ((v - m) / std).powi(3))
        .sum::<f64>()
        / slice.len() as f64;
    let kurt = slice
        .iter()
        .map(|v| ((v - m) / std).powi(4))
        .sum::<f64>()
        / slice.len() as f64
        - 3.0;
    (
        safe_f64(m),
        safe_f64(std),
        safe_f64(skew),
        safe_f64(kurt),
    )
}

fn high_low_range(candles: &[Candle], window: usize) -> (f64, f64, f64, f64) {
    let n = candles.len();
    if n == 0 || window == 0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let take = window.min(n);
    let slice = &candles[n - take..];
    let m = slice
        .iter()
        .map(|c| (c.high - c.low).abs())
        .sum::<f64>()
        / slice.len() as f64;
    let var = slice
        .iter()
        .map(|c| ((c.high - c.low) - m).powi(2))
        .sum::<f64>()
        / slice.len() as f64;
    let std = var.sqrt().max(STD_FLOOR);
    let skew = slice
        .iter()
        .map(|c| (((c.high - c.low) - m) / std).powi(3))
        .sum::<f64>()
        / slice.len() as f64;
    let kurt = slice
        .iter()
        .map(|c| (((c.high - c.low) - m) / std).powi(4))
        .sum::<f64>()
        / slice.len() as f64
        - 3.0;
    (
        safe_f64(m),
        safe_f64(std),
        safe_f64(skew),
        safe_f64(kurt),
    )
}

fn body_rolling_stats(candles: &[Candle], window: usize) -> (f64, f64, f64, f64) {
    let n = candles.len();
    if n == 0 || window == 0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let take = window.min(n);
    let slice = &candles[n - take..];
    let m = slice
        .iter()
        .map(|c| (c.close - c.open).abs())
        .sum::<f64>()
        / slice.len() as f64;
    let var = slice
        .iter()
        .map(|c| ((c.close - c.open) - m).powi(2))
        .sum::<f64>()
        / slice.len() as f64;
    let std = var.sqrt().max(STD_FLOOR);
    let skew = slice
        .iter()
        .map(|c| (((c.close - c.open) - m) / std).powi(3))
        .sum::<f64>()
        / slice.len() as f64;
    let kurt = slice
        .iter()
        .map(|c| (((c.close - c.open) - m) / std).powi(4))
        .sum::<f64>()
        / slice.len() as f64
        - 3.0;
    (
        safe_f64(m),
        safe_f64(std),
        safe_f64(skew),
        safe_f64(kurt),
    )
}

// ════════════════════════════════════════════════════════════════════
//  extract_features — main entry point
// ════════════════════════════════════════════════════════════════════

/// Returns a [`FeatureVec`] of exactly [`FEATURE_DIM`] elements,
/// guaranteed to be free of NaN / Inf. Empty / very short input
/// yields a zero vector (still sized 128).
pub fn extract_features(ctx: &MarketContext) -> FeatureVec {
    let candles = &ctx.candles;
    let mut v = Vec::with_capacity(FEATURE_DIM);

    // 0..3 returns (4)
    v.push(returns_window(candles, 1));
    v.push(returns_window(candles, 5));
    v.push(returns_window(candles, 15));
    v.push(returns_window(candles, 60));

    // 4..7 log volatility (4)
    v.push(log_volatility(candles, 5));
    v.push(log_volatility(candles, 15));
    v.push(log_volatility(candles, 30));
    v.push(log_volatility(candles, 60));

    // 8..10 momentum (3)
    v.push(returns_window(candles, 10));
    v.push(returns_window(candles, 30));
    v.push(returns_window(candles, 60));

    // 11..13 volume (3)
    v.push(safe_f64(ctx.rvol));
    let (vmean, vstd, _, _) = volume_rolling_stats(candles, 20);
    let last_vol = candles.last().map(|c| c.volume).unwrap_or(0.0);
    let vz = if vstd > STD_FLOOR {
        (last_vol - vmean) / vstd
    } else {
        0.0
    };
    v.push(safe_f64(vz));
    let ema_vol = if vmean > 0.0 {
        last_vol / vmean
    } else {
        0.0
    };
    v.push(safe_f64(ema_vol));

    // 14..15 microstructure (2)
    v.push(safe_f64(ctx.spread_bps));
    let imbalance = candles.last().map_or(0.0, |c| {
        let body = (c.close - c.open).abs();
        let range = (c.high - c.low).abs().max(1e-12);
        (c.close - c.open).signum() * (body / range)
    });
    v.push(safe_f64(imbalance));

    // 16..20 technical (5)
    v.push(rsi(candles, 14));
    let (macd_line, macd_signal) = macd(candles);
    v.push(macd_line);
    v.push(macd_signal);
    v.push(bb_percent_b(candles, 20, 2.0));
    let atr_pct = if ctx.price > 1e-9 {
        ctx.atr / ctx.price
    } else {
        0.0
    };
    v.push(safe_f64(atr_pct));

    // 21..27 time (7)
    let mod_min = (ctx.now_et_min as f64).rem_euclid(1440.0);
    let theta = std::f64::consts::TAU * mod_min / 1440.0;
    v.push(theta.sin());
    v.push(theta.cos());
    let dow = ((ctx.now_et_min / 1440) % 7) as usize;
    for i in 0..5 {
        v.push(if i == (dow % 5) { 1.0 } else { 0.0 });
    }

    // 28..31 regime one-hot (4)
    let regime_idx = match ctx.regime {
        Some(MarketRegime::Calm) => Some(0usize),
        Some(MarketRegime::Trending) => Some(1usize),
        Some(MarketRegime::Volatile) => Some(2usize),
        Some(MarketRegime::Panic) => Some(3usize),
        None => None,
    };
    for i in 0..4 {
        v.push(if regime_idx == Some(i) { 1.0 } else { 0.0 });
    }

    // 32..35 catalyst (4)
    v.push(if ctx.catalyst_score.is_some() { 1.0 } else { 0.0 });
    v.push(0.0); // earnings flag — populated by Phase 3.5 from MarketContext extension
    v.push(safe_f64(ctx.catalyst_score.unwrap_or(0.0)));
    v.push(if ctx.is_halted { 1.0 } else { 0.0 });

    // 36..127 rolling stats (92)
    // We pack 4 stat-sets (close-returns, volume, hi-lo range, body)
    // × 4 windows (5/15/30/60) × 4 stats (mean/std/skew/kurt) = 64,
    // then 28 cross/lag features.
    for window in &[5usize, 15, 30, 60] {
        let (m, s, sk, kt) = rolling_stats(candles, *window);
        v.push(m);
        v.push(s);
        v.push(sk);
        v.push(kt);
    }
    for window in &[5usize, 15, 30, 60] {
        let (m, s, sk, kt) = volume_rolling_stats(candles, *window);
        v.push(m);
        v.push(s);
        v.push(sk);
        v.push(kt);
    }
    for window in &[5usize, 15, 30, 60] {
        let (m, s, sk, kt) = high_low_range(candles, *window);
        v.push(m);
        v.push(s);
        v.push(sk);
        v.push(kt);
    }
    for window in &[5usize, 15, 30, 60] {
        let (m, s, sk, kt) = body_rolling_stats(candles, *window);
        v.push(m);
        v.push(s);
        v.push(sk);
        v.push(kt);
    }
    // Cross / lag features (28):
    // - 8 EMA crossovers between fast(5) and slow(20) on close
    // - 8 close-vs-VWAP-proxy (close vs window mean) at 5/15/30/60
    // - 4 sum returns (5/15/30/60)
    // - 4 max drawdown over windows
    // - 4 max gain over windows
    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let ema_fast = ema(&closes, 5);
    let ema_slow = ema(&closes, 20);
    for i in 0..8 {
        let idx = closes.len().saturating_sub(i + 1);
        let f_v = ema_fast.get(idx).copied().unwrap_or(0.0);
        let s_v = ema_slow.get(idx).copied().unwrap_or(0.0);
        let denom = (f_v.abs() + s_v.abs()).max(1e-12);
        v.push(safe_f64((f_v - s_v) / denom));
    }
    for window in &[5usize, 15, 30, 60] {
        let n = candles.len();
        if n == 0 || *window == 0 {
            v.push(0.0);
            v.push(0.0);
        } else {
            let take = (*window).min(n);
            let slice = &candles[n - take..];
            let mean = slice.iter().map(|c| c.close).sum::<f64>() / take as f64;
            let cur = candles[n - 1].close;
            v.push(pct_change(mean, cur));
            let sum_ret: f64 = (n - take + 1..n)
                .map(|i| pct_change(candles[i - 1].close, candles[i].close))
                .sum();
            v.push(safe_f64(sum_ret));
        }
    }
    for window in &[5usize, 15, 30, 60] {
        let n = candles.len();
        if n == 0 || *window == 0 {
            v.push(0.0);
        } else {
            let take = (*window).min(n);
            let slice = &candles[n - take..];
            let max_p = slice
                .iter()
                .map(|c| c.high)
                .fold(f64::NEG_INFINITY, f64::max);
            let cur = candles[n - 1].close;
            v.push(safe_f64(pct_change(max_p, cur).min(0.0)));
        }
    }
    for window in &[5usize, 15, 30, 60] {
        let n = candles.len();
        if n == 0 || *window == 0 {
            v.push(0.0);
        } else {
            let take = (*window).min(n);
            let slice = &candles[n - take..];
            let min_p = slice.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
            let cur = candles[n - 1].close;
            v.push(safe_f64(pct_change(min_p, cur).max(0.0)));
        }
    }
    // 4 volume z-scores per window (close out the 28 cross/lag block).
    for window in &[5usize, 15, 30, 60] {
        let (vmean, vstd, _, _) = volume_rolling_stats(candles, *window);
        let last_vol = candles.last().map(|c| c.volume).unwrap_or(0.0);
        let z = if vstd > STD_FLOOR {
            (last_vol - vmean) / vstd
        } else {
            0.0
        };
        v.push(safe_f64(z));
    }

    // Final scrub + length pin.
    for x in v.iter_mut() {
        *x = safe_f64(*x);
    }
    debug_assert_eq!(
        v.len(),
        FEATURE_DIM,
        "FeatureVec length pin: expected {} got {}",
        FEATURE_DIM,
        v.len()
    );
    if v.len() != FEATURE_DIM {
        // Hard fallback: pad/truncate so the trait contract holds.
        v.resize(FEATURE_DIM, 0.0);
    }
    FeatureVec(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use titan_types::Candle;

    fn flat_candles(n: usize, price: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| Candle {
                time: i as i64,
                open: price,
                high: price,
                low: price,
                close: price,
                volume: 1_000.0,
            })
            .collect()
    }

    fn ramp_candles(n: usize, start: f64, step: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let p = start + step * i as f64;
                Candle {
                    time: i as i64,
                    open: p,
                    high: p + 0.5,
                    low: p - 0.5,
                    close: p,
                    volume: 1_000.0 + i as f64,
                }
            })
            .collect()
    }

    fn mk_ctx(candles: Vec<Candle>) -> MarketContext {
        let last = candles.last().map(|c| c.close).unwrap_or(100.0);
        MarketContext {
            symbol: "AAPL".into(),
            timeframe: "5m",
            price: last,
            prev_close: last,
            session_open: last,
            candles,
            atr: 1.0,
            rvol: 1.5,
            gap_pct: 0.0,
            spread_bps: 5.0,
            catalyst_score: Some(75.0),
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

    #[test]
    fn feature_vec_zero_has_correct_length() {
        let v = FeatureVec::zero();
        assert_eq!(v.len(), FEATURE_DIM);
        assert!(v.0.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn extract_returns_exact_dim_with_full_window() {
        let ctx = mk_ctx(ramp_candles(60, 100.0, 0.1));
        let fv = extract_features(&ctx);
        assert_eq!(fv.len(), FEATURE_DIM);
    }

    #[test]
    fn extract_returns_correct_dim_with_empty_candles() {
        let mut ctx = mk_ctx(Vec::new());
        ctx.price = 100.0;
        let fv = extract_features(&ctx);
        assert_eq!(fv.len(), FEATURE_DIM);
        assert!(fv.0.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn extract_handles_single_candle() {
        let ctx = mk_ctx(flat_candles(1, 100.0));
        let fv = extract_features(&ctx);
        assert_eq!(fv.len(), FEATURE_DIM);
        assert!(fv.0.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn extract_scrubs_nan_infinity_input() {
        let mut candles = ramp_candles(30, 100.0, 0.0);
        candles[5].close = f64::NAN;
        candles[10].volume = f64::INFINITY;
        let mut ctx = mk_ctx(candles);
        ctx.spread_bps = f64::NAN;
        ctx.atr = f64::INFINITY;
        let fv = extract_features(&ctx);
        assert!(
            fv.0.iter().all(|x| x.is_finite()),
            "all features must be finite even with NaN/Inf inputs"
        );
    }

    #[test]
    fn flat_candles_have_zero_volatility() {
        let ctx = mk_ctx(flat_candles(60, 100.0));
        let fv = extract_features(&ctx);
        // log volatility indices 4..8
        for i in 4..8 {
            assert!(
                fv.0[i].abs() < 1e-6,
                "flat candles must yield ~0 vol at idx {} got {}",
                i,
                fv.0[i]
            );
        }
    }

    #[test]
    fn rsi_on_flat_candles_returns_neutral_or_oversold_default() {
        let ctx = mk_ctx(flat_candles(30, 100.0));
        let fv = extract_features(&ctx);
        // RSI on perfectly flat data (no gain, no loss) returns
        // either 100 (no loss → infinite RS clamped) or 50 from
        // the cold-start path. We accept either.
        let rsi = fv.0[16];
        assert!(
            (rsi - 100.0).abs() < 1e-6 || (rsi - 50.0).abs() < 1e-6,
            "RSI must be 100 or 50 for flat data, got {rsi}"
        );
    }

    #[test]
    fn ramp_up_returns_are_positive() {
        let ctx = mk_ctx(ramp_candles(60, 100.0, 0.5));
        let fv = extract_features(&ctx);
        // r1 (idx 0) should be > 0 because every step is +0.5.
        assert!(fv.0[0] > 0.0, "ramp-up r1 must be positive, got {}", fv.0[0]);
        // r60 (idx 3) likewise.
        assert!(fv.0[3] > 0.0);
    }

    #[test]
    fn time_sin_cos_at_market_open() {
        let mut ctx = mk_ctx(ramp_candles(30, 100.0, 0.1));
        ctx.now_et_min = 0;
        let fv = extract_features(&ctx);
        // theta=0 → sin=0, cos=1
        assert!(fv.0[21].abs() < 1e-9);
        assert!((fv.0[22] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn day_of_week_one_hot_has_exactly_one_one() {
        let ctx = mk_ctx(ramp_candles(30, 100.0, 0.1));
        let fv = extract_features(&ctx);
        // dow indices 23..28
        let ones: f64 = fv.0[23..28].iter().sum();
        assert!((ones - 1.0).abs() < 1e-9);
    }

    #[test]
    fn regime_one_hot_correctly_encoded() {
        let mut ctx = mk_ctx(ramp_candles(30, 100.0, 0.1));
        ctx.regime = Some(MarketRegime::Trending);
        let fv = extract_features(&ctx);
        // regime indices 28..32 → trending should be at 29
        assert!(fv.0[28].abs() < 1e-9);
        assert!((fv.0[29] - 1.0).abs() < 1e-9);
        assert!(fv.0[30].abs() < 1e-9);
        assert!(fv.0[31].abs() < 1e-9);
    }

    #[test]
    fn regime_none_has_all_zeros() {
        let mut ctx = mk_ctx(ramp_candles(30, 100.0, 0.1));
        ctx.regime = None;
        let fv = extract_features(&ctx);
        for i in 28..32 {
            assert!(fv.0[i].abs() < 1e-9);
        }
    }

    #[test]
    fn catalyst_flags_encoded() {
        let mut ctx = mk_ctx(ramp_candles(30, 100.0, 0.1));
        ctx.catalyst_score = Some(82.0);
        ctx.is_halted = true;
        let fv = extract_features(&ctx);
        assert!((fv.0[32] - 1.0).abs() < 1e-9, "news flag from Some(score)");
        assert!((fv.0[34] - 82.0).abs() < 1e-9, "score passthrough");
        assert!((fv.0[35] - 1.0).abs() < 1e-9, "halt flag");
    }

    #[test]
    fn normalizer_warmup_is_identity() {
        let n = FeatureNormalizer::new();
        let v = FeatureVec(vec![5.0; FEATURE_DIM]);
        let out = v.normalize(&n);
        assert_eq!(out.0, v.0, "below warmup count, normalizer is identity");
    }

    #[test]
    fn normalizer_converges_on_synthetic_data() {
        let mut n = FeatureNormalizer::new();
        // Push 200 samples with mean 5, std 2 in dim 0 only.
        let mut rng_state = 1u64;
        for _ in 0..200 {
            // tiny LCG for reproducibility, draw from normal-ish
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u1 = (rng_state >> 32) as u32 as f64 / u32::MAX as f64;
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u2 = (rng_state >> 32) as u32 as f64 / u32::MAX as f64;
            let z = (-2.0 * (u1 + 1e-12).ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
            let mut v = vec![0.0; FEATURE_DIM];
            v[0] = 5.0 + 2.0 * z;
            n.update(&FeatureVec(v));
        }
        assert!(
            (n.mean[0] - 5.0).abs() < 0.5,
            "mean should converge near 5, got {}",
            n.mean[0]
        );
        assert!(
            (n.std[0] - 2.0).abs() < 0.5,
            "std should converge near 2, got {}",
            n.std[0]
        );
    }

    #[test]
    fn normalizer_division_by_zero_safe() {
        let mut n = FeatureNormalizer::new();
        for _ in 0..50 {
            n.update(&FeatureVec(vec![3.25; FEATURE_DIM]));
        }
        let out = FeatureVec(vec![3.25; FEATURE_DIM]).normalize(&n);
        assert!(
            out.0.iter().all(|x| x.is_finite()),
            "constant input must not produce NaN/Inf"
        );
    }

    #[test]
    fn normalizer_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "st_feat_norm_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let mut n = FeatureNormalizer::new();
        for _ in 0..40 {
            n.update(&FeatureVec(vec![1.0; FEATURE_DIM]));
        }
        n.save(&dir).expect("save");
        let loaded = FeatureNormalizer::load(&dir);
        assert_eq!(loaded.count, n.count);
        for i in 0..FEATURE_DIM {
            assert!((loaded.mean[i] - n.mean[i]).abs() < 1e-12);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalizer_load_missing_returns_default() {
        let dir = std::env::temp_dir().join(format!(
            "st_feat_norm_missing_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let loaded = FeatureNormalizer::load(&dir);
        assert_eq!(loaded.count, 0);
        assert_eq!(loaded.mean.len(), FEATURE_DIM);
    }

    #[test]
    fn normalize_clamps_extreme_values() {
        let mut n = FeatureNormalizer::new();
        for _ in 0..100 {
            n.update(&FeatureVec(vec![1.0; FEATURE_DIM]));
        }
        // Manually inject a non-zero std (counts > 1 path) by varying one update.
        n.update(&FeatureVec({
            let mut v = vec![1.0; FEATURE_DIM];
            v[0] = 100.0;
            v
        }));
        let mut extreme = vec![1.0; FEATURE_DIM];
        extreme[0] = 1e9;
        let out = FeatureVec(extreme).normalize(&n);
        assert!(
            out.0[0].abs() <= 6.0,
            "normalize should clamp to ±6, got {}",
            out.0[0]
        );
    }

    #[test]
    fn extract_then_normalize_produces_finite_output() {
        let mut n = FeatureNormalizer::new();
        for i in 0..100 {
            let ctx = mk_ctx(ramp_candles(60, 100.0 + i as f64, 0.1));
            let fv = extract_features(&ctx);
            n.update(&fv);
        }
        let ctx = mk_ctx(ramp_candles(60, 200.0, 0.1));
        let fv = extract_features(&ctx);
        let normed = fv.normalize(&n);
        assert!(
            normed.0.iter().all(|x| x.is_finite()),
            "normalized features must all be finite"
        );
    }

    #[test]
    fn extract_with_market_open_time_does_not_panic() {
        let mut ctx = mk_ctx(ramp_candles(60, 100.0, 0.1));
        ctx.now_et_min = 9 * 60 + 30; // 09:30 ET
        let _ = extract_features(&ctx);
        ctx.now_et_min = 16 * 60; // 16:00 ET close
        let _ = extract_features(&ctx);
        ctx.now_et_min = 0; // midnight
        let _ = extract_features(&ctx);
        ctx.now_et_min = 1439; // last minute of day
        let _ = extract_features(&ctx);
    }
}
