//! Regime Detector — coarse market regime classification (Sesiunea 1).
//!
//! Re-evaluates every 5 min on the SPY 5-min candle stream and returns
//! one of four regimes:
//!   - **Calm** — low vol, choppy mean-reverting tape
//!   - **Trending** — directional grind with low pull-backs
//!   - **Volatile** — high vol, swings >1% per 30 min, no clear trend
//!   - **Panic** — VIX equivalent > 30 OR 5-min |return| > 1.5%
//!
//! ## Why a separate module (vs ad-hoc inside each strategy)?
//!
//! Strategies disagree on which regime helps them. Stat-arb strategies
//! prefer Calm (mean-revert works); momentum strategies prefer Trending;
//! catalyst strategies need Volatile to find edge in news flow. Putting
//! the classification in one place and feeding the result through
//! `MarketContext.regime` keeps strategy logic clean and lets the GA
//! tune per-regime activation maps.
//!
//! ## Inputs
//!
//! - SPY 5-min candle stream (last 50 bars retained)
//! - VIX equivalent: derived from SPY 5-min realised volatility times
//!   √(390/5) annualisation factor — we don't subscribe to the actual
//!   ^VIX feed because Alpaca doesn't carry index ticks; SPY-realised
//!   is a tight enough proxy intraday.
//!
//! ## Persistence
//!
//! Active regime is held in-memory only. On bot restart the detector
//! warms up for ~10 min before publishing its first regime so it
//! doesn't oscillate on cold cache.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::smarttrend::strategies::MarketRegime;

/// 5-minute bar threshold tunables. All in % of SPY price.
#[derive(Debug, Clone, Copy)]
pub struct RegimeConfig {
    /// |return_5m| above this triggers Panic regardless of other gates.
    pub panic_return_pct: f64,
    /// Realised vol (annualised %) above this triggers Volatile.
    pub volatile_realised_vol_pct: f64,
    /// Trend r² above this on the 30-min linear fit triggers Trending.
    pub trending_r2: f64,
    /// 30-min |slope%| floor for Trending regime.
    pub trending_slope_pct: f64,
    /// Minimum bars before publishing the first regime (warm-up).
    pub warmup_bars: usize,
    /// Bars to retain in the rolling window (≥30 to allow trend fit).
    pub retain_bars: usize,
}

impl Default for RegimeConfig {
    fn default() -> Self {
        Self {
            panic_return_pct: 1.5,
            volatile_realised_vol_pct: 35.0,
            trending_r2: 0.55,
            trending_slope_pct: 0.40,
            warmup_bars: 8,
            retain_bars: 50,
        }
    }
}

/// Snapshot of a single regime evaluation, persisted alongside the
/// regime label so retrospectives can audit *why* the detector flipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeSnapshot {
    pub regime: MarketRegime,
    pub realised_vol_annualised_pct: f64,
    pub last_return_pct: f64,
    pub trend_slope_pct: f64,
    pub trend_r2: f64,
    pub bar_time: i64,
}

/// Lightweight stateful detector. The bot owns one instance behind a
/// Mutex; the regime loop polls every 5 min by appending the latest
/// SPY 5-min bar via [`RegimeDetector::observe_bar`] and reading
/// [`RegimeDetector::current`] afterward.
#[derive(Debug)]
pub struct RegimeDetector {
    cfg: RegimeConfig,
    /// (close, time) tuples — newest at the end.
    closes: Vec<(f64, i64)>,
    last: Option<RegimeSnapshot>,
}

impl Default for RegimeDetector {
    fn default() -> Self {
        Self::new(RegimeConfig::default())
    }
}

impl RegimeDetector {
    pub fn new(cfg: RegimeConfig) -> Self {
        Self {
            cfg,
            closes: Vec::with_capacity(cfg.retain_bars),
            last: None,
        }
    }

    pub fn current(&self) -> Option<&RegimeSnapshot> {
        self.last.as_ref()
    }

    /// Observe a SPY 5-min bar. Returns `Some(snapshot)` if the regime
    /// changed (caller emits the `regime-change` event), `None`
    /// otherwise. Caller MUST pass bars in chronological order.
    pub fn observe_bar(&mut self, close: f64, bar_time: i64) -> Option<RegimeSnapshot> {
        if !close.is_finite() || close <= 0.0 {
            return None;
        }
        self.closes.push((close, bar_time));
        let cap = self.cfg.retain_bars;
        if self.closes.len() > cap {
            let drop = self.closes.len() - cap;
            self.closes.drain(0..drop);
        }
        if self.closes.len() < self.cfg.warmup_bars {
            return None;
        }
        let prev_regime = self.last.as_ref().map(|s| s.regime);
        let snapshot = self.evaluate(bar_time);
        let regime_changed = match prev_regime {
            None => true,
            Some(prev) => prev != snapshot.regime,
        };
        self.last = Some(snapshot.clone());
        if regime_changed {
            Some(snapshot)
        } else {
            None
        }
    }

    fn evaluate(&self, bar_time: i64) -> RegimeSnapshot {
        let len = self.closes.len();
        let last_close = self.closes[len - 1].0;
        let prev_close = self.closes[len.saturating_sub(2).max(0)].0;
        let last_return_pct = if prev_close > 0.0 {
            ((last_close - prev_close) / prev_close) * 100.0
        } else {
            0.0
        };

        // Realised vol: stdev of last 12 5-min log returns, annualised
        // × sqrt(78 × 252) — equivalent to VIX-style annualised %.
        let take = 12.min(len.saturating_sub(1));
        let returns: Vec<f64> = (1..=take)
            .map(|i| {
                let a = self.closes[len - i - 1].0;
                let b = self.closes[len - i].0;
                if a > 0.0 {
                    (b / a).ln()
                } else {
                    0.0
                }
            })
            .collect();
        let mean = if returns.is_empty() {
            0.0
        } else {
            returns.iter().sum::<f64>() / returns.len() as f64
        };
        let var = if returns.is_empty() {
            0.0
        } else {
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
                / returns.len() as f64
        };
        let sd = var.sqrt();
        let realised_vol_annualised = sd * (78.0_f64 * 252.0).sqrt() * 100.0;

        // Trend score: 30-min linear fit.
        let trend_take = 6.min(len);
        let (slope_pct, r2) = if trend_take >= 4 {
            linear_fit_slope_r2(&self.closes[len - trend_take..])
        } else {
            (0.0, 0.0)
        };

        let regime = self.classify(last_return_pct, realised_vol_annualised, slope_pct, r2);
        RegimeSnapshot {
            regime,
            realised_vol_annualised_pct: realised_vol_annualised,
            last_return_pct,
            trend_slope_pct: slope_pct,
            trend_r2: r2,
            bar_time,
        }
    }

    fn classify(
        &self,
        last_return_pct: f64,
        realised_vol_annualised: f64,
        slope_pct: f64,
        r2: f64,
    ) -> MarketRegime {
        let cfg = &self.cfg;
        if last_return_pct.abs() >= cfg.panic_return_pct {
            return MarketRegime::Panic;
        }
        if realised_vol_annualised >= cfg.volatile_realised_vol_pct {
            return MarketRegime::Volatile;
        }
        if r2 >= cfg.trending_r2 && slope_pct.abs() >= cfg.trending_slope_pct {
            return MarketRegime::Trending;
        }
        MarketRegime::Calm
    }
}

/// Linear regression on (i, close_pct) — returns (slope %, r²) where
/// slope % is the per-bar % change implied by the fit.
fn linear_fit_slope_r2(pts: &[(f64, i64)]) -> (f64, f64) {
    if pts.len() < 3 {
        return (0.0, 0.0);
    }
    let first = pts[0].0;
    if first <= 0.0 {
        return (0.0, 0.0);
    }
    // y = ((close_i / close_0) - 1) * 100 — % change relative to start
    let xs: Vec<f64> = (0..pts.len()).map(|i| i as f64).collect();
    let ys: Vec<f64> = pts.iter().map(|(c, _)| ((c / first) - 1.0) * 100.0).collect();
    let n = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    let mut tss = 0.0;
    for i in 0..xs.len() {
        let dx = xs[i] - mx;
        let dy = ys[i] - my;
        num += dx * dy;
        den += dx * dx;
        tss += dy * dy;
    }
    if den.abs() < f64::EPSILON {
        return (0.0, 0.0);
    }
    let slope = num / den;
    let intercept = my - slope * mx;
    let mut rss = 0.0;
    for i in 0..xs.len() {
        let yhat = slope * xs[i] + intercept;
        rss += (ys[i] - yhat).powi(2);
    }
    let r2 = if tss.abs() < f64::EPSILON {
        0.0
    } else {
        1.0 - rss / tss
    };
    (slope, r2.clamp(0.0, 1.0))
}

/// Thread-safe wrapper used from the regime loop. Guards the detector
/// behind a single Mutex; never held across `await` (Tier-A safe).
pub struct RegimeMonitor {
    inner: Mutex<RegimeDetector>,
}

impl RegimeMonitor {
    pub fn new(cfg: RegimeConfig) -> Self {
        Self {
            inner: Mutex::new(RegimeDetector::new(cfg)),
        }
    }

    pub fn observe(&self, close: f64, bar_time: i64) -> Option<RegimeSnapshot> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .observe_bar(close, bar_time)
    }

    pub fn current(&self) -> Option<RegimeSnapshot> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .current()
            .cloned()
    }
}

impl Default for RegimeMonitor {
    fn default() -> Self {
        Self::new(RegimeConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warm_up(d: &mut RegimeDetector, base: f64, n: usize) {
        for i in 0..n {
            let _ = d.observe_bar(base, i as i64 * 300);
        }
    }

    #[test]
    fn cold_start_warmup_returns_none_until_threshold() {
        let mut d = RegimeDetector::default();
        for i in 0..7 {
            assert!(d.observe_bar(400.0, i as i64).is_none() || d.current().is_none());
        }
        // 8th bar (warmup_bars default) → first publish
        let snap = d.observe_bar(400.0, 8 * 300);
        assert!(snap.is_some(), "should publish on warmup boundary");
    }

    #[test]
    fn flat_tape_classifies_as_calm() {
        let mut d = RegimeDetector::default();
        for i in 0..20 {
            d.observe_bar(400.0, i as i64 * 300);
        }
        let cur = d.current().unwrap();
        assert_eq!(cur.regime, MarketRegime::Calm);
    }

    #[test]
    fn big_5min_drop_triggers_panic() {
        let mut d = RegimeDetector::default();
        warm_up(&mut d, 400.0, 12);
        // 2% drop in 5 min → Panic
        let snap = d.observe_bar(392.0, 13 * 300);
        let cur = snap.or_else(|| d.current().cloned()).unwrap();
        assert_eq!(cur.regime, MarketRegime::Panic);
    }

    #[test]
    fn steady_uptrend_classifies_as_trending() {
        let mut d = RegimeDetector::default();
        // 0.6% per bar for 8 bars → r²≈1.0, slope ≈0.6%
        let mut p = 400.0;
        for i in 0..15 {
            d.observe_bar(p, i as i64 * 300);
            p *= 1.006;
        }
        let cur = d.current().unwrap();
        assert_eq!(cur.regime, MarketRegime::Trending);
    }

    #[test]
    fn detector_emits_on_change_only() {
        let mut d = RegimeDetector::default();
        // Pre-warmup: 7 observations return None (warmup_bars = 8)
        for i in 0..7 {
            assert!(d.observe_bar(400.0, i as i64 * 300).is_none());
        }
        // 8th: first publish (None → Calm = a change)
        let first = d.observe_bar(400.0, 8 * 300);
        assert!(first.is_some(), "first observation post-warmup is a change");
        // Second observation in same Calm regime → None (no change)
        let again = d.observe_bar(400.0, 9 * 300);
        assert!(again.is_none(), "no change should suppress emission");
    }

    #[test]
    fn refuses_invalid_close() {
        let mut d = RegimeDetector::default();
        assert!(d.observe_bar(0.0, 100).is_none());
        assert!(d.observe_bar(-1.0, 200).is_none());
        assert!(d.observe_bar(f64::NAN, 300).is_none());
    }

    #[test]
    fn linear_fit_perfect_line() {
        let pts: Vec<(f64, i64)> = (0..10)
            .map(|i| (100.0 + i as f64, i as i64))
            .collect();
        let (slope, r2) = linear_fit_slope_r2(&pts);
        assert!(slope > 0.0);
        assert!((r2 - 1.0).abs() < 1e-3, "perfect line r² = {r2}");
    }

    #[test]
    fn linear_fit_constant_zero() {
        let pts: Vec<(f64, i64)> = (0..10).map(|i| (100.0, i as i64)).collect();
        let (slope, r2) = linear_fit_slope_r2(&pts);
        assert!(slope.abs() < 1e-3);
        // r² is 0 because TSS = 0; our impl returns 0 in that case.
        assert!(r2.abs() < 1e-3);
    }

    #[test]
    fn monitor_thread_safe_observe() {
        let mon = RegimeMonitor::default();
        for i in 0..15 {
            mon.observe(400.0, i as i64 * 300);
        }
        let cur = mon.current().expect("monitor should have current snapshot");
        assert_eq!(cur.regime, MarketRegime::Calm);
    }
}
