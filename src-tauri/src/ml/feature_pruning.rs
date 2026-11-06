//! Entropy-Based Dynamic Feature Pruning.
//!
//! Computes per-dimension Mutual Information I(X_d; Y) between feature
//! columns and the binarised label, then zeros out dimensions whose MI
//! falls below a noise floor. This removes noisy or uninformative
//! features before training, improving model generalisation and
//! convergence speed.
//!
//! ## Algorithm
//!
//! 1. Discretise each feature dimension into `NUM_BINS` (10) histogram
//!    bins using equal-frequency quantiles (percentile-based).
//! 2. Discretise the label Y into `NUM_BINS` bins the same way.
//! 3. For each dimension d, compute the joint and marginal probability
//!    tables, then Shannon MI:
//!    `I(X_d; Y) = Σ_{x,y} P(x,y) · log(P(x,y) / (P(x)·P(y)))`
//! 4. The pruning threshold is `max(MI_FLOOR, percentile_25(MI_scores))`.
//! 5. Features with MI below the threshold are zeroed in-place.
//!
//! ## Complexity
//!
//! O(D × N) where D = FEATURE_DIM (128) and N = sample count. Fast
//! enough for ~100k samples.

use crate::ml::features::{FeatureVec, FEATURE_DIM};

/// Number of bins for the MI discretisation.
const NUM_BINS: usize = 10;

/// Absolute MI floor — even if the 25th percentile is lower, we never
/// prune features whose MI exceeds this value.
const MI_FLOOR: f64 = 0.005;

/// Compute per-dimension Mutual Information scores between features
/// and binarised labels (positive return → 1, else → 0).
///
/// Returns a `Vec<f64>` of length `FEATURE_DIM`.
pub fn compute_feature_mutual_information(
    samples: &[(FeatureVec, f64)],
) -> Vec<f64> {
    let n = samples.len();
    if n < 20 {
        // Too few samples for meaningful MI estimation.
        return vec![0.0; FEATURE_DIM];
    }

    // Binarise labels: positive return → 1, else → 0.
    let y_bins: Vec<usize> = samples
        .iter()
        .map(|(_, label)| if *label > 0.0 { 1 } else { 0 })
        .collect();
    let n_y_bins = 2usize; // binary classification

    let nf = n as f64;
    let mut mi_scores = Vec::with_capacity(FEATURE_DIM);

    for d in 0..FEATURE_DIM {
        // Collect feature values for dimension d.
        let mut vals: Vec<f64> = samples.iter().map(|(fv, _)| {
            if d < fv.0.len() { fv.0[d] } else { 0.0 }
        }).collect();

        // Discretise using equal-frequency quantile bins.
        let x_bins = quantile_discretise(&mut vals, NUM_BINS);

        // Build joint frequency table [x_bin][y_bin].
        let mut joint = vec![vec![0u32; n_y_bins]; NUM_BINS];
        let mut x_margin = [0u32; NUM_BINS];
        let mut y_margin = vec![0u32; n_y_bins];

        for i in 0..n {
            let xb = x_bins[i].min(NUM_BINS - 1);
            let yb = y_bins[i].min(n_y_bins - 1);
            joint[xb][yb] += 1;
            x_margin[xb] += 1;
            y_margin[yb] += 1;
        }

        // Compute MI = Σ P(x,y) * log(P(x,y) / (P(x) * P(y)))
        let mut mi = 0.0_f64;
        for xb in 0..NUM_BINS {
            let px = x_margin[xb] as f64 / nf;
            if px < 1e-15 {
                continue;
            }
            for yb in 0..n_y_bins {
                let py = y_margin[yb] as f64 / nf;
                let pxy = joint[xb][yb] as f64 / nf;
                if pxy < 1e-15 || py < 1e-15 {
                    continue;
                }
                mi += pxy * (pxy / (px * py)).ln();
            }
        }
        mi_scores.push(mi.max(0.0));
    }

    mi_scores
}

/// Apply entropy-based pruning mask in-place: zero out features whose
/// MI is below `max(MI_FLOOR, 25th-percentile(mi_scores))`.
///
/// Returns the number of features pruned.
pub fn apply_entropy_pruning(
    samples: &mut [(FeatureVec, f64)],
    mi_scores: &[f64],
) -> usize {
    if mi_scores.len() != FEATURE_DIM {
        return 0;
    }

    let threshold = {
        let mut sorted: Vec<f64> = mi_scores.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p25_idx = (sorted.len() as f64 * 0.25).floor() as usize;
        let p25 = sorted.get(p25_idx).copied().unwrap_or(0.0);
        p25.max(MI_FLOOR)
    };

    // Build mask: true = keep, false = prune.
    let mask: Vec<bool> = mi_scores.iter().map(|&mi| mi >= threshold).collect();
    let pruned_count = mask.iter().filter(|&&keep| !keep).count();

    if pruned_count == 0 {
        return 0;
    }

    tracing::info!(
        component = "omega",
        threshold = format!("{:.6}", threshold),
        pruned = pruned_count,
        kept = FEATURE_DIM - pruned_count,
        "Entropy pruning: zeroed noisy feature dimensions"
    );

    for (fv, _) in samples.iter_mut() {
        for (d, &keep) in mask.iter().enumerate() {
            if !keep && d < fv.0.len() {
                fv.0[d] = 0.0;
            }
        }
    }

    pruned_count
}

/// Apply entropy-based pruning mask to Transformer sequences in-place:
/// zeros out feature dimensions in each `FeatureVec` inside the sequence
/// if they fall below the threshold determined by `mi_scores`.
pub fn apply_entropy_pruning_xfmr(
    samples: &mut [(Vec<FeatureVec>, crate::smarttrend::strategies::MarketRegime)],
    mi_scores: &[f64],
) -> usize {
    if mi_scores.len() != FEATURE_DIM {
        return 0;
    }

    let threshold = {
        let mut sorted: Vec<f64> = mi_scores.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p25_idx = (sorted.len() as f64 * 0.25).floor() as usize;
        let p25 = sorted.get(p25_idx).copied().unwrap_or(0.0);
        p25.max(MI_FLOOR)
    };

    // Build mask: true = keep, false = prune.
    let mask: Vec<bool> = mi_scores.iter().map(|&mi| mi >= threshold).collect();
    let pruned_count = mask.iter().filter(|&&keep| !keep).count();

    if pruned_count == 0 {
        return 0;
    }

    for (seq, _) in samples.iter_mut() {
        for fv in seq.iter_mut() {
            for (d, &keep) in mask.iter().enumerate() {
                if !keep && d < fv.0.len() {
                    fv.0[d] = 0.0;
                }
            }
        }
    }

    pruned_count
}

/// Discretise `vals` into `n_bins` using equal-frequency quantiles.
/// Returns a vec of bin indices [0, n_bins) parallel to the original order.
fn quantile_discretise(vals: &mut [f64], n_bins: usize) -> Vec<usize> {
    let n = vals.len();
    if n == 0 {
        return Vec::new();
    }
    // Pair each value with its original index.
    let mut indexed: Vec<(usize, f64)> = vals.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut bins = vec![0usize; n];
    let bin_size = (n as f64 / n_bins as f64).ceil() as usize;
    for (rank, &(orig_idx, _)) in indexed.iter().enumerate() {
        bins[orig_idx] = (rank / bin_size.max(1)).min(n_bins - 1);
    }
    bins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_samples(n: usize) -> Vec<(FeatureVec, f64)> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let mut v = vec![0.0; FEATURE_DIM];
            // Feature 0: strongly correlated with label.
            let signal = if i % 2 == 0 { 1.0 } else { -1.0 };
            v[0] = signal;
            // Feature 1: pure noise.
            v[1] = (i as f64 * 17.3).sin();
            // Feature 2: weakly correlated.
            v[2] = signal * 0.3 + (i as f64 * 3.7).cos() * 0.7;
            let label = if i % 2 == 0 { 1.0 } else { -1.0 };
            out.push((FeatureVec(v), label));
        }
        out
    }

    #[test]
    fn mi_returns_feature_dim_length() {
        let samples = mk_samples(100);
        let mi = compute_feature_mutual_information(&samples);
        assert_eq!(mi.len(), FEATURE_DIM);
    }

    #[test]
    fn mi_all_non_negative() {
        let samples = mk_samples(200);
        let mi = compute_feature_mutual_information(&samples);
        for (d, &score) in mi.iter().enumerate() {
            assert!(score >= 0.0, "MI[{d}] = {score} must be >= 0");
        }
    }

    #[test]
    fn correlated_feature_has_higher_mi_than_noise() {
        let samples = mk_samples(500);
        let mi = compute_feature_mutual_information(&samples);
        // Feature 0 (perfect signal) should have higher MI than feature 1 (noise).
        assert!(
            mi[0] > mi[1],
            "Signal MI ({}) should exceed noise MI ({})",
            mi[0],
            mi[1]
        );
    }

    #[test]
    fn pruning_zeros_low_mi_features() {
        let mut samples = mk_samples(300);
        let mi = compute_feature_mutual_information(&samples);
        let pruned = apply_entropy_pruning(&mut samples, &mi);
        // At least some noise dimensions should be pruned.
        assert!(pruned > 0, "Expected some features to be pruned");
        // Feature 0 (high MI) should NOT be zeroed.
        assert!(
            samples.iter().any(|(fv, _)| fv.0[0] != 0.0),
            "Signal feature 0 should not be fully zeroed"
        );
    }

    #[test]
    fn pruning_preserves_high_mi_features() {
        let mut samples = mk_samples(500);
        let mi = compute_feature_mutual_information(&samples);
        apply_entropy_pruning(&mut samples, &mi);
        // Verify high-MI feature 0 values are preserved.
        let has_nonzero = samples.iter().any(|(fv, _)| fv.0[0].abs() > 0.5);
        assert!(has_nonzero, "Feature 0 with high MI should be preserved");
    }

    #[test]
    fn too_few_samples_returns_zeros() {
        let samples = mk_samples(10);
        let mi = compute_feature_mutual_information(&samples);
        assert!(mi.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn wrong_dimension_scores_returns_zero_pruned() {
        let mut samples = mk_samples(100);
        let bad_scores = vec![0.1; 5]; // wrong length
        let pruned = apply_entropy_pruning(&mut samples, &bad_scores);
        assert_eq!(pruned, 0);
    }

    #[test]
    fn xfmr_pruning_zeros_low_mi_features() {
        let samples = mk_samples(300);
        let mi = compute_feature_mutual_information(&samples);
        let mut xfmr_samples = vec![
            (vec![FeatureVec(vec![1.0; FEATURE_DIM]), FeatureVec(vec![2.0; FEATURE_DIM])], crate::smarttrend::strategies::MarketRegime::Calm)
        ];
        let pruned = apply_entropy_pruning_xfmr(&mut xfmr_samples, &mi);
        assert!(pruned > 0);
        // Verify low MI features are zeroed.
        // Assuming at least index 1 is pruned.
        let threshold = {
            let mut sorted: Vec<f64> = mi.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            sorted[(sorted.len() as f64 * 0.25).floor() as usize].max(0.005)
        };
        for d in 0..FEATURE_DIM {
            if mi[d] < threshold {
                assert_eq!(xfmr_samples[0].0[0].0[d], 0.0);
                assert_eq!(xfmr_samples[0].0[1].0[d], 0.0);
            } else {
                assert_ne!(xfmr_samples[0].0[0].0[d], 0.0);
            }
        }
    }
}
