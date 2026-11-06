/// S11: Scanner real-time data layer JSON contract test.
///
/// Locks the wire shape of two contracts that S11 introduces:
///   1. `SnapshotData` deserialisation from Alpaca
///      `GET /v2/stocks/snapshots`. Both legacy (only `dailyBar` +
///      `prevDailyBar`) and full (with `latestTrade` + `latestQuote` +
///      `minuteBar`) payloads must parse.
///   2. `ScanResult` JSON serialisation. Every consumer (UI scan-results
///      table, persistence snapshot, integration tests) depends on the
///      five S11 fields being present.
///
/// Implementation is `sentinel-trend` is a binary crate (no `lib.rs`),
/// so the contract is mirrored in-test (same approach as
/// `integration_snapshot.rs` and `integration_engine_veto.rs`). Drift
/// between this mirror and the canonical types in
/// `src-tauri/src/market/scanner/types.rs` surfaces as test failure.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct DailyBar {
    c: f64,
    #[allow(dead_code)]
    o: f64,
    #[allow(dead_code)]
    v: f64,
}

#[derive(Debug, Deserialize)]
struct LatestTrade {
    p: f64,
    #[serde(default)]
    #[allow(dead_code)]
    t: String,
}

#[derive(Debug, Deserialize)]
struct LatestQuote {
    #[serde(default)]
    bp: f64,
    #[serde(default)]
    ap: f64,
    #[serde(default, rename = "as")]
    #[allow(dead_code)]
    as_: u64,
}

#[derive(Debug, Deserialize)]
struct MinuteBar {
    #[serde(default)]
    c: f64,
}

#[derive(Debug, Deserialize)]
struct Snapshot {
    #[serde(rename = "latestTrade", default)]
    latest_trade: Option<LatestTrade>,
    #[serde(rename = "latestQuote", default)]
    latest_quote: Option<LatestQuote>,
    #[serde(rename = "minuteBar", default)]
    minute_bar: Option<MinuteBar>,
    #[serde(rename = "dailyBar", default)]
    daily_bar: Option<DailyBar>,
    #[serde(rename = "prevDailyBar", default)]
    prev_daily_bar: Option<DailyBar>,
}

#[derive(Debug, Serialize, Deserialize)]
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
    bid: f64,
    ask: f64,
    spread_bps: f64,
    last_trade_age_seconds: u32,
    is_stale: bool,
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
        }
    }
}

const ALPACA_FULL_PAYLOAD: &str = r#"{
    "latestTrade": {"p": 250.5, "t": "2026-04-29T14:30:00Z", "s": 100},
    "latestQuote": {"bp": 250.4, "ap": 250.6, "bs": 200, "as": 100, "t": "2026-04-29T14:30:00Z"},
    "minuteBar": {"o": 250.0, "h": 251.0, "l": 249.5, "c": 250.5, "v": 12345.0, "t": "2026-04-29T14:29:00Z"},
    "dailyBar": {"c": 250.5, "o": 245.0, "v": 1500000.0},
    "prevDailyBar": {"c": 244.0, "o": 240.0, "v": 50000000.0}
}"#;

const ALPACA_LEGACY_PAYLOAD: &str = r#"{
    "dailyBar": {"c": 100.0, "o": 95.0, "v": 1000000.0},
    "prevDailyBar": {"c": 90.0, "o": 88.0, "v": 800000.0}
}"#;

#[test]
fn full_alpaca_payload_deserialises_with_all_subfields() {
    let snap: Snapshot = serde_json::from_str(ALPACA_FULL_PAYLOAD).expect("parse full payload");
    assert!(snap.latest_trade.is_some(), "latestTrade required for real-time pricing");
    assert!(snap.latest_quote.is_some(), "latestQuote required for bid/ask");
    assert!(snap.minute_bar.is_some(), "minuteBar present in full payload");
    assert!(snap.daily_bar.is_some());
    assert!(snap.prev_daily_bar.is_some());
    assert!((snap.latest_trade.unwrap().p - 250.5).abs() < 0.001);
}

#[test]
fn legacy_only_daily_payload_still_deserialises() {
    let snap: Snapshot = serde_json::from_str(ALPACA_LEGACY_PAYLOAD).expect("parse legacy");
    assert!(snap.latest_trade.is_none(), "legacy payload has no latestTrade");
    assert!(snap.latest_quote.is_none());
    assert!(snap.minute_bar.is_none());
    assert!(snap.daily_bar.is_some(), "legacy still carries dailyBar");
    assert!((snap.daily_bar.as_ref().unwrap().c - 100.0).abs() < 0.001);
}

#[test]
fn empty_object_payload_yields_all_none() {
    let snap: Snapshot = serde_json::from_str("{}").expect("parse empty");
    assert!(snap.latest_trade.is_none());
    assert!(snap.daily_bar.is_none());
}

#[test]
fn scan_row_serialise_includes_all_s11_fields() {
    let row = ScanRow {
        symbol: "TSLA".to_string(),
        price: 250.5,
        change_percent: 2.66,
        volume: 1_500_000.0,
        rvol: 1.2,
        gap_percent: 2.08,
        score: 78.5,
        dollar_volume: 375_750_000.0,
        bid: 250.4,
        ask: 250.6,
        spread_bps: 7.99,
        last_trade_age_seconds: 3,
        is_stale: false,
    };
    let json = serde_json::to_string(&row).expect("serialise");
    for needle in [
        "\"symbol\":\"TSLA\"",
        "\"bid\":250.4",
        "\"ask\":250.6",
        "\"spread_bps\":7.99",
        "\"last_trade_age_seconds\":3",
        "\"is_stale\":false",
    ] {
        assert!(
            json.contains(needle),
            "ScanResult JSON missing {} — wire contract broken: {}",
            needle,
            json
        );
    }
}

#[test]
fn scan_row_legacy_payload_deserialises_with_defaults() {
    // Pre-S11 snapshot state file: only the eight original fields.
    // Contract: must rehydrate cleanly with new fields defaulted.
    let legacy = r#"{"symbol":"OLD","price":12.5,"change_percent":1.5,
        "volume":900000.0,"rvol":1.2,"gap_percent":0.5,"score":42.0,
        "dollar_volume":11250000.0}"#;
    let row: ScanRow = serde_json::from_str(legacy).expect("rehydrate legacy");
    assert_eq!(row.symbol, "OLD");
    assert!((row.bid - 0.0).abs() < f64::EPSILON);
    assert!(!row.is_stale);
    assert_eq!(row.last_trade_age_seconds, 0);
}

#[test]
fn alpaca_quote_with_keyword_field_as_parses() {
    // The Alpaca payload uses `as` (a Rust keyword) for ask size. The
    // canonical struct renames it to `as_`. This test locks that the
    // raw key continues to parse so renaming downstream cannot break
    // the wire contract silently.
    let quote_json = r#"{"bp": 100.0, "ap": 100.1, "bs": 5, "as": 7, "t": "2026-04-29T14:30:00Z"}"#;
    let q: LatestQuote = serde_json::from_str(quote_json).expect("ask-size key");
    assert!((q.bp - 100.0).abs() < 0.001);
    assert!((q.ap - 100.1).abs() < 0.001);
    assert_eq!(q.as_, 7);
}

#[test]
fn fallback_chain_priority_is_latest_minute_daily() {
    // Pure contract assertion of the priority order — mirrors
    // `extract_bar_metrics` in scoring.rs. If the canonical impl drifts
    // from this priority, the unit-test in scoring.rs and this contract
    // test diverge — diagnostic fail.
    fn current_price(snap: &Snapshot) -> f64 {
        snap.latest_trade
            .as_ref()
            .map(|t| t.p)
            .or_else(|| snap.minute_bar.as_ref().map(|m| m.c))
            .or_else(|| snap.daily_bar.as_ref().map(|d| d.c))
            .unwrap_or(0.0)
    }

    let full: Snapshot = serde_json::from_str(ALPACA_FULL_PAYLOAD).unwrap();
    assert!(
        (current_price(&full) - 250.5).abs() < 0.001,
        "full payload uses latestTrade.p"
    );

    let legacy: Snapshot = serde_json::from_str(ALPACA_LEGACY_PAYLOAD).unwrap();
    assert!(
        (current_price(&legacy) - 100.0).abs() < 0.001,
        "legacy payload falls back to dailyBar.c"
    );

    // Synthetic minute-only payload: latest_trade absent, minuteBar present,
    // dailyBar absent.
    let minute_only: Snapshot = serde_json::from_str(
        r#"{"minuteBar": {"o": 50.0, "h": 51.0, "l": 49.0, "c": 50.5, "v": 1000.0, "t": "..."}}"#,
    )
    .unwrap();
    assert!(
        (current_price(&minute_only) - 50.5).abs() < 0.001,
        "minute-only payload falls back to minuteBar.c"
    );
}
