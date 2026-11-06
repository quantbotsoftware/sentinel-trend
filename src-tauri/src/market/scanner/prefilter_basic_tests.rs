//! S15 split — basic prefilter pipeline tests (price/volume/score/sort
//! gates + gap_scanner profile). Lifted from `prefilter_tests.rs`
//! alongside `prefilter_compliance_tests.rs` and
//! `prefilter_catalyst_tests.rs` to keep every sibling under R2's
//! 500-LOC cap. Helpers are duplicated across the three sibling files
//! (each is its own `mod`) — that is intentional: each sibling stays
//! self-contained for cohesion-driven reading.

use super::*;
use crate::market::market_state::HaltState;
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

/// S12: tests now always populate a fresh `latest_trade` (timestamp =
/// `rth_now()`) so the new STALE exclusion gate doesn't filter
/// unrelated test fixtures. Setting `p = close` keeps `current_price`
/// identical to the pre-S12 numerics (legacy snapshots resolved
/// `current_price` from `dailyBar.c`).
fn make_snapshot(close: f64, open: f64, vol: f64, prev_close: f64, prev_vol: f64) -> SnapshotData {
    SnapshotData {
        latest_trade: Some(LatestTrade {
            p: close,
            t: "2026-04-29T14:30:00Z".to_string(), // 10:30 ET — matches rth_now()
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

#[test]
fn test_prefilter_basic() {
    let mut snapshots = HashMap::new();
    snapshots.insert("AAPL".to_string(), make_snapshot(150.0, 145.0, 2_000_000.0, 140.0, 1_000_000.0));
    snapshots.insert("TSLA".to_string(), make_snapshot(200.0, 195.0, 3_000_000.0, 180.0, 1_500_000.0));
    snapshots.insert("PENNY".to_string(), make_snapshot(0.50, 0.45, 100_000.0, 0.40, 50_000.0));

    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };

    let assets = make_assets(&["AAPL", "TSLA", "PENNY"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.symbol != "PENNY"));
}

#[test]
fn test_prefilter_price_filter() {
    let mut snapshots = HashMap::new();
    snapshots.insert("CHEAP".to_string(), make_snapshot(0.50, 0.40, 5_000_000.0, 0.30, 2_000_000.0));
    snapshots.insert("GOOD".to_string(), make_snapshot(15.0, 14.0, 2_000_000.0, 12.0, 1_000_000.0));

    let params = ScanParams {
        price_min: 1.0,
        price_max: 20.0,
        volume_min: 1_000_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };

    let assets = make_assets(&["CHEAP", "GOOD"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].symbol, "GOOD");
}

#[test]
fn test_prefilter_earnings_exclusion() {
    let mut snapshots = HashMap::new();
    snapshots.insert("EARN".to_string(), make_snapshot(50.0, 48.0, 3_000_000.0, 40.0, 1_000_000.0));
    snapshots.insert("SAFE".to_string(), make_snapshot(50.0, 48.0, 3_000_000.0, 40.0, 1_000_000.0));

    let mut earnings = HashSet::new();
    earnings.insert("EARN".to_string());

    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 1_000_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        exclude_earnings: true,
        ..Default::default()
    };

    let assets = make_assets(&["EARN", "SAFE"]);
    let results = quant_prefilter(
        &snapshots, &params, &earnings, &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].symbol, "SAFE");
}

#[test]
fn test_prefilter_max_candidates() {
    let mut snapshots = HashMap::new();
    for i in 0..50 {
        let sym = format!("SYM{i:03}");
        snapshots.insert(sym, make_snapshot(
            10.0 + i as f64, 9.0, 2_000_000.0, 8.0, 1_000_000.0,
        ));
    }

    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        max_candidates: 10,
        ..Default::default()
    };

    let asset_syms: Vec<String> = (0..50).map(|i| format!("SYM{i:03}")).collect();
    let asset_refs: Vec<&str> = asset_syms.iter().map(String::as_str).collect();
    let assets = make_assets(&asset_refs);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 10);
}

#[test]
fn test_prefilter_sorted_by_score_desc() {
    let mut snapshots = HashMap::new();
    snapshots.insert("LOW".to_string(), make_snapshot(10.0, 9.5, 1_000_000.0, 9.0, 800_000.0));
    snapshots.insert("HIGH".to_string(), make_snapshot(20.0, 15.0, 8_000_000.0, 10.0, 1_000_000.0));

    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };

    let assets = make_assets(&["LOW", "HIGH"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert!(results.len() >= 2);
    assert!(results[0].score >= results[1].score);
}

#[test]
fn test_gap_scanner_accepts_continuation_pullback_setups() {
    let mut snapshots = HashMap::new();
    snapshots.insert(
        "CONT".to_string(),
        make_snapshot(104.5, 102.0, 2_200_000.0, 100.0, 1_000_000.0),
    );
    snapshots.insert(
        "PULL".to_string(),
        make_snapshot(102.0, 100.8, 1_900_000.0, 100.0, 1_000_000.0),
    );

    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 1_000_000.0,
        change_min: 1.5,
        rvol_min: 1.5,
        gap_min: 0.5,
        score_preset: "gap_scanner".to_string(),
        ..Default::default()
    };

    let assets = make_assets(&["CONT", "PULL"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|r| r.symbol == "CONT"));
    assert!(results.iter().any(|r| r.symbol == "PULL"));
}

#[test]
fn test_gap_scanner_rejects_overextended_and_noisy_setups() {
    let mut snapshots = HashMap::new();
    snapshots.insert(
        "GOOD".to_string(),
        make_snapshot(104.5, 102.0, 2_500_000.0, 100.0, 1_000_000.0),
    );
    snapshots.insert(
        "OVER".to_string(),
        make_snapshot(114.0, 110.0, 5_000_000.0, 100.0, 1_000_000.0),
    );
    snapshots.insert(
        "NOISE".to_string(),
        make_snapshot(101.05, 101.0, 1_200_000.0, 100.0, 1_050_000.0),
    );

    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 1_000_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        gap_min: 0.5,
        score_preset: "gap_scanner".to_string(),
        ..Default::default()
    };

    let assets = make_assets(&["GOOD", "OVER", "NOISE"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].symbol, "GOOD");
}
