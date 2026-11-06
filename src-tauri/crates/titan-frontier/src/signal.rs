//! Sleeve T signal and weights — pre-registration §Regula T.
//!
//! * momentum: `C(t) / C(t − L) − 1 > 0` ⇒ long, else flat (no short);
//! * volatility: sample stdev of simple daily returns over `vol_window`
//!   sessions, annualised √252; covariance on the same window;
//! * weights: `r_i = 1/σ_i` on active instruments, scaled so the ex-ante
//!   sleeve vol `√(wᵀΣw)` equals `target_vol`; then cap per instrument;
//!   then gross cap by proportional scaling.

use crate::daily::Panel;

pub const TRADING_DAYS: f64 = 252.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalParams {
    /// momentum lookback in sessions (252 primary, 126 declared variant)
    pub lookback: usize,
    pub vol_window: usize,
    /// annualised ex-ante volatility target of the sleeve
    pub target_vol: f64,
    /// per-instrument weight cap
    pub max_weight: f64,
    /// gross exposure cap (long-only ⇒ sum of weights)
    pub max_gross: f64,
}

impl Default for SignalParams {
    fn default() -> Self {
        Self { lookback: 252, vol_window: 60, target_vol: 0.08, max_weight: 0.25, max_gross: 1.0 }
    }
}

/// `C(t)/C(t−L) − 1`, `None` without `L + 1` closes.
pub fn momentum(panel: &Panel, t: usize, i: usize, lookback: usize) -> Option<f64> {
    let now = panel.close(t, i)?;
    let then = panel.close(t.checked_sub(lookback)?, i)?;
    (then > 0.0).then(|| now / then - 1.0)
}

/// The `window` simple daily returns ending at session `t` (inclusive).
pub fn trailing_returns(panel: &Panel, t: usize, i: usize, window: usize) -> Option<Vec<f64>> {
    let start = t.checked_sub(window)?;
    let mut out = Vec::with_capacity(window);
    for k in start..t {
        let a = panel.close(k, i)?;
        let b = panel.close(k + 1, i)?;
        if a <= 0.0 {
            return None;
        }
        out.push(b / a - 1.0);
    }
    Some(out)
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// Sample covariance (N − 1) of equally long return series, annualised.
pub fn covariance(rets: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let k = rets.len();
    let n = rets.first().map(|r| r.len()).unwrap_or(0);
    let means: Vec<f64> = rets.iter().map(|r| mean(r)).collect();
    let mut cov = vec![vec![0.0; k]; k];
    if n < 2 {
        return cov;
    }
    for a in 0..k {
        for b in a..k {
            let s: f64 = (0..n).map(|t| (rets[a][t] - means[a]) * (rets[b][t] - means[b])).sum();
            let c = s / (n as f64 - 1.0) * TRADING_DAYS;
            cov[a][b] = c;
            cov[b][a] = c;
        }
    }
    cov
}

/// Annualised sample volatility of one return series.
pub fn ann_vol(rets: &[f64]) -> f64 {
    covariance(&[rets.to_vec()])[0][0].sqrt()
}

/// Target weights on session `t`, aligned with `panel.symbols`.
pub fn target_weights(panel: &Panel, t: usize, p: &SignalParams) -> Vec<f64> {
    let n = panel.n_symbols();
    let mut active: Vec<(usize, Vec<f64>)> = Vec::new();
    for i in 0..n {
        let Some(m) = momentum(panel, t, i, p.lookback) else { continue };
        if m <= 0.0 {
            continue;
        }
        let Some(r) = trailing_returns(panel, t, i, p.vol_window) else { continue };
        if ann_vol(&r) > 0.0 {
            active.push((i, r));
        }
    }
    let mut w = vec![0.0; n];
    if active.is_empty() {
        return w;
    }
    let rets: Vec<Vec<f64>> = active.iter().map(|(_, r)| r.clone()).collect();
    let cov = covariance(&rets);
    let raw: Vec<f64> = (0..active.len()).map(|a| 1.0 / cov[a][a].sqrt()).collect();
    let mut var = 0.0;
    for a in 0..raw.len() {
        for b in 0..raw.len() {
            var += raw[a] * raw[b] * cov[a][b];
        }
    }
    let lambda = if var > 0.0 { p.target_vol / var.sqrt() } else { 0.0 };
    for (a, (i, _)) in active.iter().enumerate() {
        w[*i] = (lambda * raw[a]).min(p.max_weight);
    }
    let gross: f64 = w.iter().sum();
    if gross > p.max_gross && gross > 0.0 {
        let s = p.max_gross / gross;
        for x in w.iter_mut() {
            *x *= s;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daily::Series;
    use chrono::NaiveDate;

    fn panel(cols: &[Vec<f64>]) -> Panel {
        let n = cols[0].len();
        let base = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let cal: Vec<NaiveDate> = (0..n).map(|k| base + chrono::Duration::days(k as i64)).collect();
        let series: Vec<Series> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| Series { symbol: format!("S{i}"), rows: cal.iter().copied().zip(c.iter().copied()).collect() })
            .collect();
        Panel::build(&cal, &series)
    }

    #[test]
    fn momentum_sign_and_availability() {
        let p = panel(&[vec![100.0, 101.0, 99.0, 102.0]]);
        assert!((momentum(&p, 3, 0, 3).unwrap() - 0.02).abs() < 1e-12);
        assert!(momentum(&p, 2, 0, 3).is_none(), "needs L+1 closes");
        assert!(momentum(&p, 3, 0, 1).unwrap() > 0.0);
    }

    #[test]
    fn vol_and_covariance_are_sample_and_annualised() {
        // returns +1 %, −1 % alternating ⇒ sd = 0.01 × sqrt(n/(n−1))
        let mut px = vec![100.0];
        for k in 0..60 {
            let last = *px.last().unwrap();
            px.push(last * if k % 2 == 0 { 1.01 } else { 0.99 });
        }
        let p = panel(&[px]);
        let r = trailing_returns(&p, 60, 0, 60).unwrap();
        assert_eq!(r.len(), 60);
        let daily_sd = ann_vol(&r) / TRADING_DAYS.sqrt();
        assert!((daily_sd - 0.01).abs() < 5e-4, "{daily_sd}");
        let two = covariance(&[r.clone(), r.iter().map(|x| -x).collect()]);
        assert!((two[0][1] + two[0][0]).abs() < 1e-12, "perfectly anti-correlated");
    }

    #[test]
    fn weights_scale_to_target_then_cap_then_gross() {
        // 3 instruments: two trending up with equal vol (uncorrelated noise), one trending down
        let n = 320;
        let mut up = vec![100.0];
        let mut up2 = vec![50.0];
        let mut down = vec![100.0];
        for k in 0..n {
            let z = if k % 2 == 0 { 0.005 } else { -0.005 };
            let z2 = if k % 4 < 2 { 0.005 } else { -0.005 }; // period 4 ⇒ orthogonal to `z`
            up.push(up.last().unwrap() * (1.0 + z + 0.0005));
            up2.push(up2.last().unwrap() * (1.0 + z2 + 0.0005));
            down.push(down.last().unwrap() * (1.0 - 0.001));
        }
        let p = panel(&[up, up2, down]);
        let sp = SignalParams { lookback: 252, vol_window: 60, target_vol: 0.08, max_weight: 0.25, max_gross: 1.0 };
        let w = target_weights(&p, n, &sp);
        assert_eq!(w[2], 0.0, "negative momentum ⇒ flat");
        assert!(w[0] > 0.0 && w[1] > 0.0);
        // equal vol ≈ 7.9 %, uncorrelated ⇒ λ/σ ≈ 0.71 each ⇒ the 25 % cap binds
        assert!((w[0] - 0.25).abs() < 1e-12 && (w[1] - 0.25).abs() < 1e-12, "{w:?}");
        // a low target is reached before the cap: 0.001/√2/σ ≈ 0.9 %
        let low = SignalParams { target_vol: 0.001, ..sp };
        let w2 = target_weights(&p, n, &low);
        assert!(w2[0] < 0.25 && w2[0] > 0.0 && (w2[0] - w2[1]).abs() < 1e-3, "{w2:?}");
        let vol = ann_vol(&trailing_returns(&p, n, 0, 60).unwrap());
        assert!((w2[0] - 0.001 / 2f64.sqrt() / vol).abs() < 1e-4, "{} vs {}", w2[0], 0.001 / 2f64.sqrt() / vol);
        // gross cap: allow 60 % per instrument, cap gross at 0.5 ⇒ 0.25 each
        let g = SignalParams { max_weight: 0.6, max_gross: 0.5, ..sp };
        let w3 = target_weights(&p, n, &g);
        assert!((w3.iter().sum::<f64>() - 0.5).abs() < 1e-12, "{w3:?}");
    }

    #[test]
    fn no_history_means_no_position() {
        let p = panel(&[vec![100.0; 100]]);
        assert!(target_weights(&p, 99, &SignalParams::default()).iter().all(|w| *w == 0.0));
    }
}
