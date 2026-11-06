//! Premarket tape — 1-min bar builder from SIP trades for **all** symbols,
//! 04:00–10:30 ET (MERIDIAN C-PM sensor, plan §3.5-PM / §4.5).
//!
//! Hot path (`on_trade`): one hash lookup + a handful of float ops. The
//! ET minute index is recomputed only when the UTC minute changes. Bars
//! are closed either by the first trade of a later minute or by
//! `close_stale`, which the 1 s premarket loop calls so that thin names
//! with no follow-up print still deliver their bar.
//!
//! Pure — no locks, no I/O. Lives behind `MarketDataState.premarket_tape`
//! (Tier B9); the streamer feeds it, `premarket_live` drains it.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, NaiveDate, Utc};
use titan_meridian::session::ext_index;
use titan_meridian::sleeves::ExtBar;

/// Last minute index that is still part of the C-PM window (10:29).
pub const WINDOW_LAST_IDX: usize = 389;
/// Completed bars kept per symbol when nobody drains them.
const KEEP_BARS: usize = 64;

#[derive(Debug, Clone, Default)]
struct SymTape {
    cur: Option<ExtBar>,
    done: VecDeque<ExtBar>,
    cum_dollar: f64,
    cum_trades: f64,
    last: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TapeSnapshot {
    pub last: f64,
    pub cum_dollar: f64,
    pub cum_trades: f64,
    /// Number of completed, undrained bars.
    pub pending_bars: usize,
}

#[derive(Debug, Default)]
pub struct PremarketTape {
    date: Option<NaiveDate>,
    syms: HashMap<String, SymTape>,
    /// (unix minute, resolved (date, idx)) — avoids a tz conversion per trade.
    minute_cache: Option<(i64, Option<(NaiveDate, usize)>)>,
}

impl PremarketTape {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `now` to (ET date, ext idx) inside the 04:00–10:29 window.
    fn resolve(&mut self, now: DateTime<Utc>) -> Option<(NaiveDate, usize)> {
        let m = now.timestamp().div_euclid(60);
        if let Some((cm, r)) = self.minute_cache {
            if cm == m {
                return r;
            }
        }
        let r = ext_index(now).filter(|(_, idx)| *idx <= WINDOW_LAST_IDX);
        self.minute_cache = Some((m, r));
        r
    }

    fn roll_day(&mut self, date: NaiveDate) {
        if self.date != Some(date) {
            self.date = Some(date);
            self.syms.clear();
        }
    }

    /// Feed one SIP print. Ignored outside the window.
    pub fn on_trade(&mut self, symbol: &str, price: f64, size: f64, now: DateTime<Utc>) {
        if price <= 0.0 || size <= 0.0 || price.is_nan() || size.is_nan() {
            return;
        }
        let Some((date, idx)) = self.resolve(now) else { return };
        self.roll_day(date);
        let st = self.syms.entry(symbol.to_string()).or_default();
        st.cum_dollar += price * size;
        st.cum_trades += 1.0;
        st.last = price;
        match st.cur.as_mut() {
            Some(b) if b.idx == idx => {
                b.h = b.h.max(price);
                b.l = b.l.min(price);
                b.c = price;
                b.v += size;
                b.n += 1.0;
            }
            _ => {
                if let Some(prev) = st.cur.take() {
                    push_done(&mut st.done, prev);
                }
                st.cur = Some(ExtBar { idx, o: price, h: price, l: price, c: price, v: size, n: 1.0 });
            }
        }
    }

    /// Close every in-progress bar whose minute is strictly before the
    /// current minute (call once per second from the premarket loop).
    pub fn close_stale(&mut self, now: DateTime<Utc>) {
        let cur_idx = match self.resolve(now) {
            Some((date, idx)) if self.date == Some(date) => idx,
            // Outside the window (≥ 10:30 or another day): flush everything.
            _ => usize::MAX,
        };
        for st in self.syms.values_mut() {
            if let Some(b) = st.cur {
                if b.idx < cur_idx {
                    st.cur = None;
                    push_done(&mut st.done, b);
                }
            }
        }
    }

    /// Completed bars for `symbol` in index order (removed from the tape).
    pub fn drain_bars(&mut self, symbol: &str) -> Vec<ExtBar> {
        self.syms.get_mut(symbol).map(|st| st.done.drain(..).collect()).unwrap_or_default()
    }

    pub fn snapshot(&self, symbol: &str) -> Option<TapeSnapshot> {
        self.syms.get(symbol).map(|st| TapeSnapshot {
            last: st.last,
            cum_dollar: st.cum_dollar,
            cum_trades: st.cum_trades,
            pending_bars: st.done.len(),
        })
    }

    pub fn session_date(&self) -> Option<NaiveDate> {
        self.date
    }

    pub fn len(&self) -> usize {
        self.syms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.syms.is_empty()
    }
}

fn push_done(done: &mut VecDeque<ExtBar>, bar: ExtBar) {
    if done.len() >= KEEP_BARS {
        done.pop_front();
    }
    done.push_back(bar);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 2026-09-03 (EDT): 05:45 ET = 09:45 UTC → idx 105.
    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 3, h, m, s).single().unwrap()
    }

    #[test]
    fn builds_minute_bars_and_cumulative_stats() {
        let mut t = PremarketTape::new();
        t.on_trade("DLTH", 3.00, 100.0, at(9, 45, 1));
        t.on_trade("DLTH", 3.10, 200.0, at(9, 45, 30));
        t.on_trade("DLTH", 2.95, 50.0, at(9, 45, 59));
        assert!(t.drain_bars("DLTH").is_empty(), "bar still open");
        t.on_trade("DLTH", 3.20, 10.0, at(9, 46, 0)); // closes idx 105
        let bars = t.drain_bars("DLTH");
        assert_eq!(bars.len(), 1);
        let b = bars[0];
        assert_eq!((b.idx, b.o, b.h, b.l, b.c, b.v, b.n), (105, 3.00, 3.10, 2.95, 2.95, 350.0, 3.0));
        let s = t.snapshot("DLTH").unwrap();
        assert_eq!(s.last, 3.20);
        assert!((s.cum_dollar - (300.0 + 620.0 + 147.5 + 32.0)).abs() < 1e-9);
        assert_eq!(s.cum_trades, 4.0);
        assert_eq!(t.session_date(), Some(NaiveDate::from_ymd_opt(2026, 9, 3).unwrap()));
    }

    #[test]
    fn close_stale_delivers_bars_for_thin_symbols() {
        let mut t = PremarketTape::new();
        t.on_trade("THIN", 5.0, 100.0, at(9, 45, 10));
        t.close_stale(at(9, 45, 59)); // same minute: nothing
        assert!(t.drain_bars("THIN").is_empty());
        t.close_stale(at(9, 47, 0)); // two minutes later
        let bars = t.drain_bars("THIN");
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].idx, 105);
        // after the window everything is flushed
        t.on_trade("THIN", 5.1, 1.0, at(14, 29, 0)); // 10:29 ET → idx 389, still in
        t.close_stale(at(14, 30, 0)); // 10:30 ET → flush
        assert_eq!(t.drain_bars("THIN").len(), 1);
    }

    #[test]
    fn ignores_out_of_window_and_bad_prints() {
        let mut t = PremarketTape::new();
        t.on_trade("X", 1.0, 1.0, at(7, 59, 0)); // 03:59 ET
        t.on_trade("X", 1.0, 1.0, at(14, 30, 0)); // 10:30 ET
        t.on_trade("X", 0.0, 1.0, at(9, 0, 0));
        t.on_trade("X", 1.0, 0.0, at(9, 0, 0));
        assert!(t.is_empty());
    }

    #[test]
    fn day_roll_clears_state() {
        let mut t = PremarketTape::new();
        t.on_trade("X", 1.0, 1.0, at(9, 0, 0));
        assert_eq!(t.len(), 1);
        let next = Utc.with_ymd_and_hms(2026, 9, 4, 9, 0, 0).single().unwrap();
        t.on_trade("Y", 1.0, 1.0, next);
        assert_eq!(t.len(), 1);
        assert!(t.snapshot("X").is_none());
    }

    #[test]
    fn keeps_bounded_history_when_undrained() {
        let mut t = PremarketTape::new();
        for i in 0..(KEEP_BARS as i64 + 10) {
            t.on_trade("Z", 1.0, 1.0, at(8, 0, 0) + chrono::Duration::minutes(i));
        }
        t.close_stale(at(14, 30, 0));
        assert_eq!(t.snapshot("Z").unwrap().pending_bars, KEEP_BARS);
    }
}
