//! Sleeve C-PM — premarket catalyst (plan §3.5-PM). Pure state machine fed
//! one extended-hours 1-min bar at a time; identical code drives the
//! historical simulation and the live loop.
//!
//! Life cycle per (symbol, day, headline):
//!   Waiting → (ignition within `window_min` of the headline) → Long
//!   Long → stop hit | 09:30 partial | VWAP trail after 09:30 | forced flat
//!
//! All fills are pessimistic: entry pays ask + slippage, stops assume the
//! stop print (or the bar open when the bar gaps through), exits pay bid.

use serde::{Deserialize, Serialize};

use crate::session::EXT_RTH_OPEN_IDX;

/// Sparse extended-hours bar (premarket has gaps — no forward fill).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtBar {
    pub idx: usize,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
    /// Number of trades in the bar.
    pub n: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PmParams {
    /// Ignition: move vs. previous close (0.05 = +5 %).
    pub min_move: f64,
    /// Ignition: move since the last print at/before the headline (0.02 =
    /// +2 %). Enforces "catalyst first, then the move" — headlines that
    /// merely report a move already made do not qualify.
    pub min_move_post: f64,
    /// Ignition: cumulative premarket dollar volume since 04:00.
    pub min_dollar_volume: f64,
    /// Ignition: cumulative premarket trade count.
    pub min_trades: f64,
    /// Ignition must occur within this many minutes after the headline.
    pub window_min: usize,
    /// Latest bar index at which an entry may occur (325 = 09:25).
    pub entry_last_idx: usize,
    /// Entry limit above the trigger close (0.005 = +0.5 %).
    pub entry_buffer: f64,
    /// Minimum stop distance as fraction of entry.
    pub min_stop_frac: f64,
    /// Risk per trade as fraction of equity.
    pub risk_frac: f64,
    /// Notional cap as fraction of equity.
    pub max_notional_frac: f64,
    pub min_price: f64,
    /// Fraction sold at the RTH open.
    pub open_exit_frac: f64,
    /// Bar index at which any remainder is flattened (389 = 10:29 close).
    pub flat_idx: usize,
    /// Per-share slippage added to every fill ($).
    pub slip_per_share: f64,
    /// Proportional slippage on every fill (0.003 = 0.3 %).
    pub slip_frac: f64,
}

impl Default for PmParams {
    fn default() -> Self {
        Self {
            min_move: 0.05,
            min_move_post: 0.02,
            min_dollar_volume: 500_000.0,
            min_trades: 200.0,
            window_min: 15,
            entry_last_idx: 325,
            entry_buffer: 0.005,
            min_stop_frac: 0.03,
            risk_frac: 0.005,
            max_notional_frac: 0.10,
            min_price: 2.0,
            open_exit_frac: 0.5,
            flat_idx: 389,
            slip_per_share: 0.01,
            slip_frac: 0.003,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    Stop,
    OpenPartial,
    VwapTrail,
    ForcedFlat,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub idx: usize,
    pub price: f64,
    /// Signed shares (+ buy, − sell).
    pub shares: f64,
    pub reason: Option<ExitReason>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Waiting,
    Long { shares: f64, stop: f64, entry: f64 },
    Done,
}

#[derive(Debug, Clone)]
pub struct PmTracker {
    pub params: PmParams,
    pub event_idx: usize,
    pub prev_close: f64,
    pub equity: f64,
    cum_dollar: f64,
    cum_trades: f64,
    post_low: f64,
    /// Last close seen at or before the headline minute.
    ref_at_event: f64,
    vwap_pv: f64,
    vwap_v: f64,
    state: State,
    /// Why the tracker ended without a trade (diagnostics).
    pub skip_reason: Option<&'static str>,
}

impl PmTracker {
    pub fn new(params: PmParams, event_idx: usize, prev_close: f64, equity: f64) -> Self {
        Self {
            params,
            event_idx,
            prev_close,
            equity,
            cum_dollar: 0.0,
            cum_trades: 0.0,
            post_low: f64::MAX,
            ref_at_event: prev_close,
            vwap_pv: 0.0,
            vwap_v: 0.0,
            state: State::Waiting,
            skip_reason: None,
        }
    }

    pub fn is_long(&self) -> bool {
        matches!(self.state, State::Long { .. })
    }
    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    fn buy_px(&self, px: f64) -> f64 {
        px * (1.0 + self.params.slip_frac) + self.params.slip_per_share
    }
    fn sell_px(&self, px: f64) -> f64 {
        (px * (1.0 - self.params.slip_frac) - self.params.slip_per_share).max(0.01)
    }

    /// Ignition test on the current cumulative state and bar.
    pub fn ignited(&self, b: &ExtBar) -> bool {
        let p = &self.params;
        b.c / self.prev_close - 1.0 >= p.min_move
            && b.c / self.ref_at_event - 1.0 >= p.min_move_post
            && self.cum_dollar >= p.min_dollar_volume
            && self.cum_trades >= p.min_trades
    }

    fn try_enter(&mut self, b: &ExtBar) -> Option<Fill> {
        let p = self.params;
        if b.idx < self.event_idx || b.idx > self.event_idx + p.window_min || b.idx > p.entry_last_idx {
            if b.idx > self.event_idx + p.window_min || b.idx > p.entry_last_idx {
                self.state = State::Done;
                self.skip_reason = Some("no ignition in window");
            }
            return None;
        }
        if !self.ignited(b) {
            return None;
        }
        if b.c < p.min_price {
            self.state = State::Done;
            self.skip_reason = Some("price below minimum");
            return None;
        }
        let entry = self.buy_px(b.c * (1.0 + p.entry_buffer));
        let stop_ref = self.post_low.min(b.l);
        let dist = (entry - stop_ref).max(p.min_stop_frac * entry);
        let stop = entry - dist;
        let by_risk = (self.equity * p.risk_frac / dist).floor();
        let by_notional = (self.equity * p.max_notional_frac / entry).floor();
        let shares = by_risk.min(by_notional);
        if shares < 1.0 {
            self.state = State::Done;
            self.skip_reason = Some("size below one share");
            return None;
        }
        self.state = State::Long { shares, stop, entry };
        Some(Fill { idx: b.idx, price: entry, shares, reason: None })
    }

    /// Feed the next bar (bars must be in increasing `idx`). Returns fills.
    pub fn on_bar(&mut self, b: &ExtBar) -> Vec<Fill> {
        let mut fills = Vec::new();
        self.cum_dollar += b.c * b.v;
        self.cum_trades += b.n;
        if b.idx <= self.event_idx {
            // the last print the market saw when the headline hit
            self.ref_at_event = b.c;
        }
        if b.idx >= self.event_idx {
            self.post_low = self.post_low.min(b.l);
        }
        if b.idx >= EXT_RTH_OPEN_IDX && b.v > 0.0 {
            self.vwap_pv += (b.h + b.l + b.c) / 3.0 * b.v;
            self.vwap_v += b.v;
        }
        match self.state {
            State::Done => {}
            State::Waiting => {
                if let Some(f) = self.try_enter(b) {
                    fills.push(f);
                }
            }
            State::Long { shares, stop, entry } => {
                let p = self.params;
                // 1. Stop first (pessimistic), gap-through at the open print.
                if b.l <= stop {
                    let px = if b.o < stop { b.o } else { stop };
                    fills.push(Fill { idx: b.idx, price: self.sell_px(px), shares: -shares, reason: Some(ExitReason::Stop) });
                    self.state = State::Done;
                    return fills;
                }
                // 2. Forced flat.
                if b.idx >= p.flat_idx {
                    fills.push(Fill { idx: b.idx, price: self.sell_px(b.c), shares: -shares, reason: Some(ExitReason::ForcedFlat) });
                    self.state = State::Done;
                    return fills;
                }
                // 3. RTH open: partial exit at the opening print.
                let mut remaining = shares;
                if b.idx == EXT_RTH_OPEN_IDX && p.open_exit_frac > 0.0 {
                    let part = (shares * p.open_exit_frac).floor().min(shares);
                    if part >= 1.0 {
                        fills.push(Fill { idx: b.idx, price: self.sell_px(b.o), shares: -part, reason: Some(ExitReason::OpenPartial) });
                        remaining -= part;
                    }
                }
                // 4. VWAP trail after the open.
                if b.idx > EXT_RTH_OPEN_IDX && remaining >= 1.0 && self.vwap_v > 0.0 {
                    let trail = stop.max(self.vwap_pv / self.vwap_v);
                    if b.c < trail {
                        fills.push(Fill { idx: b.idx, price: self.sell_px(b.c), shares: -remaining, reason: Some(ExitReason::VwapTrail) });
                        remaining = 0.0;
                    }
                }
                self.state = if remaining >= 1.0 { State::Long { shares: remaining, stop, entry } } else { State::Done };
            }
        }
        fills
    }

    /// Close whatever is still open at `px` (end of data).
    pub fn force_close(&mut self, idx: usize, px: f64) -> Option<Fill> {
        if let State::Long { shares, .. } = self.state {
            self.state = State::Done;
            return Some(Fill { idx, price: self.sell_px(px), shares: -shares, reason: Some(ExitReason::ForcedFlat) });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(idx: usize, o: f64, h: f64, l: f64, c: f64, v: f64, n: f64) -> ExtBar {
        ExtBar { idx, o, h, l, c, v, n }
    }

    /// DLTH-like day: prev close 3.00; headline at 05:45 (idx 105); quiet,
    /// then ignition at 05:49 (idx 109) to 3.45 on heavy volume.
    fn dlth() -> Vec<ExtBar> {
        vec![
            bar(100, 3.00, 3.02, 2.98, 3.00, 2_000.0, 10.0),
            bar(106, 3.00, 3.05, 2.99, 3.04, 20_000.0, 60.0),
            bar(107, 3.04, 3.20, 3.03, 3.18, 60_000.0, 120.0),
            bar(109, 3.20, 3.50, 3.18, 3.45, 120_000.0, 300.0), // ignition: +15 %, $≈600k, 490 trades
            bar(120, 3.45, 3.60, 3.40, 3.55, 50_000.0, 100.0),
            bar(330, 3.70, 3.75, 3.60, 3.65, 200_000.0, 800.0), // RTH open 3.70
            bar(331, 3.65, 3.70, 3.62, 3.68, 100_000.0, 400.0), // holds above VWAP (≈3.667)
            bar(332, 3.58, 3.60, 3.40, 3.45, 100_000.0, 400.0), // below VWAP ⇒ trail exit
        ]
    }

    #[test]
    fn dlth_enters_on_ignition_partial_at_open_trail_after() {
        let mut t = PmTracker::new(PmParams::default(), 105, 3.00, 100_000.0);
        let mut fills = Vec::new();
        for b in dlth() {
            fills.extend(t.on_bar(&b));
        }
        assert_eq!(fills[0].idx, 109, "entry at the ignition bar");
        let entry = 3.45 * 1.005 * 1.003 + 0.01;
        assert!((fills[0].price - entry).abs() < 1e-9);
        // stop: post-headline low = 2.99 → dist = entry − 2.99 ≈ 0.49 > 3 % ⇒ risk $500 / 0.49
        let dist = entry - 2.99;
        let want_shares = (500.0 / dist).floor().min((10_000.0 / entry).floor());
        assert_eq!(fills[0].shares, want_shares);
        assert_eq!(fills[1].reason, Some(ExitReason::OpenPartial));
        assert_eq!(fills[1].idx, 330);
        assert!((fills[1].price - (3.70 * 0.997 - 0.01)).abs() < 1e-9, "sold at the opening print, bid side");
        assert_eq!(fills[1].shares, -(want_shares * 0.5).floor());
        assert_eq!(fills[2].reason, Some(ExitReason::VwapTrail));
        assert_eq!(fills[2].idx, 332);
        assert!(t.is_done());
        let net: f64 = fills.iter().map(|f| f.shares).sum();
        assert_eq!(net, 0.0);
    }

    #[test]
    fn headline_reporting_an_existing_move_does_not_qualify() {
        // price already +15 % before the headline at idx 105; afterwards it
        // only drifts +1 % ⇒ post-headline move < 2 % ⇒ no entry.
        let mut t = PmTracker::new(PmParams::default(), 105, 3.00, 100_000.0);
        t.on_bar(&bar(100, 3.30, 3.50, 3.28, 3.45, 300_000.0, 400.0));
        t.on_bar(&bar(105, 3.45, 3.46, 3.44, 3.45, 100_000.0, 100.0));
        let f = t.on_bar(&bar(108, 3.45, 3.50, 3.44, 3.48, 100_000.0, 100.0));
        assert!(f.is_empty() && !t.is_long());
        // same setup but the move comes after the headline ⇒ entry
        let mut t2 = PmTracker::new(PmParams::default(), 105, 3.00, 100_000.0);
        t2.on_bar(&bar(100, 3.00, 3.02, 2.98, 3.00, 300_000.0, 400.0));
        let f2 = t2.on_bar(&bar(108, 3.05, 3.50, 3.04, 3.45, 100_000.0, 100.0));
        assert_eq!(f2.len(), 1);
    }

    #[test]
    fn no_ignition_in_window_skips() {
        let mut t = PmTracker::new(PmParams::default(), 105, 3.00, 100_000.0);
        // volume but no move
        t.on_bar(&bar(110, 3.0, 3.02, 2.99, 3.01, 500_000.0, 500.0));
        assert!(!t.is_long());
        t.on_bar(&bar(121, 3.5, 3.6, 3.4, 3.5, 500_000.0, 500.0)); // late by 1 minute
        assert!(t.is_done());
        assert_eq!(t.skip_reason, Some("no ignition in window"));
    }

    #[test]
    fn stop_hit_and_gap_through() {
        let mut t = PmTracker::new(PmParams::default(), 105, 3.00, 100_000.0);
        let mut f = t.on_bar(&bar(109, 3.20, 3.50, 3.18, 3.45, 200_000.0, 300.0));
        assert_eq!(f.len(), 1);
        let stop = fills_stop(&t);
        // gap through the stop: exit at the bar open, not at the stop
        f = t.on_bar(&bar(115, stop - 0.20, stop - 0.10, stop - 0.30, stop - 0.15, 10_000.0, 50.0));
        assert_eq!(f[0].reason, Some(ExitReason::Stop));
        assert!(f[0].price < stop - 0.20, "gap-through fills at the open, minus slippage");
        assert!(t.is_done());
    }

    fn fills_stop(t: &PmTracker) -> f64 {
        match t.state {
            State::Long { stop, .. } => stop,
            _ => panic!("not long"),
        }
    }

    #[test]
    fn forced_flat_and_force_close() {
        let mut t = PmTracker::new(PmParams { open_exit_frac: 0.0, ..Default::default() }, 105, 3.00, 100_000.0);
        t.on_bar(&bar(109, 3.20, 3.50, 3.18, 3.45, 200_000.0, 300.0));
        let f = t.on_bar(&bar(389, 3.9, 3.95, 3.85, 3.9, 1000.0, 10.0));
        assert_eq!(f[0].reason, Some(ExitReason::ForcedFlat));
        let mut t2 = PmTracker::new(PmParams::default(), 105, 3.00, 100_000.0);
        t2.on_bar(&bar(109, 3.20, 3.50, 3.18, 3.45, 200_000.0, 300.0));
        assert!(t2.force_close(200, 3.5).is_some());
        assert!(t2.force_close(201, 3.5).is_none());
    }

    #[test]
    fn min_price_and_size_guards() {
        let mut t = PmTracker::new(PmParams::default(), 10, 1.00, 100_000.0);
        t.on_bar(&bar(12, 1.0, 1.2, 1.0, 1.15, 600_000.0, 300.0));
        assert_eq!(t.skip_reason, Some("price below minimum"));
        // tiny equity ⇒ < 1 share
        let mut t = PmTracker::new(PmParams::default(), 10, 100.0, 1_000.0);
        t.on_bar(&bar(12, 100.0, 110.0, 100.0, 108.0, 600_000.0, 300.0));
        assert_eq!(t.skip_reason, Some("size below one share"));
    }
}
