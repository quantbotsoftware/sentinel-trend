//! Diagnostic wrappers for the D1–D8 batch
//! (`docs/meridian/validation/2026-09-27_diagnostice_astra_preregistration.md`).
//!
//! * [`PortfolioNightCap`] — D2: caps the **portfolio** overnight gross at
//!   `cap` × equity. The engine's own rail is per symbol; this wrapper shares
//!   one accumulator per session date across the B instances, allocating in
//!   decision order (SPY first, then QQQ in the time-synchronous loop).
//!   Exits (exposure 0) pass through untouched.
//! * [`DelayOneBar`] — D5: a target decided on bar `t` is handed to the
//!   netting book on bar `t+1` (so it fills at the next close). The closing
//!   bar is never delayed — A must be flat at 16:00 — and supersedes any
//!   pending target.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use titan_meridian::sleeve::{DayCtx, Sleeve, SleeveId, Target};

/// Shared per-date accumulator of |exposure| already granted to B.
#[derive(Debug, Default)]
pub struct NightBook {
    used: Mutex<BTreeMap<NaiveDate, f64>>,
    /// Requests that were clipped, for the report.
    pub clipped: Mutex<Vec<(NaiveDate, String, f64, f64)>>,
}

pub struct PortfolioNightCap<S: Sleeve> {
    inner: S,
    book: Arc<NightBook>,
    cap: f64,
    date: Option<NaiveDate>,
    symbol: String,
}

impl<S: Sleeve> PortfolioNightCap<S> {
    pub fn new(inner: S, book: Arc<NightBook>, cap: f64) -> Self {
        Self { inner, book, cap, date: None, symbol: String::new() }
    }
}

impl<S: Sleeve> Sleeve for PortfolioNightCap<S> {
    fn id(&self) -> SleeveId {
        self.inner.id()
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.date = Some(ctx.date);
        self.symbol = ctx.symbol.to_string();
        self.inner.on_day_start(ctx);
    }

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target> {
        let t = self.inner.on_bar(ctx, idx, vwap)?;
        if t.exposure == 0.0 || t.sleeve != SleeveId::B {
            return Some(t);
        }
        let Some(date) = self.date else { return Some(t) };
        let Ok(mut used) = self.book.used.lock() else { return Some(t) };
        let u = used.entry(date).or_insert(0.0);
        let room = (self.cap - *u).max(0.0);
        let want = t.exposure.abs();
        let granted = want.min(room);
        *u += granted;
        if granted < want {
            if let Ok(mut c) = self.book.clipped.lock() {
                c.push((date, self.symbol.clone(), want, granted));
            }
        }
        if granted == 0.0 {
            return None;
        }
        Some(Target { exposure: t.exposure.signum() * granted, ..t })
    }
}

pub struct DelayOneBar<S: Sleeve> {
    inner: S,
    pending: Option<Target>,
}

impl<S: Sleeve> DelayOneBar<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, pending: None }
    }
}

impl<S: Sleeve> Sleeve for DelayOneBar<S> {
    fn id(&self) -> SleeveId {
        self.inner.id()
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.pending = None;
        self.inner.on_day_start(ctx);
    }

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target> {
        let cur = self.inner.on_bar(ctx, idx, vwap);
        if ctx.is_close(idx) {
            // The close is sacred: whatever was pending is superseded by the
            // inner sleeve's own closing decision (flat for A). A pending
            // entry that never reached the book needs no exit either.
            self.pending = None;
            return cur;
        }
        let out = self.pending.take();
        if cur.is_some() {
            self.pending = cur;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use titan_meridian::bars::{Bar, DaySeries};
    use titan_meridian::sleeve::FillAt;
    use titan_meridian::vol::VolStats;

    struct Scripted {
        id: SleeveId,
        plan: Vec<(usize, f64)>,
    }
    impl Sleeve for Scripted {
        fn id(&self) -> SleeveId {
            self.id
        }
        fn on_day_start(&mut self, _ctx: &DayCtx) {}
        fn on_bar(&mut self, ctx: &DayCtx, idx: usize, _vwap: f64) -> Option<Target> {
            self.plan.iter().find(|(i, _)| *i == idx).map(|&(_, e)| Target {
                sleeve: self.id,
                variant: 0,
                exposure: e,
                price_ref: ctx.today.open(),
                fill_at: FillAt::Close,
            })
        }
    }

    fn day(n: usize) -> DaySeries {
        let bars = (0..n).map(|_| Bar { o: 100.0, h: 100.0, l: 100.0, c: 100.0, v: 1.0 }).collect();
        DaySeries { date: NaiveDate::from_ymd_opt(2026, 3, 2).unwrap(), bars, real_bars: n }
    }

    fn ctx<'a>(d: &'a DaySeries, sym: &'a str) -> DayCtx<'a> {
        DayCtx {
            date: d.date,
            symbol: sym,
            today: d,
            prev_close: 100.0,
            envelope: None,
            vol: VolStats { sigma_cc: Some(0.01), sigma_on: Some(0.005), atr: None, sma: None, ibs_last: 0.5 },
            equity: 100_000.0,
            session_bars: d.len(),
            force_close: false,
        }
    }

    #[test]
    fn portfolio_cap_allocates_in_decision_order_and_passes_exits() {
        let d = day(390);
        let book = Arc::new(NightBook::default());
        let mut spy = PortfolioNightCap::new(Scripted { id: SleeveId::B, plan: vec![(389, 0.8)] }, book.clone(), 1.0);
        let mut qqq = PortfolioNightCap::new(Scripted { id: SleeveId::B, plan: vec![(389, 0.9), (0, 0.0)] }, book.clone(), 1.0);
        let (cs, cq) = (ctx(&d, "SPY"), ctx(&d, "QQQ"));
        spy.on_day_start(&cs);
        qqq.on_day_start(&cq);
        // exits pass (exposure 0), even before anything was granted
        assert_eq!(qqq.on_bar(&cq, 0, 100.0).map(|t| t.exposure), Some(0.0));
        assert_eq!(spy.on_bar(&cs, 389, 100.0).map(|t| t.exposure), Some(0.8));
        let q = qqq.on_bar(&cq, 389, 100.0).map(|t| t.exposure);
        assert!((q.unwrap() - 0.2).abs() < 1e-12, "QQQ gets the remaining 0.2: {q:?}");
        assert_eq!(book.clipped.lock().unwrap().len(), 1);
    }

    #[test]
    fn portfolio_cap_leaves_a_untouched_and_returns_none_when_no_room() {
        let d = day(390);
        let book = Arc::new(NightBook::default());
        let mut a = PortfolioNightCap::new(Scripted { id: SleeveId::A, plan: vec![(29, 2.0)] }, book.clone(), 1.0);
        let c = ctx(&d, "SPY");
        a.on_day_start(&c);
        assert_eq!(a.on_bar(&c, 29, 100.0).map(|t| t.exposure), Some(2.0), "A is not a night position");
        let mut b1 = PortfolioNightCap::new(Scripted { id: SleeveId::B, plan: vec![(389, 1.0)] }, book.clone(), 1.0);
        let mut b2 = PortfolioNightCap::new(Scripted { id: SleeveId::B, plan: vec![(389, 1.0)] }, book, 1.0);
        b1.on_day_start(&c);
        b2.on_day_start(&c);
        assert_eq!(b1.on_bar(&c, 389, 100.0).map(|t| t.exposure), Some(1.0));
        assert!(b2.on_bar(&c, 389, 100.0).is_none(), "no room left");
    }

    #[test]
    fn delay_shifts_targets_by_one_bar_but_never_past_the_close() {
        let d = day(390);
        let c = ctx(&d, "SPY");
        let mut s = DelayOneBar::new(Scripted { id: SleeveId::A, plan: vec![(29, 1.0), (59, -1.0), (389, 0.0)] });
        s.on_day_start(&c);
        let ev: Vec<(usize, f64)> = (0..390).filter_map(|i| s.on_bar(&c, i, 100.0).map(|t| (i, t.exposure))).collect();
        assert_eq!(ev, vec![(30, 1.0), (60, -1.0), (389, 0.0)]);
        // a decision on the bar right before the close is superseded by the close
        let mut s = DelayOneBar::new(Scripted { id: SleeveId::A, plan: vec![(388, 1.0), (389, 0.0)] });
        s.on_day_start(&c);
        let ev: Vec<(usize, f64)> = (0..390).filter_map(|i| s.on_bar(&c, i, 100.0).map(|t| (i, t.exposure))).collect();
        assert_eq!(ev, vec![(389, 0.0)], "never carries A past 16:00");
    }
}
