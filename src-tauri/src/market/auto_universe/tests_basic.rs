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

fn now() -> DateTime<Utc> {
    Utc::now()
}

#[test]
fn empty_universe_active_set_is_empty() {
    let u = AutoUniverse::default();
    assert!(u.active_set().is_empty());
}

#[test]
fn pin_idempotent_and_uppercase() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.pin("aapl", &cfg).unwrap();
    u.pin("AAPL", &cfg).unwrap(); // idempotent
    u.pin("aaPL ", &cfg).unwrap(); // whitespace + case
    assert_eq!(u.pinned, vec!["AAPL".to_string()]);
}

#[test]
fn pin_cap_is_enforced() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test(); // max_pinned = 2
    u.pin("AAPL", &cfg).unwrap();
    u.pin("MSFT", &cfg).unwrap();
    let err = u.pin("NVDA", &cfg).unwrap_err();
    assert!(err.contains("pinned cap"));
}

#[test]
fn pin_evicts_from_auto_pool() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.auto_pool
        .insert("TSLA".into(), AutoEntry::new("TSLA".into(), 60.0, now()));
    u.pin("TSLA", &cfg).unwrap();
    assert!(!u.auto_pool.contains_key("TSLA"));
    assert!(u.is_pinned("TSLA"));
}

#[test]
fn unpin_idempotent() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.pin("AAPL", &cfg).unwrap();
    u.unpin("aapl").unwrap();
    u.unpin("AAPL").unwrap(); // already gone, no error
    assert!(u.pinned.is_empty());
}

#[test]
fn qbts_is_permanent_and_cannot_be_unpinned() {
    let mut u = AutoUniverse::default();
    assert!(u.ensure_permanent_anchors());
    assert!(u.is_pinned("QBTS"));
    assert!(u.unpin("QBTS").is_err());
    assert!(u.force_remove("QBTS").is_err());
    assert!(u.is_pinned("QBTS"));
    // Second ensure is idempotent.
    assert!(!u.ensure_permanent_anchors());
}

#[test]
fn migrate_from_favorites_only_when_empty() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    let favs = vec!["AAPL".into(), "MSFT".into(), "NVDA".into()];
    let did = u.migrate_from_favorites_if_empty(&favs, &cfg);
    assert!(did);
    // Migration intentionally bypasses `max_pinned_slots` so an
    // existing user with 10–20 curated favorites doesn't lose
    // half of them on the S18 upgrade. All three migrate even
    // though `max_pinned_slots == 2`.
    assert_eq!(
        u.pinned,
        vec!["AAPL".to_string(), "MSFT".to_string(), "NVDA".to_string()]
    );
    // Idempotent — second call must NOT re-migrate.
    let did2 = u.migrate_from_favorites_if_empty(&favs, &cfg);
    assert!(!did2);
    // Cap still enforced on subsequent manual pins.
    let err = u.pin("EXTRA", &cfg).unwrap_err();
    assert!(err.contains("pinned cap"));
}

#[test]
fn record_outcome_consecutive_losses_blacklists() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.auto_pool
        .insert("LSR".into(), AutoEntry::new("LSR".into(), 70.0, now()));

    // First loss: streak becomes 1, no blacklist yet.
    let r1 = u.record_trade_outcome("LSR", -5.0, 300.0, 1, &cfg, now());
    assert!(!r1.now_blacklisted);
    assert!(u.auto_pool.contains_key("LSR"));

    // Second loss: streak becomes 2 — blacklist.
    let r2 = u.record_trade_outcome("LSR", -5.0, 300.0, 2, &cfg, now());
    assert!(r2.now_blacklisted);
    assert_eq!(r2.blacklist_reason, Some(BanReason::ConsecutiveLosses));
    assert!(u.blacklist.contains_key("LSR"));
    assert!(!u.auto_pool.contains_key("LSR"));
}

#[test]
fn record_outcome_big_loss_blacklists_immediately() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.auto_pool
        .insert("BLR".into(), AutoEntry::new("BLR".into(), 70.0, now()));
    // 2.5% loss on $300 allocated → $7.50 → over the 2% threshold.
    let r = u.record_trade_outcome("BLR", -7.5, 300.0, 1, &cfg, now());
    assert!(r.now_blacklisted);
    assert_eq!(r.blacklist_reason, Some(BanReason::BigLoss));
}

#[test]
fn record_outcome_pinned_never_auto_blacklisted() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.pin("PIN", &cfg).unwrap();
    // Big loss + many strikes — must not blacklist a pinned.
    let r = u.record_trade_outcome("PIN", -50.0, 300.0, 5, &cfg, now());
    assert!(!r.now_blacklisted);
    assert!(!u.blacklist.contains_key("PIN"));
    assert!(u.is_pinned("PIN"));
}

#[test]
fn record_outcome_winning_trade_no_blacklist() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.auto_pool
        .insert("WIN".into(), AutoEntry::new("WIN".into(), 70.0, now()));
    let r = u.record_trade_outcome("WIN", 25.0, 300.0, 0, &cfg, now());
    assert!(!r.now_blacklisted);
    assert!(u.auto_pool.contains_key("WIN"));
    let entry = &u.auto_pool["WIN"];
    assert_eq!(entry.wins, 1);
    assert!((entry.realized_pnl - 25.0).abs() < f64::EPSILON);
}

#[test]
fn blacklist_ttl_expires() {
    let mut u = AutoUniverse::default();
    let banned_at = Utc::now() - ChronoDuration::hours(5);
    u.blacklist.insert(
        "EXP".into(),
        BlacklistEntry {
            symbol: "EXP".into(),
            banned_at,
            ttl_hours: 4,
            reason: BanReason::ConsecutiveLosses,
            last_loss_amount: -5.0,
            loss_strikes_at_ban: 2,
        },
    );
    assert!(u.is_blacklisted("EXP", banned_at + ChronoDuration::hours(2)));
    assert!(!u.is_blacklisted("EXP", banned_at + ChronoDuration::hours(5)));
    // Lazy cleanup happened during is_blacklisted.
    assert!(!u.blacklist.contains_key("EXP"));
}

#[test]
fn blacklist_ttl_zero_means_until_reset() {
    let u = AutoUniverse::default();
    let entry = BlacklistEntry {
        symbol: "PERM".into(),
        banned_at: Utc::now() - ChronoDuration::hours(48),
        ttl_hours: 0,
        reason: BanReason::Manual,
        last_loss_amount: 0.0,
        loss_strikes_at_ban: 0,
    };
    assert!(!entry.is_expired(Utc::now()));
    assert!(entry.remaining_secs(Utc::now()).is_none());
    // Clear-all wipes it (daily reset).
    let mut u2 = u;
    u2.blacklist.insert("PERM".into(), entry);
    u2.clear_all_blacklist();
    assert!(u2.blacklist.is_empty());
}

#[test]
fn manual_blacklist_works() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.auto_pool
        .insert("MAN".into(), AutoEntry::new("MAN".into(), 70.0, now()));
    u.manual_blacklist("MAN", &cfg, Some(0), now());
    assert!(u.blacklist.contains_key("MAN"));
    assert_eq!(u.blacklist["MAN"].reason, BanReason::Manual);
    assert!(!u.auto_pool.contains_key("MAN"));
}

#[test]
fn force_remove_clears_pinned_and_auto() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.pin("AAA", &cfg).unwrap();
    u.auto_pool
        .insert("BBB".into(), AutoEntry::new("BBB".into(), 70.0, now()));
    u.force_remove("AAA").unwrap();
    u.force_remove("BBB").unwrap();
    assert!(u.pinned.is_empty());
    assert!(u.auto_pool.is_empty());
}

#[test]
fn promote_auto_to_pinned_round_trip() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.auto_pool
        .insert("X".into(), AutoEntry::new("X".into(), 80.0, now()));
    u.promote_auto_to_pinned("X", &cfg).unwrap();
    assert!(u.is_pinned("X"));
    assert!(!u.auto_pool.contains_key("X"));
}

#[test]
fn active_set_is_uppercase_union() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.pin("aapl", &cfg).unwrap();
    u.auto_pool
        .insert("MSFT".into(), AutoEntry::new("MSFT".into(), 70.0, now()));
    let s = u.active_set();
    assert!(s.contains("AAPL"));
    assert!(s.contains("MSFT"));
    assert_eq!(s.len(), 2);
}

#[test]
fn snapshot_serde_roundtrip() {
    let mut u = AutoUniverse::default();
    let cfg = cfg_test();
    u.pin("AAPL", &cfg).unwrap();
    u.auto_pool.insert(
        "MSFT".into(),
        AutoEntry::new("MSFT".into(), 70.0, Utc::now()),
    );
    u.blacklist.insert(
        "BAD".into(),
        BlacklistEntry {
            symbol: "BAD".into(),
            banned_at: Utc::now(),
            ttl_hours: 4,
            reason: BanReason::ConsecutiveLosses,
            last_loss_amount: -5.0,
            loss_strikes_at_ban: 2,
        },
    );
    let json = serde_json::to_string(&u).unwrap();
    let loaded: AutoUniverse = serde_json::from_str(&json).unwrap();
    assert_eq!(loaded.pinned, vec!["AAPL".to_string()]);
    assert!(loaded.auto_pool.contains_key("MSFT"));
    assert!(loaded.blacklist.contains_key("BAD"));
}

#[test]
fn legacy_snapshot_without_auto_universe_loads_default() {
    // Snapshot saved before S18 — no auto_universe field.
    let json = "{}";
    let loaded: AutoUniverse = serde_json::from_str(json).unwrap();
    assert!(loaded.pinned.is_empty());
    assert!(loaded.auto_pool.is_empty());
    assert!(loaded.blacklist.is_empty());
}

#[test]
fn auto_universe_config_default_is_disabled() {
    let cfg = AutoUniverseConfig::default();
    assert!(!cfg.enabled, "S18 must be opt-in to preserve legacy behaviour");
    assert_eq!(cfg.max_total_slots, 20);
    assert_eq!(cfg.max_pinned_slots, 8);
    assert_eq!(cfg.loss_strikes_to_blacklist, 2);
}

#[test]
fn auto_universe_config_serde_legacy_compat() {
    // Old config without auto_universe block: defaults apply.
    let json = "{}";
    let cfg: AutoUniverseConfig = serde_json::from_str(json).unwrap();
    assert!(!cfg.enabled);
}

#[test]
fn cleanup_expired_blacklist_removes_old() {
    let mut u = AutoUniverse::default();
    u.blacklist.insert(
        "OLD".into(),
        BlacklistEntry {
            symbol: "OLD".into(),
            banned_at: Utc::now() - ChronoDuration::hours(10),
            ttl_hours: 4,
            reason: BanReason::ConsecutiveLosses,
            last_loss_amount: -5.0,
            loss_strikes_at_ban: 2,
        },
    );
    u.blacklist.insert(
        "FRESH".into(),
        BlacklistEntry {
            symbol: "FRESH".into(),
            banned_at: Utc::now(),
            ttl_hours: 4,
            reason: BanReason::ConsecutiveLosses,
            last_loss_amount: -5.0,
            loss_strikes_at_ban: 2,
        },
    );
    let removed = u.cleanup_expired_blacklist(Utc::now());
    assert!(removed.contains(&"OLD".to_string()));
    assert!(u.blacklist.contains_key("FRESH"));
    assert!(!u.blacklist.contains_key("OLD"));
}
