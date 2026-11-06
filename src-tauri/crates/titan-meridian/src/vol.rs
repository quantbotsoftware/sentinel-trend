//! Daily volatility / trend statistics consumed by sizing and Sleeve B.

use crate::bars::DaySeries;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolStats {
    /// Sample stdev of close-to-close daily returns over `cc_window`.
    pub sigma_cc: Option<f64>,
    /// Sample stdev of overnight returns (open_t / close_{t−1} − 1) over `on_window`.
    pub sigma_on: Option<f64>,
    /// Average true range over `atr_window` days.
    pub atr: Option<f64>,
    /// Simple moving average of closes over `sma_window`.
    pub sma: Option<f64>,
    pub ibs_last: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct VolWindows {
    pub cc: usize,
    pub on: usize,
    pub atr: usize,
    pub sma: usize,
}

impl Default for VolWindows {
    fn default() -> Self {
        Self { cc: 14, on: 20, atr: 14, sma: 200 }
    }
}

pub fn stdev(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let n = xs.len() as f64;
    let mu = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / (n - 1.0);
    Some(var.sqrt())
}

pub fn mean(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        None
    } else {
        Some(xs.iter().sum::<f64>() / xs.len() as f64)
    }
}

/// Compute stats from `history` (chronological, most recent last), as of
/// the close of the last element. Windows that lack data yield `None`.
pub fn compute(history: &[DaySeries], w: VolWindows) -> VolStats {
    let closes: Vec<f64> = history.iter().map(|d| d.close()).collect();
    let cc: Vec<f64> = closes.windows(2).map(|p| p[1] / p[0] - 1.0).collect();
    let on: Vec<f64> = history.windows(2).map(|p| p[1].open() / p[0].close() - 1.0).collect();
    let tr: Vec<f64> = history.windows(2).map(|p| p[1].true_range(p[0].close())).collect();

    let tail = |v: &[f64], k: usize| -> Option<Vec<f64>> {
        (v.len() >= k && k > 0).then(|| v[v.len() - k..].to_vec())
    };
    VolStats {
        sigma_cc: tail(&cc, w.cc).and_then(|t| stdev(&t)),
        sigma_on: tail(&on, w.on).and_then(|t| stdev(&t)),
        atr: tail(&tr, w.atr).and_then(|t| mean(&t)),
        sma: tail(&closes, w.sma).and_then(|t| mean(&t)),
        ibs_last: history.last().map(|d| d.ibs()).unwrap_or(0.5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bars::Bar;
    use chrono::NaiveDate;

    fn day(i: u32, o: f64, h: f64, l: f64, c: f64) -> DaySeries {
        let date = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Days::new(i as u64);
        let bars = vec![Bar { o, h, l, c, v: 1.0 }; 390];
        DaySeries { date, bars, real_bars: 390 }
    }

    #[test]
    fn stdev_and_mean_basics() {
        assert!((stdev(&[1.0, 2.0, 3.0, 4.0]).unwrap() - 1.2909944).abs() < 1e-6);
        assert_eq!(stdev(&[1.0]), None);
        assert_eq!(mean(&[]), None);
    }

    #[test]
    fn compute_windows() {
        let h = vec![
            day(0, 100.0, 101.0, 99.0, 100.0),
            day(1, 101.0, 103.0, 100.0, 102.0), // on +1%, cc +2%, tr 3
            day(2, 101.0, 102.0, 99.0, 101.0),  // on −0.98%, cc −0.98%, tr 3
            day(3, 102.0, 104.0, 101.0, 103.0), // on +0.99%, cc +1.98%, tr 3
        ];
        let s = compute(&h, VolWindows { cc: 3, on: 3, atr: 3, sma: 4 });
        assert!(s.sigma_cc.unwrap() > 0.01 && s.sigma_cc.unwrap() < 0.02);
        assert!(s.sigma_on.unwrap() > 0.01);
        assert!((s.atr.unwrap() - 3.0).abs() < 1e-9);
        assert!((s.sma.unwrap() - 101.5).abs() < 1e-9);
        // IBS of last day: (103−101)/(104−101) = 2/3
        assert!((s.ibs_last - 2.0 / 3.0).abs() < 1e-9);
        let short = compute(&h[..2], VolWindows::default());
        assert_eq!(short.sigma_cc, None);
        assert_eq!(short.sma, None);
    }
}
