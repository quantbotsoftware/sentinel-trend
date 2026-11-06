//! Sleeve B — overnight carry on SPY/QQQ (Lou, Polk & Skouras 2019; IBS
//! filter after Pagonidis 2014).
//!
//! Buy on the closing bar, sell on the next session's opening print.
//! Exposure = min(max_exposure, σ_target_night / σ_on20) · w_ibs · w_trend,
//! where w_ibs = 1 for IBS ≤ ibs_full, ibs_mid_weight for IBS ≤ ibs_cut,
//! else 0 (no position), and w_trend = trend_weight_below when close <
//! SMA(trend_sma). No overnight position ⇒ nothing to sell at the open.

use serde::{Deserialize, Serialize};

use crate::sleeve::{DayCtx, FillAt, Sleeve, SleeveId, Target};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OvernightParams {
    pub ibs_full: f64,
    pub ibs_cut: f64,
    pub ibs_mid_weight: f64,
    pub trend_weight_below: f64,
    /// Overnight vol target as a fraction (0.006 = 0.6 %/night).
    pub sigma_target_night: f64,
    pub max_exposure: f64,
    /// Bars before the close at which the decision inputs (IBS, close) are
    /// frozen. 0 = decide on the closing bar (research). Live execution uses
    /// a market-on-close order, which Alpaca accepts only until 15:50 ET, so
    /// the decision is taken at 15:48 (offset 11) and filled at the close.
    pub decide_bars_before_close: usize,
}

impl Default for OvernightParams {
    fn default() -> Self {
        Self {
            ibs_full: 0.3,
            ibs_cut: 0.7,
            ibs_mid_weight: 0.5,
            trend_weight_below: 0.5,
            sigma_target_night: 0.006,
            max_exposure: 1.0,
            decide_bars_before_close: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OvernightCarry {
    pub params: OvernightParams,
    holding: bool,
}

impl OvernightCarry {
    pub fn new(params: OvernightParams) -> Self {
        Self { params, holding: false }
    }

    /// Exposure to carry into the night given today's IBS, close, SMA, σ_on.
    pub fn night_exposure(&self, ibs: f64, close: f64, sma: Option<f64>, sigma_on: Option<f64>) -> f64 {
        let p = &self.params;
        let w_ibs = if ibs <= p.ibs_full {
            1.0
        } else if ibs <= p.ibs_cut {
            p.ibs_mid_weight
        } else {
            0.0
        };
        if w_ibs == 0.0 {
            return 0.0;
        }
        let w_tr = match sma {
            Some(s) if close < s => p.trend_weight_below,
            _ => 1.0,
        };
        let base = match sigma_on {
            Some(sd) if sd > 0.0 => (p.sigma_target_night / sd).min(p.max_exposure),
            _ => p.max_exposure.min(0.5),
        };
        base * w_ibs * w_tr
    }
}

/// (IBS, close) as known `offset` bars before the scheduled close
/// (`session_bars`), i.e. from `bars[..session_bars − offset]`, clamped to
/// what is available.
pub fn decision_inputs(today: &crate::bars::DaySeries, session_bars: usize, offset: usize) -> (f64, f64) {
    let cut = session_bars.saturating_sub(offset).min(today.len());
    if offset == 0 || cut < 2 {
        return (today.ibs(), today.close());
    }
    let upto = &today.bars[..cut];
    let hi = upto.iter().map(|b| b.h).fold(f64::MIN, f64::max);
    let lo = upto.iter().map(|b| b.l).fold(f64::MAX, f64::min);
    let c = upto[upto.len() - 1].c;
    let ibs = if hi > lo { (c - lo) / (hi - lo) } else { 0.5 };
    (ibs, c)
}

impl Sleeve for OvernightCarry {
    fn id(&self) -> SleeveId {
        SleeveId::B
    }

    fn on_day_start(&mut self, _ctx: &DayCtx) {}

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, _vwap: f64) -> Option<Target> {
        let n = ctx.session_bars;
        if idx == 0 && self.holding {
            self.holding = false;
            return Some(Target {
                sleeve: SleeveId::B,
                variant: 0,
                exposure: 0.0,
                price_ref: ctx.today.open(),
                fill_at: FillAt::Open,
            });
        }
        if ctx.is_close(idx) {
            // Today's series is complete at the close bar: IBS/SMA/σ_on for
            // *today* = stats over history + today. `ctx.vol` is as of the
            // previous close, so SMA/σ_on lag one day (acceptable, causal).
            // Sizing uses the price known at decision time (= the close when
            // offset is 0), so a live 15:48 preview and the full-day replay
            // produce the identical share count.
            let (ibs, dec_close) = decision_inputs(ctx.today, n, self.params.decide_bars_before_close);
            let exp = self.night_exposure(ibs, dec_close, ctx.vol.sma, ctx.vol.sigma_on);
            if exp > 0.0 {
                self.holding = true;
                return Some(Target { sleeve: SleeveId::B, variant: 0, exposure: exp, price_ref: dec_close, fill_at: FillAt::Close });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_inputs_freeze_before_the_close() {
        use crate::bars::{Bar, DaySeries};
        let mut bars: Vec<Bar> = (0..390).map(|i| { let p = 100.0 + i as f64 * 0.01; Bar { o: p, h: p, l: p, c: p, v: 1.0 } }).collect();
        // last 11 bars crash to the low ⇒ IBS at the close = 0, but at 15:48 it was 1
        for b in bars.iter_mut().skip(379) { *b = Bar { o: 99.0, h: 99.0, l: 99.0, c: 99.0, v: 1.0 }; }
        let d = DaySeries { date: chrono::NaiveDate::default(), bars, real_bars: 390 };
        assert_eq!(decision_inputs(&d, 390, 0).0, d.ibs());
        let (ibs, c) = decision_inputs(&d, 390, 11);
        assert!((ibs - 1.0).abs() < 1e-9 && (c - 103.78).abs() < 1e-9, "{ibs} {c}");
        // live preview at 15:48 (379 bars available): same inputs as the full-day replay
        let partial = DaySeries { date: d.date, bars: d.bars[..379].to_vec(), real_bars: 379 };
        assert_eq!(decision_inputs(&partial, 390, 11), (ibs, c));
    }

    #[test]
    fn exposure_rules() {
        let s = OvernightCarry::new(OvernightParams::default());
        // full IBS, uptrend, σ_on 0.6 % → 1.0
        assert!((s.night_exposure(0.2, 100.0, Some(90.0), Some(0.006)) - 1.0).abs() < 1e-12);
        // mid IBS → 0.5
        assert!((s.night_exposure(0.5, 100.0, Some(90.0), Some(0.006)) - 0.5).abs() < 1e-12);
        // high IBS → 0
        assert_eq!(s.night_exposure(0.8, 100.0, Some(90.0), Some(0.006)), 0.0);
        // downtrend halves
        assert!((s.night_exposure(0.2, 100.0, Some(110.0), Some(0.006)) - 0.5).abs() < 1e-12);
        // calm night scales up but capped at max_exposure
        assert!((s.night_exposure(0.2, 100.0, None, Some(0.003)) - 1.0).abs() < 1e-12);
        // volatile night scales down
        assert!((s.night_exposure(0.2, 100.0, None, Some(0.012)) - 0.5).abs() < 1e-12);
        // no vol stats → conservative half
        assert!((s.night_exposure(0.2, 100.0, None, None) - 0.5).abs() < 1e-12);
    }
}
