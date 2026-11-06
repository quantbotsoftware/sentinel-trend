//! Sleeve A — intraday momentum on SPY/QQQ (Zarattini, Barbon & Aziz 2024).
//!
//! At each half-hour decision bar: close above UB ⇒ long, below LB ⇒ short.
//! Long trails at max(UB, VWAP), short at min(LB, VWAP); a stop-out may
//! flip into the opposite band on the same bar. Flat on the last bar.
//! Size = min(max_leverage, σ_target / σ_cc14) of equity, shares fixed at
//! the session open.
//!
//! Ensemble (plan §4.4 X0, decided 2026-09-03 after PBO 64 % — re-measured
//! 90 % on 2026-09-18): one instance per band multiplier k, each with its
//! share of the sleeve's vol budget (`ensemble` builds them). Same
//! envelope, σ scaled by `band_mult`; the netting book keys on `variant`.

use serde::{Deserialize, Serialize};

use crate::session::is_decision_bar;
use crate::sleeve::{DayCtx, FillAt, Sleeve, SleeveId, Target};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IntradayParams {
    pub lookback_days: usize,
    /// Multiplier applied to the fitted envelope's σ (k of the paper).
    pub band_mult: f64,
    /// Daily vol target as a fraction (0.02 = 2 %/day).
    pub sigma_target_daily: f64,
    pub max_leverage: f64,
    pub allow_short: bool,
}

impl Default for IntradayParams {
    fn default() -> Self {
        Self {
            lookback_days: 14,
            band_mult: 1.0,
            sigma_target_daily: 0.02,
            max_leverage: 4.0,
            allow_short: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IntradayMomentum {
    pub params: IntradayParams,
    variant: u8,
    pos: i8,
    lev: f64,
    active: bool,
}

impl IntradayMomentum {
    pub fn new(params: IntradayParams) -> Self {
        Self { params, variant: 0, pos: 0, lev: 0.0, active: false }
    }

    /// Equal-budget ensemble over `mults`: member i uses band multiplier
    /// `mults[i]`, σ_target / n and L_max / n, so the sleeve's total
    /// exposure never exceeds what a single instance with `params` could
    /// take. An empty `mults` yields the single instance.
    pub fn ensemble(params: IntradayParams, mults: &[f64]) -> Vec<Self> {
        if mults.is_empty() {
            return vec![Self::new(params)];
        }
        let n = mults.len() as f64;
        mults
            .iter()
            .enumerate()
            .map(|(i, &k)| Self {
                params: IntradayParams { band_mult: k, sigma_target_daily: params.sigma_target_daily / n, max_leverage: params.max_leverage / n, ..params },
                variant: i as u8,
                pos: 0,
                lev: 0.0,
                active: false,
            })
            .collect()
    }

    pub fn leverage_for(&self, sigma_cc: Option<f64>) -> f64 {
        match sigma_cc {
            Some(sd) if sd > 0.0 => (self.params.sigma_target_daily / sd).min(self.params.max_leverage),
            _ => 1.0_f64.min(self.params.max_leverage),
        }
    }

    fn target(&self, ctx: &DayCtx, pos: i8) -> Target {
        Target {
            sleeve: SleeveId::A,
            variant: self.variant,
            exposure: pos as f64 * self.lev,
            price_ref: ctx.today.open(),
            fill_at: FillAt::Close,
        }
    }
}

impl Sleeve for IntradayMomentum {
    fn id(&self) -> SleeveId {
        SleeveId::A
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.pos = 0;
        self.lev = self.leverage_for(ctx.vol.sigma_cc);
        // Early-close sessions (210 bars) are outside the validated grid and
        // the envelope's day-length; A sits out (plan §3.8.9), B still runs.
        let full_session = ctx.session_bars >= crate::session::FULL_SESSION_BARS;
        self.active = full_session && ctx.envelope.is_some() && ctx.prev_close > 0.0 && ctx.today.open() > 0.0;
    }

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target> {
        if !self.active {
            return None;
        }
        let n = ctx.session_bars;
        // Session close: always flat.
        if ctx.is_close(idx) {
            if self.pos != 0 {
                self.pos = 0;
                return Some(self.target(ctx, 0));
            }
            return None;
        }
        if !is_decision_bar(idx, n) {
            return None;
        }
        let env = ctx.envelope?;
        let c = ctx.today.bars[idx].c;
        let (ub, lb) = env.bands_scaled(idx, ctx.today.open(), ctx.prev_close, self.params.band_mult);
        let before = self.pos;
        match self.pos {
            1 if c < ub.max(vwap) => self.pos = 0,
            -1 if c > lb.min(vwap) => self.pos = 0,
            _ => {}
        }
        if self.pos == 0 {
            if c > ub {
                self.pos = 1;
            } else if c < lb && self.params.allow_short {
                self.pos = -1;
            }
        }
        (self.pos != before).then(|| self.target(ctx, self.pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bars::{Bar, DaySeries};
    use crate::envelope::NoiseEnvelope;
    use crate::vol::VolStats;
    use chrono::NaiveDate;

    fn day(path: &[(usize, f64)]) -> DaySeries {
        // piecewise: price constant between given (idx, close) points
        let mut bars = Vec::with_capacity(390);
        let mut p = path[0].1;
        for m in 0..390 {
            if let Some(&(_, np)) = path.iter().find(|(i, _)| *i == m) {
                p = np;
            }
            bars.push(Bar { o: p, h: p, l: p, c: p, v: 1.0 });
        }
        DaySeries { date: NaiveDate::from_ymd_opt(2026, 3, 2).unwrap(), bars, real_bars: 390 }
    }

    fn ctx<'a>(d: &'a DaySeries, env: &'a NoiseEnvelope, sigma_cc: f64) -> DayCtx<'a> {
        DayCtx {
            date: d.date,
            symbol: "SPY",
            today: d,
            prev_close: 100.0,
            envelope: Some(env),
            vol: VolStats { sigma_cc: Some(sigma_cc), sigma_on: None, atr: None, sma: None, ibs_last: 0.5 },
            equity: 100_000.0,
            session_bars: d.len(),
            force_close: false,
        }
    }

    fn run(s: &mut IntradayMomentum, c: &DayCtx) -> Vec<(usize, f64)> {
        s.on_day_start(c);
        let vw = crate::vwap::Vwap::series(&c.today.bars);
        (0..c.today.len()).filter_map(|i| s.on_bar(c, i, vw[i]).map(|t| (i, t.exposure))).collect()
    }

    #[test]
    fn trend_day_enters_long_and_flats_at_close() {
        let env = NoiseEnvelope { sigma: vec![0.005; 390] };
        // open 100, jumps to 101 at bar 20 (above UB 100.5), holds.
        let d = day(&[(0, 100.0), (20, 101.0)]);
        let mut s = IntradayMomentum::new(IntradayParams::default());
        let c = ctx(&d, &env, 0.01); // lev = min(4, 0.02/0.01) = 2
        let ev = run(&mut s, &c);
        assert_eq!(ev, vec![(29, 2.0), (389, 0.0)]);
    }

    #[test]
    fn stop_out_on_vwap_and_flip_short() {
        let env = NoiseEnvelope { sigma: vec![0.005; 390] };
        // up to 101 at bar 10 → long at 29; crashes to 99 at bar 40 → at 59
        // c=99 < max(ub,vwap) ⇒ exit, and 99 < lb=99.5 ⇒ flip short same bar.
        let d = day(&[(0, 100.0), (10, 101.0), (40, 99.0)]);
        let mut s = IntradayMomentum::new(IntradayParams::default());
        let c = ctx(&d, &env, 0.02); // lev 1
        let ev = run(&mut s, &c);
        assert_eq!(ev[0], (29, 1.0));
        assert_eq!(ev[1], (59, -1.0));
        assert_eq!(ev.last(), Some(&(389, 0.0)));
    }

    #[test]
    fn chop_inside_bands_never_trades_and_shorts_can_be_disabled() {
        let env = NoiseEnvelope { sigma: vec![0.02; 390] };
        let d = day(&[(0, 100.0), (15, 100.5), (45, 99.6)]);
        let mut s = IntradayMomentum::new(IntradayParams::default());
        assert!(run(&mut s, &ctx(&d, &env, 0.01)).is_empty());
        let env2 = NoiseEnvelope { sigma: vec![0.001; 390] };
        let d2 = day(&[(0, 100.0), (15, 98.0)]);
        let mut long_only = IntradayMomentum::new(IntradayParams { allow_short: false, ..Default::default() });
        assert!(run(&mut long_only, &ctx(&d2, &env2, 0.01)).is_empty());
    }

    #[test]
    fn early_close_session_is_skipped() {
        let env = NoiseEnvelope { sigma: vec![0.005; 390] };
        let d = day(&[(0, 100.0), (20, 101.0)]);
        let mut s = IntradayMomentum::new(IntradayParams::default());
        let mut c = ctx(&d, &env, 0.01);
        c.session_bars = 210; // 13:00 close
        s.on_day_start(&c);
        let vw = crate::vwap::Vwap::series(&c.today.bars);
        assert!((0..210).all(|i| s.on_bar(&c, i, vw[i]).is_none()), "A does not trade an early-close day");
    }

    #[test]
    fn ensemble_members_split_the_budget_and_trigger_in_k_order() {
        let env = NoiseEnvelope { sigma: vec![0.005; 390] };
        // open 100 → 100.6 at bar 20 (above UB for k=1.0: 100.5, below k=1.25: 100.625)
        // → 100.8 at bar 40 (above k=1.25 and k=1.5: 100.75)
        let d = day(&[(0, 100.0), (20, 100.6), (40, 100.8)]);
        let members = IntradayMomentum::ensemble(IntradayParams::default(), &[1.0, 1.25, 1.5]);
        assert_eq!(members.len(), 3);
        let c = ctx(&d, &env, 0.01); // single instance lev = 2 ⇒ each member 2/3
        let mut evs = Vec::new();
        for mut m in members {
            for (i, e) in run(&mut m, &c) {
                evs.push((m.params.band_mult, i, e));
            }
        }
        evs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(evs.iter().filter(|(_, i, e)| *i == 29 && *e > 0.0).count(), 1, "only k=1.0 enters at 10:00");
        assert_eq!(evs.iter().filter(|(_, i, e)| *i == 59 && *e > 0.0).count(), 2, "k=1.25 and 1.5 enter at 10:30");
        for (_, _, e) in &evs {
            assert!(e.abs() < 2.0 / 3.0 + 1e-9, "each member ≤ L/3: {e}");
        }
        let total: f64 = evs.iter().filter(|(_, i, _)| *i < 389).map(|(_, _, e)| e).sum();
        assert!((total - 2.0).abs() < 1e-9, "all three long ⇒ the full single-instance leverage");
        assert_eq!(IntradayMomentum::ensemble(IntradayParams::default(), &[]).len(), 1);
    }

    #[test]
    fn leverage_capped_and_neutral_without_vol() {
        let s = IntradayMomentum::new(IntradayParams::default());
        assert_eq!(s.leverage_for(Some(0.001)), 4.0);
        assert!((s.leverage_for(Some(0.04)) - 0.5).abs() < 1e-12);
        assert_eq!(s.leverage_for(None), 1.0);
    }
}
