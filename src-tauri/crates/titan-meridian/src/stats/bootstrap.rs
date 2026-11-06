//! Stationary/circular block bootstrap on a daily return series — confidence
//! intervals for Sharpe and max drawdown that respect autocorrelation.

use serde::{Deserialize, Serialize};

use super::metrics::{max_drawdown, performance};
use super::rng::XorShift;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BootstrapSummary {
    pub replicas: usize,
    pub block_len: usize,
    pub sharpe_p05: f64,
    pub sharpe_p50: f64,
    pub sharpe_p95: f64,
    pub mdd_p05: f64,
    pub mdd_p50: f64,
    pub mdd_p95: f64,
    /// Fraction of replicas with a negative total return.
    pub prob_loss: f64,
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = (q * (sorted.len() - 1) as f64).round() as usize;
    sorted[pos.min(sorted.len() - 1)]
}

/// Circular block bootstrap: resample `n` observations by concatenating
/// blocks of `block_len` consecutive returns starting at uniform offsets.
pub fn block_bootstrap(returns: &[f64], block_len: usize, replicas: usize, seed: u64) -> Option<BootstrapSummary> {
    let n = returns.len();
    if n < 2 * block_len.max(1) || replicas == 0 {
        return None;
    }
    let block_len = block_len.max(1);
    let mut rng = XorShift::new(seed);
    let mut sharpes = Vec::with_capacity(replicas);
    let mut mdds = Vec::with_capacity(replicas);
    let mut losses = 0usize;
    let mut sample = Vec::with_capacity(n + block_len);
    for _ in 0..replicas {
        sample.clear();
        while sample.len() < n {
            let start = rng.below(n);
            for k in 0..block_len {
                sample.push(returns[(start + k) % n]);
            }
        }
        sample.truncate(n);
        let p = performance(&sample);
        sharpes.push(p.sharpe);
        mdds.push(max_drawdown(&sample));
        if p.total_return < 0.0 {
            losses += 1;
        }
    }
    sharpes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    mdds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(BootstrapSummary {
        replicas,
        block_len,
        sharpe_p05: quantile(&sharpes, 0.05),
        sharpe_p50: quantile(&sharpes, 0.50),
        sharpe_p95: quantile(&sharpes, 0.95),
        // drawdowns are negative: p05 is the *worst* tail
        mdd_p05: quantile(&mdds, 0.05),
        mdd_p50: quantile(&mdds, 0.50),
        mdd_p95: quantile(&mdds, 0.95),
        prob_loss: losses as f64 / replicas as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_brackets_the_point_estimate_and_is_deterministic() {
        let mut rng = XorShift::new(3);
        let r: Vec<f64> = (0..1000).map(|_| 0.0006 + 0.01 * rng.normal()).collect();
        let p = performance(&r);
        let a = block_bootstrap(&r, 20, 500, 11).unwrap();
        let b = block_bootstrap(&r, 20, 500, 11).unwrap();
        assert_eq!(a, b);
        assert!(a.sharpe_p05 < p.sharpe && p.sharpe < a.sharpe_p95, "{a:?} vs {}", p.sharpe);
        assert!(a.mdd_p05 <= a.mdd_p50 && a.mdd_p50 <= a.mdd_p95);
        assert!(a.prob_loss < 0.5);
    }

    #[test]
    fn too_short_is_none() {
        assert!(block_bootstrap(&[0.1; 10], 20, 10, 1).is_none());
        assert!(block_bootstrap(&[0.1; 100], 20, 0, 1).is_none());
    }
}
