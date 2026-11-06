//! Premarket Ignition profile — integration tests on `quant_prefilter`.
//! Sibling of the other `prefilter_*_tests.rs` files (helpers duplicated
//! per file by design, see `prefilter_basic_tests.rs`).

use super::*;
use crate::market::scanner::premarket::{IgnitionParams, PREMARKET_IGNITION_PRESET};
use crate::market::scanner::types::{LatestQuote, LatestTrade};
use crate::market::scanner::SnapshotDailyBar;
use chrono::TimeZone;

/// Wednesday 2026-04-29 07:30 ET — premarket.
fn premarket_now() -> chrono::DateTime<chrono_tz::Tz> {
    chrono_tz::US::Eastern
        .with_ymd_and_hms(2026, 4, 29, 7, 30, 0)
        .unwrap()
}

/// Snapshot with a 60s-old premarket print (fresh under the 300s rule).
fn pm_snapshot(price: f64, vol: f64, prev_close: f64, prev_vol: f64, spread_bps: f64) -> SnapshotData {
    let half = price * spread_bps / 10_000.0 / 2.0;
    SnapshotData {
        latest_trade: Some(LatestTrade {
            p: price,
            t: "2026-04-29T11:29:00Z".to_string(),
            s: None,
        }),
        latest_quote: (spread_bps > 0.0).then(|| LatestQuote {
            bp: price - half,
            ap: price + half,
            bs: 100,
            as_: 100,
            t: "2026-04-29T11:29:00Z".to_string(),
        }),
        minute_bar: None,
        // dailyBar.o = 04:00 print ≈ prev_close: no "gap" yet, all in change%.
        daily_bar: Some(SnapshotDailyBar { c: price, o: prev_close * 1.001, v: vol }),
        prev_daily_bar: Some(SnapshotDailyBar { c: prev_close, o: prev_close, v: prev_vol }),
    }
}

fn assets(symbols: &[&str]) -> HashMap<String, AlpacaAsset> {
    symbols
        .iter()
        .map(|s| {
            (
                s.to_string(),
                AlpacaAsset {
                    symbol: s.to_string(),
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

fn news_for(sym: &str, created_at: &str) -> HashMap<String, Vec<NewsItem>> {
    let mut m = HashMap::new();
    m.insert(
        sym.to_string(),
        vec![NewsItem {
            id: 1,
            headline: format!("{sym} announces FDA approval"),
            author: String::new(),
            created_at: created_at.to_string(),
            updated_at: String::new(),
            symbols: vec![sym.to_string()],
            source: "benzinga".into(),
            url: "https://example.com".into(),
        }],
    );
    m
}

fn ignition_params() -> ScanParams {
    ScanParams {
        price_min: 1.0,
        price_max: 100.0,
        volume_min: 0.0,
        change_min: 8.0,
        rvol_min: 3.0,
        gap_min: 5.0, // must be ignored by the ignition profile
        max_candidates: 40,
        score_preset: PREMARKET_IGNITION_PRESET.to_string(),
        ignition: IgnitionParams::default(),
        ..ScanParams::default()
    }
}

fn run(snaps: HashMap<String, SnapshotData>, params: &ScanParams, news: HashMap<String, Vec<NewsItem>>) -> Vec<ScanResult> {
    let syms: Vec<&str> = snaps.keys().map(|s| s.as_str()).collect();
    let asset_map = assets(&syms);
    quant_prefilter(
        &snaps,
        params,
        &HashSet::new(),
        &asset_map,
        false,
        premarket_now(),
        &HashMap::new(),
        &news,
        &CatalystWeights::default(),
    )
}

/// The whole point: a $4 biotech +45 % on fresh news with 400 k shares
/// premarket must outrank a $60 large cap +9 % on 2 M shares and no story,
/// even though the legacy composite would prefer the latter's volume.
#[test]
fn ignition_ranks_catalyst_runner_above_quiet_large_cap() {
    let mut snaps = HashMap::new();
    // RUNR: 4.35 vs 3.00 prev (+45 %), 400k premarket vs 800k prev day → rvol huge.
    snaps.insert("RUNR".to_string(), pm_snapshot(4.35, 400_000.0, 3.0, 800_000.0, 80.0));
    // BIGC: 65.4 vs 60 (+9 %), 2M premarket vs 10M prev day (rvol ≈ 3.9 at 07:30).
    snaps.insert("BIGC".to_string(), pm_snapshot(65.4, 2_000_000.0, 60.0, 10_000_000.0, 5.0));
    let news = news_for("RUNR", "2026-04-29T10:45:00Z"); // 44 min old → fresh tier
    let res = run(snaps, &ignition_params(), news);
    let syms: Vec<&str> = res.iter().map(|r| r.symbol.as_str()).collect();
    assert_eq!(syms, vec!["RUNR", "BIGC"], "runner must rank first: {res:?}");
    assert!(res[0].score > res[1].score);
    assert!(res[0].catalyst_flags.news_recent);
    assert!(res[0].score >= 55.0, "runner should clear the alert floor: {}", res[0].score);
}

#[test]
fn ignition_uses_dollar_volume_floor_not_share_floor() {
    let mut snaps = HashMap::new();
    // 20 k shares × $40 = $800 k → passes $300 k floor despite tiny share count
    // (prev day 100 k → rvol ≈ 3.9 at 07:30, clears rvol_min).
    snaps.insert("PRICY".to_string(), pm_snapshot(40.0, 20_000.0, 33.0, 100_000.0, 20.0));
    // 200 k × $1.20 = $240 k → rejected by the dollar_volume floor even
    // though rvol ≈ 7.9 and change +20 % pass.
    snaps.insert("THIN".to_string(), pm_snapshot(1.20, 200_000.0, 1.0, 500_000.0, 20.0));
    let res = run(snaps, &ignition_params(), HashMap::new());
    let syms: Vec<&str> = res.iter().map(|r| r.symbol.as_str()).collect();
    assert_eq!(syms, vec!["PRICY"]);
}

#[test]
fn ignition_ignores_gap_min_and_enforces_change_min_and_spread() {
    let mut snaps = HashMap::new();
    // +12 % change, "gap" (04:00 print) only +0.1 % → legacy gap_min=5 would kill it.
    snaps.insert("MOVE".to_string(), pm_snapshot(11.2, 200_000.0, 10.0, 1_000_000.0, 40.0));
    // +5 % change → below change_min 8.
    snaps.insert("MEH".to_string(), pm_snapshot(10.5, 500_000.0, 10.0, 1_000_000.0, 40.0));
    // +30 % but 400 bps spread → rejected (max 200).
    snaps.insert("WIDE".to_string(), pm_snapshot(13.0, 500_000.0, 10.0, 1_000_000.0, 400.0));
    let res = run(snaps, &ignition_params(), HashMap::new());
    let syms: Vec<&str> = res.iter().map(|r| r.symbol.as_str()).collect();
    assert_eq!(syms, vec!["MOVE"]);
}

#[test]
fn ignition_require_catalyst_drops_storyless_movers() {
    let mut snaps = HashMap::new();
    snaps.insert("STORY".to_string(), pm_snapshot(12.0, 300_000.0, 10.0, 1_000_000.0, 30.0));
    snaps.insert("NOSTORY".to_string(), pm_snapshot(12.0, 300_000.0, 10.0, 1_000_000.0, 30.0));
    let mut params = ignition_params();
    params.ignition.require_catalyst = true;
    let res = run(snaps, &params, news_for("STORY", "2026-04-29T09:00:00Z"));
    let syms: Vec<&str> = res.iter().map(|r| r.symbol.as_str()).collect();
    assert_eq!(syms, vec!["STORY"]);
}

/// Same universe, legacy `balanced` preset → the share-count floor and
/// RTH-shaped composite still apply (regression guard: ignition changes
/// nothing unless the preset is selected).
#[test]
fn legacy_preset_unchanged_by_ignition_code_path() {
    let mut snaps = HashMap::new();
    snaps.insert("PRICY".to_string(), pm_snapshot(40.0, 20_000.0, 33.0, 100_000.0, 20.0));
    let mut params = ignition_params();
    params.score_preset = "balanced".into();
    params.volume_min = 50_000.0;
    params.gap_min = 0.0;
    let res = run(snaps, &params, HashMap::new());
    assert!(res.is_empty(), "20k shares < 50k volume_min under legacy preset");
}
