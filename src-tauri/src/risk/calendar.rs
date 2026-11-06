//! NYSE trading calendar — single source of truth for "is the
//! market open today" questions.
//!
//! ## Why this lives in `risk/`
//! The first consumer was the `daily_reset` loop (which must skip
//! weekends/holidays). S19 (May 2026) added the auto-scanner and
//! EOD-flush loops as second consumers — both must agree with
//! `daily_reset` about which days are "trading days" or they
//! drift out of sync (e.g. scanner runs Memorial Day, gets stale
//! quotes, promotes garbage, then daily_reset skips Monday and
//! the bot trades the stale watchlist on Tuesday).
//!
//! ## Holiday source
//! Hardcoded from the NYSE official calendar. The 2026 + 2027
//! lists are exhaustive for full-day closures. Early closes
//! (1pm ET on day before some holidays) are NOT modelled here
//! — the bot's `flat_by_close` and the operator's discretion
//! cover those cases.
//!
//! ## Future work
//! Wire to Alpaca's `GET /v2/calendar` so the list refreshes
//! automatically. Hardcoded today because NYSE publishes the
//! calendar years in advance and drift risk is zero between
//! updates. See discussion in `daily_reset.rs` for rationale.

use chrono::{Datelike, NaiveDate};

/// NYSE full-day market closures for 2026.
pub const NYSE_HOLIDAYS_2026: &[&str] = &[
    "2026-01-01", // New Year's Day
    "2026-01-19", // Martin Luther King Jr. Day
    "2026-02-16", // Presidents' Day
    "2026-04-03", // Good Friday
    "2026-05-25", // Memorial Day
    "2026-06-19", // Juneteenth
    "2026-07-03", // Independence Day (observed Fri, since 7/4 = Sat)
    "2026-09-07", // Labor Day
    "2026-11-26", // Thanksgiving Day
    "2026-12-25", // Christmas Day
];

/// NYSE full-day market closures for 2027.
pub const NYSE_HOLIDAYS_2027: &[&str] = &[
    "2027-01-01", // New Year's Day
    "2027-01-18", // Martin Luther King Jr. Day
    "2027-02-15", // Presidents' Day
    "2027-03-26", // Good Friday
    "2027-05-31", // Memorial Day
    "2027-06-18", // Juneteenth (observed Fri, since 6/19 = Sat)
    "2027-07-05", // Independence Day (observed Mon, since 7/4 = Sun)
    "2027-09-06", // Labor Day
    "2027-11-25", // Thanksgiving Day
    "2027-12-24", // Christmas Day (observed Fri, since 12/25 = Sat)
];

/// `true` if `date` is a full-day NYSE closure (any of the
/// holiday lists above). Returns `false` for unknown years —
/// the caller should treat unknown years as "trading days"
/// to avoid silently shutting the bot down past the calendar
/// horizon. Update the lists annually as part of year-end
/// maintenance.
pub fn is_nyse_holiday(date: NaiveDate) -> bool {
    let s = date.format("%Y-%m-%d").to_string();
    NYSE_HOLIDAYS_2026.contains(&s.as_str())
        || NYSE_HOLIDAYS_2027.contains(&s.as_str())
}

/// `true` if `date` is a weekday AND not on the holiday list.
/// This is the canonical "should the bot operate today?" check
/// — use it from any timing gate (scanner, EOD flush, reset).
pub fn is_nyse_trading_day(date: NaiveDate) -> bool {
    let wd = date.weekday();
    if matches!(wd, chrono::Weekday::Sat | chrono::Weekday::Sun) {
        return false;
    }
    !is_nyse_holiday(date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid date")
    }

    #[test]
    fn detects_2026_full_day_holidays() {
        assert!(is_nyse_holiday(d(2026, 1, 1)), "New Year's Day");
        assert!(is_nyse_holiday(d(2026, 5, 25)), "Memorial Day");
        assert!(is_nyse_holiday(d(2026, 7, 3)), "Independence Day observed");
        assert!(is_nyse_holiday(d(2026, 11, 26)), "Thanksgiving");
        assert!(is_nyse_holiday(d(2026, 12, 25)), "Christmas Day");
    }

    #[test]
    fn detects_2027_full_day_holidays() {
        assert!(is_nyse_holiday(d(2027, 1, 1)));
        assert!(is_nyse_holiday(d(2027, 11, 25)), "Thanksgiving 2027");
    }

    #[test]
    fn rejects_non_holiday_weekdays() {
        // 2026-05-18 is a Monday, no holiday.
        assert!(!is_nyse_holiday(d(2026, 5, 18)));
        // 2026-12-24 is a Thursday — NYSE has early-close at 1pm
        // but is NOT a full-day closure; we don't model early
        // closes in this calendar.
        assert!(!is_nyse_holiday(d(2026, 12, 24)));
    }

    #[test]
    fn rejects_unknown_year_silently() {
        // Year past calendar horizon — defaults to false so the
        // bot keeps running. Operator must update the lists.
        assert!(!is_nyse_holiday(d(2030, 7, 4)));
    }

    #[test]
    fn trading_day_combines_weekend_and_holiday_checks() {
        // Normal Monday.
        assert!(is_nyse_trading_day(d(2026, 5, 18)));
        // Memorial Day 2026 (Monday) — holiday.
        assert!(!is_nyse_trading_day(d(2026, 5, 25)));
        // Saturday 2026-05-23 — weekend.
        assert!(!is_nyse_trading_day(d(2026, 5, 23)));
        // Sunday 2026-05-24 — weekend.
        assert!(!is_nyse_trading_day(d(2026, 5, 24)));
    }
}
