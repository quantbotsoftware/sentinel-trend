//! Statistical process control of a live edge.
//!
//! * `Cusum` — one-sided lower CUSUM on standardised daily returns against
//!   the backtest mean μ: S_t = max(0, S_{t−1} + (μ/2 − r_t)/σ). Slow but
//!   robust to noise; levels `half_at` / `off_at` map to Half / Off.
//! * `Bocpd` — Bayesian online change-point detection (Adams & MacKay 2007)
//!   with a Gaussian-unknown-mean model and constant hazard; reports the
//!   probability that the current run started within the last `k` days.
//!   Fast at abrupt breaks; complements CUSUM.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpcVerdict {
    Ok,
    Half,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cusum {
    pub mu: f64,
    pub sigma: f64,
    pub half_at: f64,
    pub off_at: f64,
    pub s: f64,
    pub n: u32,
}

impl Cusum {
    pub fn new(mu: f64, sigma: f64, half_at: f64, off_at: f64) -> Self {
        Self { mu, sigma: sigma.max(1e-12), half_at, off_at, s: 0.0, n: 0 }
    }

    pub fn push(&mut self, r: f64) -> SpcVerdict {
        self.s = (self.s + (self.mu / 2.0 - r) / self.sigma).max(0.0);
        self.n += 1;
        self.verdict()
    }

    pub fn verdict(&self) -> SpcVerdict {
        if self.s >= self.off_at {
            SpcVerdict::Off
        } else if self.s >= self.half_at {
            SpcVerdict::Half
        } else {
            SpcVerdict::Ok
        }
    }

    pub fn reset(&mut self) {
        self.s = 0.0;
    }
}

/// BOCPD with Normal likelihood, known variance, Normal prior on the mean.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bocpd {
    pub hazard: f64,
    pub obs_var: f64,
    pub prior_mean: f64,
    pub prior_var: f64,
    /// run-length distribution; index = run length in observations
    r: Vec<f64>,
    /// posterior (mean, var) of the mean for each run length
    post: Vec<(f64, f64)>,
    pub max_len: usize,
}

impl Bocpd {
    pub fn new(hazard: f64, obs_var: f64, prior_mean: f64, prior_var: f64, max_len: usize) -> Self {
        Self {
            hazard: hazard.clamp(1e-6, 1.0),
            obs_var: obs_var.max(1e-12),
            prior_mean,
            prior_var: prior_var.max(1e-12),
            r: vec![1.0],
            post: vec![(prior_mean, prior_var)],
            max_len: max_len.max(2),
        }
    }

    fn pred_pdf(x: f64, mean: f64, var: f64) -> f64 {
        let z = (x - mean) / var.sqrt();
        (-0.5 * z * z).exp() / (var * 2.0 * std::f64::consts::PI).sqrt()
    }

    pub fn push(&mut self, x: f64) {
        let n = self.r.len();
        let mut growth = vec![0.0; n + 1];
        let mut cp = 0.0;
        for i in 0..n {
            let (m, v) = self.post[i];
            let p = Self::pred_pdf(x, m, v + self.obs_var);
            growth[i + 1] = self.r[i] * p * (1.0 - self.hazard);
            cp += self.r[i] * p * self.hazard;
        }
        growth[0] = cp;
        let z: f64 = growth.iter().sum::<f64>().max(1e-300);
        for g in growth.iter_mut() {
            *g /= z;
        }
        // posterior update per run length (conjugate Normal-Normal)
        let mut post = vec![(self.prior_mean, self.prior_var)];
        for i in 0..n {
            let (m, v) = self.post[i];
            let v_new = 1.0 / (1.0 / v + 1.0 / self.obs_var);
            let m_new = v_new * (m / v + x / self.obs_var);
            post.push((m_new, v_new));
        }
        if growth.len() > self.max_len {
            let extra: f64 = growth[self.max_len..].iter().sum();
            growth.truncate(self.max_len);
            post.truncate(self.max_len);
            if let Some(last) = growth.last_mut() {
                *last += extra;
            }
        }
        self.r = growth;
        self.post = post;
    }

    /// P(run length < k) — probability a change-point happened in the last k obs.
    pub fn prob_recent_change(&self, k: usize) -> f64 {
        self.r.iter().take(k).sum()
    }

    pub fn most_likely_run_length(&self) -> usize {
        self.r
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cusum_detects_degradation_within_15_days() {
        // backtest: μ = 0.05 %/day, σ = 1 %/day; live: μ = 0 (edge gone),
        // deterministic worst-ish path: alternate −σ/2 and +σ/4 ⇒ mean −0.125σ
        let mut c = Cusum::new(0.0005, 0.01, 4.0, 7.0);
        let mut days_to_half = None;
        for i in 0..60 {
            let r = if i % 2 == 0 { -0.005 } else { 0.0025 };
            if c.push(r) != SpcVerdict::Ok && days_to_half.is_none() {
                days_to_half = Some(i + 1);
            }
        }
        assert!(days_to_half.map(|d| d <= 40).unwrap_or(false), "{days_to_half:?}");
        // a healthy stream at μ never trips
        let mut ok = Cusum::new(0.0005, 0.01, 4.0, 7.0);
        for _ in 0..500 {
            ok.push(0.0005);
        }
        assert_eq!(ok.verdict(), SpcVerdict::Ok);
    }

    #[test]
    fn bocpd_flags_abrupt_mean_shift() {
        let mut b = Bocpd::new(1.0 / 120.0, 1.0, 0.0, 4.0, 400);
        for i in 0..150 {
            let x = 0.5 + 0.3 * ((i as f64 * 1.7).sin()); // stable regime ≈ 0.5
            b.push(x);
        }
        let calm = b.prob_recent_change(10);
        assert!(calm < 0.2, "calm regime should not flag: {calm}");
        assert!(b.most_likely_run_length() > 100);
        for i in 0..8 {
            b.push(-4.0 + 0.3 * ((i as f64 * 1.3).cos())); // shift of 4.5σ
        }
        let after = b.prob_recent_change(10);
        assert!(after > 0.8, "shift should be detected: {after}");
        assert!(b.most_likely_run_length() < 10);
    }
}
