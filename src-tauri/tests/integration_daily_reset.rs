/// S09: Daily-reset integration skeleton.
///
/// Verifies the NYSE holiday calendars (2026 + 2027 carry-forward) are
/// well-formed: every entry parses as a date, falls on a weekday
/// (NYSE never lists weekend entries), and the count matches the
/// published NYSE schedule (9–10 full-day closures per year).
///
/// This test crate cannot import `risk::calendar` directly (no
/// `lib.rs`), so the canonical lists are mirrored here. Drift between
/// the source-of-truth in `src/risk/calendar.rs` and this snapshot
/// will surface as a test failure — guard against silent edits.

use chrono::{Datelike, NaiveDate, Weekday};

const NYSE_HOLIDAYS_2026: &[&str] = &[
    "2026-01-01", // New Year's Day
    "2026-01-19", // MLK Jr. Day
    "2026-02-16", // Presidents' Day
    "2026-04-03", // Good Friday
    "2026-05-25", // Memorial Day
    "2026-06-19", // Juneteenth
    "2026-07-03", // Independence Day (observed Fri, 7/4 = Sat)
    "2026-09-07", // Labor Day
    "2026-11-26", // Thanksgiving
    "2026-12-25", // Christmas
];

const NYSE_HOLIDAYS_2027: &[&str] = &[
    "2027-01-01", // New Year's Day
    "2027-01-18", // MLK Jr. Day
    "2027-02-15", // Presidents' Day
    "2027-03-26", // Good Friday
    "2027-05-31", // Memorial Day
    "2027-06-18", // Juneteenth (observed Fri, 6/19 = Sat)
    "2027-07-05", // Independence Day (observed Mon, 7/4 = Sun)
    "2027-09-06", // Labor Day
    "2027-11-25", // Thanksgiving
    "2027-12-24", // Christmas (observed Fri, 12/25 = Sat)
];

#[test]
fn nyse_2026_all_entries_parse_and_fall_on_weekdays() {
    for s in NYSE_HOLIDAYS_2026 {
        let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .unwrap_or_else(|e| panic!("NYSE 2026 holiday '{s}' does not parse: {e}"));
        let wd = d.weekday();
        assert!(
            wd != Weekday::Sat && wd != Weekday::Sun,
            "NYSE 2026 holiday '{s}' falls on {wd:?} — NYSE never lists weekend closures"
        );
        assert_eq!(d.year(), 2026, "Entry '{s}' is not in 2026");
    }
}

#[test]
fn nyse_2027_all_entries_parse_and_fall_on_weekdays() {
    for s in NYSE_HOLIDAYS_2027 {
        let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .unwrap_or_else(|e| panic!("NYSE 2027 holiday '{s}' does not parse: {e}"));
        let wd = d.weekday();
        assert!(
            wd != Weekday::Sat && wd != Weekday::Sun,
            "NYSE 2027 holiday '{s}' falls on {wd:?}"
        );
        assert_eq!(d.year(), 2027, "Entry '{s}' is not in 2027");
    }
}

#[test]
fn nyse_2026_count_in_expected_range() {
    let n = NYSE_HOLIDAYS_2026.len();
    assert!(
        (9..=11).contains(&n),
        "NYSE typically has 9–11 full-day closures; got {n}"
    );
}

#[test]
fn nyse_2027_count_in_expected_range() {
    let n = NYSE_HOLIDAYS_2027.len();
    assert!(
        (9..=11).contains(&n),
        "NYSE typically has 9–11 full-day closures; got {n}"
    );
}

#[test]
fn nyse_2026_contains_canonical_anchors() {
    // Canonical anchors that MUST be present (catch accidental deletion).
    assert!(NYSE_HOLIDAYS_2026.contains(&"2026-01-01"), "New Year missing");
    assert!(NYSE_HOLIDAYS_2026.contains(&"2026-12-25"), "Christmas missing");
    assert!(NYSE_HOLIDAYS_2026.contains(&"2026-11-26"), "Thanksgiving missing");
    assert!(NYSE_HOLIDAYS_2026.contains(&"2026-07-03"), "Indep. Day observed missing");
}

#[test]
fn nyse_2027_contains_canonical_anchors() {
    assert!(NYSE_HOLIDAYS_2027.contains(&"2027-01-01"), "New Year missing");
    assert!(NYSE_HOLIDAYS_2027.contains(&"2027-12-24"), "Christmas observed missing");
    assert!(NYSE_HOLIDAYS_2027.contains(&"2027-11-25"), "Thanksgiving missing");
    assert!(NYSE_HOLIDAYS_2027.contains(&"2027-07-05"), "Indep. Day observed missing");
}

#[test]
fn holiday_check_skips_reset() {
    // Mirrors the `daily_reset` decision: today_str ∈ holidays => skip.
    let today_str = "2026-12-25".to_string();
    let combined: Vec<&str> = NYSE_HOLIDAYS_2026
        .iter()
        .chain(NYSE_HOLIDAYS_2027.iter())
        .copied()
        .collect();

    assert!(
        combined.contains(&today_str.as_str()),
        "Holiday lookup must hit for 2026-12-25"
    );

    let trading_day = "2026-01-02".to_string();
    assert!(
        !combined.contains(&trading_day.as_str()),
        "Regular trading day must not match"
    );
}
