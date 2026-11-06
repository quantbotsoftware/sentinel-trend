//! S15 split — S13 catalyst integration tests on `quant_prefilter`.
//! Sibling of `prefilter_basic_tests.rs` and `prefilter_compliance_tests.rs`.
//! See the basic-tests file for the rationale for the per-file helper
//! duplication.

use super::*;
use crate::market::market_state::{HaltSource, HaltState};
use crate::market::scanner::types::LatestTrade;
use crate::market::scanner::SnapshotDailyBar;
use chrono::TimeZone;

fn cw() -> CatalystWeights {
    CatalystWeights::default()
}

fn empty_halts() -> HashMap<String, HaltState> {
    HashMap::new()
}

fn empty_news() -> HashMap<String, Vec<NewsItem>> {
    HashMap::new()
}

fn make_snapshot(close: f64, open: f64, vol: f64, prev_close: f64, prev_vol: f64) -> SnapshotData {
    SnapshotData {
        latest_trade: Some(LatestTrade {
            p: close,
            t: "2026-04-29T14:30:00Z".to_string(),
            s: None,
        }),
        latest_quote: None,
        minute_bar: None,
        daily_bar: Some(SnapshotDailyBar { c: close, o: open, v: vol }),
        prev_daily_bar: Some(SnapshotDailyBar { c: prev_close, o: prev_close, v: prev_vol }),
    }
}

fn make_assets(symbols: &[&str]) -> HashMap<String, AlpacaAsset> {
    symbols
        .iter()
        .map(|s| {
            (
                s.to_uppercase(),
                AlpacaAsset {
                    symbol: s.to_uppercase(),
                    tradable: true,
                    exchange: "NASDAQ".to_string(),
                    shortable: true,
                    easy_to_borrow: true,
                    fractionable: true,
                    marginable: true,
                    maintenance_margin_requirement: None,
                },
            )
        })
        .collect()
}

fn rth_now() -> chrono::DateTime<chrono_tz::Tz> {
    chrono_tz::US::Eastern
        .with_ymd_and_hms(2026, 4, 29, 10, 30, 0)
        .unwrap()
}

/// S13: earnings + gap up + RVOL spike → catalyst score >70 and pills
/// include EARN, GAP↑, VOL!.
///
/// IMPORTANT: a fresh `latest_trade` (matching `rth_now()`) is required
/// — the `make_snapshot` helper above already populates one — otherwise
/// `extract_bar_metrics` reports `last_trade_age_seconds = u32::MAX` and
/// the S13 RTH heuristic correctly flags the symbol as halted, which
/// would zero the catalyst score before earnings/gap/vol weights sum.
#[test]
fn prefilter_populates_catalyst_pills_on_earnings_match() {
    let mut snapshots = HashMap::new();
    // open=95 vs prev_close=90 → +5.55% gap; vol=5M vs prev=1M → vol_spike.
    let snap = make_snapshot(95.0, 95.0, 5_000_000.0, 90.0, 1_000_000.0);
    snapshots.insert("EARN".to_string(), snap);
    let mut earnings = HashSet::new();
    earnings.insert("EARN".to_string());
    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };
    let assets = make_assets(&["EARN"]);
    let results = quant_prefilter(
        &snapshots, &params, &earnings, &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(r.catalyst_flags.earnings, "earnings flag must be set");
    assert!(!r.catalyst_flags.halted);
    assert!(
        r.catalyst_flags.score > 70.0,
        "earn+gap+vol triple should score >70 (got {})",
        r.catalyst_flags.score
    );
    assert!(r.catalyst_pills.contains(&"EARN".to_string()));
    assert!(r.catalyst_pills.contains(&"GAP↑".to_string()));
    assert!(r.catalyst_pills.contains(&"VOL!".to_string()));
    assert!(!r.catalyst_pills.contains(&"HALT".to_string()));
}

/// S13: pre-existing status-sub halt → catalyst zeroes out and pills
/// surface only ["HALT"]. Symbol still appears in results (halt is
/// metadata, NOT a hard exclusion).
#[test]
fn prefilter_zeroes_catalyst_when_halted() {
    let mut snapshots = HashMap::new();
    snapshots.insert(
        "HLT".to_string(),
        make_snapshot(150.0, 145.0, 5_000_000.0, 140.0, 1_000_000.0),
    );
    let mut halts: HashMap<String, HaltState> = HashMap::new();
    halts.insert(
        "HLT".to_string(),
        HaltState {
            halted_since: chrono::Utc::now(),
            reason: Some("Trading Halt".to_string()),
            source: HaltSource::StatusSub,
            halt_code: Some("H".into()),
            pre_halt_price: None,
            resumed_at: None,
            resume_prints: Vec::new(),
        },
    );
    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };
    let assets = make_assets(&["HLT"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &halts, &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 1, "halt is metadata, not exclusion");
    let r = &results[0];
    assert!(r.catalyst_flags.halted);
    assert_eq!(r.catalyst_flags.score, 0.0);
    assert_eq!(r.catalyst_pills, vec!["HALT".to_string()]);
}
