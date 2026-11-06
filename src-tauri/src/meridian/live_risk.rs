//! Live risk rail on **broker equity** (real money), independent of the
//! engine's simulated curve.
//!
//! The replay's risk overlay marks the *simulated* book (a $100k start on
//! the cache, compounding to ≈$117k by 2026-09-18 while the account held
//! $98k), so its daily-loss threshold was 20 % looser than intended and its
//! weekly/monthly states came from a curve the account never had. This
//! module evaluates the same budgets (`RiskParams`) on the account itself:
//!
//! * daily: `equity / last_equity − 1 ≤ −daily_loss_pct` ⇒ **Halted** for
//!   the rest of the session — A is flattened now, B takes no carry tonight;
//! * weekly / monthly: `last_equity` vs. the equity at the last close before
//!   the current ISO week / calendar month ⇒ **Half** — every desired share
//!   count is halved for the day (constant intraday, decided from closes).
//!
//! Pure; the driver feeds the marks and keeps the day's halt sticky.

use chrono::{Datelike, NaiveDate};
use serde::Serialize;
use titan_meridian::risk::RiskParams;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccountMarks {
    /// Current account equity.
    pub equity: f64,
    /// Equity at the previous session's close (Alpaca `last_equity`).
    pub last_equity: f64,
    /// Equity at the last close before the current ISO week began.
    pub week_start: Option<f64>,
    /// Equity at the last close before the current calendar month began.
    pub month_start: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum LiveRisk {
    Normal,
    /// New and existing exposure halved for the day.
    Half { reason: String },
    /// Flat now, nothing new today.
    Halted { reason: String },
}

fn pct(a: f64, b: f64) -> Option<f64> {
    if b > 0.0 && a.is_finite() { Some(a / b - 1.0) } else { None }
}

/// Evaluate the budgets on the account. Daily loss dominates.
pub fn evaluate(m: &AccountMarks, p: &RiskParams) -> LiveRisk {
    if let Some(d) = pct(m.equity, m.last_equity) {
        if d <= -p.daily_loss_pct {
            return LiveRisk::Halted { reason: format!("day {:+.2} % ≤ −{:.1} % (equity {:.0} vs last close {:.0})", d * 100.0, p.daily_loss_pct * 100.0, m.equity, m.last_equity) };
        }
    }
    if let Some(w) = m.week_start.and_then(|s| pct(m.last_equity, s)) {
        if w <= -p.weekly_loss_pct {
            return LiveRisk::Half { reason: format!("week {:+.2} % ≤ −{:.1} %", w * 100.0, p.weekly_loss_pct * 100.0) };
        }
    }
    if let Some(mo) = m.month_start.and_then(|s| pct(m.last_equity, s)) {
        if mo <= -p.monthly_loss_pct {
            return LiveRisk::Half { reason: format!("month {:+.2} % ≤ −{:.1} %", mo * 100.0, p.monthly_loss_pct * 100.0) };
        }
    }
    LiveRisk::Normal
}

/// Desired (net, A, B) shares after the rail. Halted ⇒ flat; Half ⇒ halved
/// (whole shares, toward zero); Normal ⇒ unchanged.
pub fn apply(state: &LiveRisk, desired_net: f64, a: f64, b: f64) -> (f64, f64, f64) {
    match state {
        LiveRisk::Normal => (desired_net, a, b),
        LiveRisk::Half { .. } => {
            let h = |x: f64| (x / 2.0).trunc();
            (h(a) + h(b), h(a), h(b))
        }
        LiveRisk::Halted { .. } => (0.0, 0.0, 0.0),
    }
}

/// Equity at the last close strictly before `boundary` from a daily
/// `(date, equity)` history (ascending or not).
pub fn equity_before(history: &[(NaiveDate, f64)], boundary: NaiveDate) -> Option<f64> {
    history.iter().filter(|(d, e)| *d < boundary && e.is_finite() && *e > 0.0).max_by_key(|(d, _)| *d).map(|(_, e)| *e)
}

/// Week / month start marks for `today` from the daily equity history.
pub fn period_marks(history: &[(NaiveDate, f64)], today: NaiveDate) -> (Option<f64>, Option<f64>) {
    let monday = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64);
    let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);
    (equity_before(history, monday), equity_before(history, first))
}

/// Account marks for `today`: equity / last close from `/v2/account`, week
/// and month starts from the daily portfolio history.
pub async fn fetch_marks(broker: &super::broker::Broker<'_>, today: NaiveDate) -> Result<AccountMarks, String> {
    let (equity, last_equity) = broker.account().await?;
    let (week_start, month_start) = match broker.portfolio_history().await {
        Ok(v) => period_marks(&parse_history(&v), today),
        Err(e) => {
            tracing::warn!(component = "meridian", "portfolio history unavailable — weekly/monthly budgets skipped: {e}");
            (None, None)
        }
    };
    Ok(AccountMarks { equity, last_equity, week_start, month_start })
}

/// Parse `GET /v2/account/portfolio/history?timeframe=1D` into `(ET date, equity)`.
pub fn parse_history(v: &serde_json::Value) -> Vec<(NaiveDate, f64)> {
    let ts = v.get("timestamp").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let eq = v.get("equity").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    ts.iter()
        .zip(eq.iter())
        .filter_map(|(t, e)| {
            let secs = t.as_i64()?;
            let e = e.as_f64()?;
            let utc = chrono::DateTime::from_timestamp(secs, 0)?;
            Some((titan_meridian::session::et_date(utc), e))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default()
    }

    #[test]
    fn daily_loss_halts_on_real_equity() {
        let p = RiskParams::default();
        let ok = AccountMarks { equity: 98_000.0, last_equity: 98_500.0, week_start: None, month_start: None };
        assert_eq!(evaluate(&ok, &p), LiveRisk::Normal);
        let bad = AccountMarks { equity: 96_900.0, last_equity: 98_500.0, week_start: None, month_start: None };
        assert!(matches!(evaluate(&bad, &p), LiveRisk::Halted { .. }), "−1.62 % on the day");
        // the threshold is on the *account*: −$1 472 on $98 153, not −$1 764 on a simulated $117k
        let edge = AccountMarks { equity: 98_153.0 * (1.0 - 0.015), last_equity: 98_153.0, week_start: None, month_start: None };
        assert!(matches!(evaluate(&edge, &p), LiveRisk::Halted { .. }));
    }

    #[test]
    fn weekly_and_monthly_budgets_halve_from_closes_only() {
        let p = RiskParams::default();
        let m = AccountMarks { equity: 100_000.0, last_equity: 95_900.0, week_start: Some(100_000.0), month_start: Some(100_000.0) };
        assert!(matches!(evaluate(&m, &p), LiveRisk::Half { ref reason } if reason.starts_with("week")));
        let m2 = AccountMarks { equity: 92_000.0, last_equity: 91_900.0, week_start: Some(92_000.0), month_start: Some(100_000.0) };
        assert!(matches!(evaluate(&m2, &p), LiveRisk::Half { ref reason } if reason.starts_with("month")));
        // intraday equity does not feed the period budgets (decided from closes)
        let m3 = AccountMarks { equity: 90_000.0, last_equity: 99_000.0, week_start: Some(100_000.0), month_start: None };
        assert!(matches!(evaluate(&m3, &p), LiveRisk::Halted { .. }), "daily dominates anyway");
        let m4 = AccountMarks { equity: 98_600.0, last_equity: 99_000.0, week_start: Some(100_000.0), month_start: None };
        assert_eq!(evaluate(&m4, &p), LiveRisk::Normal);
        let none = AccountMarks { equity: 0.0, last_equity: 0.0, week_start: None, month_start: None };
        assert_eq!(evaluate(&none, &p), LiveRisk::Normal, "no marks ⇒ no verdict (fail open, logged by the driver)");
    }

    #[test]
    fn apply_flattens_or_halves_whole_shares() {
        assert_eq!(apply(&LiveRisk::Normal, -197.0, -197.0, 0.0), (-197.0, -197.0, 0.0));
        assert_eq!(apply(&LiveRisk::Halted { reason: String::new() }, -197.0, -197.0, 0.0), (0.0, 0.0, 0.0));
        assert_eq!(apply(&LiveRisk::Half { reason: String::new() }, -127.0, -197.0, 70.0), (-63.0, -98.0, 35.0));
    }

    #[test]
    fn period_marks_pick_the_last_close_before_the_boundary() {
        // 2026-09-18 is a Friday; Monday 09-14; month starts 09-01 (Tuesday)
        let h = vec![(d(2026, 8, 31), 100_000.0), (d(2026, 9, 1), 100_500.0), (d(2026, 9, 11), 99_000.0), (d(2026, 9, 14), 98_800.0), (d(2026, 9, 17), 98_300.0)];
        let (w, m) = period_marks(&h, d(2026, 9, 18));
        assert_eq!(w, Some(99_000.0), "Friday 09-11 close");
        assert_eq!(m, Some(100_000.0), "08-31 close");
        assert_eq!(period_marks(&h[3..], d(2026, 9, 18)), (None, None));
        assert_eq!(equity_before(&[(d(2026, 9, 11), 0.0)], d(2026, 9, 14)), None, "zero equity is not a mark");
    }

    #[test]
    fn parses_portfolio_history_to_et_dates() {
        // 2026-09-17 20:00:00Z = 16:00 ET
        let v = serde_json::json!({"timestamp": [1_789_675_200_i64, 1_789_761_600_i64], "equity": [98_300.5, 98_153.02], "timeframe": "1D"});
        let h = parse_history(&v);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].0, d(2026, 9, 17));
        assert!((h[1].1 - 98_153.02).abs() < 1e-9);
        assert!(parse_history(&serde_json::json!({})).is_empty());
    }
}
