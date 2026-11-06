//! MERIDIAN C-PM live book — pure state around `titan_meridian`'s
//! `PmTracker`: one tracker per (symbol, session) armed by a classified
//! headline, fed by completed premarket bars, producing `PmSignal`s.
//!
//! No I/O, no locks — `premarket_live.rs` owns the plumbing. Rules
//! (plan §3.5-PM): first tradeable headline per symbol-day arms the
//! tracker; an excluded-class headline (offering, miss, CRL, halt)
//! disarms it even if already armed; nothing arms after `entry_last_idx`.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDate;
use serde::Serialize;
use titan_meridian::catalyst::CatalystClass;
use titan_meridian::sleeves::{ExitReason, ExtBar, Fill, PmParams, PmTracker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SignalKind {
    Entry,
    Exit(ExitReason),
}

#[derive(Debug, Clone, Serialize)]
pub struct PmSignal {
    pub symbol: String,
    pub class: CatalystClass,
    pub headline: String,
    pub url: String,
    pub kind: SignalKind,
    pub bar_idx: usize,
    pub event_idx: usize,
    pub price: f64,
    /// Signed shares of the fill (+ buy / − sell).
    pub shares: f64,
    pub prev_close: f64,
    pub change_pct: f64,
    pub cum_dollar: f64,
}

/// A classified headline as seen by the book.
#[derive(Debug, Clone)]
pub struct Headline {
    pub date: NaiveDate,
    /// Extended-session minute index of publication (0 = 04:00 ET).
    pub idx: usize,
    pub symbol: String,
    pub class: CatalystClass,
    pub headline: String,
    pub url: String,
    /// Previous session close (ignored for excluded classes).
    pub prev_close: f64,
}

struct Armed {
    tracker: PmTracker,
    class: CatalystClass,
    headline: String,
    url: String,
    cum_dollar: f64,
}

pub struct PmLiveBook {
    pub params: PmParams,
    pub equity: f64,
    date: Option<NaiveDate>,
    armed: HashMap<String, Armed>,
    /// (symbol) already armed today — one headline per symbol-day.
    seen: HashSet<String>,
    /// Symbols disarmed today by an excluded-class headline.
    banned: HashSet<String>,
}

impl PmLiveBook {
    pub fn new(params: PmParams, equity: f64) -> Self {
        Self { params, equity, date: None, armed: HashMap::new(), seen: HashSet::new(), banned: HashSet::new() }
    }

    fn roll(&mut self, date: NaiveDate) {
        if self.date != Some(date) {
            self.date = Some(date);
            self.armed.clear();
            self.seen.clear();
            self.banned.clear();
        }
    }

    /// Arm / disarm from a classified headline. Returns the outcome
    /// (diagnostics / tests).
    pub fn on_headline(&mut self, h: &Headline) -> &'static str {
        let Headline { date, idx, class, prev_close, .. } = *h;
        self.roll(date);
        let symbol = h.symbol.to_uppercase();
        if class.is_excluded() {
            self.banned.insert(symbol.clone());
            return if self.armed.remove(&symbol).is_some() { "disarmed" } else { "banned" };
        }
        if !class.is_tradeable_long() {
            return "class not tradeable";
        }
        if self.banned.contains(&symbol) {
            return "symbol banned today";
        }
        if idx > self.params.entry_last_idx {
            return "after entry window";
        }
        if prev_close <= 0.0 || prev_close.is_nan() {
            return "no previous close";
        }
        if !self.seen.insert(symbol.clone()) {
            return "already armed today";
        }
        self.armed.insert(
            symbol,
            Armed { tracker: PmTracker::new(self.params, idx, prev_close, self.equity), class, headline: h.headline.clone(), url: h.url.clone(), cum_dollar: 0.0 },
        );
        "armed"
    }

    pub fn armed_symbols(&self) -> Vec<String> {
        let mut v: Vec<String> = self.armed.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn is_long(&self, symbol: &str) -> bool {
        self.armed.get(symbol).map(|a| a.tracker.is_long()).unwrap_or(false)
    }

    /// Feed completed bars for one symbol; returns the signals produced.
    pub fn on_bars(&mut self, symbol: &str, bars: &[ExtBar]) -> Vec<PmSignal> {
        let Some(a) = self.armed.get_mut(symbol) else { return Vec::new() };
        let mut out = Vec::new();
        for b in bars {
            a.cum_dollar += b.c * b.v;
            for f in a.tracker.on_bar(b) {
                out.push(to_signal(symbol, a, &f, b));
            }
            if a.tracker.is_done() {
                break;
            }
        }
        if a.tracker.is_done() {
            self.armed.remove(symbol);
        }
        out
    }

    /// End of window: close anything still open at `last_px` (10:30 flat).
    pub fn flatten_all(&mut self, idx: usize, last_px: &HashMap<String, f64>) -> Vec<PmSignal> {
        let mut out = Vec::new();
        let syms = self.armed_symbols();
        for s in syms {
            let Some(a) = self.armed.get_mut(&s) else { continue };
            let px = last_px.get(&s).copied().unwrap_or(0.0);
            if px > 0.0 {
                if let Some(f) = a.tracker.force_close(idx, px) {
                    let b = ExtBar { idx, o: px, h: px, l: px, c: px, v: 0.0, n: 0.0 };
                    out.push(to_signal(&s, a, &f, &b));
                }
            }
            self.armed.remove(&s);
        }
        out
    }
}

fn to_signal(symbol: &str, a: &Armed, f: &Fill, b: &ExtBar) -> PmSignal {
    PmSignal {
        symbol: symbol.to_string(),
        class: a.class,
        headline: a.headline.clone(),
        url: a.url.clone(),
        kind: f.reason.map(SignalKind::Exit).unwrap_or(SignalKind::Entry),
        bar_idx: f.idx,
        event_idx: a.tracker.event_idx,
        price: f.price,
        shares: f.shares,
        prev_close: a.tracker.prev_close,
        change_pct: (b.c / a.tracker.prev_close - 1.0) * 100.0,
        cum_dollar: a.cum_dollar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 3).unwrap()
    }
    fn bar(idx: usize, o: f64, h: f64, l: f64, c: f64, v: f64, n: f64) -> ExtBar {
        ExtBar { idx, o, h, l, c, v, n }
    }
    fn hl(date: NaiveDate, idx: usize, symbol: &str, class: CatalystClass, headline: &str, prev_close: f64) -> Headline {
        Headline { date, idx, symbol: symbol.into(), class, headline: headline.into(), url: "u".into(), prev_close }
    }

    #[test]
    fn arms_once_per_symbol_day_and_signals_entry() {
        let mut book = PmLiveBook::new(PmParams::default(), 100_000.0);
        assert_eq!(book.on_headline(&hl(d(), 105, "dlth", CatalystClass::EarningsBeat, "beat", 3.0)), "armed");
        assert_eq!(book.on_headline(&hl(d(), 106, "DLTH", CatalystClass::Contract, "x", 3.0)), "already armed today");
        assert_eq!(book.on_headline(&hl(d(), 106, "ABC", CatalystClass::Analyst, "x", 3.0)), "class not tradeable");
        assert_eq!(book.on_headline(&hl(d(), 340, "LATE", CatalystClass::EarningsBeat, "x", 3.0)), "after entry window");
        assert_eq!(book.on_headline(&hl(d(), 106, "NOPC", CatalystClass::EarningsBeat, "x", 0.0)), "no previous close");
        assert_eq!(book.armed_symbols(), vec!["DLTH"]);
        let sig = book.on_bars("DLTH", &[bar(100, 3.0, 3.0, 3.0, 3.0, 1000.0, 5.0), bar(109, 3.2, 3.5, 3.18, 3.45, 200_000.0, 300.0)]);
        assert_eq!(sig.len(), 1);
        assert_eq!(sig[0].kind, SignalKind::Entry);
        assert!(sig[0].shares > 0.0 && sig[0].price > 3.45);
        assert!((sig[0].change_pct - 15.0).abs() < 1e-9);
        assert!(book.is_long("DLTH"));
        // exit via stop removes the tracker
        let sig = book.on_bars("DLTH", &[bar(115, 2.5, 2.6, 2.4, 2.5, 1000.0, 10.0)]);
        assert_eq!(sig[0].kind, SignalKind::Exit(ExitReason::Stop));
        assert!(book.armed_symbols().is_empty());
    }

    #[test]
    fn offering_headline_disarms_and_bans() {
        let mut book = PmLiveBook::new(PmParams::default(), 100_000.0);
        book.on_headline(&hl(d(), 105, "X", CatalystClass::FdaPositive, "h", 5.0));
        assert_eq!(book.on_headline(&hl(d(), 110, "X", CatalystClass::Offering, "offering", 5.0)), "disarmed");
        assert!(book.armed_symbols().is_empty());
        assert_eq!(book.on_headline(&hl(d(), 111, "X", CatalystClass::EarningsBeat, "h", 5.0)), "symbol banned today");
        assert_eq!(book.on_headline(&hl(d(), 111, "Y", CatalystClass::EarningsMiss, "h", 5.0)), "banned");
    }

    #[test]
    fn day_roll_resets_and_flatten_closes_open_positions() {
        let mut book = PmLiveBook::new(PmParams::default(), 100_000.0);
        book.on_headline(&hl(d(), 105, "X", CatalystClass::Contract, "h", 3.0));
        book.on_bars("X", &[bar(109, 3.2, 3.5, 3.18, 3.45, 200_000.0, 300.0)]);
        assert!(book.is_long("X"));
        let mut px = HashMap::new();
        px.insert("X".to_string(), 3.6);
        let sig = book.flatten_all(389, &px);
        assert_eq!(sig.len(), 1);
        assert_eq!(sig[0].kind, SignalKind::Exit(ExitReason::ForcedFlat));
        assert!(book.armed_symbols().is_empty());
        let next = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        assert_eq!(book.on_headline(&hl(next, 105, "X", CatalystClass::Contract, "h", 3.0)), "armed");
    }
}
