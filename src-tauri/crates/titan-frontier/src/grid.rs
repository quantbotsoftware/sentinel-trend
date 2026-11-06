//! Sleeve A on a shifted decision grid (chat 8 E1, pre-registered in
//! `docs/meridian/validation/2026-09-26_chat8_preregistration.md`).
//!
//! Same rule as `titan_meridian::sleeves::IntradayMomentum` — band breakout,
//! VWAP trail, flat on the closing bar, size fixed at the open — except that
//! member decisions fall on bars whose minute count minus `offset` is a
//! multiple of 30. Offset 0 is the live grid (10:00, 10:30, … 15:30).

use titan_meridian::session::FULL_SESSION_BARS;
use titan_meridian::sleeve::{DayCtx, FillAt, Sleeve, SleeveId, Target};
use titan_meridian::sleeves::IntradayParams;

/// Decision bar for a grid shifted by `offset` minutes: first decision at
/// least 30 min after the open, last strictly before the closing bar.
pub fn is_offset_decision_bar(idx: usize, session_bars: usize, offset: usize) -> bool {
    let m = idx + 1;
    m >= 30 + offset && (m - offset).is_multiple_of(30) && m < session_bars
}

#[derive(Debug, Clone)]
pub struct GridMomentum {
    pub params: IntradayParams,
    pub offset: usize,
    variant: u8,
    pos: i8,
    lev: f64,
    active: bool,
}

impl GridMomentum {
    /// Equal-budget ensemble over `mults × offsets`: each member gets
    /// σ_target / n and L_max / n, n = |mults| · |offsets|.
    pub fn ensemble(params: IntradayParams, mults: &[f64], offsets: &[usize]) -> Vec<Self> {
        let n = (mults.len() * offsets.len()).max(1) as f64;
        let mut out = Vec::new();
        for &o in offsets {
            for &k in mults {
                out.push(Self {
                    params: IntradayParams { band_mult: k, sigma_target_daily: params.sigma_target_daily / n, max_leverage: params.max_leverage / n, ..params },
                    offset: o,
                    variant: out.len() as u8,
                    pos: 0,
                    lev: 0.0,
                    active: false,
                });
            }
        }
        out
    }

    fn leverage_for(&self, sigma_cc: Option<f64>) -> f64 {
        match sigma_cc {
            Some(sd) if sd > 0.0 => (self.params.sigma_target_daily / sd).min(self.params.max_leverage),
            _ => 1.0_f64.min(self.params.max_leverage),
        }
    }

    fn target(&self, ctx: &DayCtx, pos: i8) -> Target {
        Target { sleeve: SleeveId::A, variant: self.variant, exposure: pos as f64 * self.lev, price_ref: ctx.today.open(), fill_at: FillAt::Close }
    }
}

impl Sleeve for GridMomentum {
    fn id(&self) -> SleeveId {
        SleeveId::A
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.pos = 0;
        self.lev = self.leverage_for(ctx.vol.sigma_cc);
        let full_session = ctx.session_bars >= FULL_SESSION_BARS;
        self.active = full_session && ctx.envelope.is_some() && ctx.prev_close > 0.0 && ctx.today.open() > 0.0;
    }

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target> {
        if !self.active {
            return None;
        }
        if ctx.is_close(idx) {
            if self.pos != 0 {
                self.pos = 0;
                return Some(self.target(ctx, 0));
            }
            return None;
        }
        if !is_offset_decision_bar(idx, ctx.session_bars, self.offset) {
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
    use chrono::NaiveDate;
    use titan_meridian::envelope::NoiseEnvelope;
    use titan_meridian::session::is_decision_bar;
    use titan_meridian::sleeves::IntradayMomentum;
    use titan_meridian::vol::VolStats;
    use titan_meridian::vwap::Vwap;
    use titan_meridian::{Bar, DaySeries};

    #[test]
    fn offset_zero_is_the_live_grid() {
        for n in [390, 210] {
            for idx in 0..n {
                assert_eq!(is_offset_decision_bar(idx, n, 0), is_decision_bar(idx, n), "idx {idx} n {n}");
            }
        }
        let shifted: Vec<usize> = (0..390).filter(|&i| is_offset_decision_bar(i, 390, 20)).collect();
        assert_eq!(shifted.first(), Some(&49));
        assert_eq!(shifted.last(), Some(&379));
        assert_eq!(shifted.len(), 12);
    }

    fn day() -> DaySeries {
        let mut bars = Vec::with_capacity(390);
        for m in 0..390usize {
            let p = 100.0 + 1.5 * ((m as f64) / 60.0).sin() + 0.002 * m as f64;
            bars.push(Bar { o: p, h: p + 0.05, l: p - 0.05, c: p, v: 1.0 });
        }
        DaySeries { date: NaiveDate::from_ymd_opt(2026, 3, 2).unwrap_or_default(), bars, real_bars: 390 }
    }

    #[test]
    fn offset_zero_ensemble_matches_the_engine_sleeve() {
        let d = day();
        let env = NoiseEnvelope { sigma: vec![0.004; 390] };
        let ctx = DayCtx {
            date: d.date,
            symbol: "SPY",
            today: &d,
            prev_close: 100.0,
            envelope: Some(&env),
            vol: VolStats { sigma_cc: Some(0.01), sigma_on: None, atr: None, sma: None, ibs_last: 0.5 },
            equity: 100_000.0,
            session_bars: 390,
            force_close: false,
        };
        let p = IntradayParams { sigma_target_daily: 0.01, max_leverage: 2.0, ..Default::default() };
        let mults = [1.0, 1.25, 1.5];
        let vw = Vwap::series(&d.bars);
        let mut live: Vec<Box<dyn Sleeve>> = IntradayMomentum::ensemble(p, &mults).into_iter().map(|m| Box::new(m) as Box<dyn Sleeve>).collect();
        let mut grid: Vec<Box<dyn Sleeve>> = GridMomentum::ensemble(p, &mults, &[0]).into_iter().map(|m| Box::new(m) as Box<dyn Sleeve>).collect();
        let run = |ss: &mut Vec<Box<dyn Sleeve>>| {
            let mut ev = Vec::new();
            for s in ss.iter_mut() {
                s.on_day_start(&ctx);
                for (i, v) in vw.iter().enumerate() {
                    if let Some(t) = s.on_bar(&ctx, i, *v) {
                        ev.push((i, t.variant, (t.exposure * 1e9).round() as i64));
                    }
                }
            }
            ev
        };
        let a = run(&mut live);
        assert!(!a.is_empty(), "fixture must trade");
        assert_eq!(a, run(&mut grid));
    }

    #[test]
    fn nine_members_split_the_budget() {
        let p = IntradayParams { sigma_target_daily: 0.01, max_leverage: 2.0, ..Default::default() };
        let m = GridMomentum::ensemble(p, &[1.0, 1.25, 1.5], &[0, 10, 20]);
        assert_eq!(m.len(), 9);
        let lsum: f64 = m.iter().map(|x| x.params.max_leverage).sum();
        assert!((lsum - 2.0).abs() < 1e-12);
        let variants: Vec<u8> = m.iter().map(|x| x.variant).collect();
        assert_eq!(variants, (0..9).collect::<Vec<u8>>());
    }
}
