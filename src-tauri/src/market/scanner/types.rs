use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::premarket::IgnitionParams;
use super::rvol::RvolMethod;
use crate::market::catalyst::CatalystFlags;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanParams {
    pub price_min: f64,
    pub price_max: f64,
    pub volume_min: f64,
    pub change_min: f64,
    pub rvol_min: f64,
    pub gap_min: f64,
    pub float_max: f64,
    pub cap_max: f64,
    pub exclude_earnings: bool,
    pub max_candidates: usize,
    /// Cadence used during RTH (09:30–15:30 ET). Default `30` minutes.
    pub scan_interval_minutes: u32,
    /// S19 — Cadence used during premarket (04:05–09:30 ET). Default
    /// `10` minutes (gappers / news catalysts move fast pre-bell, so
    /// the watchlist needs a tighter refresh). Falls back to
    /// `scan_interval_minutes` if a legacy `settings.json` omits it.
    pub scan_interval_premarket_minutes: u32,
    pub score_preset: String,
    /// S12: which RVOL formula `quant_prefilter` applies.
    /// Defaults to `TimeAdjusted` so legacy `settings.json` without
    /// this field upgrades silently via `#[serde(default)]`.
    pub rvol_method: RvolMethod,
    /// Premarket Ignition profile knobs (active only when
    /// `score_preset == "premarket_ignition"`). See `scanner::premarket`.
    pub ignition: IgnitionParams,
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
            scan_interval_premarket_minutes: 10,
            score_preset: "balanced".to_string(),
            rvol_method: RvolMethod::TimeAdjusted,
            ignition: IgnitionParams::default(),
        }
    }
}

/// Result row for a single symbol surfaced by the scanner.
///
/// S11 (2026-04-29) extended this struct with five real-time fields
/// (`bid`, `ask`, `spread_bps`, `last_trade_age_seconds`, `is_stale`).
/// S13 (2026-04-29 parallel) extended again with `catalyst_flags` and
/// `catalyst_pills`. `#[serde(default)]` keeps legacy cached
/// `scan_results` deserialising cleanly — missing fields fall back to
/// `Default::default()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanResult {
    pub symbol: String,
    pub price: f64,
    pub change_percent: f64,
    pub volume: f64,
    pub rvol: f64,
    pub gap_percent: f64,
    pub score: f64,
    pub dollar_volume: f64,
    pub bid: f64,
    pub ask: f64,
    pub spread_bps: f64,
    pub last_trade_age_seconds: u32,
    pub is_stale: bool,
    /// S13: catalyst flags (earn/news/halt/gap_dir/vol_spike + score 0-100).
    pub catalyst_flags: CatalystFlags,
    /// S13: UI-ready pill labels derived from `catalyst_flags`. Pre-computed
    /// so the frontend (S14) can render without re-implementing the rules.
    pub catalyst_pills: Vec<String>,
    /// Newest Alpaca headline for this symbol (empty when no news cached).
    /// Wired so the UI can show why a premarket name is exploding.
    #[serde(default)]
    pub news_headline: String,
    /// Canonical article URL for `news_headline` (http/https). Empty when
    /// the wire item had no URL — UI then falls back to headline-only.
    #[serde(default)]
    pub news_url: String,
    /// SC11 — ET calendar date (`YYYY-MM-DD`) of the scan session that
    /// produced this row. UI/API ignore rows whose date ≠ today ET.
    #[serde(default)]
    pub session_date_et: String,
}

impl Default for ScanResult {
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
            news_headline: String::new(),
            news_url: String::new(),
            session_date_et: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProgressEvent {
    pub phase: String,
    pub label: String,
    pub percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
}

pub type ProgressReporter = Arc<dyn Fn(ScanProgressEvent) + Send + Sync>;

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotDailyBar {
    pub c: f64,
    pub o: f64,
    pub v: f64,
}

/// S11: real-time print from Alpaca SIP feed. `t` is RFC3339 UTC.
/// `s` (size) is parsed when present but currently unused by the scanner.
#[derive(Debug, Clone, Deserialize)]
pub struct LatestTrade {
    pub p: f64,
    pub t: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub s: Option<u64>,
}

/// S11: NBBO snapshot. `as` collides with a Rust keyword and is renamed.
#[derive(Debug, Clone, Deserialize)]
pub struct LatestQuote {
    #[serde(default)]
    pub bp: f64,
    #[serde(default)]
    pub ap: f64,
    #[serde(default)]
    #[allow(dead_code)]
    pub bs: u64,
    #[serde(default, rename = "as")]
    #[allow(dead_code)]
    pub as_: u64,
    #[serde(default)]
    #[allow(dead_code)]
    pub t: String,
}

/// S11: latest 1-minute aggregate bar. Fallback price/volume source when
/// `dailyBar` is absent (e.g. brand-new IPO, weekend snapshot, very early
/// premarket before the daily bar has been minted).
#[derive(Debug, Clone, Deserialize)]
pub struct MinuteBar {
    #[serde(default)]
    pub o: f64,
    #[serde(default)]
    #[allow(dead_code)]
    pub h: f64,
    #[serde(default)]
    #[allow(dead_code)]
    pub l: f64,
    #[serde(default)]
    pub c: f64,
    #[serde(default)]
    pub v: f64,
    #[serde(default)]
    #[allow(dead_code)]
    pub t: String,
}

/// Full payload returned by Alpaca `GET /v2/stocks/snapshots`. Every
/// sub-field is `Option` so that scanners can tolerate partial payloads
/// and so legacy snapshots (pre-S11, only `dailyBar` populated) keep
/// deserialising. Resolution into a usable price/volume happens in
/// `scoring::extract_bar_metrics`.
#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotData {
    #[serde(rename = "latestTrade", default)]
    pub latest_trade: Option<LatestTrade>,
    #[serde(rename = "latestQuote", default)]
    pub latest_quote: Option<LatestQuote>,
    #[serde(rename = "minuteBar", default)]
    pub minute_bar: Option<MinuteBar>,
    #[serde(rename = "dailyBar", default)]
    pub daily_bar: Option<SnapshotDailyBar>,
    #[serde(rename = "prevDailyBar", default)]
    pub prev_daily_bar: Option<SnapshotDailyBar>,
}

/// Subset of Alpaca `/v2/assets` response used by the scanner and by S02
/// compliance gates (shortable/easy_to_borrow/fractionable). All extra
/// fields default to `false`/`None` so a stale cache (pre-S02) deserialises
/// without breaking; bot.rs Gate 0 then treats missing flags as
/// `AssetUnknown` and rejects conservatively.
#[derive(Debug, Clone, Deserialize)]
pub struct AlpacaAsset {
    pub symbol: String,
    pub tradable: bool,
    #[serde(default)]
    pub exchange: String,
    #[serde(default)]
    pub shortable: bool,
    #[serde(default)]
    pub easy_to_borrow: bool,
    #[serde(default)]
    pub fractionable: bool,
    /// S02: populated for completeness; consumed in S03+ margin calculations.
    #[serde(default)]
    #[allow(dead_code)]
    pub marginable: bool,
    /// S02: populated for completeness; consumed in S03+ margin calculations.
    #[serde(default)]
    #[allow(dead_code)]
    pub maintenance_margin_requirement: Option<f64>,
}

pub struct ScoreInput {
    pub change_pct: f64,
    pub rvol: f64,
    pub gap_pct: f64,
    pub volume: f64,
    pub prev_volume: f64,
}

pub struct ScoreWeights {
    pub w_change: f64,
    pub w_rvol: f64,
    pub w_gap: f64,
    pub w_volume: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_params_default() {
        let p = ScanParams::default();
        assert!((p.price_min - 1.0).abs() < 0.01);
        assert!((p.price_max - 500.0).abs() < 0.01);
        assert_eq!(p.max_candidates, 100);
        assert_eq!(p.score_preset, "balanced");
        assert!(!p.exclude_earnings);
    }

    #[test]
    fn test_scan_params_serde_roundtrip() {
        let p = ScanParams {
            price_min: 5.0,
            price_max: 50.0,
            volume_min: 2_000_000.0,
            change_min: 10.0,
            rvol_min: 5.0,
            gap_min: 2.0,
            float_max: 20_000_000.0,
            cap_max: 2_000_000_000.0,
            exclude_earnings: true,
            max_candidates: 50,
            scan_interval_minutes: 15,
            scan_interval_premarket_minutes: 5,
            score_preset: "momentum".to_string(),
            rvol_method: super::super::rvol::RvolMethod::Naive,
            ignition: IgnitionParams::default(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: ScanParams = serde_json::from_str(&json).unwrap();
        assert!((p2.price_min - 5.0).abs() < 0.01);
        assert_eq!(p2.score_preset, "momentum");
        assert!(p2.exclude_earnings);
        assert_eq!(p2.rvol_method, super::super::rvol::RvolMethod::Naive);
    }

    /// S12 backward-compat: legacy ScanParams JSON without `rvol_method`
    /// must deserialise with `rvol_method = TimeAdjusted` (default).
    #[test]
    fn config_without_rvol_method_defaults_timeadjusted() {
        let legacy_json = r#"{
            "price_min": 1.0,
            "price_max": 500.0,
            "volume_min": 500000.0,
            "change_min": 1.0,
            "rvol_min": 1.0,
            "gap_min": 0.0,
            "float_max": 0.0,
            "cap_max": 0.0,
            "exclude_earnings": false,
            "max_candidates": 100,
            "scan_interval_minutes": 30,
            "score_preset": "balanced"
        }"#;
        let p: ScanParams = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(
            p.rvol_method,
            super::super::rvol::RvolMethod::TimeAdjusted,
            "missing rvol_method in legacy JSON must default to TimeAdjusted"
        );
    }

    #[test]
    fn test_scan_result_serialize_includes_new_fields() {
        let r = ScanResult {
            symbol: "TEST".into(),
            price: 10.0,
            change_percent: 5.0,
            volume: 1_000_000.0,
            rvol: 3.0,
            gap_percent: 2.0,
            score: 75.0,
            dollar_volume: 10_000_000.0,
            bid: 9.99,
            ask: 10.01,
            spread_bps: 20.0,
            last_trade_age_seconds: 3,
            is_stale: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("TEST"));
        assert!(json.contains("\"bid\":9.99"));
        assert!(json.contains("\"ask\":10.01"));
        assert!(json.contains("\"spread_bps\":20.0"));
        assert!(json.contains("\"last_trade_age_seconds\":3"));
        assert!(json.contains("\"is_stale\":false"));
        assert!(json.contains("\"catalyst_flags\""));
        assert!(json.contains("\"catalyst_pills\""));
    }

    /// S13: legacy cached `scan_results` (post-S11 / pre-S13) without
    /// catalyst fields must rehydrate cleanly via `#[serde(default)]`.
    #[test]
    fn test_scan_result_legacy_post_s11_payload_defaults_catalyst() {
        let json = r#"{"symbol":"OLD","price":12.5,"change_percent":1.5,
            "volume":900000.0,"rvol":1.2,"gap_percent":0.5,"score":42.0,
            "dollar_volume":11250000.0,"bid":12.4,"ask":12.6,
            "spread_bps":160.0,"last_trade_age_seconds":2,"is_stale":false}"#;
        let r: ScanResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.symbol, "OLD");
        assert!(r.catalyst_pills.is_empty());
        assert_eq!(r.catalyst_flags.score, 0.0);
        assert!(!r.catalyst_flags.halted);
    }

    #[test]
    fn test_scan_result_default_safe_values() {
        let r = ScanResult::default();
        assert_eq!(r.symbol, "");
        assert!(!r.is_stale);
        assert_eq!(r.last_trade_age_seconds, 0);
        assert!((r.bid - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scan_result_legacy_payload_deserializes() {
        let legacy_json = r#"{"symbol":"OLD","price":12.5,"change_percent":1.5,
            "volume":900000.0,"rvol":1.2,"gap_percent":0.5,"score":42.0,
            "dollar_volume":11250000.0}"#;
        let r: ScanResult = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(r.symbol, "OLD");
        assert!((r.price - 12.5).abs() < 0.01);
        assert!((r.bid - 0.0).abs() < f64::EPSILON);
        assert!(!r.is_stale);
        assert_eq!(r.last_trade_age_seconds, 0);
    }

    #[test]
    fn test_snapshot_serde_with_latest_trade() {
        let json = r#"{
            "latestTrade": {"p": 250.5, "t": "2026-04-29T14:30:00Z", "s": 100},
            "latestQuote": {"bp": 250.4, "ap": 250.6, "bs": 200, "as": 100, "t": "2026-04-29T14:30:00Z"},
            "minuteBar": {"o": 250.0, "h": 251.0, "l": 249.5, "c": 250.5, "v": 12345.0, "t": "2026-04-29T14:29:00Z"},
            "dailyBar": {"c": 250.5, "o": 245.0, "v": 1500000.0},
            "prevDailyBar": {"c": 244.0, "o": 240.0, "v": 50000000.0}
        }"#;
        let snap: SnapshotData = serde_json::from_str(json).unwrap();
        assert!(snap.latest_trade.is_some());
        assert!(snap.latest_quote.is_some());
        assert!(snap.minute_bar.is_some());
        assert!(snap.daily_bar.is_some());
        assert!(snap.prev_daily_bar.is_some());
        assert!((snap.latest_trade.as_ref().unwrap().p - 250.5).abs() < 0.001);
        assert!((snap.latest_quote.as_ref().unwrap().bp - 250.4).abs() < 0.001);
        assert!((snap.latest_quote.as_ref().unwrap().ap - 250.6).abs() < 0.001);
        assert!((snap.minute_bar.as_ref().unwrap().c - 250.5).abs() < 0.001);
    }

    #[test]
    fn test_snapshot_serde_legacy_only_daily() {
        let legacy_json = r#"{
            "dailyBar": {"c": 100.0, "o": 95.0, "v": 1000000.0},
            "prevDailyBar": {"c": 90.0, "o": 88.0, "v": 800000.0}
        }"#;
        let snap: SnapshotData = serde_json::from_str(legacy_json).unwrap();
        assert!(snap.latest_trade.is_none());
        assert!(snap.latest_quote.is_none());
        assert!(snap.minute_bar.is_none());
        assert!(snap.daily_bar.is_some());
        assert!(snap.prev_daily_bar.is_some());
        assert!((snap.daily_bar.as_ref().unwrap().c - 100.0).abs() < 0.001);
    }

    #[test]
    fn test_snapshot_serde_empty_payload_all_none() {
        let snap: SnapshotData = serde_json::from_str("{}").unwrap();
        assert!(snap.latest_trade.is_none());
        assert!(snap.latest_quote.is_none());
        assert!(snap.minute_bar.is_none());
        assert!(snap.daily_bar.is_none());
        assert!(snap.prev_daily_bar.is_none());
    }
}
