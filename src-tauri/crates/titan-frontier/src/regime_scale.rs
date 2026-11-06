//! VIX term-structure regime scaling around any [`Sleeve`] (pre-registered in
//! `docs/meridian/validation/2026-09-26_vix_regime_preregistration.md`).
//!
//! [`RegimeMap`] holds one label per CBOE trading day — *contango* when
//! `VIX < VIX3M` at that day's close — and answers, for a session date `d`,
//! with the label of the **last CBOE date strictly before `d`** (VIX settles
//! at 16:15 ET, after equities, so the same-day close is never available to
//! any decision inside `d`). [`RegimeScaled`] fixes a multiplicative factor
//! at `on_day_start` from that label and the sleeve's id, and multiplies
//! every non-zero target exposure by it; exits (exposure 0) pass through
//! unchanged. Dates without a label ⇒ factor 1.0.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use titan_meridian::sleeve::{DayCtx, Sleeve, SleeveId, Target};

/// Per-day term-structure label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// `VIX < VIX3M` — calm.
    Contango,
    /// `VIX ≥ VIX3M` — stress.
    Backwardation,
}

/// Parse a CBOE `*_History.csv` (`DATE,OPEN,HIGH,LOW,CLOSE`, `MM/DD/YYYY`)
/// into `(date → close)`. Unparseable lines are skipped.
pub fn parse_cboe_closes(csv: &str) -> BTreeMap<NaiveDate, f64> {
    let mut out = BTreeMap::new();
    for line in csv.lines().skip(1) {
        let mut it = line.split(',');
        let (Some(d), Some(c)) = (it.next(), it.nth(3)) else { continue };
        let Ok(date) = NaiveDate::parse_from_str(d.trim(), "%m/%d/%Y") else { continue };
        let Ok(close) = c.trim().parse::<f64>() else { continue };
        if close.is_finite() && close > 0.0 {
            out.insert(date, close);
        }
    }
    out
}

/// Term-structure labels by CBOE date.
#[derive(Debug, Clone, Default)]
pub struct RegimeMap {
    labels: BTreeMap<NaiveDate, Regime>,
}

impl RegimeMap {
    /// Join VIX and VIX3M closes on common dates.
    pub fn from_closes(vix: &BTreeMap<NaiveDate, f64>, vix3m: &BTreeMap<NaiveDate, f64>) -> Self {
        let labels = vix
            .iter()
            .filter_map(|(d, v)| vix3m.get(d).map(|m| (*d, if v < m { Regime::Contango } else { Regime::Backwardation })))
            .collect();
        Self { labels }
    }

    pub fn from_cboe_csv(vix_csv: &str, vix3m_csv: &str) -> Self {
        Self::from_closes(&parse_cboe_closes(vix_csv), &parse_cboe_closes(vix3m_csv))
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// Label known at the start of session `date`: the last CBOE date
    /// strictly before it.
    pub fn for_session(&self, date: NaiveDate) -> Option<Regime> {
        self.labels.range(..date).next_back().map(|(_, r)| *r)
    }

    /// Fraction of labelled days in backwardation.
    pub fn backwardation_share(&self) -> f64 {
        if self.labels.is_empty() {
            return f64::NAN;
        }
        self.labels.values().filter(|r| **r == Regime::Backwardation).count() as f64 / self.labels.len() as f64
    }
}

/// Which sleeve is halved in which regime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleRule {
    /// The plan: contango ⇒ A ×0.5; backwardation ⇒ B ×0.5.
    Plan,
    /// Diagnostic mirror: contango ⇒ B ×0.5; backwardation ⇒ A ×0.5.
    Reverse,
}

impl ScaleRule {
    /// Exposure factor for `sleeve` under `regime`.
    pub fn factor(&self, sleeve: SleeveId, regime: Option<Regime>) -> f64 {
        let Some(r) = regime else { return 1.0 };
        let halve_a = match (self, r) {
            (ScaleRule::Plan, Regime::Contango) | (ScaleRule::Reverse, Regime::Backwardation) => true,
            (ScaleRule::Plan, Regime::Backwardation) | (ScaleRule::Reverse, Regime::Contango) => false,
        };
        match sleeve {
            SleeveId::A if halve_a => 0.5,
            SleeveId::B if !halve_a => 0.5,
            _ => 1.0,
        }
    }
}

pub struct RegimeScaled<S: Sleeve> {
    inner: S,
    map: std::sync::Arc<RegimeMap>,
    rule: ScaleRule,
    factor: f64,
}

impl<S: Sleeve> RegimeScaled<S> {
    pub fn new(inner: S, map: std::sync::Arc<RegimeMap>, rule: ScaleRule) -> Self {
        Self { inner, map, rule, factor: 1.0 }
    }
}

impl<S: Sleeve> Sleeve for RegimeScaled<S> {
    fn id(&self) -> SleeveId {
        self.inner.id()
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.factor = self.rule.factor(self.inner.id(), self.map.for_session(ctx.date));
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

    const VIX: &str = "DATE,OPEN,HIGH,LOW,CLOSE\n01/04/2016,20,20,20,22.50\n01/05/2016,20,20,20,19.00\n01/06/2016,20,20,20,20.00\nbad line\n";
    const VIX3M: &str = "DATE,OPEN,HIGH,LOW,CLOSE\n01/04/2016,20,20,20,21.00\n01/05/2016,20,20,20,21.00\n01/07/2016,20,20,20,21.00\n";

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn parses_cboe_and_joins_on_common_dates() {
        let v = parse_cboe_closes(VIX);
        assert_eq!(v.len(), 3);
        assert_eq!(v[&d(2016, 1, 4)], 22.5);
        let m = RegimeMap::from_cboe_csv(VIX, VIX3M);
        assert_eq!(m.len(), 2, "01/06 has no VIX3M, 01/07 no VIX");
        assert_eq!(m.for_session(d(2016, 1, 5)), Some(Regime::Backwardation), "uses 01/04: 22.5 ≥ 21");
        assert_eq!(m.for_session(d(2016, 1, 6)), Some(Regime::Contango), "uses 01/05: 19 < 21");
        assert_eq!(m.for_session(d(2016, 1, 4)), None, "nothing strictly before");
        assert_eq!(m.for_session(d(2016, 2, 1)), Some(Regime::Contango), "carries the last label forward");
        assert!((m.backwardation_share() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn same_day_close_is_never_used() {
        let m = RegimeMap::from_cboe_csv(VIX, VIX3M);
        // 01/05 itself is contango, but a session on 01/05 must see 01/04.
        assert_eq!(m.for_session(d(2016, 1, 5)), Some(Regime::Backwardation));
    }

    #[test]
    fn factors_follow_the_plan_and_its_mirror() {
        use Regime::*;
        use ScaleRule::*;
        assert_eq!(Plan.factor(SleeveId::A, Some(Contango)), 0.5);
        assert_eq!(Plan.factor(SleeveId::B, Some(Contango)), 1.0);
        assert_eq!(Plan.factor(SleeveId::A, Some(Backwardation)), 1.0);
        assert_eq!(Plan.factor(SleeveId::B, Some(Backwardation)), 0.5);
        assert_eq!(Reverse.factor(SleeveId::A, Some(Contango)), 1.0);
        assert_eq!(Reverse.factor(SleeveId::B, Some(Contango)), 0.5);
        assert_eq!(Plan.factor(SleeveId::A, None), 1.0);
        assert_eq!(Plan.factor(SleeveId::C, Some(Contango)), 1.0, "other sleeves untouched");
    }

    #[test]
    fn wrapper_scales_entries_and_leaves_exits_flat() {
        use titan_meridian::bars::{Bar, DaySeries};
        use titan_meridian::sleeve::FillAt;
        use titan_meridian::vol::VolStats;

        struct Scripted;
        impl Sleeve for Scripted {
            fn id(&self) -> SleeveId {
                SleeveId::A
            }
            fn on_day_start(&mut self, _ctx: &DayCtx) {}
            fn on_bar(&mut self, _ctx: &DayCtx, idx: usize, _vwap: f64) -> Option<Target> {
                let e = match idx {
                    1 => 2.0,
                    3 => 0.0,
                    _ => return None,
                };
                Some(Target { sleeve: SleeveId::A, variant: 0, exposure: e, price_ref: 100.0, fill_at: FillAt::Close })
            }
        }
        let bars = (0..5).map(|_| Bar { o: 100.0, h: 100.0, l: 100.0, c: 100.0, v: 1.0 }).collect();
        let day = DaySeries { date: d(2016, 1, 6), bars, real_bars: 5 };
        let ctx = DayCtx {
            date: day.date,
            symbol: "SPY",
            today: &day,
            prev_close: 100.0,
            envelope: None,
            vol: VolStats { sigma_cc: Some(0.01), sigma_on: None, atr: None, sma: None, ibs_last: 0.5 },
            equity: 100_000.0,
            session_bars: 5,
            force_close: false,
        };
        let map = std::sync::Arc::new(RegimeMap::from_cboe_csv(VIX, VIX3M));
        // 01/06 sees 01/05 = contango ⇒ plan halves A.
        let mut s = RegimeScaled::new(Scripted, map, ScaleRule::Plan);
        s.on_day_start(&ctx);
        let ev: Vec<(usize, f64)> = (0..5).filter_map(|i| s.on_bar(&ctx, i, 100.0).map(|t| (i, t.exposure))).collect();
        assert_eq!(ev, vec![(1, 1.0), (3, 0.0)]);
    }
}
