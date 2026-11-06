//! Driver tests (kept apart from `driver.rs` for the 500-line rule).

use super::*;
use chrono::TimeZone;

#[test]
fn last_state_roundtrip() {
    remember_state(serde_json::json!({"execute": false, "snapshot": {"equity": 1.0}}));
    let v = last_state_json().unwrap_or_default();
    assert_eq!(v["snapshot"]["equity"], 1.0);
}

#[test]
fn bar_age_from_snapshot() {
    let d = chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap_or_default();
    let mut snap = ReplaySnapshot { date: Some(d), ..Default::default() };
    assert_eq!(newest_bar_age_min(&snap, Utc::now()), None);
    snap.bars_today.insert("SPY".into(), 30); // bars 0..29 ⇒ last = 09:59 ET, closes 10:00 ET
    let t10_01 = chrono::DateTime::from_timestamp(super::super::shadow::bar_start_utc(d, 31).unwrap_or(0), 0).unwrap_or_default();
    assert_eq!(newest_bar_age_min(&snap, t10_01), Some(1));
    let t10_10 = chrono::DateTime::from_timestamp(super::super::shadow::bar_start_utc(d, 40).unwrap_or(0), 0).unwrap_or_default();
    assert_eq!(newest_bar_age_min(&snap, t10_10), Some(10));
    // QQQ's sync failed since 09:45: the snapshot age reports the stale symbol,
    // and per symbol SPY is fresh while QQQ is not
    snap.bars_today.insert("QQQ".into(), 15);
    assert_eq!(newest_bar_age_min(&snap, t10_01), Some(16));
    assert_eq!(bar_age_min(d, 30, t10_01), Some(1));
    assert_eq!(bar_age_min(d, 15, t10_01), Some(16));
    assert_eq!(bar_age_min(d, 0, t10_01), None);
}

#[test]
fn bar_gap_detection_waits_for_the_closed_bar() {
    let t = |h: u32, m: u32, s: u32| Utc.with_ymd_and_hms(2026, 9, 18, h + 4, m, s).single().unwrap_or_default();
    let clock = SessionClock::default();
    let d = chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap_or_default();
    let mut snap = ReplaySnapshot { date: Some(d), ..Default::default() };
    snap.bars_today.insert("SPY".into(), 29);
    snap.bars_today.insert("QQQ".into(), 30);
    assert!(bars_missing(&snap, &clock, t(10, 0, 2)), "10:00 needs 30 bars (0..29) — SPY has 29");
    snap.bars_today.insert("SPY".into(), 30);
    assert!(!bars_missing(&snap, &clock, t(10, 0, 2)));
    assert!(!bars_missing(&snap, &clock, t(16, 0, 2)), "after the close the close-watch owns the retry");
    assert!(!bars_missing(&snap, &clock, t(9, 30, 5)), "nothing has closed yet");
    let early = SessionClock::new(super::super::clock::EARLY_CLOSE_MIN);
    assert!(!bars_missing(&snap, &early, t(13, 5, 0)), "early close: afternoon is not a gap");
    let yesterday = ReplaySnapshot { date: chrono::NaiveDate::from_ymd_opt(2026, 9, 17), ..Default::default() };
    assert!(!bars_missing(&yesterday, &clock, t(10, 0, 2)));
}

#[test]
fn replay_kind_waits_for_the_decision_bar_and_previews_through_late_close() {
    // 15:48:02 — 378 bars closed, B decides on bars[..379] ⇒ wait
    assert_eq!(replay_kind(Phase::ClosePreview, Some(378), 378, 390, 11), ReplayKind::WaitDecisionBar);
    // 15:49:02 — bar 378 has closed ⇒ preview (the cache may still lag: bars_missing retries)
    assert_eq!(replay_kind(Phase::ClosePreview, Some(379), 378, 390, 11), ReplayKind::ClosePreview);
    assert_eq!(replay_kind(Phase::ClosePreview, Some(379), 379, 390, 11), ReplayKind::ClosePreview);
    // LateClose / AfterClose: close book until the closing bar is cached
    assert_eq!(replay_kind(Phase::LateClose, Some(385), 385, 390, 11), ReplayKind::ClosePreview);
    assert_eq!(replay_kind(Phase::LateClose, None, 389, 390, 11), ReplayKind::ClosePreview, "16:00:12, bar 389 not in the cache yet");
    assert_eq!(replay_kind(Phase::LateClose, None, 390, 390, 11), ReplayKind::Regular);
    assert_eq!(replay_kind(Phase::AfterClose, None, 389, 390, 11), ReplayKind::ClosePreview, "residual vs the close book, not a late A");
    assert_eq!(replay_kind(Phase::AfterClose, None, 390, 390, 11), ReplayKind::Regular);
    // early close: 12:48 with 198 closed ⇒ wait; 12:49 ⇒ preview
    assert_eq!(replay_kind(Phase::ClosePreview, Some(198), 198, 210, 11), ReplayKind::WaitDecisionBar);
    assert_eq!(replay_kind(Phase::ClosePreview, Some(199), 199, 210, 11), ReplayKind::ClosePreview);
    // offset 0 (research semantics): preview immediately
    assert_eq!(replay_kind(Phase::ClosePreview, Some(378), 378, 390, 0), ReplayKind::ClosePreview);
    for p in [Phase::Session, Phase::OpgWindow, Phase::Idle] {
        assert_eq!(replay_kind(p, Some(100), 100, 390, 11), ReplayKind::Regular);
    }
}

#[test]
fn today_bars_min_is_zero_without_todays_session() {
    use titan_meridian::bars::{Bar, DaySeries};
    let d = chrono::NaiveDate::from_ymd_opt(2026, 9, 21).unwrap_or_default();
    let day = |date, n: usize| DaySeries { date, bars: vec![Bar { o: 1.0, h: 1.0, l: 1.0, c: 1.0, v: 1.0 }; n], real_bars: n };
    let spy = || titan_meridian::sim::SymbolData { symbol: "SPY".into(), days: vec![day(d, 379)] };
    let qqq = titan_meridian::sim::SymbolData { symbol: "QQQ".into(), days: vec![day(d, 378)] };
    assert_eq!(today_bars_min(&[spy(), qqq], Some(d)), 378);
    assert_eq!(today_bars_min(&[spy()], Some(d + chrono::Days::new(1))), 0, "yesterday's session is not today's");
    assert_eq!(today_bars_min(&[], Some(d)), 0);
}

#[test]
fn close_bar_seen_needs_every_symbol() {
    let mut snap = ReplaySnapshot::default();
    assert!(!close_bar_seen(&snap, 390), "no symbols ⇒ not done");
    snap.bars_today.insert("SPY".into(), 390);
    snap.bars_today.insert("QQQ".into(), 389);
    assert!(!close_bar_seen(&snap, 390));
    snap.bars_today.insert("QQQ".into(), 390);
    assert!(close_bar_seen(&snap, 390));
    snap.bars_today.insert("QQQ".into(), 210);
    assert!(!close_bar_seen(&snap, 390) && close_bar_seen(&snap, 210), "early close: 210 bars is the full session");
}
