use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use chrono::Duration as ChronoDuration;

use crate::market::scanner::ScanResult;
use crate::market::catalyst::CatalystFlags;
use super::types::{
    AutoUniverseConfig, AutoEntry, BlacklistEntry, BanReason,
};
use super::AutoUniverse;

fn cfg_test() -> AutoUniverseConfig {
    AutoUniverseConfig {
        enabled: true,
        max_total_slots: 5,
        max_pinned_slots: 2,
        min_promote_score: 50.0,
        min_promote_rvol: 1.5,
        loss_strikes_to_blacklist: 2,
        big_loss_pct_threshold: 2.0,
        blacklist_ttl_hours: 4,
        score_decay_threshold: 30.0,
        score_decay_scans: 2,
        stale_catalyst_minutes: 60,
        preserve_winners: true,
        recently_traded_ttl_hours: 18,
    }
}

fn scan(symbol: &str, score: f64, rvol: f64) -> ScanResult {
    ScanResult {
        symbol: symbol.into(),
        price: 10.0,
        change_percent: 5.0,
        volume: 1_000_000.0,
        rvol,
        gap_percent: 2.0,
        score,
        dollar_volume: 10_000_000.0,
        bid: 9.99,
        ask: 10.01,
        spread_bps: 20.0,
        last_trade_age_seconds: 1,
        is_stale: false,
        catalyst_flags: CatalystFlags::default(),
        catalyst_pills: Vec::new(),
        ..Default::default()
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

#[test]
fn promote_top_candidates() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    let scans = vec![
        scan("AAA", 80.0, 3.0),
        scan("BBB", 65.0, 2.0),
        scan("CCC", 30.0, 1.0), // below score threshold
        scan("DDD", 70.0, 1.0), // below RVOL threshold
    ];
    let report = u.process_scan_results(&scans, &HashSet::new(), &cfg, now());
    assert!(report.promoted.contains(&"AAA".to_string()));
    assert!(report.promoted.contains(&"BBB".to_string()));
    assert!(!report.promoted.contains(&"CCC".to_string()));
    assert!(!report.promoted.contains(&"DDD".to_string()));
    assert!(report.skipped_low_score.contains(&"CCC".to_string()));
    assert!(report.skipped_low_score.contains(&"DDD".to_string()));
}

#[test]
fn blacklisted_symbols_are_not_promoted() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.blacklist.insert(
        "BAD".into(),
        BlacklistEntry {
            symbol: "BAD".into(),
            banned_at: now(),
            ttl_hours: 4,
            reason: BanReason::ConsecutiveLosses,
            last_loss_amount: -5.0,
            loss_strikes_at_ban: 2,
        },
    );
    let scans = vec![scan("BAD", 90.0, 5.0)];
    let report = u.process_scan_results(&scans, &HashSet::new(), &cfg, now());
    assert!(report.promoted.is_empty());
    assert!(report.skipped_blacklisted.contains(&"BAD".to_string()));
}

#[test]
fn slot_eviction_chooses_weakest_unprotected() {
    let mut u = AutoUniverse::default();
    let cfg = AutoUniverseConfig {
        max_total_slots: 3,
        max_pinned_slots: 0,
        ..cfg_test()
    };
    // Fill auto-pool to capacity.
    u.auto_pool
        .insert("AAA".into(), AutoEntry::new("AAA".into(), 60.0, now()));
    u.auto_pool
        .insert("BBB".into(), AutoEntry::new("BBB".into(), 55.0, now()));
    u.auto_pool
        .insert("CCC".into(), AutoEntry::new("CCC".into(), 70.0, now()));

    let scans = vec![scan("ZZZ", 80.0, 3.0)];
    let report = u.process_scan_results(&scans, &HashSet::new(), &cfg, now());
    assert!(report.promoted.contains(&"ZZZ".to_string()));
    // BBB (55) should have been evicted.
    assert!(!u.auto_pool.contains_key("BBB"));
    assert!(report.demoted.iter().any(|(s, _)| s == "BBB"));
}

#[test]
fn slot_eviction_respects_protection() {
    let mut u = AutoUniverse::default();
    let cfg = AutoUniverseConfig {
        max_total_slots: 3,
        max_pinned_slots: 0,
        ..cfg_test()
    };
    u.auto_pool
        .insert("AAA".into(), AutoEntry::new("AAA".into(), 60.0, now()));
    u.auto_pool
        .insert("BBB".into(), AutoEntry::new("BBB".into(), 55.0, now()));
    u.auto_pool
        .insert("CCC".into(), AutoEntry::new("CCC".into(), 70.0, now()));

    // Protect BBB (open position) — must NOT be evicted even
    // though it's the weakest.
    let mut protected = HashSet::new();
    protected.insert("BBB".to_string());

    let scans = vec![scan("ZZZ", 80.0, 3.0)];
    u.process_scan_results(&scans, &protected, &cfg, now());
    // BBB still there.
    assert!(u.auto_pool.contains_key("BBB"));
    // The weakest unprotected (AAA at 60) was evicted.
    assert!(!u.auto_pool.contains_key("AAA"));
}

#[test]
fn slot_eviction_skipped_if_candidate_weaker_than_weakest() {
    let mut u = AutoUniverse::default();
    let cfg = AutoUniverseConfig {
        max_total_slots: 2,
        max_pinned_slots: 0,
        ..cfg_test()
    };
    u.auto_pool
        .insert("AAA".into(), AutoEntry::new("AAA".into(), 75.0, now()));
    u.auto_pool
        .insert("BBB".into(), AutoEntry::new("BBB".into(), 80.0, now()));

    // Candidate score 60 — weaker than weakest (75) — must NOT
    // displace.
    let scans = vec![scan("ZZZ", 60.0, 3.0)];
    let report = u.process_scan_results(&scans, &HashSet::new(), &cfg, now());
    assert!(!report.promoted.contains(&"ZZZ".to_string()));
    assert!(u.auto_pool.contains_key("AAA"));
    assert!(u.auto_pool.contains_key("BBB"));
}

#[test]
fn score_decay_demotes_after_n_scans() {
    let mut u = AutoUniverse::default();
    let cfg = AutoUniverseConfig {
        score_decay_threshold: 30.0,
        score_decay_scans: 2,
        ..cfg_test()
    };
    u.auto_pool
        .insert("AAA".into(), AutoEntry::new("AAA".into(), 70.0, now()));

    // Scan 1 — score crashed to 20. Streak = 1, still in pool.
    u.process_scan_results(
        &vec![scan("AAA", 20.0, 3.0)],
        &HashSet::new(),
        &cfg,
        now(),
    );
    assert!(u.auto_pool.contains_key("AAA"));
    assert_eq!(u.auto_pool["AAA"].low_score_streak, 1);

    // Scan 2 — still 20. Streak = 2, demote.
    let report = u.process_scan_results(
        &vec![scan("AAA", 20.0, 3.0)],
        &HashSet::new(),
        &cfg,
        now(),
    );
    assert!(!u.auto_pool.contains_key("AAA"));
    assert!(report.demoted.iter().any(|(s, r)| s == "AAA" && r == "score_decay"));
}

#[test]
fn score_decay_resets_on_recovery() {
    let mut u = AutoUniverse::default();
    let cfg = AutoUniverseConfig {
        score_decay_threshold: 30.0,
        score_decay_scans: 3,
        ..cfg_test()
    };
    u.auto_pool
        .insert("AAA".into(), AutoEntry::new("AAA".into(), 70.0, now()));

    // Two low scans bring streak to 2.
    u.process_scan_results(
        &vec![scan("AAA", 20.0, 3.0)],
        &HashSet::new(),
        &cfg,
        now(),
    );
    u.process_scan_results(
        &vec![scan("AAA", 20.0, 3.0)],
        &HashSet::new(),
        &cfg,
        now(),
    );
    assert_eq!(u.auto_pool["AAA"].low_score_streak, 2);

    // Recovery — score back above threshold. Streak resets.
    u.process_scan_results(
        &vec![scan("AAA", 60.0, 3.0)],
        &HashSet::new(),
        &cfg,
        now(),
    );
    assert!(u.auto_pool.contains_key("AAA"));
    assert_eq!(u.auto_pool["AAA"].low_score_streak, 0);
}

#[test]
fn stale_catalyst_demotes_unprotected() {
    let mut u = AutoUniverse::default();
    let cfg = AutoUniverseConfig {
        stale_catalyst_minutes: 30,
        ..cfg_test()
    };
    let then = Utc::now() - ChronoDuration::minutes(45);
    u.auto_pool.insert(
        "STL".into(),
        AutoEntry {
            symbol: "STL".into(),
            added_at: then,
            score_at_add: 70.0,
            last_score: 70.0,
            last_seen_in_scan: then,
            low_score_streak: 0,
            realized_pnl: 0.0,
            wins: 0,
            losses: 0,
        },
    );
    // Scan returns no STL — stale check fires.
    let report = u.process_scan_results(
        &vec![scan("AAA", 80.0, 3.0)],
        &HashSet::new(),
        &cfg,
        Utc::now(),
    );
    assert!(!u.auto_pool.contains_key("STL"));
    // Prefer "stale"; S42 same-session edge near ET midnight may tag
    // prior_session_carryover instead — both remove the unprotected entry.
    assert!(
        report
            .demoted
            .iter()
            .any(|(s, r)| s == "STL" && (r == "stale" || r == "prior_session_carryover")),
        "STL must be demoted, got {:?}",
        report.demoted
    );
}

#[test]
fn stale_catalyst_is_skipped_when_protected() {
    let mut u = AutoUniverse::default();
    let cfg = AutoUniverseConfig {
        stale_catalyst_minutes: 30,
        ..cfg_test()
    };
    let then = Utc::now() - ChronoDuration::minutes(45);
    u.auto_pool.insert(
        "STL".into(),
        AutoEntry {
            symbol: "STL".into(),
            added_at: then,
            score_at_add: 70.0,
            last_score: 70.0,
            last_seen_in_scan: then,
            low_score_streak: 0,
            realized_pnl: 0.0,
            wins: 0,
            losses: 0,
        },
    );
    let mut protected = HashSet::new();
    protected.insert("STL".to_string());

    u.process_scan_results(
        &vec![scan("AAA", 80.0, 3.0)],
        &protected,
        &cfg,
        Utc::now(),
    );
    assert!(u.auto_pool.contains_key("STL"));
}

#[test]
fn promotion_skips_halted_symbols() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    let mut s = scan("HLT", 90.0, 5.0);
    s.catalyst_flags.halted = true;
    let report = u.process_scan_results(&[s], &HashSet::new(), &cfg, now());
    assert!(report.promoted.is_empty());
    assert!(report.skipped_low_score.contains(&"HLT".to_string()));
}

// ─── S19 "Fresh-born" tests ─────────────────────────────────────

#[test]
fn eod_flush_wipes_auto_and_blacklist_keeps_pinned() {
    // S22 (operator directive 2026-05-20): pinned must survive
    // the EOD flush and must NOT be added to the recently_traded
    // deny-list. Auto-pool and blacklist behave as before.
    let mut u = AutoUniverse::default();
    u.auto_pool
        .insert("AAA".into(), AutoEntry::new("AAA".into(), 80.0, now()));
    u.auto_pool
        .insert("BBB".into(), AutoEntry::new("BBB".into(), 70.0, now()));
    u.pinned.push("PIN".into());
    u.blacklist.insert(
        "BAD".into(),
        BlacklistEntry {
            symbol: "BAD".into(),
            banned_at: now(),
            ttl_hours: 4,
            reason: BanReason::Manual,
            last_loss_amount: 0.0,
            loss_strikes_at_ban: 0,
        },
    );

    let report = u.eod_flush(&HashSet::new(), now());

    assert_eq!(report.auto_cleared, 2);
    assert_eq!(report.pinned_cleared, 0, "pinned must persist (S22)");
    assert_eq!(report.blacklist_cleared, 1);
    assert!(u.auto_pool.is_empty(), "auto_pool wiped");
    assert!(u.is_pinned("PIN"), "pinned survives EOD");
    assert!(
        u.is_pinned("QBTS"),
        "permanent chart anchor re-asserted after EOD"
    );
    assert!(u.blacklist.is_empty(), "blacklist wiped");
    // Only auto symbols snapshotted forward; pinned is NEVER
    // self-blocked.
    assert_eq!(report.recently_traded_size, 2);
    assert!(u.recently_traded.contains_key("AAA"));
    assert!(u.recently_traded.contains_key("BBB"));
    assert!(
        !u.recently_traded.contains_key("PIN"),
        "pinned must not enter recently_traded (S22)"
    );
}

#[test]
fn eod_flush_retains_symbols_with_open_positions() {
    // S22: protected symbols (open positions) still survive in
    // auto_pool; pinned now also survives unconditionally.
    let mut u = AutoUniverse::default();
    u.auto_pool
        .insert("HELD".into(), AutoEntry::new("HELD".into(), 80.0, now()));
    u.auto_pool
        .insert("DROP".into(), AutoEntry::new("DROP".into(), 70.0, now()));
    u.pinned.push("PIN".into()); // pinned now persists (S22)

    let mut protected = HashSet::new();
    protected.insert("HELD".into());

    let report = u.eod_flush(&protected, now());

    assert_eq!(report.auto_cleared, 1, "only DROP cleared");
    assert!(u.auto_pool.contains_key("HELD"), "open position preserved");
    assert!(!u.auto_pool.contains_key("DROP"));
    assert!(u.is_pinned("PIN"), "pinned survives (S22)");
    assert!(u.is_pinned("QBTS"), "permanent chart anchor re-asserted");
    // Auto names snapshotted into recently_traded so a fresh
    // promotion can't slip them back in tomorrow morning. PIN
    // is NOT snapshotted — editorial tier is exempt (S22).
    assert!(u.recently_traded.contains_key("HELD"));
    assert!(u.recently_traded.contains_key("DROP"));
    assert!(
        !u.recently_traded.contains_key("PIN"),
        "pinned must not enter recently_traded (S22)"
    );
}

#[test]
fn recently_traded_blocks_promotion_within_ttl() {
    let mut u = AutoUniverse::default();
    u.recently_traded.insert("YESTERDAY".into(), now());

    let cfg = cfg_test();
    let s = scan("YESTERDAY", 90.0, 3.0); // way over thresholds
    let report = u.process_scan_results(&[s], &HashSet::new(), &cfg, now());

    assert!(report.promoted.is_empty(), "YESTERDAY blocked");
    assert!(report.skipped_recently_traded.contains(&"YESTERDAY".to_string()));
    assert!(!u.auto_pool.contains_key("YESTERDAY"));
}

#[test]
fn recently_traded_releases_after_ttl_expires() {
    let mut u = AutoUniverse::default();
    // Inserted 20 hours ago — past the 18h TTL.
    u.recently_traded
        .insert("STALE".into(), now() - ChronoDuration::hours(20));

    let cfg = cfg_test();
    let s = scan("STALE", 90.0, 3.0);
    let report = u.process_scan_results(&[s], &HashSet::new(), &cfg, now());

    assert!(report.promoted.contains(&"STALE".to_string()), "TTL expired → promotable");
    assert!(!u.recently_traded.contains_key("STALE"), "stale entry cleaned up");
}

#[test]
fn recently_traded_disabled_when_ttl_zero() {
    let mut u = AutoUniverse::default();
    u.recently_traded.insert("ANYTHING".into(), now());

    let mut cfg = cfg_test();
    cfg.recently_traded_ttl_hours = 0;

    let s = scan("ANYTHING", 90.0, 3.0);
    let report = u.process_scan_results(&[s], &HashSet::new(), &cfg, now());

    // TTL=0 disables the exclusion AND wipes the map on cleanup.
    assert!(report.promoted.contains(&"ANYTHING".to_string()));
    assert!(u.recently_traded.is_empty());
}

#[test]
fn is_recently_traded_helper_respects_ttl() {
    let mut u = AutoUniverse::default();
    u.recently_traded.insert("FRESH".into(), now());
    u.recently_traded
        .insert("OLD".into(), now() - ChronoDuration::hours(20));

    // FRESH within 18h window → true.
    assert!(u.is_recently_traded("FRESH", now(), 18));
    // OLD past TTL → false AND map cleaned.
    assert!(!u.is_recently_traded("OLD", now(), 18));
    assert!(!u.recently_traded.contains_key("OLD"));
    // TTL=0 always returns false.
    assert!(!u.is_recently_traded("FRESH", now(), 0));
}

#[test]
fn cleanup_recently_traded_with_ttl_zero_wipes_all() {
    let mut u = AutoUniverse::default();
    u.recently_traded.insert("A".into(), now());
    u.recently_traded.insert("B".into(), now());

    let removed = u.cleanup_recently_traded(now(), 0);
    assert_eq!(removed.len(), 2);
    assert!(u.recently_traded.is_empty());
}

// ─── S42 prior-session carryover guard ──────────────────────────

#[test]
fn s42_prior_session_carryover_is_demoted_and_blocked() {
    // Scenario: app was closed at 16:00 ET yesterday, so the EOD
    // flush never ran. A persisted auto_pool entry whose last-seen
    // is on a PRIOR ET date must be purged on the first scan of the
    // new session AND added to recently_traded (no tangency with
    // the previous day), even though it scores well today.
    let mut u = AutoUniverse::default();
    // 26h guarantees a PRIOR ET date at any wall-clock hour (20h only
    // crossed ET midnight when the test ran before ~20:00 ET — the
    // test was red every evening).
    let yesterday = Utc::now() - ChronoDuration::hours(26);
    u.auto_pool.insert(
        "OLD".into(),
        AutoEntry {
            symbol: "OLD".into(),
            added_at: yesterday,
            score_at_add: 80.0,
            last_score: 80.0,
            last_seen_in_scan: yesterday,
            low_score_streak: 0,
            realized_pnl: 0.0,
            wins: 0,
            losses: 0,
        },
    );

    let cfg = cfg_test();
    // OLD reappears in today's scan with a strong score — must
    // still be blocked because it is prior-session carryover.
    let s = scan("OLD", 95.0, 3.0);
    let report = u.process_scan_results(&[s], &HashSet::new(), &cfg, now());

    assert!(
        !u.auto_pool.contains_key("OLD"),
        "prior-session carryover must be removed from auto_pool"
    );
    assert!(
        u.recently_traded.contains_key("OLD"),
        "carryover must be added to recently_traded deny-list"
    );
    assert!(
        !report.promoted.contains(&"OLD".to_string()),
        "re-mover from yesterday must NOT be re-promoted today"
    );
}

#[test]
fn s42_protected_carryover_survives() {
    // An open-position symbol from a prior session is protected and
    // must NOT be yanked (the data feed has to keep flowing to
    // manage the exit).
    let mut u = AutoUniverse::default();
    // 26h — deterministically a prior ET date at any wall-clock hour.
    let yesterday = Utc::now() - ChronoDuration::hours(26);
    u.auto_pool.insert(
        "HELD".into(),
        AutoEntry {
            symbol: "HELD".into(),
            added_at: yesterday,
            score_at_add: 80.0,
            last_score: 80.0,
            last_seen_in_scan: yesterday,
            low_score_streak: 0,
            realized_pnl: 0.0,
            wins: 0,
            losses: 0,
        },
    );
    let mut protected = HashSet::new();
    protected.insert("HELD".to_string());

    let cfg = cfg_test();
    u.process_scan_results(&[], &protected, &cfg, now());

    assert!(
        u.auto_pool.contains_key("HELD"),
        "protected (open-position) carryover must survive"
    );
    assert!(!u.recently_traded.contains_key("HELD"));
}
