//! Risk Overlay as a state machine: Normal → Half → Halted, driven by
//! realised P/L against daily / weekly / monthly loss budgets and gross
//! exposure caps. Pure; the host feeds equity marks and dates.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RiskParams {
    pub daily_loss_pct: f64,
    pub weekly_loss_pct: f64,
    pub monthly_loss_pct: f64,
    pub gross_intraday_max: f64,
    pub gross_overnight_max: f64,
    pub per_symbol_max_frac: f64,
    /// R6 (2026-09-27, D2): **portfolio** overnight gross cap as a fraction
    /// of equity, allocated in decision order across symbols. The
    /// per-symbol rail above let two full B legs reach 2.0× (Reg-T edge);
    /// 1.0× halves the first-gap loss (−$5 669 → −$3 145 on the archive)
    /// for −14 % of $/yr. `serde(default)` keeps old settings valid.
    pub gross_overnight_portfolio_max: f64,
}

impl Default for RiskParams {
    fn default() -> Self {
        Self {
            daily_loss_pct: 0.015,
            weekly_loss_pct: 0.04,
            monthly_loss_pct: 0.08,
            gross_intraday_max: 2.5,
            gross_overnight_max: 1.0,
            per_symbol_max_frac: 1.5,
            gross_overnight_portfolio_max: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskState {
    Normal,
    /// New exposure scaled by 50 % until the given date (exclusive).
    Half,
    /// No new exposure for the rest of the day; existing positions flatten.
    Halted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskOverlay {
    pub params: RiskParams,
    pub state: RiskState,
    day_start_equity: f64,
    week_start_equity: f64,
    month_start_equity: f64,
    current: Option<NaiveDate>,
    pub halts: u32,
    /// Denominator for the loss percentages. `None` = the period's start
    /// equity (compounding). `Some(base)` = a fixed capital base, so a
    /// fixed-sizing run has fixed $ budgets — the live rail measures the
    /// account, whose equity stays close to the sizing capital.
    fixed_base: Option<f64>,
    /// Overnight gross already granted today across symbols (R6 portfolio cap).
    night_used: f64,
}

fn iso_week(d: NaiveDate) -> (i32, u32) {
    let w = d.iso_week();
    (w.year(), w.week())
}

impl RiskOverlay {
    pub fn new(params: RiskParams, equity: f64) -> Self {
        Self {
            params,
            state: RiskState::Normal,
            day_start_equity: equity,
            week_start_equity: equity,
            month_start_equity: equity,
            current: None,
            halts: 0,
            fixed_base: None,
            night_used: 0.0,
        }
    }

    /// Loss budgets as fractions of a fixed capital `base` (see `fixed_base`).
    pub fn with_fixed_base(mut self, base: f64) -> Self {
        self.fixed_base = (base > 0.0).then_some(base);
        self
    }

    fn loss(&self, equity: f64, start: f64) -> f64 {
        match self.fixed_base {
            Some(b) => (equity - start) / b,
            None => equity / start - 1.0,
        }
    }

    /// Roll calendars at the start of `date`. Daily halt clears; weekly /
    /// monthly Half persists until the corresponding period rolls.
    pub fn start_day(&mut self, date: NaiveDate, equity: f64) {
        let new_week = self.current.map(|c| iso_week(c) != iso_week(date)).unwrap_or(true);
        let new_month = self
            .current
            .map(|c| (c.year(), c.month()) != (date.year(), date.month()))
            .unwrap_or(true);
        if new_week {
            self.week_start_equity = equity;
        }
        if new_month {
            self.month_start_equity = equity;
        }
        self.day_start_equity = equity;
        self.current = Some(date);
        self.night_used = 0.0;
        self.state = self.period_state(equity);
    }

    fn period_state(&self, equity: f64) -> RiskState {
        let w = self.loss(equity, self.week_start_equity);
        let m = self.loss(equity, self.month_start_equity);
        if w <= -self.params.weekly_loss_pct || m <= -self.params.monthly_loss_pct {
            RiskState::Half
        } else {
            RiskState::Normal
        }
    }

    /// Mark-to-market update during the day. Returns the state after.
    pub fn on_mark(&mut self, equity: f64) -> RiskState {
        if self.state == RiskState::Halted {
            return self.state;
        }
        let d = self.loss(equity, self.day_start_equity);
        if d <= -self.params.daily_loss_pct {
            self.state = RiskState::Halted;
            self.halts += 1;
        }
        self.state
    }

    /// Multiplier applied to *new* exposure requests.
    pub fn exposure_scale(&self) -> f64 {
        match self.state {
            RiskState::Normal => 1.0,
            RiskState::Half => 0.5,
            RiskState::Halted => 0.0,
        }
    }

    /// Clamp a net exposure (fraction of equity, signed) to the caps.
    pub fn clamp_exposure(&mut self, exposure: f64, overnight: bool) -> f64 {
        self.clamp_member_exposure(exposure, 0.0, overnight)
    }

    /// Clamp one ensemble member's exposure so that the **sleeve's** net
    /// exposure in the symbol (`others` = the other members' current
    /// exposure, signed) stays within the cap. The cap is a per-symbol
    /// rail: applying it per member let a 3-member ensemble reach 3 × L/3 =
    /// L = 2.0× while a single instance was held at 1.5× (found 2026-09-19,
    /// ANALIZA chat 5 L23). The last member to enter gets the remaining room.
    pub fn clamp_member_exposure(&mut self, member: f64, others: f64, overnight: bool) -> f64 {
        let cap = if overnight { self.params.gross_overnight_max } else { self.params.gross_intraday_max };
        let cap = cap.min(self.params.per_symbol_max_frac);
        if member == 0.0 {
            return 0.0;
        }
        let total = (others + member).clamp(-cap, cap);
        let mut m = total - others;
        // never turn a request into its opposite (others already at/over the cap)
        if m * member < 0.0 {
            return 0.0;
        }
        if overnight {
            // R6 portfolio rail: whatever this symbol adds to tonight's gross
            // must fit in the room left by the symbols decided before it.
            let room = (self.params.gross_overnight_portfolio_max - self.night_used).max(0.0);
            let add = ((others + m).abs() - others.abs()).max(0.0);
            if add > room {
                let allowed = others.abs() + room;
                m = (others + m).signum() * allowed - others;
                if m * member < 0.0 {
                    return 0.0;
                }
            }
            self.night_used += ((others + m).abs() - others.abs()).max(0.0);
        }
        m
    }

    /// Overnight gross granted so far today (fraction of equity).
    pub fn night_used(&self) -> f64 {
        self.night_used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn daily_halt_and_reset_next_day() {
        let mut r = RiskOverlay::new(RiskParams::default(), 100_000.0);
        r.start_day(d(2026, 3, 2), 100_000.0);
        assert_eq!(r.on_mark(99_000.0), RiskState::Normal);
        assert_eq!(r.on_mark(98_400.0), RiskState::Halted);
        assert_eq!(r.exposure_scale(), 0.0);
        assert_eq!(r.on_mark(99_900.0), RiskState::Halted, "halt is sticky for the day");
        r.start_day(d(2026, 3, 3), 98_400.0);
        assert_eq!(r.state, RiskState::Normal);
        assert_eq!(r.halts, 1);
    }

    #[test]
    fn weekly_loss_enters_half_until_week_rolls() {
        let mut r = RiskOverlay::new(RiskParams::default(), 100_000.0);
        r.start_day(d(2026, 3, 2), 100_000.0); // Monday
        r.start_day(d(2026, 3, 4), 95_900.0); // −4.1 % on the week
        assert_eq!(r.state, RiskState::Half);
        assert_eq!(r.exposure_scale(), 0.5);
        r.start_day(d(2026, 3, 9), 95_900.0); // next Monday: week resets
        assert_eq!(r.state, RiskState::Normal);
    }

    #[test]
    fn monthly_loss_half_persists_across_weeks() {
        let mut r = RiskOverlay::new(RiskParams::default(), 100_000.0);
        r.start_day(d(2026, 3, 2), 100_000.0);
        r.start_day(d(2026, 3, 16), 91_000.0); // −9 % month
        assert_eq!(r.state, RiskState::Half);
        r.start_day(d(2026, 3, 23), 91_500.0);
        assert_eq!(r.state, RiskState::Half, "still −8.5 % on the month");
        r.start_day(d(2026, 4, 1), 91_500.0);
        assert_eq!(r.state, RiskState::Normal);
    }

    #[test]
    fn fixed_base_keeps_dollar_budgets_constant() {
        // compounded to $400k but sized on $100k: a −$1 500 day is still a halt
        let mut r = RiskOverlay::new(RiskParams::default(), 100_000.0).with_fixed_base(100_000.0);
        r.start_day(d(2026, 3, 2), 400_000.0);
        assert_eq!(r.on_mark(398_600.0), RiskState::Normal);
        assert_eq!(r.on_mark(398_500.0), RiskState::Halted);
        let mut c = RiskOverlay::new(RiskParams::default(), 100_000.0);
        c.start_day(d(2026, 3, 2), 400_000.0);
        assert_eq!(c.on_mark(398_500.0), RiskState::Normal, "compounding: −0.375 % is not a halt");
    }

    #[test]
    fn exposure_caps() {
        let mut r = RiskOverlay::new(RiskParams::default(), 1.0);
        assert_eq!(r.clamp_exposure(3.0, false), 1.5, "per-symbol cap binds first");
        assert_eq!(r.clamp_exposure(-3.0, true), -1.0);
        // R6: the first symbol used the whole 1.0× portfolio night budget
        assert_eq!(r.clamp_exposure(0.7, true), 0.0, "portfolio overnight cap binds across symbols");
        r.start_day(d(2026, 3, 3), 1.0);
        assert_eq!(r.clamp_exposure(0.7, true), 0.7, "budget resets at the start of the day");
    }

    /// R6 portfolio overnight cap (D2, 2026-09-27): allocated in decision
    /// order, exits untouched, intraday untouched, reset daily. Mirrors
    /// `titan_frontier::diag::PortfolioNightCap`, whose archive run gave
    /// 17 498 fills / $39 916 / Sharpe 1.332 — the engine must reproduce it.
    #[test]
    fn portfolio_overnight_cap_allocates_in_decision_order() {
        let mut r = RiskOverlay::new(RiskParams::default(), 1.0);
        r.start_day(d(2026, 3, 2), 1.0);
        // SPY's B asks 0.8 ⇒ granted; QQQ's B asks 0.9 ⇒ only the remaining 0.2
        assert!((r.clamp_member_exposure(0.8, 0.0, true) - 0.8).abs() < 1e-12);
        assert!((r.night_used() - 0.8).abs() < 1e-12);
        assert!((r.clamp_member_exposure(0.9, 0.0, true) - 0.2).abs() < 1e-12);
        assert!((r.night_used() - 1.0).abs() < 1e-12);
        // a third request gets nothing, never a reversal
        assert_eq!(r.clamp_member_exposure(0.5, 0.0, true), 0.0);
        // intraday requests are not charged to the night budget
        assert!((r.clamp_member_exposure(1.2, 0.0, false) - 1.2).abs() < 1e-12);
        assert!((r.night_used() - 1.0).abs() < 1e-12);
        // exits (0) pass and do not consume budget
        assert_eq!(r.clamp_member_exposure(0.0, 0.8, true), 0.0);
        // short side symmetric
        r.start_day(d(2026, 3, 3), 1.0);
        assert!((r.clamp_member_exposure(-0.6, 0.0, true) + 0.6).abs() < 1e-12);
        assert!((r.clamp_member_exposure(-0.6, 0.0, true) + 0.4).abs() < 1e-12);
        // a wider portfolio cap restores the pre-R6 behaviour (two full legs)
        let mut wide = RiskOverlay::new(RiskParams { gross_overnight_portfolio_max: 2.0, ..RiskParams::default() }, 1.0);
        wide.start_day(d(2026, 3, 2), 1.0);
        assert!((wide.clamp_member_exposure(1.0, 0.0, true) - 1.0).abs() < 1e-12);
        assert!((wide.clamp_member_exposure(1.0, 0.0, true) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn member_clamp_caps_the_sleeve_total_not_the_member() {
        let mut r = RiskOverlay::new(RiskParams::default(), 1.0);
        // two members already long 2/3 each (1.333); the third asks 2/3 ⇒ gets the room to 1.5
        let third = r.clamp_member_exposure(2.0 / 3.0, 4.0 / 3.0, false);
        assert!((third - (1.5 - 4.0 / 3.0)).abs() < 1e-12, "{third}");
        // nothing else held ⇒ unchanged
        assert!((r.clamp_member_exposure(2.0 / 3.0, 0.0, false) - 2.0 / 3.0).abs() < 1e-12);
        // a flip against the others reduces the net ⇒ never clamped
        assert!((r.clamp_member_exposure(-2.0 / 3.0, 4.0 / 3.0, false) + 2.0 / 3.0).abs() < 1e-12);
        // exit (0) with the others at the cap ⇒ stays 0
        assert_eq!(r.clamp_member_exposure(0.0, 1.5, false), 0.0);
        // symmetric on the short side
        let s = r.clamp_member_exposure(-2.0 / 3.0, -4.0 / 3.0, false);
        assert!((s + (1.5 - 4.0 / 3.0)).abs() < 1e-12, "{s}");
        // room smaller than the request ⇒ the remainder; no room at all ⇒ 0, never a reversal
        assert!((r.clamp_member_exposure(0.5, 1.4, false) - 0.1).abs() < 1e-12);
        assert_eq!(r.clamp_member_exposure(0.5, 1.6, false), 0.0);
        assert_eq!(r.clamp_member_exposure(0.0, 1.6, false), 0.0);
    }
}
