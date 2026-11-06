//! Probabilistic and Deflated Sharpe Ratio (Bailey & López de Prado 2012,
//! 2014). Sharpe ratios here are per-observation (not annualised) and the
//! sample length is the number of observations.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SharpeMoments {
    pub sharpe: f64,
    pub n: usize,
    pub skew: f64,
    pub kurtosis: f64,
}

pub fn sharpe_moments(returns: &[f64]) -> SharpeMoments {
    let p = super::metrics::performance(returns);
    let per_obs = if p.ann_vol > 0.0 { p.sharpe / super::metrics::TRADING_DAYS.sqrt() } else { 0.0 };
    SharpeMoments { sharpe: per_obs, n: returns.len(), skew: p.skew, kurtosis: p.kurtosis }
}

/// Standard normal CDF via erfc-free rational approximation (Abramowitz–Stegun 7.1.26).
pub fn norm_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.2316419 * x.abs());
    let poly = t * (0.319381530 + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    let pdf = (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let cdf_abs = 1.0 - pdf * poly;
    if x >= 0.0 {
        cdf_abs
    } else {
        1.0 - cdf_abs
    }
}

/// Inverse normal CDF (Acklam's algorithm), adequate to ~1e-9.
#[allow(clippy::excessive_precision)]
pub fn norm_ppf(p: f64) -> f64 {
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    const A: [f64; 6] = [-3.969683028665376e1, 2.209460984245205e2, -2.759285104469687e2, 1.383577518672690e2, -3.066479806614716e1, 2.506628277459239];
    const B: [f64; 5] = [-5.447609879822406e1, 1.615858368580409e2, -1.556989798598866e2, 6.680131188771972e1, -1.328068155288572e1];
    const C: [f64; 6] = [-7.784894002430293e-3, -3.223964580411365e-1, -2.400758277161838, -2.549732539343734, 4.374664141464968, 2.938163982698783];
    const D: [f64; 4] = [7.784695709041462e-3, 3.224671290700398e-1, 2.445134137142996, 3.754408661907416];
    let (pl, ph) = (0.02425, 1.0 - 0.02425);
    if p < pl {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= ph {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

/// PSR: probability that the true Sharpe exceeds `benchmark` given the
/// observed Sharpe, sample length and higher moments.
pub fn prob_sharpe_ratio(m: &SharpeMoments, benchmark: f64) -> f64 {
    if m.n < 2 {
        return 0.5;
    }
    let sr = m.sharpe;
    let var = (1.0 - m.skew * sr + (m.kurtosis - 1.0) / 4.0 * sr * sr) / (m.n as f64 - 1.0);
    if var <= 0.0 {
        return if sr > benchmark { 1.0 } else { 0.0 };
    }
    norm_cdf((sr - benchmark) / var.sqrt())
}

/// Expected maximum Sharpe among `trials` independent trials whose true
/// Sharpe is zero, given the cross-trial variance of estimated Sharpes.
pub fn expected_max_sharpe(trials: usize, var_across_trials: f64) -> f64 {
    if trials < 2 {
        return 0.0;
    }
    let n = trials as f64;
    let euler = 0.5772156649015329;
    var_across_trials.sqrt() * ((1.0 - euler) * norm_ppf(1.0 - 1.0 / n) + euler * norm_ppf(1.0 - 1.0 / (n * std::f64::consts::E)))
}

/// Deflated Sharpe Ratio = PSR with the benchmark set to the expected max
/// Sharpe from selection among `trials` configurations.
pub fn deflated_sharpe(m: &SharpeMoments, trials: usize, var_across_trials: f64) -> f64 {
    prob_sharpe_ratio(m, expected_max_sharpe(trials, var_across_trials))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_helpers() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-3);
        assert!((norm_ppf(0.975) - 1.96).abs() < 1e-3);
        assert!((norm_cdf(norm_ppf(0.3)) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn zero_sharpe_gives_half_and_positive_sharpe_high_psr() {
        let zero = SharpeMoments { sharpe: 0.0, n: 1000, skew: 0.0, kurtosis: 3.0 };
        assert!((prob_sharpe_ratio(&zero, 0.0) - 0.5).abs() < 1e-9);
        // annual Sharpe 1.5 over 4 years daily ≈ per-obs 0.0945, n=1008
        let good = SharpeMoments { sharpe: 1.5 / 252f64.sqrt(), n: 1008, skew: -0.3, kurtosis: 5.0 };
        assert!(prob_sharpe_ratio(&good, 0.0) > 0.99);
        // after 100 trials the hurdle rises; still passes at this sample size
        let dsr = deflated_sharpe(&good, 100, 0.02f64.powi(2));
        assert!(dsr > 0.9 && dsr < 1.0, "{dsr}");
        // a weak Sharpe selected from many trials is deflated below 0.95
        let weak = SharpeMoments { sharpe: 0.5 / 252f64.sqrt(), n: 500, skew: 0.0, kurtosis: 3.0 };
        assert!(deflated_sharpe(&weak, 200, 0.03f64.powi(2)) < 0.95);
    }

    #[test]
    fn expected_max_grows_with_trials() {
        let a = expected_max_sharpe(10, 1.0);
        let b = expected_max_sharpe(1000, 1.0);
        assert!(b > a && a > 1.0);
        assert_eq!(expected_max_sharpe(1, 1.0), 0.0);
    }
}
