//! A+B+T combination and the pre-registered F1 gate
//! (`validation/2026-09-18_sleeve_t_preregistration.md` §Poarta).
//!
//! Combination: `P = ΔAB + ΔT` (A+B unchanged, T as constructed at its 8 %
//! ex-ante target, no ex-post rescaling), then **one** factor
//! `s = sd(ΔAB) / sd(P)` on the common window. Sharpe is invariant to `s`;
//! the drawdown is compared on the scaled series.

use titan_meridian::stats::{deflated_sharpe, prob_sharpe_ratio, sharpe_moments, SharpeMoments};

use crate::curve::{ann_vol, correlation, max_drawdown_dollars, sharpe, stdev};

#[derive(Debug, Clone, PartialEq)]
pub struct Combination {
    pub n: usize,
    pub scale: f64,
    pub sharpe_ab: f64,
    pub sharpe_t: f64,
    pub sharpe_comb: f64,
    pub vol_ab: f64,
    pub vol_t: f64,
    pub vol_comb: f64,
    pub mdd_ab: f64,
    pub mdd_t: f64,
    pub mdd_comb: f64,
    pub corr: f64,
    pub mean_ab_per_year: f64,
    pub mean_t_per_year: f64,
    pub mean_comb_per_year: f64,
    /// scaled combined $ P/L per session
    pub comb_pnl: Vec<f64>,
}

pub fn combine(ab: &[f64], t: &[f64], capital: f64) -> Combination {
    let n = ab.len().min(t.len());
    let raw: Vec<f64> = (0..n).map(|i| ab[i] + t[i]).collect();
    let (sd_ab, sd_raw) = (stdev(&ab[..n]), stdev(&raw));
    let scale = if sd_raw > 0.0 { sd_ab / sd_raw } else { 1.0 };
    let comb: Vec<f64> = raw.iter().map(|p| p * scale).collect();
    let per_year = |x: &[f64]| if n > 0 { x.iter().sum::<f64>() / n as f64 * 252.0 } else { 0.0 };
    Combination {
        n,
        scale,
        sharpe_ab: sharpe(&ab[..n], capital),
        sharpe_t: sharpe(&t[..n], capital),
        sharpe_comb: sharpe(&comb, capital),
        vol_ab: ann_vol(&ab[..n], capital),
        vol_t: ann_vol(&t[..n], capital),
        vol_comb: ann_vol(&comb, capital),
        mdd_ab: max_drawdown_dollars(&ab[..n]),
        mdd_t: max_drawdown_dollars(&t[..n]),
        mdd_comb: max_drawdown_dollars(&comb),
        corr: correlation(&ab[..n], &t[..n]),
        mean_ab_per_year: per_year(&ab[..n]),
        mean_t_per_year: per_year(&t[..n]),
        mean_comb_per_year: per_year(&comb),
        comb_pnl: comb,
    }
}

/// Thresholds exactly as pre-registered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub min_sharpe_t: f64,
    pub max_corr: f64,
    pub min_sharpe_gain: f64,
    pub max_cost_share: f64,
    pub min_dsr: f64,
    /// X0 27 + A+B 37 + T 2
    pub trials: usize,
    /// floor for the cross-trial variance of per-observation Sharpes (X0 family)
    pub var_floor: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { min_sharpe_t: 0.50, max_corr: 0.40, min_sharpe_gain: 0.10, max_cost_share: 0.20, min_dsr: 0.95, trials: 66, var_floor: 1.38e-5 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Check {
    pub value: f64,
    pub pass: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateResult {
    /// G1 Sharpe T net
    pub g1: Check,
    /// G2 corr(T, A+B)
    pub g2: Check,
    /// G3 Sharpe(Comb) − Sharpe(AB)
    pub g3: Check,
    /// G4 MDD(Comb) − MDD(AB) in $ (≥ 0 passes: not deeper)
    pub g4: Check,
    /// G5 costs / gross P/L of T
    pub g5: Check,
    /// G6 DSR of the combination
    pub g6: Check,
    /// informational: PSR of T alone vs 0 (not a gate)
    pub psr_t: f64,
    pub moments_comb: SharpeMoments,
    pub pass: bool,
}

/// Cross-trial variance of per-observation Sharpes, floored at the X0 value.
pub fn var_across_trials(per_obs_sharpes: &[f64], floor: f64) -> f64 {
    let s = stdev(per_obs_sharpes);
    (s * s).max(floor)
}

/// `t_costs / t_gross`; a non-positive gross P/L is reported as +∞ (fails).
pub fn cost_share(t_costs: f64, t_gross: f64) -> f64 {
    if t_gross > 0.0 {
        t_costs / t_gross
    } else {
        f64::INFINITY
    }
}

pub fn evaluate(c: &Combination, t_returns: &[f64], cost_share: f64, var_across: f64, thr: &Thresholds, capital: f64) -> GateResult {
    let comb_returns: Vec<f64> = c.comb_pnl.iter().map(|p| p / capital).collect();
    let moments_comb = sharpe_moments(&comb_returns);
    let dsr = deflated_sharpe(&moments_comb, thr.trials, var_across);
    let g1 = Check { value: c.sharpe_t, pass: c.sharpe_t >= thr.min_sharpe_t };
    let g2 = Check { value: c.corr, pass: c.corr <= thr.max_corr };
    let gain = c.sharpe_comb - c.sharpe_ab;
    let g3 = Check { value: gain, pass: gain >= thr.min_sharpe_gain };
    let dd_gain = c.mdd_comb - c.mdd_ab;
    let g4 = Check { value: dd_gain, pass: dd_gain >= 0.0 };
    let g5 = Check { value: cost_share, pass: cost_share <= thr.max_cost_share };
    let g6 = Check { value: dsr, pass: dsr >= thr.min_dsr };
    let psr_t = prob_sharpe_ratio(&sharpe_moments(t_returns), 0.0);
    let pass = [g1, g2, g3, g4, g5, g6].iter().all(|g| g.pass);
    GateResult { g1, g2, g3, g4, g5, g6, psr_t, moments_comb, pass }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(n: usize, mu: f64, amp: f64, phase: usize) -> Vec<f64> {
        (0..n).map(|k| mu + amp * if (k + phase).is_multiple_of(2) { 1.0 } else { -1.0 }).collect()
    }

    #[test]
    fn combination_scales_to_ab_volatility_and_sharpe_is_invariant() {
        let ab = series(1000, 75.0, 800.0, 0);
        let t = series(1000, 20.0, 300.0, 1); // anti-phase ⇒ negatively correlated
        let c = combine(&ab, &t, 100_000.0);
        assert!((c.vol_comb - c.vol_ab).abs() < 1e-9, "scaled to A+B vol");
        assert!(c.corr < -0.99);
        let raw: Vec<f64> = ab.iter().zip(&t).map(|(a, b)| a + b).collect();
        assert!((c.sharpe_comb - sharpe(&raw, 100_000.0)).abs() < 1e-9, "Sharpe does not depend on s");
        assert!(c.scale > 1.0, "a hedge reduces raw vol, so the scale-up is > 1");
        assert!((c.mean_ab_per_year - 75.0 * 252.0).abs() < 1e-6);
    }

    #[test]
    fn gate_passes_a_clean_diversifier_and_fails_a_beta_clone() {
        // A+B: four up days then four down days (a multi-day drawdown of 4 × 725)
        let ab: Vec<f64> = (0..2000).map(|k| 75.0 + 800.0 * if k % 8 < 4 { 1.0 } else { -1.0 }).collect();
        // diversifier: orthogonal pattern (period 4), positive Sharpe
        let t: Vec<f64> = (0..2000).map(|k| 25.0 + 250.0 * [1.0, 0.0, -1.0, 0.0][k % 4]).collect();
        let c = combine(&ab, &t, 100_000.0);
        let tr: Vec<f64> = t.iter().map(|p| p / 100_000.0).collect();
        let r = evaluate(&c, &tr, 0.05, 1.38e-5, &Thresholds::default(), 100_000.0);
        assert!(r.g1.pass && r.g2.pass && r.g3.pass && r.g5.pass, "{r:?}");
        assert!(r.g6.value > 0.99, "DSR {}", r.g6.value);
        assert!(r.pass, "{r:?}");
        // a clone of A+B at half size: corr 1, no Sharpe gain ⇒ fails G2 and G3
        let clone: Vec<f64> = ab.iter().map(|p| p * 0.5).collect();
        let c2 = combine(&ab, &clone, 100_000.0);
        let r2 = evaluate(&c2, &tr, 0.05, 1.38e-5, &Thresholds::default(), 100_000.0);
        assert!(!r2.g2.pass && !r2.g3.pass && !r2.pass);
        assert!((c2.scale - 1.0 / 1.5).abs() < 1e-9);
    }

    #[test]
    fn cost_share_and_variance_floor() {
        assert!((cost_share(20.0, 100.0) - 0.2).abs() < 1e-12);
        assert!(cost_share(1.0, 0.0).is_infinite());
        assert_eq!(var_across_trials(&[0.08, 0.08, 0.08], 1.38e-5), 1.38e-5);
        assert!(var_across_trials(&[0.05, 0.09, 0.07], 1.38e-5) > 1.38e-5);
    }
}
