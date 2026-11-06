//! Hurst entry gate around any [`Sleeve`] (pre-registered in
//! `docs/meridian/validation/2026-09-26_hurst_gate_preregistration.md`).
//!
//! The wrapper streams the symbol's intraday 1-minute log returns (the
//! overnight return, bar 0 vs. the previous close, is excluded) into a
//! [`HurstWindow`] of `window` observations and estimates `H` by DFA-1 at
//! the moment the inner sleeve wants a **non-flat** exposure. If the rule
//! refuses, the wrapper stays (or goes) flat; the inner sleeve keeps its own
//! state, so its later exit is simply swallowed when the wrapper holds
//! nothing. The gate never closes an open position on its own, and never
//! touches the inner sleeve's parameters. Window not yet full ⇒ gate open.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use titan_indicators::hurst::HurstWindow;
use titan_meridian::sleeve::{DayCtx, Sleeve, SleeveId, Target};

/// Which `H` values let an entry through.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GateRule {
    /// Entry only when `H > hi` (trend regime).
    TrendAbove(f64),
    /// Entry when `H < lo` or `H > hi` — the literal "noise band" proposal.
    OutsideBand(f64, f64),
}

impl GateRule {
    pub fn allows(&self, h: f64) -> bool {
        match *self {
            GateRule::TrendAbove(hi) => h > hi,
            GateRule::OutsideBand(lo, hi) => h < lo || h > hi,
        }
    }

    pub fn label(&self) -> String {
        match *self {
            GateRule::TrendAbove(hi) => format!("H>{hi:.2}"),
            GateRule::OutsideBand(lo, hi) => format!("H∉[{lo:.2},{hi:.2}]"),
        }
    }
}

/// Shared counters for one scenario (all symbols, all ensemble members).
#[derive(Debug, Default)]
pub struct GateStats {
    /// Non-flat targets the inner sleeve produced (entries and flips).
    pub decisions: AtomicU64,
    /// Of those, how many the gate refused.
    pub blocked: AtomicU64,
    /// Decisions taken before the window was full (gate open by default).
    pub warmup: AtomicU64,
    /// Every `H` seen at a decision, for the percentile report.
    pub h_samples: Mutex<Vec<f64>>,
}

impl GateStats {
    pub fn blocked_fraction(&self) -> f64 {
        let d = self.decisions.load(Ordering::Relaxed);
        if d == 0 {
            return 0.0;
        }
        self.blocked.load(Ordering::Relaxed) as f64 / d as f64
    }

    /// `(p10, p50, p90)` of the recorded `H` values; NaN when empty.
    pub fn h_percentiles(&self) -> (f64, f64, f64) {
        let Ok(g) = self.h_samples.lock() else { return (f64::NAN, f64::NAN, f64::NAN) };
        if g.is_empty() {
            return (f64::NAN, f64::NAN, f64::NAN);
        }
        let mut v = g.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pick = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        (pick(0.10), pick(0.50), pick(0.90))
    }
}

pub struct HurstGated<S: Sleeve> {
    inner: S,
    rule: GateRule,
    window: HurstWindow,
    /// Exposure the wrapper last handed to the netting book.
    out_exposure: f64,
    stats: Arc<GateStats>,
}

impl<S: Sleeve> HurstGated<S> {
    pub fn new(inner: S, rule: GateRule, window: usize, stats: Arc<GateStats>) -> Self {
        Self { inner, rule, window: HurstWindow::new(window), out_exposure: 0.0, stats }
    }

    fn flat_from(&mut self, t: Target) -> Option<Target> {
        if self.out_exposure != 0.0 {
            self.out_exposure = 0.0;
            Some(Target { exposure: 0.0, ..t })
        } else {
            None
        }
    }
}

impl<S: Sleeve> Sleeve for HurstGated<S> {
    fn id(&self) -> SleeveId {
        self.inner.id()
    }

    fn on_day_start(&mut self, ctx: &DayCtx) {
        self.inner.on_day_start(ctx);
    }

    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target> {
        if idx >= 1 {
            let bars = &ctx.today.bars;
            if let (Some(cur), Some(prev)) = (bars.get(idx), bars.get(idx - 1)) {
                if cur.c > 0.0 && prev.c > 0.0 {
                    self.window.push((cur.c / prev.c).ln());
                }
            }
        }
        let t = self.inner.on_bar(ctx, idx, vwap)?;
        if t.exposure == 0.0 {
            return self.flat_from(t);
        }
        self.stats.decisions.fetch_add(1, Ordering::Relaxed);
        let allowed = match self.window.dfa() {
            None => {
                self.stats.warmup.fetch_add(1, Ordering::Relaxed);
                true
            }
            Some(h) => {
                if let Ok(mut g) = self.stats.h_samples.lock() {
                    g.push(h);
                }
                self.rule.allows(h)
            }
        };
        if allowed {
            self.out_exposure = t.exposure;
            Some(t)
        } else {
            self.stats.blocked.fetch_add(1, Ordering::Relaxed);
            self.flat_from(t)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use titan_meridian::bars::{Bar, DaySeries};
    use titan_meridian::sleeve::{FillAt, SleeveId};
    use titan_meridian::vol::VolStats;

    /// Inner sleeve that emits a scripted exposure at a scripted bar.
    struct Scripted {
        plan: Vec<(usize, f64)>,
    }

    impl Sleeve for Scripted {
        fn id(&self) -> SleeveId {
            SleeveId::A
        }
        fn on_day_start(&mut self, _ctx: &DayCtx) {}
        fn on_bar(&mut self, ctx: &DayCtx, idx: usize, _vwap: f64) -> Option<Target> {
            self.plan.iter().find(|(i, _)| *i == idx).map(|&(_, e)| Target {
                sleeve: SleeveId::A,
                variant: 0,
                exposure: e,
                price_ref: ctx.today.open(),
                fill_at: FillAt::Close,
            })
        }
    }

    fn day(closes: &[f64]) -> DaySeries {
        let bars = closes.iter().map(|&c| Bar { o: c, h: c, l: c, c, v: 1.0 }).collect();
        DaySeries { date: NaiveDate::from_ymd_opt(2026, 3, 2).unwrap(), bars, real_bars: closes.len() }
    }

    fn ctx<'a>(d: &'a DaySeries) -> DayCtx<'a> {
        DayCtx {
            date: d.date,
            symbol: "SPY",
            today: d,
            prev_close: 100.0,
            envelope: None,
            vol: VolStats { sigma_cc: Some(0.01), sigma_on: None, atr: None, sma: None, ibs_last: 0.5 },
            equity: 100_000.0,
            session_bars: d.len(),
            force_close: false,
        }
    }

    fn run(g: &mut HurstGated<Scripted>, d: &DaySeries) -> Vec<(usize, f64)> {
        let c = ctx(d);
        g.on_day_start(&c);
        (0..d.len()).filter_map(|i| g.on_bar(&c, i, 100.0).map(|t| (i, t.exposure))).collect()
    }

    #[test]
    fn rule_semantics() {
        assert!(GateRule::TrendAbove(0.55).allows(0.56));
        assert!(!GateRule::TrendAbove(0.55).allows(0.55));
        assert!(GateRule::OutsideBand(0.45, 0.55).allows(0.40));
        assert!(GateRule::OutsideBand(0.45, 0.55).allows(0.60));
        assert!(!GateRule::OutsideBand(0.45, 0.55).allows(0.50));
    }

    #[test]
    fn warmup_passes_everything_through() {
        // 200 bars < window 390 ⇒ gate open; entry and exit both forwarded.
        let d = day(&vec![100.0; 200]);
        let stats = Arc::new(GateStats::default());
        let mut g = HurstGated::new(Scripted { plan: vec![(29, 1.0), (199, 0.0)] }, GateRule::TrendAbove(0.55), 390, stats.clone());
        assert_eq!(run(&mut g, &d), vec![(29, 1.0), (199, 0.0)]);
        assert_eq!(stats.warmup.load(Ordering::Relaxed), 1);
        assert_eq!(stats.blocked.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn strong_trend_is_allowed_and_pure_noise_is_blocked() {
        // Persistent returns (AR(1), φ = 0.9) ⇒ H high ⇒ allowed. A pure
        // linear drift would NOT do: centring removes it and DFA then sees
        // only the residual noise, which is the correct behaviour.
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut r = 0.0;
        let mut p = 100.0;
        let trend: Vec<f64> = (0..400)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let u = (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
                r = 0.9 * r + u * 1e-3;
                p *= (1.0 + r).max(0.5);
                p
            })
            .collect();
        let d = day(&trend);
        let stats = Arc::new(GateStats::default());
        let mut g = HurstGated::new(Scripted { plan: vec![(395, 1.0), (399, 0.0)] }, GateRule::TrendAbove(0.55), 390, stats.clone());
        assert_eq!(run(&mut g, &d), vec![(395, 1.0), (399, 0.0)]);
        assert_eq!(stats.blocked.load(Ordering::Relaxed), 0);

        // Alternating ±1 tick ⇒ strongly anti-persistent ⇒ H ≪ 0.55 ⇒ blocked,
        // and the later exit is swallowed because nothing was opened.
        let zig: Vec<f64> = (0..400).map(|i| if i % 2 == 0 { 100.0 } else { 100.1 }).collect();
        let d = day(&zig);
        let stats = Arc::new(GateStats::default());
        let mut g = HurstGated::new(Scripted { plan: vec![(395, 1.0), (399, 0.0)] }, GateRule::TrendAbove(0.55), 390, stats.clone());
        assert!(run(&mut g, &d).is_empty());
        assert_eq!(stats.blocked.load(Ordering::Relaxed), 1);
        assert!(stats.blocked_fraction() > 0.99);
        let (_, p50, _) = stats.h_percentiles();
        assert!(p50 < 0.3, "zigzag H {p50}");
    }

    #[test]
    fn blocked_flip_closes_the_open_leg() {
        // Allowed long during warm-up, then a flip to short once the window is
        // full on anti-persistent data ⇒ wrapper emits flat, not short.
        let zig: Vec<f64> = (0..400).map(|i| if i % 2 == 0 { 100.0 } else { 100.1 }).collect();
        let d = day(&zig);
        let stats = Arc::new(GateStats::default());
        let mut g = HurstGated::new(Scripted { plan: vec![(29, 1.0), (395, -1.0)] }, GateRule::TrendAbove(0.55), 390, stats);
        assert_eq!(run(&mut g, &d), vec![(29, 1.0), (395, 0.0)]);
    }
}
