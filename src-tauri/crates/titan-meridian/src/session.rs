//! ET session clock: minute index ↔ wall-clock, decision grid, half-days.
//!
//! Index 0 = the 1-min bar opening at 09:30 ET (closing 09:31). A regular
//! session has 390 bars (last index 389 opens 15:59, closes 16:00). Early
//! closes (13:00 ET) have 210 bars.

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::America::New_York;

pub const RTH_OPEN_MIN: u32 = 9 * 60 + 30;
pub const FULL_SESSION_BARS: usize = 390;
pub const HALF_SESSION_BARS: usize = 210;

/// NYSE early closes (13:00 ET), 2016 → 2027: the day after Thanksgiving,
/// and 3 July / 24 December when they fall on a weekday that is not itself
/// an observed holiday. Verified against the SIP archive (RTH prints stop
/// at 12:59, after-hours prints continue until 17:00 — which is exactly
/// why the archive loader needs this list: without it those days look like
/// full sessions whose afternoon is after-hours tape).
const EARLY_CLOSES: &[(i32, u32, u32)] = &[
    (2016, 11, 25),
    (2017, 7, 3),
    (2017, 11, 24),
    (2018, 7, 3),
    (2018, 11, 23),
    (2018, 12, 24),
    (2019, 7, 3),
    (2019, 11, 29),
    (2019, 12, 24),
    (2020, 11, 27),
    (2020, 12, 24),
    (2021, 11, 26),
    (2022, 11, 25),
    (2023, 7, 3),
    (2023, 11, 24),
    (2024, 7, 3),
    (2024, 11, 29),
    (2024, 12, 24),
    (2025, 7, 3),
    (2025, 11, 28),
    (2025, 12, 24),
    (2026, 11, 27),
    (2026, 12, 24),
    (2027, 11, 26),
];

/// Is `date` a scheduled 13:00 ET early close?
pub fn is_early_close(date: NaiveDate) -> bool {
    use chrono::Datelike;
    EARLY_CLOSES.contains(&(date.year(), date.month(), date.day()))
}

/// Scheduled RTH bars for `date` (390, or 210 on an early close). Live code
/// prefers the broker calendar; the archive loader and the replay use this.
pub fn scheduled_session_bars(date: NaiveDate) -> usize {
    if is_early_close(date) { HALF_SESSION_BARS } else { FULL_SESSION_BARS }
}

/// Convert a UTC timestamp to (ET date, minute index in RTH). `None` when
/// outside 09:30–16:00 ET.
pub fn rth_index(t_utc: DateTime<Utc>) -> Option<(NaiveDate, usize)> {
    let et = New_York.from_utc_datetime(&t_utc.naive_utc());
    let mins = et.hour() * 60 + et.minute();
    if !(RTH_OPEN_MIN..16 * 60).contains(&mins) {
        return None;
    }
    Some((et.date_naive(), (mins - RTH_OPEN_MIN) as usize))
}

/// Extended session (04:00–20:00 ET): index 0 = 04:00, 330 = 09:30 (RTH
/// open), 390 = 10:30, 959 = 19:59. `None` outside 04:00–20:00.
pub const EXT_OPEN_MIN: u32 = 4 * 60;
pub const EXT_RTH_OPEN_IDX: usize = 330;
pub const EXT_SESSION_BARS: usize = 960;

pub fn ext_index(t_utc: DateTime<Utc>) -> Option<(NaiveDate, usize)> {
    let et = New_York.from_utc_datetime(&t_utc.naive_utc());
    let mins = et.hour() * 60 + et.minute();
    if !(EXT_OPEN_MIN..20 * 60).contains(&mins) {
        return None;
    }
    Some((et.date_naive(), (mins - EXT_OPEN_MIN) as usize))
}

/// Extended index for a wall-clock ET time (e.g. `ext_idx_at(9, 25)` = 325).
pub fn ext_idx_at(h: u32, m: u32) -> usize {
    (h * 60 + m).saturating_sub(EXT_OPEN_MIN) as usize
}

/// ET date for any UTC timestamp (used for daily grouping incl. extended hours).
pub fn et_date(t_utc: DateTime<Utc>) -> NaiveDate {
    New_York.from_utc_datetime(&t_utc.naive_utc()).date_naive()
}

/// Wall-clock ET for a minute index (bar open time).
pub fn index_to_time(idx: usize) -> NaiveTime {
    let mins = RTH_OPEN_MIN + idx as u32;
    NaiveTime::from_hms_opt(mins / 60, mins % 60, 0).unwrap_or(NaiveTime::MIN)
}

/// Half-hour decision grid (Zarattini et al.): decisions are taken at
/// 10:00, 10:30, …, up to 30 min before the close, evaluated on the close
/// of the bar ending at that time (index m−1).
pub fn decision_indices(session_bars: usize) -> Vec<usize> {
    (30..session_bars).step_by(30).map(|m| m - 1).collect()
}

/// Is `idx` a decision bar for a session of `session_bars` bars?
pub fn is_decision_bar(idx: usize, session_bars: usize) -> bool {
    idx + 1 >= 30 && (idx + 1).is_multiple_of(30) && idx + 1 < session_bars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rth_index_maps_open_and_last_bar() {
        // 2026-04-29 13:30 UTC = 09:30 ET (EDT)
        let t = Utc.with_ymd_and_hms(2026, 4, 29, 13, 30, 0).single().unwrap();
        assert_eq!(rth_index(t), Some((NaiveDate::from_ymd_opt(2026, 4, 29).unwrap(), 0)));
        let t = Utc.with_ymd_and_hms(2026, 4, 29, 19, 59, 0).single().unwrap();
        assert_eq!(rth_index(t).map(|x| x.1), Some(389));
        let t = Utc.with_ymd_and_hms(2026, 4, 29, 20, 0, 0).single().unwrap();
        assert_eq!(rth_index(t), None);
        // Winter (EST): 2026-01-15 14:30 UTC = 09:30 ET
        let t = Utc.with_ymd_and_hms(2026, 1, 15, 14, 30, 0).single().unwrap();
        assert_eq!(rth_index(t).map(|x| x.1), Some(0));
    }

    #[test]
    fn ext_index_premarket_and_rth_open() {
        // 2026-09-03 08:00 UTC = 04:00 EDT → idx 0
        let t = Utc.with_ymd_and_hms(2026, 9, 3, 8, 0, 0).single().unwrap();
        assert_eq!(ext_index(t).map(|x| x.1), Some(0));
        // 09:49 EDT = 13:49 UTC → 349
        let t = Utc.with_ymd_and_hms(2026, 9, 3, 13, 49, 0).single().unwrap();
        assert_eq!(ext_index(t).map(|x| x.1), Some(349));
        assert_eq!(ext_idx_at(9, 30), EXT_RTH_OPEN_IDX);
        assert_eq!(ext_idx_at(10, 30), 390);
        // 03:59 EDT is outside
        let t = Utc.with_ymd_and_hms(2026, 9, 3, 7, 59, 0).single().unwrap();
        assert_eq!(ext_index(t), None);
        // 19:59 EDT = 23:59 UTC → 959; 20:00 → None
        let t = Utc.with_ymd_and_hms(2026, 9, 3, 23, 59, 0).single().unwrap();
        assert_eq!(ext_index(t).map(|x| x.1), Some(959));
    }

    #[test]
    fn decision_grid_full_and_half_day() {
        let full = decision_indices(FULL_SESSION_BARS);
        assert_eq!(full.first(), Some(&29));
        assert_eq!(full.last(), Some(&359));
        assert_eq!(full.len(), 12);
        let half = decision_indices(HALF_SESSION_BARS);
        assert_eq!(half, vec![29, 59, 89, 119, 149, 179]);
        assert!(is_decision_bar(29, 390));
        assert!(!is_decision_bar(30, 390));
        assert!(!is_decision_bar(389, 390), "close bar is not a decision bar");
    }

    #[test]
    fn early_close_calendar() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert!(is_early_close(d(2025, 11, 28)) && is_early_close(d(2024, 7, 3)) && is_early_close(d(2026, 12, 24)));
        assert!(!is_early_close(d(2026, 9, 18)) && !is_early_close(d(2021, 12, 24)), "2021-12-24 was a full holiday");
        assert_eq!(scheduled_session_bars(d(2025, 12, 24)), HALF_SESSION_BARS);
        assert_eq!(scheduled_session_bars(d(2025, 12, 23)), FULL_SESSION_BARS);
        assert_eq!(EARLY_CLOSES.len(), 24);
    }

    #[test]
    fn index_to_time_roundtrip() {
        assert_eq!(index_to_time(0), NaiveTime::from_hms_opt(9, 30, 0).unwrap());
        assert_eq!(index_to_time(29), NaiveTime::from_hms_opt(9, 59, 0).unwrap());
        assert_eq!(index_to_time(389), NaiveTime::from_hms_opt(15, 59, 0).unwrap());
    }
}
