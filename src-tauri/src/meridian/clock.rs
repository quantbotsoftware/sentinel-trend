//! ET session clock for the driver, parametrised by the day's closing time
//! so early closes (13:00 ET: day after Thanksgiving, Christmas Eve, …) get
//! the same phase sequence three hours earlier. The close comes from
//! `GET /v2/calendar`; when it is unavailable the clock assumes 16:00 and
//! the driver says so.

use chrono::{Datelike, Timelike, Utc};
use chrono_tz::America::New_York;

use super::exec::Phase;

pub const FULL_CLOSE_MIN: u32 = 16 * 60;
pub const EARLY_CLOSE_MIN: u32 = 13 * 60;
const OPEN_MIN: u32 = 9 * 60 + 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionClock {
    /// Closing time as minutes after midnight ET (960 regular, 780 early).
    pub close_min: u32,
}

impl Default for SessionClock {
    fn default() -> Self {
        Self { close_min: FULL_CLOSE_MIN }
    }
}

fn et_minutes(now: chrono::DateTime<Utc>) -> Option<u32> {
    let et = now.with_timezone(&New_York);
    if matches!(et.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
        return None;
    }
    Some(et.hour() * 60 + et.minute())
}

impl SessionClock {
    pub fn new(close_min: u32) -> Self {
        Self { close_min: close_min.clamp(EARLY_CLOSE_MIN, FULL_CLOSE_MIN) }
    }

    /// Scheduled RTH bars in this session (390 / 210).
    pub fn session_bars(&self) -> usize {
        (self.close_min - OPEN_MIN) as usize
    }

    pub fn is_early_close(&self) -> bool {
        self.close_min < FULL_CLOSE_MIN
    }

    /// Execution phase (weekdays only). The MOC cutoff is 10 min before the
    /// close, so the preview runs in the two minutes before it; LateClose
    /// ends at the close itself (nothing is sent after it — AfterClose).
    pub fn phase_at(&self, now: chrono::DateTime<Utc>) -> Phase {
        let Some(m) = et_minutes(now) else { return Phase::Idle };
        let c = self.close_min;
        match m {
            m if (OPEN_MIN..c - 12).contains(&m) => Phase::Session,
            m if (c - 12..c - 10).contains(&m) => Phase::ClosePreview,
            m if (c - 10..c).contains(&m) => Phase::LateClose,
            m if (c..=c + 15).contains(&m) => Phase::AfterClose,
            m if (19 * 60..=19 * 60 + 15).contains(&m) || (9 * 60..=9 * 60 + 27).contains(&m) => Phase::OpgWindow,
            _ => Phase::Idle,
        }
    }

    /// Seconds left in the current execution phase (R6 E2): the deadline
    /// `exec_apply::apply` enforces on every submit. Session ends at the
    /// close-preview start (close − 12 min), ClosePreview at the MOC cutoff
    /// (close − 10 min), LateClose at the close, the OPG windows at 09:27 /
    /// 19:15. Observe-only phases get 0.
    pub fn secs_left_in_phase(&self, now: chrono::DateTime<Utc>) -> i64 {
        let et = now.with_timezone(&New_York);
        let Some(m) = et_minutes(now) else { return 0 };
        let sod = et.hour() as i64 * 3600 + et.minute() as i64 * 60 + et.second() as i64;
        let c = self.close_min as i64;
        let end_min: i64 = match self.phase_at(now) {
            Phase::Session => c - 12,
            Phase::ClosePreview => c - 10,
            Phase::LateClose => c,
            Phase::OpgWindow => {
                if (m as i64) >= 19 * 60 { 19 * 60 + 16 } else { 9 * 60 + 28 }
            }
            Phase::AfterClose | Phase::Idle => return 0,
        };
        (end_min * 60 - sod).max(0)
    }

    /// Seconds until the scheduled close (negative after it; `i64::MIN` on
    /// weekends). Drives the paper close emulation (`exec::CLOSE_MARKET_SECS`).
    pub fn secs_to_close(&self, now: chrono::DateTime<Utc>) -> i64 {
        let et = now.with_timezone(&New_York);
        if et_minutes(now).is_none() {
            return i64::MIN;
        }
        (self.close_min as i64) * 60 - (et.hour() as i64 * 3600 + et.minute() as i64 * 60 + et.second() as i64)
    }

    /// Poll window: 09:29 → close + 15 min.
    pub fn in_poll_window(&self, now: chrono::DateTime<Utc>) -> bool {
        et_minutes(now).is_some_and(|m| (OPEN_MIN - 1..=self.close_min + 15).contains(&m))
    }

    /// Close watch: from 5 min before the close until 15 min after, the
    /// driver retries every 10 s until the closing bar is replayed.
    pub fn is_close_watch(&self, now: chrono::DateTime<Utc>) -> bool {
        et_minutes(now).is_some_and(|m| (self.close_min - 5..=self.close_min + 15).contains(&m))
    }

    /// Seconds to sleep when nothing is due: inside the poll window land
    /// ~2 s after the next minute boundary (the bar that just closed is what
    /// the next decision needs); outside it, the configured cadence (≤ 30 s).
    pub fn next_poll_secs(&self, now: chrono::DateTime<Utc>, poll_secs: u64) -> u64 {
        if self.in_poll_window(now) {
            let s = now.second() as u64;
            if s < 2 { 2 - s } else { 62 - s }
        } else {
            poll_secs.clamp(15, 300).min(30)
        }
    }

    /// Bars that have closed so far today (0 before the first close, capped
    /// at the session length; `None` outside RTH or on weekends).
    pub fn bars_closed(&self, now: chrono::DateTime<Utc>) -> Option<usize> {
        let m = et_minutes(now)?;
        if (OPEN_MIN..self.close_min).contains(&m) { Some((m - OPEN_MIN) as usize) } else { None }
    }
}

/// Closing minute for `date` from the `/v2/calendar` array. `None` when the
/// date is not a trading day (holiday) or the payload is unusable.
pub fn parse_close_minutes(v: &serde_json::Value, date: chrono::NaiveDate) -> Option<u32> {
    let d = date.to_string();
    let day = v.as_array()?.iter().find(|e| e.get("date").and_then(|x| x.as_str()) == Some(d.as_str()))?;
    let close = day.get("close")?.as_str()?;
    let (h, m) = close.split_once(':')?;
    Some(h.parse::<u32>().ok()? * 60 + m.parse::<u32>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32) -> chrono::DateTime<Utc> {
        // 2026-09-16 (Wed, EDT = UTC−4); minutes offset so 20:00 ET (00:00Z next day) is valid
        Utc.with_ymd_and_hms(2026, 9, 16, 0, 0, 0).single().unwrap_or_default() + chrono::Duration::minutes(((h + 4) * 60 + m) as i64)
    }

    #[test]
    fn regular_day_phases_follow_the_et_clock() {
        let c = SessionClock::default();
        assert_eq!(c.session_bars(), 390);
        assert_eq!(c.phase_at(t(9, 0)), Phase::OpgWindow);
        assert_eq!(c.phase_at(t(9, 27)), Phase::OpgWindow);
        assert_eq!(c.phase_at(t(9, 28)), Phase::Idle);
        assert_eq!(c.phase_at(t(9, 30)), Phase::Session);
        assert_eq!(c.phase_at(t(15, 47)), Phase::Session);
        assert_eq!(c.phase_at(t(15, 48)), Phase::ClosePreview);
        assert_eq!(c.phase_at(t(15, 50)), Phase::LateClose);
        assert_eq!(c.phase_at(t(15, 59)), Phase::LateClose);
        assert_eq!(c.phase_at(t(16, 0)), Phase::AfterClose, "nothing is sent after the close");
        assert_eq!(c.phase_at(t(16, 15)), Phase::AfterClose);
        assert_eq!(c.phase_at(t(16, 16)), Phase::Idle);
        assert_eq!(c.phase_at(t(19, 0)), Phase::OpgWindow);
        assert_eq!(c.secs_to_close(t(15, 59) + chrono::Duration::seconds(20)), 40);
        assert_eq!(c.secs_to_close(t(16, 0) + chrono::Duration::seconds(12)), -12);
        assert_eq!(c.secs_to_close(t(10, 0)), 6 * 3600);
        assert_eq!(c.phase_at(t(20, 0)), Phase::Idle);
        let sat = Utc.with_ymd_and_hms(2026, 9, 19, 14, 0, 0).single().unwrap_or_default();
        assert_eq!(c.phase_at(sat), Phase::Idle);
        assert!(!c.in_poll_window(t(9, 28)) && c.in_poll_window(t(9, 29)) && c.in_poll_window(t(16, 15)) && !c.in_poll_window(t(16, 16)));
        assert!(!c.is_close_watch(t(15, 54)) && c.is_close_watch(t(15, 55)) && c.is_close_watch(t(16, 15)));
        assert_eq!(c.bars_closed(t(10, 0)), Some(30));
        assert_eq!(c.bars_closed(t(9, 30)), Some(0));
        assert_eq!(c.bars_closed(t(16, 0)), None);
    }

    #[test]
    fn early_close_shifts_every_phase_to_13_00() {
        let c = SessionClock::new(EARLY_CLOSE_MIN);
        assert!(c.is_early_close());
        assert_eq!(c.session_bars(), 210);
        assert_eq!(c.phase_at(t(12, 47)), Phase::Session);
        assert_eq!(c.phase_at(t(12, 48)), Phase::ClosePreview, "MOC cutoff 12:50 on early-close days");
        assert_eq!(c.phase_at(t(12, 50)), Phase::LateClose);
        assert_eq!(c.phase_at(t(13, 0)), Phase::AfterClose);
        assert_eq!(c.phase_at(t(13, 15)), Phase::AfterClose);
        assert_eq!(c.phase_at(t(13, 16)), Phase::Idle);
        assert_eq!(c.secs_to_close(t(12, 59) + chrono::Duration::seconds(30)), 30);
        assert_eq!(c.phase_at(t(15, 48)), Phase::Idle, "nothing happens at the regular preview time");
        assert!(c.is_close_watch(t(12, 55)) && !c.is_close_watch(t(15, 55)));
        assert!(c.in_poll_window(t(13, 15)) && !c.in_poll_window(t(13, 16)));
        assert_eq!(c.bars_closed(t(12, 59)), Some(209));
        assert_eq!(c.bars_closed(t(13, 0)), None);
        assert_eq!(SessionClock::new(0).close_min, EARLY_CLOSE_MIN, "clamped");
    }

    #[test]
    fn poll_aligns_to_two_seconds_past_the_minute() {
        let c = SessionClock::default();
        let at = |h: u32, m: u32, s: u32| Utc.with_ymd_and_hms(2026, 9, 18, h + 4, m, s).single().unwrap_or_default();
        assert_eq!(c.next_poll_secs(at(10, 0, 25), 60), 37, "→ 10:01:02");
        assert_eq!(c.next_poll_secs(at(10, 0, 2), 60), 60);
        assert_eq!(c.next_poll_secs(at(10, 0, 1), 60), 1);
        assert_eq!(c.next_poll_secs(at(18, 0, 25), 60), 30, "outside the window: cadence");
    }

    #[test]
    fn calendar_close_parses_regular_early_and_holiday() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 11, 27).unwrap_or_default();
        let v = serde_json::json!([{"date":"2026-11-27","open":"09:30","close":"13:00","session_open":"0400","session_close":"1700"}]);
        assert_eq!(parse_close_minutes(&v, d), Some(EARLY_CLOSE_MIN));
        let v2 = serde_json::json!([{"date":"2026-09-18","open":"09:30","close":"16:00"}]);
        assert_eq!(parse_close_minutes(&v2, chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap_or_default()), Some(FULL_CLOSE_MIN));
        assert_eq!(parse_close_minutes(&serde_json::json!([]), d), None, "holiday: no entry");
        assert_eq!(parse_close_minutes(&v2, d), None, "wrong date is not trusted");
        assert_eq!(parse_close_minutes(&serde_json::json!({"code": 40010001}), d), None);
    }
}
