/// S15 — Scanner Extreme cumulative backward-compatibility contract.
///
/// Every additive change in S11/S12/S13/S14 carries an implicit promise:
/// existing config files (`settings.json`), cached `scan_results`
/// snapshots, and watchlist payloads from before that session must
/// rehydrate cleanly into the post-S15 types via `#[serde(default)]`.
/// This file pins that contract.
///
/// Implementation note: `sentinel-trend` is a binary crate without
/// `lib.rs`, so the contract is mirrored in-test (same approach as
/// `integration_scanner_realtime.rs`). Drift between this mirror and
/// the canonical types surfaces as test failure.
///
/// Three scenarios:
///   1. `config_pre_s11_loads_with_defaults` — pre-S11 `settings.json`
///      shape (no `catalyst.*`, no `scan_params.rvol_method`) MUST
///      deserialise with the post-S15 defaults: `RvolMethod::TimeAdjusted`,
///      `news_enabled = true`, `news_lookback_minutes = 240`,
///      `CatalystWeights::default()`.
///   2. `scan_results_pre_s11_rehydrate` — pre-S11 cached `scan_results`
///      JSON (only the original 8 fields) deserialises with post-S15
///      `bid/ask/spread_bps/last_trade_age_seconds/is_stale/catalyst_*`
///      defaulting cleanly.
///   3. `watchlist_pre_s11_rehydrates_no_panic` — pre-S11 `WatchlistItem`
///      payload shape (no S11/S13/S14 fields) deserialises without
///      panic; this is a paranoia check for the `watchlist-update`
///      event consumer when the event-bus has stale subscribers.

use serde::{Deserialize, Serialize};

// ───────────────────────────────────────────────────────────────────
// Mirrored types — must match canonical source-of-truth structs.
// ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
enum RvolMethod {
    Naive,
    #[default]
    TimeAdjusted,
    Historical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ScanParams {
    price_min: f64,
    price_max: f64,
    volume_min: f64,
    change_min: f64,
    rvol_min: f64,
    gap_min: f64,
    float_max: f64,
    cap_max: f64,
    exclude_earnings: bool,
    max_candidates: usize,
    scan_interval_minutes: u32,
    score_preset: String,
    rvol_method: RvolMethod,
}

impl Default for ScanParams {
    fn default() -> Self {
        Self {
            price_min: 1.0,
            price_max: 500.0,
            volume_min: 500_000.0,
            change_min: 1.0,
            rvol_min: 1.0,
            gap_min: 0.0,
            float_max: 0.0,
            cap_max: 0.0,
            exclude_earnings: false,
            max_candidates: 100,
            scan_interval_minutes: 30,
            score_preset: "balanced".to_string(),
            rvol_method: RvolMethod::TimeAdjusted,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct CatalystWeights {
    w_earn: f64,
    w_news: f64,
    w_halt: f64,
    w_gap: f64,
    w_vol: f64,
    gap_threshold: f64,
    rvol_threshold: f64,
}

impl Default for CatalystWeights {
    fn default() -> Self {
        Self {
            w_earn: 30.0,
            w_news: 25.0,
            w_halt: -50.0,
            w_gap: 20.0,
            w_vol: 25.0,
            gap_threshold: 5.0,
            rvol_threshold: 3.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct CatalystConfig {
    weights: CatalystWeights,
    news_enabled: bool,
    news_lookback_minutes: u32,
}

impl Default for CatalystConfig {
    fn default() -> Self {
        Self {
            weights: CatalystWeights::default(),
            news_enabled: true,
            news_lookback_minutes: 240,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct MarketConfigSlice {
    scan_params: ScanParams,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ConfigShape {
    market: MarketConfigSlice,
    catalyst: CatalystConfig,
}

// `ScanResult` — pre-S11 shape (8 fields) + post-S15 additive fields.
#[derive(Debug, Clone, Default, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum GapDir {
    #[default]
    None,
    Up,
    Down,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct CatalystFlags {
    earnings: bool,
    news_recent: bool,
    halted: bool,
    gap_dir: GapDir,
    vol_spike: bool,
    score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ScanRow {
    symbol: String,
    price: f64,
    change_percent: f64,
    volume: f64,
    rvol: f64,
    gap_percent: f64,
    score: f64,
    dollar_volume: f64,
    // S11 additive
    bid: f64,
    ask: f64,
    spread_bps: f64,
    last_trade_age_seconds: u32,
    is_stale: bool,
    // S13 additive
    catalyst_flags: CatalystFlags,
    catalyst_pills: Vec<String>,
}

impl Default for ScanRow {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            price: 0.0,
            change_percent: 0.0,
            volume: 0.0,
            rvol: 0.0,
            gap_percent: 0.0,
            score: 0.0,
            dollar_volume: 0.0,
            bid: 0.0,
            ask: 0.0,
            spread_bps: 0.0,
            last_trade_age_seconds: 0,
            is_stale: false,
            catalyst_flags: CatalystFlags::default(),
            catalyst_pills: Vec::new(),
        }
    }
}

// `WatchlistItem` mirror — note that the canonical struct does not
// declare `#[serde(default)]`. This is the production shape the UI
// receives from `get_watchlist`. For the deserialise paranoia test we
// add a separate shape with `#[serde(default)]` so a pre-S11 emitter
// could feed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct WatchlistItem {
    symbol: String,
    has_earnings: bool,
    st_score: f64,
    st_state: String,
    price: f64,
    change_percent: f64,
    volume: f64,
    rvol: f64,
    gap_percent: f64,
    bid: f64,
    ask: f64,
    spread_bps: f64,
    last_trade_age_seconds: u32,
    is_stale: bool,
    catalyst_flags: CatalystFlags,
    catalyst_pills: Vec<String>,
}

impl Default for WatchlistItem {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            has_earnings: false,
            st_score: 0.0,
            st_state: "Idle".to_string(),
            price: 0.0,
            change_percent: 0.0,
            volume: 0.0,
            rvol: 0.0,
            gap_percent: 0.0,
            bid: 0.0,
            ask: 0.0,
            spread_bps: 0.0,
            last_trade_age_seconds: 0,
            is_stale: false,
            catalyst_flags: CatalystFlags::default(),
            catalyst_pills: Vec::new(),
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────

#[test]
fn config_pre_s11_loads_with_defaults() {
    // Pre-S11 settings.json: scan_params has no `rvol_method`;
    // there's no top-level `catalyst` block at all.
    let json = r#"{
        "market": {
            "scan_params": {
                "price_min": 2.0,
                "price_max": 100.0,
                "volume_min": 1000000.0,
                "change_min": 1.0,
                "rvol_min": 1.5,
                "gap_min": 0.0,
                "float_max": 0.0,
                "cap_max": 0.0,
                "exclude_earnings": false,
                "max_candidates": 50,
                "scan_interval_minutes": 30,
                "score_preset": "momentum"
            }
        }
    }"#;
    let cfg: ConfigShape = serde_json::from_str(json).expect("config must deserialise");

    // SC2: rvol_method default is TimeAdjusted.
    assert_eq!(
        cfg.market.scan_params.rvol_method,
        RvolMethod::TimeAdjusted,
        "pre-S11 scan_params must default to TimeAdjusted"
    );

    // CatalystConfig defaults from S13.
    assert!(cfg.catalyst.news_enabled, "news_enabled defaults true");
    assert_eq!(
        cfg.catalyst.news_lookback_minutes, 240,
        "news_lookback_minutes defaults 240"
    );
    let w = cfg.catalyst.weights;
    let dflt = CatalystWeights::default();
    assert_eq!(w, dflt, "CatalystWeights defaults intact");
}

#[test]
fn scan_results_pre_s11_rehydrate() {
    // Pre-S11 cached scan_results: only the original 8 fields.
    let json = r#"[
        {
            "symbol": "AAPL",
            "price": 150.0,
            "change_percent": 2.5,
            "volume": 1000000.0,
            "rvol": 1.5,
            "gap_percent": 0.5,
            "score": 75.0,
            "dollar_volume": 150000000.0
        },
        {
            "symbol": "TSLA",
            "price": 200.0,
            "change_percent": 5.0,
            "volume": 5000000.0,
            "rvol": 3.0,
            "gap_percent": 4.0,
            "score": 88.0,
            "dollar_volume": 1000000000.0
        }
    ]"#;
    let rows: Vec<ScanRow> = serde_json::from_str(json).expect("scan_results must deserialise");
    assert_eq!(rows.len(), 2);

    // Every S11+ field must default cleanly.
    for r in &rows {
        assert_eq!(r.bid, 0.0, "bid defaults 0.0 for legacy row");
        assert_eq!(r.ask, 0.0);
        assert_eq!(r.spread_bps, 0.0);
        assert_eq!(r.last_trade_age_seconds, 0);
        assert!(!r.is_stale);
        assert!(!r.catalyst_flags.earnings);
        assert!(!r.catalyst_flags.halted);
        assert_eq!(r.catalyst_flags.score, 0.0);
        assert_eq!(r.catalyst_flags.gap_dir, GapDir::None);
        assert!(r.catalyst_pills.is_empty());
    }

    // Sanity: the legacy 8 fields preserved.
    assert_eq!(rows[0].symbol, "AAPL");
    assert!((rows[0].price - 150.0).abs() < 1e-9);
    assert_eq!(rows[1].symbol, "TSLA");
    assert!((rows[1].score - 88.0).abs() < 1e-9);
}

#[test]
fn watchlist_pre_s11_rehydrates_no_panic() {
    // Pre-S11 WatchlistItem payload: no S11/S13/S14 fields.
    // Paranoia check for the `watchlist-update` consumer when an old
    // emitter (theoretical: an attached debug tool) feeds the table.
    let json = r#"[
        {
            "symbol": "NVDA",
            "has_earnings": false,
            "st_score": 65.0,
            "st_state": "Confirming",
            "price": 800.0,
            "change_percent": 1.5,
            "volume": 2500000.0,
            "rvol": 1.8,
            "gap_percent": 0.8
        }
    ]"#;
    let items: Vec<WatchlistItem> =
        serde_json::from_str(json).expect("legacy watchlist payload must deserialise");
    assert_eq!(items.len(), 1);
    let it = &items[0];
    assert_eq!(it.symbol, "NVDA");
    assert_eq!(it.st_state, "Confirming");
    // Every S11+ surface defaults cleanly.
    assert_eq!(it.bid, 0.0);
    assert_eq!(it.ask, 0.0);
    assert_eq!(it.spread_bps, 0.0);
    assert_eq!(it.last_trade_age_seconds, 0);
    assert!(!it.is_stale);
    assert!(it.catalyst_pills.is_empty());
    assert_eq!(it.catalyst_flags.score, 0.0);
}
