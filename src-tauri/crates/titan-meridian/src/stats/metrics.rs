//! Performance metrics on a daily return series.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Performance {
    pub n: usize,
    pub total_return: f64,
    pub cagr: f64,
    pub ann_vol: f64,
    pub sharpe: f64,
    pub sortino: f64,
    pub max_drawdown: f64,
    pub hit_rate: f64,
    pub profit_factor: f64,
    pub avg_daily: f64,
    pub skew: f64,
    pub kurtosis: f64,
}

pub const TRADING_DAYS: f64 = 252.0;

pub fn max_drawdown(returns: &[f64]) -> f64 {
    let mut eq = 1.0_f64;
    let mut peak = 1.0_f64;
    let mut mdd = 0.0_f64;
    for r in returns {
        eq *= 1.0 + r;
        peak = peak.max(eq);
        mdd = f64::min(mdd, eq / peak - 1.0);
    }
    mdd
}

pub fn performance(returns: &[f64]) -> Performance {
    let n = returns.len();
    if n < 2 {
        return Performance { n, ..Default::default() };
    }
    let nf = n as f64;
    let mean = returns.iter().sum::<f64>() / nf;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (nf - 1.0);
    let sd = var.sqrt();
    let m3 = returns.iter().map(|r| (r - mean).powi(3)).sum::<f64>() / nf;
    let m4 = returns.iter().map(|r| (r - mean).powi(4)).sum::<f64>() / nf;
    let skew = if sd > 0.0 { m3 / sd.powi(3) } else { 0.0 };
    let kurtosis = if sd > 0.0 { m4 / sd.powi(4) } else { 3.0 };
    let downside: Vec<f64> = returns.iter().map(|r| r.min(0.0).powi(2)).collect();
    let dd = (downside.iter().sum::<f64>() / nf).sqrt();
    let eq: f64 = returns.iter().map(|r| 1.0 + r).product();
    let gains: f64 = returns.iter().filter(|r| **r > 0.0).sum();
    let losses: f64 = returns.iter().filter(|r| **r < 0.0).map(|r| -r).sum();
    Performance {
        n,
        total_return: eq - 1.0,
        cagr: if eq > 0.0 { eq.powf(TRADING_DAYS / nf) - 1.0 } else { -1.0 },
        ann_vol: sd * TRADING_DAYS.sqrt(),
        sharpe: if sd > 0.0 { mean / sd * TRADING_DAYS.sqrt() } else { 0.0 },
        sortino: if dd > 0.0 { mean / dd * TRADING_DAYS.sqrt() } else { 0.0 },
        max_drawdown: max_drawdown(returns),
        hit_rate: returns.iter().filter(|r| **r > 0.0).count() as f64 / nf,
        profit_factor: if losses > 0.0 { gains / losses } else if gains > 0.0 { f64::INFINITY } else { 0.0 },
        avg_daily: mean,
        skew,
        kurtosis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        let r = [0.01, -0.005, 0.02, -0.01];
        let p = performance(&r);
        assert_eq!(p.n, 4);
        assert!((p.total_return - (1.01 * 0.995 * 1.02 * 0.99 - 1.0)).abs() < 1e-12);
        assert!((p.hit_rate - 0.5).abs() < 1e-12);
        assert!((p.profit_factor - 0.03 / 0.015).abs() < 1e-12);
        assert!(p.sharpe > 0.0);
        // drawdown: after +1% then −0.5% → −0.5 %; later peak; then −1 %
        assert!((max_drawdown(&r) + 0.01).abs() < 1e-12);
    }

    #[test]
    fn degenerate_inputs() {
        assert_eq!(performance(&[]).n, 0);
        let flat = performance(&[0.0, 0.0, 0.0]);
        assert_eq!(flat.sharpe, 0.0);
        assert_eq!(flat.profit_factor, 0.0);
        assert_eq!(max_drawdown(&[0.1, 0.1]), 0.0);
    }
}
