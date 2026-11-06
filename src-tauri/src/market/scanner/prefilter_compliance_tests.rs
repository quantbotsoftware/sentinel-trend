//! S15 split — S11 compliance gate (Issue #14) + S12 stale exclusion +
//! S12 RVOL method dispatch tests. Sibling of `prefilter_basic_tests.rs`
//! and `prefilter_catalyst_tests.rs`. See the basic-tests file for the
//! rationale for the per-file helper duplication.

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

/// Tiny shared fixture for compliance tests — single tradable+shortable
/// symbol at numerics that easily clear the rest of the prefilter.
fn compliance_fixture() -> (HashMap<String, SnapshotData>, ScanParams) {
    let mut snapshots = HashMap::new();
    snapshots.insert(
        "SYM".to_string(),
        make_snapshot(150.0, 145.0, 5_000_000.0, 140.0, 1_000_000.0),
    );
    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };
    (snapshots, params)
}

// ─── S11 compliance gate (Issue #14) ───────────────────────────────

#[test]
fn compliance_gate_rejects_non_tradable() {
    let (snapshots, params) = compliance_fixture();
    let mut assets = make_assets(&["SYM"]);
    assets.get_mut("SYM").unwrap().tradable = false;
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert!(results.is_empty(), "tradable=false must be excluded");
}

#[test]
fn compliance_gate_rejects_unknown_asset() {
    let (snapshots, params) = compliance_fixture();
    let assets: HashMap<String, AlpacaAsset> = HashMap::new();
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert!(results.is_empty(), "missing asset metadata must be excluded");
}

#[test]
fn compliance_gate_allows_long_when_allow_short_false_and_not_shortable() {
    let (snapshots, params) = compliance_fixture();
    let mut assets = make_assets(&["SYM"]);
    assets.get_mut("SYM").unwrap().shortable = false;
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 1, "non-shortable still passes when allow_short=false");
}

#[test]
fn compliance_gate_blocks_non_shortable_when_allow_short_true() {
    let (snapshots, params) = compliance_fixture();
    let mut assets = make_assets(&["SYM"]);
    assets.get_mut("SYM").unwrap().shortable = false;
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, true, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert!(results.is_empty(), "non-shortable must be excluded when allow_short=true");
}

#[test]
fn s11_new_fields_populated_on_results() {
    use crate::market::scanner::types::LatestQuote;
    let (mut snapshots, params) = compliance_fixture();
    let snap = snapshots.get_mut("SYM").unwrap();
    snap.latest_trade = Some(LatestTrade {
        p: 152.0,
        t: "2026-04-29T14:30:00Z".to_string(),
        s: Some(100),
    });
    snap.latest_quote = Some(LatestQuote {
        bp: 151.95, ap: 152.05, bs: 100, as_: 100,
        t: "2026-04-29T14:30:00Z".to_string(),
    });
    let assets = make_assets(&["SYM"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!((r.price - 152.0).abs() < 0.01, "price must come from latestTrade.p");
    assert!((r.bid - 151.95).abs() < 0.01);
    assert!((r.ask - 152.05).abs() < 0.01);
    assert!(r.spread_bps > 0.0);
    assert!(r.last_trade_age_seconds < 5);
    assert!(!r.is_stale);
}

// ─── S12 stale exclusion + RVOL dispatch ───────────────────────────

/// rth_now() is 10:30 ET = 14:30 UTC. A trade at 14:24:00 UTC is
/// 6 minutes old in RTH → `is_stale = true` and S12 must drop the
/// candidate before any scoring.
#[test]
fn prefilter_skips_stale_candidates() {
    let (mut snapshots, params) = compliance_fixture();
    let snap = snapshots.get_mut("SYM").unwrap();
    snap.latest_trade = Some(LatestTrade {
        p: 150.0,
        t: "2026-04-29T14:24:00Z".to_string(),
        s: None,
    });
    let assets = make_assets(&["SYM"]);
    let results = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert!(
        results.is_empty(),
        "RTH-stale symbol must be excluded by S12 carry-forward"
    );
}

/// At 10:30 ET TimeAdjusted progress ≈ 0.21, so vol=10M / prev=10M
/// projects rvol ≈ 4.78 — passes a `rvol_min=4.0` filter.
/// The same numerics under Naive give rvol = 1.0 — fails the filter
/// and produces an empty result. This proves the dispatch is wired.
#[test]
fn rvol_method_naive_uses_legacy() {
    use crate::market::scanner::rvol::RvolMethod;
    let mut snapshots = HashMap::new();
    snapshots.insert(
        "SYM".to_string(),
        make_snapshot(150.0, 145.0, 10_000_000.0, 140.0, 10_000_000.0),
    );
    let mut params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 4.0,
        ..Default::default()
    };
    let assets = make_assets(&["SYM"]);

    params.rvol_method = RvolMethod::Naive;
    let r_naive = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert!(
        r_naive.is_empty(),
        "Naive rvol=1.0 must fail rvol_min=4.0, got {} candidates",
        r_naive.len()
    );

    params.rvol_method = RvolMethod::TimeAdjusted;
    let r_ta = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    assert_eq!(
        r_ta.len(), 1,
        "TimeAdjusted rvol≈4.78 must clear rvol_min=4.0"
    );
    assert!(
        r_ta[0].rvol > 4.0,
        "TimeAdjusted rvol = {}, want > 4.0",
        r_ta[0].rvol
    );
}

/// S12 hand-over: `Historical` is API-defined but maps to
/// `TimeAdjusted` internally. Verify the two variants produce
/// numerically identical results for the same inputs.
#[test]
fn rvol_method_historical_falls_back_to_time_adjusted() {
    use crate::market::scanner::rvol::RvolMethod;
    let mut snapshots = HashMap::new();
    snapshots.insert(
        "SYM".to_string(),
        make_snapshot(150.0, 145.0, 10_000_000.0, 140.0, 10_000_000.0),
    );
    let mut params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };
    let assets = make_assets(&["SYM"]);

    params.rvol_method = RvolMethod::TimeAdjusted;
    let r_ta = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );
    params.rvol_method = RvolMethod::Historical;
    let r_hist = quant_prefilter(
        &snapshots, &params, &HashSet::new(), &assets, false, rth_now(),
        &empty_halts(), &empty_news(), &cw(),
    );

    assert_eq!(r_ta.len(), 1);
    assert_eq!(r_hist.len(), 1);
    assert!(
        (r_ta[0].rvol - r_hist[0].rvol).abs() < 1e-9,
        "Historical (S12 fallback) must equal TimeAdjusted: ta={}, hist={}",
        r_ta[0].rvol, r_hist[0].rvol
    );
}

/// SC11: a print from yesterday ET must never enter actionable results,
/// even when age-based stale checks would still pass in dead time.
#[test]
fn sc11_rejects_prior_et_date_trade() {
    let mut snap = make_snapshot(150.0, 145.0, 2_000_000.0, 140.0, 1_000_000.0);
    snap.latest_trade = Some(LatestTrade {
        p: 150.0,
        t: "2026-04-28T19:59:00Z".to_string(), // prior ET day
        s: None,
    });
    let mut snapshots = HashMap::new();
    snapshots.insert("OLD".to_string(), snap);
    let params = ScanParams {
        price_min: 1.0,
        price_max: 500.0,
        volume_min: 500_000.0,
        change_min: 1.0,
        rvol_min: 1.0,
        ..Default::default()
    };
    let results = quant_prefilter(
        &snapshots,
        &params,
        &HashSet::new(),
        &make_assets(&["OLD"]),
        false,
        rth_now(),
        &empty_halts(),
        &empty_news(),
        &cw(),
    );
    assert!(results.is_empty(), "prior-session trade must be fail-closed");
}
