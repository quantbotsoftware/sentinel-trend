//! S15.5 — session boundary helpers + session-aware stale-detection.
//!
//! Extracted from `scoring.rs` so that adding `in_premarket` /
//! `in_extended` / `compute_is_stale` doesn't push `scoring.rs` past
//! the R2 500-LOC cap. `in_rth` lives here now (single source of
//! truth for ET session predicates) and `mod.rs` re-exports it.
//!
//! Time-zone is always `chrono_tz::US::Eastern` (S04 invariant).
//!
//! ## Stale thresholds (SC3 — extended in S15.5)
//!
//! Premarket activity is intrinsically thinner than RTH; a 60-second
//! threshold falsely marked healthy quiet ticks as stale. The new
//! contract is session-aware:
//!
//! | Session              | Threshold (`last_trade_age >`) |
//! |----------------------|-------------------------------|
//! | RTH (09:30–16:00)    | 60 s                          |
//! | Premarket (04:00–09:30) | 300 s (5 min)             |
//! | Extended (16:00–20:00)  | 300 s (5 min)             |
//! | Dead time (overnight, weekend) | 3600 s (1 h)       |

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Timelike, Utc, Weekday};
use chrono_tz::Tz;

use crate::risk::calendar::is_nyse_trading_day;

/// S04 invariant: regular trading hours = weekday AND `09:30 <= t < 16:00`
/// in `chrono_tz::US::Eastern`. NEVER use `Utc::now().date_naive()` here.
///
/// S13: shared with `market::catalyst::is_halted_combined` (heuristic
/// halt fallback). Pure helper, no module coupling.
pub(crate) fn in_rth(now_et: DateTime<Tz>) -> bool {
    if is_weekend(now_et) {
        return false;
    }
    let t = now_et.time();
    // unwrap()s on hard-coded literals are safe — they cannot fail.
    let open = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
    let close = NaiveTime::from_hms_opt(16, 0, 0).unwrap();
    t >= open && t < close
}

/// S15.5: 04:00 ET (incl) → 09:30 ET (excl), weekdays only.
pub fn in_premarket(now_et: DateTime<Tz>) -> bool {
    if is_weekend(now_et) {
        return false;
    }
    let t = now_et.time();
    let open = NaiveTime::from_hms_opt(4, 0, 0).unwrap();
    let rth_open = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
    t >= open && t < rth_open
}

/// S15.5: 16:00 ET (incl) → 20:00 ET (excl), weekdays only.
pub(crate) fn in_extended(now_et: DateTime<Tz>) -> bool {
    if is_weekend(now_et) {
        return false;
    }
    let t = now_et.time();
    let rth_close = NaiveTime::from_hms_opt(16, 0, 0).unwrap();
    let ext_close = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
    t >= rth_close && t < ext_close
}

/// SC3 — session-aware stale detection.
///
/// Pre-S15.5 the rule was `is_stale = in_rth && age > 60s`, which had
/// the side-effect of NEVER marking premarket/extended/weekend rows
/// stale even when an obviously dead 13-hour-old quote was on screen
/// (DGSN-style). The new contract surfaces stale even outside RTH,
/// but with thresholds appropriate to the session's natural cadence.
pub(crate) fn compute_is_stale(now_et: DateTime<Tz>, last_trade_age: u32) -> bool {
    if in_rth(now_et) {
        last_trade_age > 60
    } else if in_premarket(now_et) || in_extended(now_et) {
        last_trade_age > 300
    } else {
        last_trade_age > 3600
    }
}

fn is_weekend(now_et: DateTime<Tz>) -> bool {
    matches!(now_et.weekday(), Weekday::Sat | Weekday::Sun)
}

/// SC11 — actionable scanner window: NYSE trading day AND
/// `04:05 ≤ ET < 15:30`. Outside this window the scanner must not
/// publish actionable results (yesterday leftovers stay purged).
pub(crate) fn in_actionable_scan_window(now_et: DateTime<Tz>) -> bool {
    if !is_nyse_trading_day(now_et.date_naive()) {
        return false;
    }
    let mins = now_et.hour() * 60 + now_et.minute();
    // 04:05 ET (245) inclusive → 15:30 ET (930) exclusive.
    (245..930).contains(&mins)
}

/// Format `now_et`'s calendar date as `YYYY-MM-DD` (ET).
pub(crate) fn session_date_et_str(now_et: DateTime<Tz>) -> String {
    now_et.date_naive().format("%Y-%m-%d").to_string()
}

/// Parse an Alpaca trade timestamp (RFC3339) into its US/Eastern date.
pub(crate) fn trade_et_date(trade_ts: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(trade_ts)
        .ok()
        .map(|dt| dt.with_timezone(&chrono_tz::US::Eastern).date_naive())
}

/// SC11 — true when `latestTrade.t` belongs to today's ET calendar date
/// on a NYSE trading day. `dailyBar` / `minuteBar` alone cannot prove
/// "today" — only a consolidated print can.
pub(crate) fn is_current_session_trade(now_et: DateTime<Tz>, trade_ts: &str) -> bool {
    if !is_nyse_trading_day(now_et.date_naive()) {
        return false;
    }
    match trade_et_date(trade_ts) {
        Some(d) => d == now_et.date_naive(),
        None => false,
    }
}

/// True when a Unix scan timestamp falls on the same ET calendar date
/// as `now_et`.
pub(crate) fn timestamp_is_today_et(ts: i64, now_et: DateTime<Tz>) -> bool {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono_tz::US::Eastern).date_naive() == now_et.date_naive())
        .unwrap_or(false)
}

/// Current wall-clock in US/Eastern (S04).
pub(crate) fn now_et() -> DateTime<Tz> {
    Utc::now().with_timezone(&chrono_tz::US::Eastern)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Wednesday 2026-04-29 10:30 ET — solidly inside RTH.
    fn rth_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 10, 30, 0)
            .unwrap()
    }

    /// Wednesday 2026-04-29 07:30 ET — premarket (outside RTH).
    fn premarket_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 7, 30, 0)
            .unwrap()
    }

    /// Wednesday 2026-04-29 17:00 ET — extended hours.
    fn extended_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 17, 0, 0)
            .unwrap()
    }

    /// Saturday 2026-05-02 12:00 ET — weekend.
    fn weekend_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 5, 2, 12, 0, 0)
            .unwrap()
    }

    /// Wednesday 2026-04-29 03:00 ET — overnight dead time.
    fn dead_time_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 3, 0, 0)
            .unwrap()
    }

    // ─── in_rth (parity with pre-S15.5 scoring.rs tests) ──────────────

    #[test]
    fn in_rth_weekend_is_false() {
        assert!(!in_rth(weekend_now()));
    }

    #[test]
    fn in_rth_premarket_is_false() {
        assert!(!in_rth(premarket_now()));
    }

    #[test]
    fn in_rth_regular_hours_is_true() {
        assert!(in_rth(rth_now()));
    }

    #[test]
    fn in_rth_close_boundary_excluded() {
        // 16:00:00 ET = closing bell, NOT in RTH (half-open interval).
        let close = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 16, 0, 0)
            .unwrap();
        assert!(!in_rth(close));
    }

    // ─── in_premarket / in_extended boundaries ────────────────────────

    #[test]
    fn in_premarket_correct_boundaries() {
        let tz = chrono_tz::US::Eastern;
        // 04:00 ET start (inclusive)
        assert!(in_premarket(tz.with_ymd_and_hms(2026, 4, 29, 4, 0, 0).unwrap()));
        // 09:29 ET still premarket
        assert!(in_premarket(tz.with_ymd_and_hms(2026, 4, 29, 9, 29, 0).unwrap()));
        // 09:30 ET RTH boundary (exclusive)
        assert!(!in_premarket(tz.with_ymd_and_hms(2026, 4, 29, 9, 30, 0).unwrap()));
        // 03:59 ET pre-premarket
        assert!(!in_premarket(tz.with_ymd_and_hms(2026, 4, 29, 3, 59, 0).unwrap()));
        // Saturday at 06:00 ET
        assert!(!in_premarket(tz.with_ymd_and_hms(2026, 5, 2, 6, 0, 0).unwrap()));
    }

    #[test]
    fn in_extended_correct_boundaries() {
        let tz = chrono_tz::US::Eastern;
        // 16:00 ET start (inclusive)
        assert!(in_extended(tz.with_ymd_and_hms(2026, 4, 29, 16, 0, 0).unwrap()));
        // 19:59 ET still extended
        assert!(in_extended(tz.with_ymd_and_hms(2026, 4, 29, 19, 59, 0).unwrap()));
        // 20:00 ET close (exclusive)
        assert!(!in_extended(tz.with_ymd_and_hms(2026, 4, 29, 20, 0, 0).unwrap()));
        // 15:59 ET still RTH
        assert!(!in_extended(tz.with_ymd_and_hms(2026, 4, 29, 15, 59, 0).unwrap()));
        // Sunday 17:00 ET
        assert!(!in_extended(tz.with_ymd_and_hms(2026, 5, 3, 17, 0, 0).unwrap()));
    }

    // ─── compute_is_stale thresholds ──────────────────────────────────

    #[test]
    fn compute_is_stale_rth_60s_threshold() {
        // RTH parity with pre-S15.5 contract.
        assert!(!compute_is_stale(rth_now(), 60));
        assert!(compute_is_stale(rth_now(), 61));
        assert!(!compute_is_stale(rth_now(), 0));
    }

    #[test]
    fn compute_is_stale_premarket_300s_threshold() {
        // 4 min (240s) of premarket silence is healthy
        assert!(!compute_is_stale(premarket_now(), 240));
        // 6 min (360s) is stale
        assert!(compute_is_stale(premarket_now(), 360));
        // exactly at 300 boundary — strict >
        assert!(!compute_is_stale(premarket_now(), 300));
        assert!(compute_is_stale(premarket_now(), 301));
    }

    #[test]
    fn compute_is_stale_extended_300s_threshold() {
        // Extended hours mirrors premarket logic.
        assert!(!compute_is_stale(extended_now(), 240));
        assert!(compute_is_stale(extended_now(), 360));
        assert!(!compute_is_stale(extended_now(), 300));
    }

    #[test]
    fn compute_is_stale_dead_time_1h_threshold() {
        // 03:00 ET weekday: 30 min silence is normal
        assert!(!compute_is_stale(dead_time_now(), 1800));
        // 2 h is stale
        assert!(compute_is_stale(dead_time_now(), 7200));
        // boundary — strict >
        assert!(!compute_is_stale(dead_time_now(), 3600));
        assert!(compute_is_stale(dead_time_now(), 3601));

        // Saturday at noon: same dead-time threshold applies
        assert!(!compute_is_stale(weekend_now(), 1800));
        assert!(compute_is_stale(weekend_now(), 7200));
    }

    // ─── SC11 today-only helpers ──────────────────────────────────────

    #[test]
    fn actionable_window_premarket_and_rth() {
        let tz = chrono_tz::US::Eastern;
        assert!(!in_actionable_scan_window(tz.with_ymd_and_hms(2026, 4, 29, 4, 4, 0).unwrap()));
        assert!(in_actionable_scan_window(tz.with_ymd_and_hms(2026, 4, 29, 4, 5, 0).unwrap()));
        assert!(in_actionable_scan_window(tz.with_ymd_and_hms(2026, 4, 29, 10, 30, 0).unwrap()));
        assert!(!in_actionable_scan_window(tz.with_ymd_and_hms(2026, 4, 29, 15, 30, 0).unwrap()));
        // Memorial Day 2026
        assert!(!in_actionable_scan_window(tz.with_ymd_and_hms(2026, 5, 25, 10, 0, 0).unwrap()));
    }

    #[test]
    fn current_session_trade_requires_same_et_date() {
        let now = rth_now();
        assert!(is_current_session_trade(now, "2026-04-29T14:30:00Z"));
        // Previous ET day close print
        assert!(!is_current_session_trade(now, "2026-04-28T19:59:00Z"));
        assert!(!is_current_session_trade(now, "not-a-timestamp"));
    }
}
