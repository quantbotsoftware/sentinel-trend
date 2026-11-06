//! Runda 5 (Astra, 2026-09-27) wrappers — pre-registered in
//! `docs/meridian/validation/2026-09-27_runda5_q2_q3_preregistration.md`.
//!
//! * [`cond_caps`] + [`CondNightCap`] — Q2: Astra's conditional portfolio
//!   overnight cap. From SPY's completed overnight returns
//!   r_ON[i] = open_i / close_{i−1} − 1, at session index i:
//!   v20 = sd(r_ON[i−19..=i]), v252 = sd(r_ON[i−251..=i]);
//!   G_t = 1.0 if v20 ≤ v252 else 1.9; 1.0 until 252 overnights exist.
//!   The wrapper allocates B's gross in decision order (SPY, then QQQ) like
//!   `diag::PortfolioNightCap`, with the cap looked up per date.
//! * [`ScaleExposure`] — Q3: multiplies a sleeve's target exposure by a fixed
//!   factor *after* its internal caps (Astra: x = 0.15 on A, y = 0.2833 on B).
//!   Exits (exposure 0) pass through.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use titan_meridian::bars::DaySeries;
use titan_meridian::sleeve::{DayCtx, Sleeve, SleeveId, Target};
use titan_meridian::vol::stdev;

pub const CAP_CALM: f64 = 1.0;
pub const CAP_STRESSED: f64 = 1.9;
pub const SHORT_WINDOW: usize = 20;
pub const LONG_WINDOW: usize = 252;

/// Per-date cap G_t from a symbol's sessions (Astra Q2, SPY). Dates before
/// the 252nd completed overnight get `CAP_CALM`.
pub fn cond_caps(days: &[DaySeries]) -> BTreeMap<NaiveDate, f64> {
    let mut r_on: Vec<f64> = Vec::with_capacity(days.len());
    let mut out = BTreeMap::new();
    for (i, d) in days.iter().enumerate() {
        if i > 0 {
            let prev = days[i - 1].close();
            if prev > 0.0 {
                r_on.push(d.open() / prev - 1.0);
            }
        }
        let n = r_on.len();
        let cap = if n >= LONG_WINDOW {
            let v20 = stdev(&r_on[n - SHORT_WINDOW..]).unwrap_or(0.0);
            let v252 = stdev(&r_on[n - LONG_WINDOW..]).unwrap_or(0.0);
            if v20 <= v252 { CAP_CALM } else { CAP_STRESSED }
        } else {
            CAP_CALM
        };
        out.insert(d.date, cap);
    }
    out
}

/// Shared per-date accumulator of |exposure| already granted to B tonight.
#[derive(Debug, Default)]
pub struct NightLedger {
    used: Mutex<BTreeMap<NaiveDate, f64>>,
    pub clipped: Mutex<usize>,
}

pub struct CondNightCap<S: Sleeve> {
    inner: S,
    ledger: Arc<NightLedger>,
    caps: Arc<BTreeMap<NaiveDate, f64>>,
    date: Option<NaiveDate>,
}

impl<S: Sleeve> CondNightCap<S> {
    pub fn new(inner: S, ledger: Arc<NightLedger>, caps: Arc<BTreeMap<NaiveDate, f64>>) -> Self {
        Self { inner, ledger, caps, date: None }
    }
}

impl<S: Sleeve> Sleeve for CondNightCap<S> {
    fn id(&self) -> SleeveId {
        self.inner.id()
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.date = Some(ctx.date);
        self.inner.on_day_start(ctx);
    }

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target> {
        let t = self.inner.on_bar(ctx, idx, vwap)?;
        if t.exposure == 0.0 || t.sleeve != SleeveId::B {
            return Some(t);
        }
        let Some(date) = self.date else { return Some(t) };
        let cap = self.caps.get(&date).copied().unwrap_or(CAP_CALM);
        let Ok(mut used) = self.ledger.used.lock() else { return Some(t) };
        let u = used.entry(date).or_insert(0.0);
        let room = (cap - *u).max(0.0);
        let want = t.exposure.abs();
        let granted = want.min(room);
        *u += granted;
        if granted < want {
            if let Ok(mut c) = self.ledger.clipped.lock() {
                *c += 1;
            }
        }
        if granted == 0.0 {
            return None;
        }
        Some(Target { exposure: t.exposure.signum() * granted, ..t })
    }
}

pub struct ScaleExposure<S: Sleeve> {
    inner: S,
    factor: f64,
}

impl<S: Sleeve> ScaleExposure<S> {
    pub fn new(inner: S, factor: f64) -> Self {
        Self { inner, factor }
    }
}

impl<S: Sleeve> Sleeve for ScaleExposure<S> {
    fn id(&self) -> SleeveId {
        self.inner.id()
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.inner.on_day_start(ctx);
    }

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target> {
        let t = self.inner.on_bar(ctx, idx, vwap)?;
        Some(Target { exposure: t.exposure * self.factor, ..t })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use titan_meridian::bars::Bar;
    use titan_meridian::sleeve::FillAt;
    use titan_meridian::vol::VolStats;

    fn day(n: u32, open: f64, close: f64) -> DaySeries {
        let date = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap() + chrono::Days::new(n as u64);
        let mut bars: Vec<Bar> = (0..390).map(|_| Bar { o: open, h: open.max(close), l: open.min(close), c: close, v: 1.0 }).collect();
        bars[0].o = open;
        DaySeries { date, bars, real_bars: 390 }
    }

    #[test]
    fn caps_are_calm_until_252_overnights_then_follow_v20_vs_v252() {
        // 280 days at ±0.2 %, 20 compressed days at ±0.1 % (v20 < v252),
        // then 30 noisy days at ±2 % (v20 > v252). Alternating signs.
        let mut days = Vec::new();
        let mut px = 100.0;
        for i in 0..330u32 {
            let amp = if i < 280 { 0.002 } else if i < 300 { 0.001 } else { 0.02 };
            let open = px * (1.0 + if i % 2 == 0 { amp } else { -amp });
            days.push(day(i, open, open));
            px = open;
        }
        let caps = cond_caps(&days);
        assert_eq!(caps.len(), 330);
        // fewer than 252 overnights ⇒ calm by construction
        assert_eq!(caps[&days[100].date], CAP_CALM);
        assert_eq!(caps[&days[251].date], CAP_CALM);
        assert_eq!(caps[&days[299].date], CAP_CALM, "compressed short window: v20 ≤ v252");
        assert_eq!(caps[&days[300].date], CAP_STRESSED, "first noisy overnight lifts v20 above v252");
        assert_eq!(caps[&days[329].date], CAP_STRESSED, "noisy tail: v20 > v252");
    }

    struct Scripted(f64);
    impl Sleeve for Scripted {
        fn id(&self) -> SleeveId {
            SleeveId::B
        }
        fn on_day_start(&mut self, _ctx: &DayCtx) {}
        fn on_bar(&mut self, ctx: &DayCtx, idx: usize, _vwap: f64) -> Option<Target> {
            (idx == 389).then_some(Target { sleeve: SleeveId::B, variant: 0, exposure: self.0, price_ref: ctx.today.open(), fill_at: FillAt::Close })
        }
    }

    fn ctx<'a>(d: &'a DaySeries, sym: &'a str) -> DayCtx<'a> {
        DayCtx { date: d.date, symbol: sym, today: d, prev_close: 100.0, envelope: None, vol: VolStats { sigma_cc: Some(0.01), sigma_on: Some(0.005), atr: None, sma: None, ibs_last: 0.5 }, equity: 100_000.0, session_bars: 390, force_close: false }
    }

    #[test]
    fn cond_cap_allocates_in_decision_order_with_the_dates_cap() {
        let d = day(0, 100.0, 100.0);
        let mut caps = BTreeMap::new();
        caps.insert(d.date, CAP_STRESSED);
        let (caps, ledger) = (Arc::new(caps), Arc::new(NightLedger::default()));
        let mut spy = CondNightCap::new(Scripted(1.0), ledger.clone(), caps.clone());
        let mut qqq = CondNightCap::new(Scripted(1.0), ledger.clone(), caps);
        let (cs, cq) = (ctx(&d, "SPY"), ctx(&d, "QQQ"));
        spy.on_day_start(&cs);
        qqq.on_day_start(&cq);
        assert_eq!(spy.on_bar(&cs, 389, 0.0).map(|t| t.exposure), Some(1.0));
        let q = qqq.on_bar(&cq, 389, 0.0).map(|t| t.exposure).unwrap();
        assert!((q - 0.9).abs() < 1e-12, "1.9 − 1.0 = 0.9 left for QQQ: {q}");
        assert_eq!(*ledger.clipped.lock().unwrap(), 1);
        // a date without a cap entry falls back to 1.0
        let d2 = day(1, 100.0, 100.0);
        let mut x = CondNightCap::new(Scripted(1.0), ledger.clone(), Arc::new(BTreeMap::new()));
        let c2 = ctx(&d2, "SPY");
        x.on_day_start(&c2);
        assert_eq!(x.on_bar(&c2, 389, 0.0).map(|t| t.exposure), Some(1.0));
        let mut y = CondNightCap::new(Scripted(0.5), ledger, Arc::new(BTreeMap::new()));
        y.on_day_start(&c2);
        assert!(y.on_bar(&c2, 389, 0.0).is_none(), "no room left under the 1.0 fallback");
    }

    #[test]
    fn scale_multiplies_exposure_and_keeps_exits() {
        let d = day(0, 100.0, 100.0);
        let c = ctx(&d, "SPY");
        let mut s = ScaleExposure::new(Scripted(-0.8), 0.2833);
        assert!((s.on_bar(&c, 389, 0.0).unwrap().exposure + 0.22664).abs() < 1e-9);
        let mut z = ScaleExposure::new(Scripted(0.0), 0.15);
        assert_eq!(z.on_bar(&c, 389, 0.0).unwrap().exposure, 0.0);
    }
}
