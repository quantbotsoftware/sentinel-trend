//! Probability of Backtest Overfitting via Combinatorially Symmetric
//! Cross-Validation (Bailey, Borwein, López de Prado & Zhu 2017).
//!
//! Input: matrix of per-period returns for `k` candidate configurations
//! (rows = observations, columns = configurations). Split observations into
//! `s` equal blocks; for every combination of s/2 blocks as in-sample, pick
//! the IS-best configuration and record its relative rank OOS. PBO is the
//! fraction of combinations where the IS-best is below the OOS median.

use serde::{Deserialize, Serialize};

use super::metrics::performance;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PboResult {
    pub pbo: f64,
    pub combinations: usize,
    pub configs: usize,
    /// Mean OOS Sharpe of the IS-selected configuration.
    pub oos_sharpe_of_selected: f64,
    /// Mean OOS Sharpe across all configurations (baseline).
    pub oos_sharpe_mean: f64,
}

/// All k-subsets of 0..n in lexicographic order.
fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    if k > n {
        return out;
    }
    let mut cur: Vec<usize> = (0..k).collect();
    loop {
        out.push(cur.clone());
        // rightmost position that can still be incremented
        let mut i = k;
        while i > 0 && cur[i - 1] == i - 1 + n - k {
            i -= 1;
        }
        if i == 0 {
            return out;
        }
        cur[i - 1] += 1;
        for j in i..k {
            cur[j] = cur[j - 1] + 1;
        }
    }
}

/// `returns[config][t]`; all configs must share the same length.
pub fn pbo(returns: &[Vec<f64>], s_blocks: usize) -> Option<PboResult> {
    let k = returns.len();
    let t = returns.first()?.len();
    if k < 2 || s_blocks < 2 || !s_blocks.is_multiple_of(2) || t < s_blocks * 2 {
        return None;
    }
    if returns.iter().any(|r| r.len() != t) {
        return None;
    }
    let block = t / s_blocks;
    let combos = combinations(s_blocks, s_blocks / 2);
    let mut below_median = 0usize;
    let mut sel_oos_sum = 0.0;
    let mut all_oos_sum = 0.0;
    for is_blocks in &combos {
        let is_set: Vec<bool> = (0..s_blocks).map(|b| is_blocks.contains(&b)).collect();
        let mut is_sharpe = vec![0.0; k];
        let mut oos_sharpe = vec![0.0; k];
        for (ci, r) in returns.iter().enumerate() {
            let mut is_v = Vec::with_capacity(t / 2);
            let mut oos_v = Vec::with_capacity(t / 2);
            for (b, in_sample) in is_set.iter().enumerate() {
                let slice = &r[b * block..(b + 1) * block];
                if *in_sample {
                    is_v.extend_from_slice(slice);
                } else {
                    oos_v.extend_from_slice(slice);
                }
            }
            is_sharpe[ci] = performance(&is_v).sharpe;
            oos_sharpe[ci] = performance(&oos_v).sharpe;
        }
        let best = (0..k)
            .max_by(|a, b| is_sharpe[*a].partial_cmp(&is_sharpe[*b]).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(0);
        let mut sorted = oos_sharpe.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[k / 2];
        if oos_sharpe[best] < median {
            below_median += 1;
        }
        sel_oos_sum += oos_sharpe[best];
        all_oos_sum += oos_sharpe.iter().sum::<f64>() / k as f64;
    }
    let c = combos.len();
    Some(PboResult {
        pbo: below_median as f64 / c as f64,
        combinations: c,
        configs: k,
        oos_sharpe_of_selected: sel_oos_sum / c as f64,
        oos_sharpe_mean: all_oos_sum / c as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::rng::XorShift;

    #[test]
    fn combinations_count() {
        assert_eq!(combinations(4, 2).len(), 6);
        assert_eq!(combinations(16, 8).len(), 12870);
        assert_eq!(combinations(2, 3).len(), 0);
    }

    /// A single dataset's PBO is a noisy estimate (the 70 combinations are
    /// strongly dependent), so average over independent datasets.
    #[test]
    fn iid_noise_configs_have_pbo_near_half() {
        let mut acc = 0.0;
        let seeds = 12;
        for seed in 0..seeds {
            let mut rng = XorShift::new(100 + seed);
            let cfgs: Vec<Vec<f64>> = (0..20).map(|_| (0..640).map(|_| 0.01 * rng.normal()).collect()).collect();
            let r = pbo(&cfgs, 8).unwrap();
            assert_eq!(r.combinations, 70);
            acc += r.pbo;
        }
        let mean = acc / seeds as f64;
        assert!(mean > 0.35 && mean < 0.65, "mean pbo={mean}");
    }

    #[test]
    fn one_truly_good_config_has_low_pbo() {
        let mut rng = XorShift::new(9);
        let mut cfgs: Vec<Vec<f64>> = (0..10).map(|_| (0..640).map(|_| 0.01 * rng.normal()).collect()).collect();
        cfgs[3] = (0..640).map(|_| 0.004 + 0.01 * rng.normal()).collect(); // Sharpe ≈ 6
        let r = pbo(&cfgs, 8).unwrap();
        assert!(r.pbo < 0.1, "pbo={}", r.pbo);
        assert!(r.oos_sharpe_of_selected > r.oos_sharpe_mean);
    }

    #[test]
    fn invalid_inputs() {
        assert!(pbo(&[vec![0.0; 10]], 4).is_none());
        assert!(pbo(&[vec![0.0; 10], vec![0.0; 9]], 4).is_none());
        assert!(pbo(&[vec![0.0; 100], vec![0.0; 100]], 3).is_none());
    }
}
