//! Exact fractional Gaussian noise (fGn) for estimator calibration.
//!
//! fGn with Hurst exponent `H` is the stationary increment process of
//! fractional Brownian motion; its autocovariance is
//! `γ(k) = ½(|k+1|^{2H} − 2|k|^{2H} + |k−1|^{2H})`. [`FgnGenerator`] builds
//! the Cholesky factor of the `n × n` covariance once (O(n³/6)) and then
//! draws paths in O(n²) each, with a self-contained xorshift64* + Box–Muller
//! source so the crate takes no `rand` dependency and every table is
//! reproducible from a seed. Davies–Harte would be faster for large `n`;
//! for `n ≤ 500` Cholesky is exact and simpler.
//!
//! Used only by tests and research binaries; nothing in the live path.

/// Autocovariance of unit-variance fGn at lag `k`.
pub fn fgn_autocov(h: f64, k: usize) -> f64 {
    let k = k as f64;
    let p = 2.0 * h;
    0.5 * ((k + 1.0).powf(p) - 2.0 * k.powf(p) + (k - 1.0).abs().powf(p))
}

/// Deterministic standard-normal source (xorshift64* + Box–Muller).
#[derive(Debug, Clone)]
pub struct Gauss {
    state: u64,
    spare: Option<f64>,
}

impl Gauss {
    pub fn new(seed: u64) -> Self {
        Self { state: seed.max(1), spare: None }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in (0, 1).
    pub fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    pub fn standard_normal(&mut self) -> f64 {
        if let Some(z) = self.spare.take() {
            return z;
        }
        let u1 = self.uniform();
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let t = 2.0 * std::f64::consts::PI * u2;
        self.spare = Some(r * t.sin());
        r * t.cos()
    }
}

/// Cholesky-based fGn path generator for a fixed `(H, n)`.
#[derive(Debug, Clone)]
pub struct FgnGenerator {
    n: usize,
    h: f64,
    /// Row-major lower-triangular factor, `n × n`.
    chol: Vec<f64>,
}

impl FgnGenerator {
    /// Builds the factor. Returns `None` if `h ∉ (0, 1)`, `n == 0`, or the
    /// covariance is numerically not positive definite.
    pub fn new(h: f64, n: usize) -> Option<Self> {
        if !(0.0 < h && h < 1.0) || n == 0 {
            return None;
        }
        let gamma: Vec<f64> = (0..n).map(|k| fgn_autocov(h, k)).collect();
        let mut l = vec![0.0f64; n * n];
        for i in 0..n {
            for j in 0..=i {
                let mut s = gamma[i - j];
                for k in 0..j {
                    s -= l[i * n + k] * l[j * n + k];
                }
                if i == j {
                    if s <= 0.0 {
                        return None;
                    }
                    l[i * n + i] = s.sqrt();
                } else {
                    l[i * n + j] = s / l[j * n + j];
                }
            }
        }
        Some(Self { n, h, chol: l })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn hurst(&self) -> f64 {
        self.h
    }

    /// One fGn path of length `n` with unit marginal variance.
    pub fn sample(&self, rng: &mut Gauss) -> Vec<f64> {
        let z: Vec<f64> = (0..self.n).map(|_| rng.standard_normal()).collect();
        (0..self.n)
            .map(|i| {
                let row = &self.chol[i * self.n..i * self.n + i + 1];
                row.iter().zip(&z).map(|(a, b)| a * b).sum()
            })
            .collect()
    }
}

/// Mean and sample standard deviation of `x`.
pub fn mean_sd(x: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    if n < 2.0 {
        return (x.first().copied().unwrap_or(f64::NAN), f64::NAN);
    }
    let m = x.iter().sum::<f64>() / n;
    let v = x.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (n - 1.0);
    (m, v.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hurst::{dfa_hurst, rs_hurst};

    #[test]
    fn autocov_is_white_at_half_and_signed_by_h() {
        assert!(fgn_autocov(0.5, 0).abs() - 1.0 < 1e-12);
        assert!(fgn_autocov(0.5, 1).abs() < 1e-12);
        assert!(fgn_autocov(0.5, 7).abs() < 1e-12);
        assert!(fgn_autocov(0.7, 1) > 0.0, "persistent");
        assert!(fgn_autocov(0.3, 1) < 0.0, "anti-persistent");
        // lag-1 autocorrelation of fGn is 2^{2H−1} − 1
        let h = 0.65;
        assert!((fgn_autocov(h, 1) - (2f64.powf(2.0 * h - 1.0) - 1.0)).abs() < 1e-12);
    }

    #[test]
    fn generator_reproduces_lag1_autocorrelation() {
        let h = 0.7;
        let g = FgnGenerator::new(h, 256).unwrap();
        let mut rng = Gauss::new(7);
        let (mut num, mut den) = (0.0, 0.0);
        for _ in 0..200 {
            let x = g.sample(&mut rng);
            for i in 1..x.len() {
                num += x[i] * x[i - 1];
            }
            den += x.iter().map(|v| v * v).sum::<f64>();
        }
        let rho = num / den;
        let target = 2f64.powf(2.0 * h - 1.0) - 1.0;
        assert!((rho - target).abs() < 0.03, "ρ₁ {rho:.3} vs {target:.3}");
    }

    #[test]
    fn rejects_bad_parameters() {
        assert!(FgnGenerator::new(0.0, 10).is_none());
        assert!(FgnGenerator::new(1.0, 10).is_none());
        assert!(FgnGenerator::new(0.5, 0).is_none());
    }

    /// Light bias check that always runs: DFA on white noise at N = 390.
    #[test]
    fn dfa_is_unbiased_on_white_noise_at_390() {
        let g = FgnGenerator::new(0.5, 390).unwrap();
        let mut rng = Gauss::new(20_260_926);
        let est: Vec<f64> = (0..120).filter_map(|_| dfa_hurst(&g.sample(&mut rng))).collect();
        let (m, sd) = mean_sd(&est);
        assert!(est.len() >= 118);
        assert!((m - 0.5).abs() < 0.04, "DFA mean {m:.3} sd {sd:.3}");
    }

    /// Full calibration table for the pre-registration (run with
    /// `cargo test -p titan-indicators --release -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn bias_table_dfa_vs_rs() {
        const PATHS: usize = 500;
        println!("| H | N | DFA mean | DFA sd | R/S raw mean | R/S raw sd | R/S corr mean | R/S corr sd |");
        println!("|---|---|---|---|---|---|---|---|");
        for &h in &[0.35, 0.50, 0.65] {
            for &n in &[100usize, 250, 390] {
                let g = FgnGenerator::new(h, n).unwrap();
                let mut rng = Gauss::new(1_000 + (h * 100.0) as u64 * 7 + n as u64);
                let (mut d, mut r, mut c) = (Vec::new(), Vec::new(), Vec::new());
                for _ in 0..PATHS {
                    let x = g.sample(&mut rng);
                    if let Some(v) = dfa_hurst(&x) {
                        d.push(v);
                    }
                    if let Some(v) = rs_hurst(&x, false) {
                        r.push(v);
                    }
                    if let Some(v) = rs_hurst(&x, true) {
                        c.push(v);
                    }
                }
                let (dm, ds) = mean_sd(&d);
                let (rm, rs) = mean_sd(&r);
                let (cm, cs) = mean_sd(&c);
                println!("| {h:.2} | {n} | {dm:.3} | {ds:.3} | {rm:.3} | {rs:.3} | {cm:.3} | {cs:.3} |");
            }
        }
    }
}
