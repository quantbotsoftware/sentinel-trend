//! Hurst exponent estimators for short return windows.
//!
//! Two estimators, both pure and allocation-light:
//!
//! * [`dfa_hurst`] — detrended fluctuation analysis of order 1 (Peng et al.
//!   1994). On the cumulative profile of the centred returns, boxes of size
//!   `s` are linearly detrended, `F(s)` is the RMS residual, and `H` is the
//!   OLS slope of `log F(s)` on `log s`. For stationary increments (fGn) the
//!   slope equals `H`. Robust to the short-range autocorrelation that
//!   bid–ask bounce induces in 1-minute returns; this is the estimator that
//!   decides in `2026-09-26_hurst_gate_preregistration.md`.
//! * [`rs_hurst`] — classical rescaled range with the Anis–Lloyd (1976)
//!   expected value under the null, so that `H = 0.5 + slope(log R/S_n −
//!   log E[R/S_n])`. The uncorrected R/S is biased upward on random walks
//!   for `n` in the hundreds (it reads ≈ 0.55–0.60 on pure noise); the
//!   corrected form removes that. Reported as a diagnostic only.
//!
//! [`HurstWindow`] is a fixed-capacity ring buffer of returns so a caller can
//! stream 1-minute returns and estimate on demand without re-allocating.

/// Minimum number of observations either estimator accepts.
pub const MIN_OBS: usize = 64;

/// Geometric grid of integer sizes in `[lo, hi]`, ~`count` points, deduplicated.
fn geometric_sizes(lo: usize, hi: usize, count: usize) -> Vec<usize> {
    if hi < lo || count == 0 {
        return Vec::new();
    }
    if hi == lo || count == 1 {
        return vec![lo];
    }
    let (a, b) = ((lo as f64).ln(), (hi as f64).ln());
    let mut out: Vec<usize> = (0..count)
        .map(|i| (a + (b - a) * i as f64 / (count - 1) as f64).exp().round() as usize)
        .map(|s| s.clamp(lo, hi))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// OLS slope of `y` on `x`; `None` when fewer than two distinct `x`.
fn slope(x: &[f64], y: &[f64]) -> Option<f64> {
    let n = x.len().min(y.len());
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mx = x[..n].iter().sum::<f64>() / nf;
    let my = y[..n].iter().sum::<f64>() / nf;
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for i in 0..n {
        let dx = x[i] - mx;
        sxx += dx * dx;
        sxy += dx * (y[i] - my);
    }
    (sxx > 0.0).then(|| sxy / sxx)
}

/// Sum of squared residuals of a linear fit on `seg` (x = 0..len).
fn linear_detrend_ssr(seg: &[f64]) -> f64 {
    let n = seg.len();
    if n < 2 {
        return 0.0;
    }
    let nf = n as f64;
    let mx = (nf - 1.0) / 2.0;
    let my = seg.iter().sum::<f64>() / nf;
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (i, v) in seg.iter().enumerate() {
        let dx = i as f64 - mx;
        sxx += dx * dx;
        sxy += dx * (v - my);
    }
    let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    seg.iter()
        .enumerate()
        .map(|(i, v)| {
            let r = v - (my + b * (i as f64 - mx));
            r * r
        })
        .sum()
}

/// DFA-1 Hurst exponent of `returns` (see module docs).
///
/// Box sizes: ~10 geometric values between `10` and `n / 4`. Boxes are taken
/// from both ends of the profile so no data is discarded. Returns `None` for
/// fewer than [`MIN_OBS`] observations or a degenerate (constant) series.
pub fn dfa_hurst(returns: &[f64]) -> Option<f64> {
    let n = returns.len();
    if n < MIN_OBS {
        return None;
    }
    let mean = returns.iter().sum::<f64>() / n as f64;
    let mut profile = Vec::with_capacity(n);
    let mut acc = 0.0;
    for r in returns {
        acc += r - mean;
        profile.push(acc);
    }
    let sizes = geometric_sizes(10, n / 4, 10);
    let mut lx = Vec::with_capacity(sizes.len());
    let mut ly = Vec::with_capacity(sizes.len());
    for &s in &sizes {
        let boxes = n / s;
        if boxes == 0 {
            continue;
        }
        let mut ssr = 0.0;
        for b in 0..boxes {
            ssr += linear_detrend_ssr(&profile[b * s..(b + 1) * s]);
            let end = n - b * s;
            ssr += linear_detrend_ssr(&profile[end - s..end]);
        }
        let f = (ssr / (2.0 * boxes as f64 * s as f64)).sqrt();
        if f > 0.0 && f.is_finite() {
            lx.push((s as f64).ln());
            ly.push(f.ln());
        }
    }
    slope(&lx, &ly).filter(|h| h.is_finite())
}

/// Lanczos approximation of `ln Γ(x)` for `x > 0` (|err| < 1e-10 on the
/// range used here).
fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // reflection
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = C[0];
    let t = x + G + 0.5;
    for (i, c) in C.iter().enumerate().skip(1) {
        a += c / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// Anis–Lloyd (1976) expected rescaled range of `n` iid Gaussian
/// observations, with the Peters (1994) `(n − ½)/n` small-sample factor.
pub fn anis_lloyd_expected_rs(n: usize) -> f64 {
    if n < 2 {
        return f64::NAN;
    }
    let nf = n as f64;
    let sum: f64 = (1..n).map(|i| ((nf - i as f64) / i as f64).sqrt()).sum();
    let front = if n <= 340 {
        (ln_gamma((nf - 1.0) / 2.0) - ln_gamma(nf / 2.0)).exp() / std::f64::consts::PI.sqrt()
    } else {
        1.0 / (nf * std::f64::consts::PI / 2.0).sqrt()
    };
    (nf - 0.5) / nf * front * sum
}

/// Mean rescaled range over the non-overlapping blocks of size `n`.
fn mean_rs(returns: &[f64], n: usize) -> Option<f64> {
    let blocks = returns.len() / n;
    if blocks == 0 || n < 8 {
        return None;
    }
    let mut acc = 0.0;
    let mut used = 0usize;
    for b in 0..blocks {
        let seg = &returns[b * n..(b + 1) * n];
        let m = seg.iter().sum::<f64>() / n as f64;
        let var = seg.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / n as f64;
        if var <= 0.0 {
            continue;
        }
        let (mut cum, mut hi, mut lo) = (0.0f64, f64::MIN, f64::MAX);
        for v in seg {
            cum += v - m;
            hi = hi.max(cum);
            lo = lo.min(cum);
        }
        acc += (hi - lo) / var.sqrt();
        used += 1;
    }
    (used > 0).then(|| acc / used as f64)
}

/// Rescaled-range Hurst exponent with (`corrected = true`) or without the
/// Anis–Lloyd correction. Block sizes: ~8 geometric values in `[16, n/2]`.
pub fn rs_hurst(returns: &[f64], corrected: bool) -> Option<f64> {
    let n = returns.len();
    if n < MIN_OBS {
        return None;
    }
    let sizes = geometric_sizes(16, n / 2, 8);
    let mut lx = Vec::with_capacity(sizes.len());
    let mut ly = Vec::with_capacity(sizes.len());
    let mut le = Vec::with_capacity(sizes.len());
    for &s in &sizes {
        if let Some(rs) = mean_rs(returns, s) {
            if rs > 0.0 {
                lx.push((s as f64).ln());
                ly.push(rs.ln());
                le.push(anis_lloyd_expected_rs(s).ln());
            }
        }
    }
    let raw = slope(&lx, &ly)?;
    if !corrected {
        return Some(raw);
    }
    let expected = slope(&lx, &le)?;
    Some(0.5 + raw - expected).filter(|h| h.is_finite())
}

/// Fixed-capacity ring buffer of returns for streaming Hurst estimation.
#[derive(Debug, Clone)]
pub struct HurstWindow {
    buf: Vec<f64>,
    head: usize,
    len: usize,
    scratch: Vec<f64>,
}

impl HurstWindow {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(MIN_OBS);
        Self { buf: vec![0.0; capacity], head: 0, len: 0, scratch: Vec::with_capacity(capacity) }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == self.buf.len()
    }

    /// Append one return (non-finite values are ignored).
    pub fn push(&mut self, r: f64) {
        if !r.is_finite() {
            return;
        }
        let cap = self.buf.len();
        let idx = (self.head + self.len) % cap;
        self.buf[idx] = r;
        if self.len < cap {
            self.len += 1;
        } else {
            self.head = (self.head + 1) % cap;
        }
    }

    /// Chronological copy of the window into the internal scratch buffer.
    fn ordered(&mut self) -> &[f64] {
        self.scratch.clear();
        let cap = self.buf.len();
        for i in 0..self.len {
            self.scratch.push(self.buf[(self.head + i) % cap]);
        }
        &self.scratch
    }

    /// DFA-1 estimate on the full window; `None` until the window is full.
    pub fn dfa(&mut self) -> Option<f64> {
        if !self.is_full() {
            return None;
        }
        let x = self.ordered();
        dfa_hurst(x)
    }

    /// Corrected R/S estimate on the full window; `None` until full.
    pub fn rs_corrected(&mut self) -> Option<f64> {
        if !self.is_full() {
            return None;
        }
        let x = self.ordered();
        rs_hurst(x, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(seed: &mut u64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    }

    #[test]
    fn rejects_short_or_constant_input() {
        assert_eq!(dfa_hurst(&[0.0; 10]), None);
        assert_eq!(dfa_hurst(&[0.0; 400]), None);
        assert_eq!(rs_hurst(&[0.0; 400], true), None);
    }

    #[test]
    fn persistent_series_reads_high_and_antipersistent_low() {
        // AR(1) with φ = +0.8 (persistent) vs φ = −0.8 (anti-persistent):
        // DFA slope must order them, with white noise in between.
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let gen = |phi: f64, s: &mut u64| {
            let mut x = 0.0;
            (0..800)
                .map(|_| {
                    x = phi * x + xorshift(s);
                    x
                })
                .collect::<Vec<f64>>()
        };
        let hi = dfa_hurst(&gen(0.8, &mut s)).unwrap();
        let mid = dfa_hurst(&gen(0.0, &mut s)).unwrap();
        let lo = dfa_hurst(&gen(-0.8, &mut s)).unwrap();
        assert!(hi > mid && mid > lo, "hi {hi:.3} mid {mid:.3} lo {lo:.3}");
        assert!(hi > 0.6 && lo < 0.4);
    }

    #[test]
    fn anis_lloyd_matches_known_values() {
        // n = 10 by hand: Σ√((10−i)/i) = 11.057; Γ(4.5)/(√π Γ(5)) = 0.2734;
        // × (9.5/10) ⇒ 2.872.
        let e10 = anis_lloyd_expected_rs(10);
        assert!((e10 - 2.872).abs() < 0.005, "{e10}");
        // Large n: E[R/S] / √(nπ/2) → 1 from below.
        for n in [500usize, 1000, 5000] {
            let ratio = anis_lloyd_expected_rs(n) / ((n as f64) * std::f64::consts::PI / 2.0).sqrt();
            assert!((0.9..1.0).contains(&ratio), "n {n} ratio {ratio}");
        }
        // Monotone in n.
        assert!(anis_lloyd_expected_rs(50) < anis_lloyd_expected_rs(100));
        // continuity across the 340 switch
        let a = anis_lloyd_expected_rs(340);
        let b = anis_lloyd_expected_rs(341);
        assert!((b - a).abs() < 0.05, "{a} vs {b}");
    }

    #[test]
    fn window_streams_and_estimates_when_full() {
        let mut w = HurstWindow::new(128);
        assert_eq!(w.capacity(), 128);
        let mut s = 42u64;
        for _ in 0..100 {
            w.push(xorshift(&mut s));
        }
        assert!(w.dfa().is_none(), "not full yet");
        for _ in 0..200 {
            w.push(xorshift(&mut s));
        }
        assert!(w.is_full());
        assert_eq!(w.len(), 128);
        let h = w.dfa().unwrap();
        assert!((0.2..0.8).contains(&h), "white noise H {h}");
        w.push(f64::NAN);
        assert_eq!(w.len(), 128, "NaN ignored");
    }

    #[test]
    fn ordered_view_is_chronological() {
        let mut w = HurstWindow::new(64);
        for i in 0..70 {
            w.push(i as f64);
        }
        let v = w.ordered().to_vec();
        assert_eq!(v.first(), Some(&6.0));
        assert_eq!(v.last(), Some(&69.0));
    }
}
