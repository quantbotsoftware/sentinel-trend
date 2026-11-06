//! Noise Envelope σ(t, m) — time-of-day dependent boundary of "normal" moves.
//!
//! σ(m) = k · mean over the previous N sessions of |C(d, m) / O(d) − 1|.
//! UB(m) = max(O_t, C_{t−1}) · (1 + σ(m)),  LB(m) = min(O_t, C_{t−1}) · (1 − σ(m)).
//! (Zarattini, Barbon & Aziz 2024.) Pure, fixture-testable.

use crate::bars::DaySeries;

#[derive(Debug, Clone, PartialEq)]
pub struct NoiseEnvelope {
    /// σ per minute index (already multiplied by `k`).
    pub sigma: Vec<f64>,
}

impl NoiseEnvelope {
    /// Fit on `history` (most recent last). Uses the last `lookback` *full*
    /// sessions (390 bars — plan §3.8.9: σ(t, m) is estimated on whole days
    /// only; a 13:00 close has no afternoon to learn from and A does not
    /// trade it). Returns `None` if fewer than `lookback` qualify.
    pub fn fit(history: &[DaySeries], lookback: usize, k: f64) -> Option<Self> {
        let used: Vec<&DaySeries> = history
            .iter()
            .rev()
            .filter(|d| d.len() >= crate::session::FULL_SESSION_BARS && d.open() > 0.0)
            .take(lookback)
            .collect();
        if used.len() < lookback || lookback == 0 {
            return None;
        }
        let len = used.iter().map(|d| d.len()).min().unwrap_or(0);
        let mut sigma = vec![0.0; len];
        for d in &used {
            let o = d.open();
            for (m, s) in sigma.iter_mut().enumerate() {
                *s += (d.bars[m].c / o - 1.0).abs();
            }
        }
        let n = used.len() as f64;
        for s in sigma.iter_mut() {
            *s = k * *s / n;
        }
        Some(Self { sigma })
    }

    pub fn sigma_at(&self, m: usize) -> f64 {
        self.sigma.get(m).copied().unwrap_or_else(|| self.sigma.last().copied().unwrap_or(0.0))
    }

    /// Upper / lower bands at minute `m` for today's open and yesterday's close.
    pub fn bands(&self, m: usize, open: f64, prev_close: f64) -> (f64, f64) {
        self.bands_scaled(m, open, prev_close, 1.0)
    }

    /// Bands with σ multiplied by `mult` — one fitted envelope serves an
    /// ensemble of band multipliers (k ∈ {1.0, 1.25, 1.5}, plan §4.4 X0).
    pub fn bands_scaled(&self, m: usize, open: f64, prev_close: f64, mult: f64) -> (f64, f64) {
        let s = self.sigma_at(m) * mult;
        (open.max(prev_close) * (1.0 + s), open.min(prev_close) * (1.0 - s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bars::Bar;
    use chrono::NaiveDate;

    /// Day whose close path is open·(1 + a·m/389): |move| grows linearly.
    fn day(n: u32, open: f64, a: f64) -> DaySeries {
        let date = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Days::new(n as u64);
        let bars = (0..390)
            .map(|m| {
                let c = open * (1.0 + a * m as f64 / 389.0);
                Bar { o: c, h: c, l: c, c, v: 1.0 }
            })
            .collect();
        DaySeries { date, bars, real_bars: 390 }
    }

    #[test]
    fn fit_matches_reference_and_is_monotone() {
        // two days: +1% and −3% linear drift → mean |move| at m = 0.02·m/389
        let hist = vec![day(0, 100.0, 0.01), day(1, 100.0, -0.03)];
        let env = NoiseEnvelope::fit(&hist, 2, 1.0).unwrap();
        assert_eq!(env.sigma.len(), 390);
        assert!(env.sigma[0].abs() < 1e-12);
        assert!((env.sigma[389] - 0.02).abs() < 1e-9);
        assert!((env.sigma_at(194) - 0.02 * 194.0 / 389.0).abs() < 1e-9);
        assert!(env.sigma.windows(2).all(|w| w[1] >= w[0]));
        let k = NoiseEnvelope::fit(&hist, 2, 1.5).unwrap();
        assert!((k.sigma[389] - 0.03).abs() < 1e-9);
    }

    #[test]
    fn bands_use_max_min_of_open_prev_close() {
        let env = NoiseEnvelope { sigma: vec![0.01; 390] };
        let (ub, lb) = env.bands(10, 102.0, 100.0);
        assert!((ub - 102.0 * 1.01).abs() < 1e-9);
        assert!((lb - 100.0 * 0.99).abs() < 1e-9);
        // out-of-range index falls back to last σ
        assert!((env.sigma_at(1000) - 0.01).abs() < 1e-12);
    }

    #[test]
    fn insufficient_history_is_none() {
        let hist = vec![day(0, 100.0, 0.01)];
        assert!(NoiseEnvelope::fit(&hist, 2, 1.0).is_none());
        assert!(NoiseEnvelope::fit(&hist, 0, 1.0).is_none());
    }

    #[test]
    fn half_days_are_skipped_and_scaled_bands_multiply_sigma() {
        let mut half = day(1, 100.0, 0.05);
        half.bars.truncate(210);
        // the 210-bar day is neither counted nor allowed to shrink the grid
        let hist = vec![day(0, 100.0, 0.01), half, day(2, 100.0, -0.03)];
        let env = NoiseEnvelope::fit(&hist, 2, 1.0).unwrap();
        assert_eq!(env.sigma.len(), 390);
        assert!((env.sigma[389] - 0.02).abs() < 1e-9, "mean of |+1 %| and |−3 %|, the half-day's 5 % ignored");
        assert!(NoiseEnvelope::fit(&hist, 3, 1.0).is_none(), "only two full sessions available");
        let (ub, lb) = env.bands_scaled(389, 100.0, 100.0, 1.5);
        assert!((ub - 103.0).abs() < 1e-9 && (lb - 97.0).abs() < 1e-9);
    }
}
