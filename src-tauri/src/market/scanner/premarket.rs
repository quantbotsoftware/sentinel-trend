//! Premarket Ignition — scanner profile for names that are *exploding*
//! before the bell (catalyst + abnormal move + abnormal volume).
//!
//! The legacy `compute_candidate_score` is RTH-shaped: the change term
//! saturates at 20 %, the volume tiers start at 2 M shares and the gap
//! term is computed from `dailyBar.o`, which in premarket is the 04:00
//! print — noise, not a gap. This module replaces the score and the
//! liquidity filter when `score_preset == "premarket_ignition"` and
//! adds a diff helper so the auto-scan loop can raise an alert the
//! first time a symbol enters the ignition list.
//!
//! Pure functions only (no locks, no I/O) — R4.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::types::ScanResult;
use crate::market::alpaca_news::NewsItem;

pub const PREMARKET_IGNITION_PRESET: &str = "premarket_ignition";

/// Score-component ceilings — sum to 100 before penalties.
const W_MOVE: f64 = 35.0;
const W_RVOL: f64 = 20.0;
const W_LIQ: f64 = 20.0;
const W_CATALYST_CAP: f64 = 25.0;
const W_NEWS_FRESH: f64 = 15.0;
const W_NEWS_RECENT: f64 = 11.0;
const W_NEWS_OLD: f64 = 7.0;
const W_EARNINGS: f64 = 10.0;
const PEN_OFFERING: f64 = 20.0;
const PEN_HALT: f64 = 15.0;
const PEN_SPREAD_MAX: f64 = 10.0;
/// Move that saturates the move term (a +40 % premarket runner).
const MOVE_SAT_PCT: f64 = 40.0;
/// RVOL that saturates the volume-anomaly term.
const RVOL_SAT: f64 = 30.0;
/// $-volume floor/ceiling for the liquidity term (log scale).
const LIQ_FLOOR_USD: f64 = 100_000.0;
const LIQ_SAT_USD: f64 = 5_000_000.0;
/// Spread penalty ramps from 50 bps (0) to 250 bps (full).
const SPREAD_PEN_START_BPS: f64 = 50.0;
const SPREAD_PEN_FULL_BPS: f64 = 250.0;
const NEWS_FRESH_MIN: f64 = 180.0;
const NEWS_RECENT_MIN: f64 = 720.0;

/// Ignition-specific knobs. Lives inside `ScanParams` as `ignition`
/// with `#[serde(default)]` so legacy `settings.json` upgrades silently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IgnitionParams {
    /// Minimum premarket $-volume (`price × volume_today`). Replaces the
    /// share-count `volume_min` for this profile — a $2 stock with 300 k
    /// shares and a $40 stock with 15 k shares are equally tradeable.
    pub min_dollar_volume: f64,
    /// Reject when the NBBO spread is wider than this (0 = no quote, skip).
    pub max_spread_bps: f64,
    /// Require a news item or earnings flag — pure momentum without a
    /// story is excluded.
    pub require_catalyst: bool,
    /// Rows scoring at least this raise an `scanner-ignition` alert the
    /// first time they appear in the list.
    pub alert_min_score: f64,
    /// Upper bound on alerts per scan tick (top-N by score).
    pub alert_max_per_scan: usize,
}

impl Default for IgnitionParams {
    fn default() -> Self {
        Self {
            min_dollar_volume: 300_000.0,
            max_spread_bps: 200.0,
            require_catalyst: false,
            alert_min_score: 55.0,
            alert_max_per_scan: 5,
        }
    }
}

/// Everything the ignition scorer needs, already resolved by the caller.
pub struct IgnitionInput {
    pub change_pct: f64,
    pub gap_pct: f64,
    pub rvol: f64,
    pub dollar_volume: f64,
    pub spread_bps: f64,
    pub in_premarket: bool,
    pub has_news: bool,
    /// Age of the newest headline in minutes (None = no news).
    pub news_age_min: Option<f64>,
    pub has_earnings: bool,
    pub has_offering: bool,
    pub is_halted: bool,
}

pub fn is_premarket_ignition(preset: &str) -> bool {
    preset == PREMARKET_IGNITION_PRESET
}

/// Hard filters specific to this profile. Returns the rejection reason
/// so tests and tracing can name it; `None` means the row passes.
pub fn ignition_reject(inp: &IgnitionInput, p: &IgnitionParams) -> Option<&'static str> {
    if !inp.dollar_volume.is_finite() || inp.dollar_volume < p.min_dollar_volume {
        return Some("dollar_volume");
    }
    if p.max_spread_bps > 0.0 && inp.spread_bps > p.max_spread_bps {
        return Some("spread");
    }
    if p.require_catalyst && !(inp.has_news || inp.has_earnings) {
        return Some("no_catalyst");
    }
    None
}

fn unit(x: f64) -> f64 {
    if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 0–100 (before penalties) premarket explosion score.
///
/// * move — sqrt curve to 40 %: +8 % → 15.6, +20 % → 24.7, +40 % → 35
/// * rvol — log curve to 30×: 3× → 8.1, 10× → 14.0, 30× → 20
/// * liquidity — log10 of $-volume between $100 k (0) and $5 M (20)
/// * catalyst — fresh news (≤3 h) 15 / ≤12 h 11 / older 7, earnings +10, cap 25
/// * penalties — offering −20, halt −15, spread ramp 50→250 bps up to −10
pub fn ignition_score(inp: &IgnitionInput) -> f64 {
    // After the bell the premarket move is partly in `gap_pct`; blend so a
    // name that gapped +25 % and now sits +18 % is still ranked by its story.
    let move_pct = if inp.in_premarket {
        inp.change_pct
    } else {
        0.6 * inp.change_pct + 0.4 * inp.gap_pct
    };
    let move_term = W_MOVE * unit(move_pct.max(0.0) / MOVE_SAT_PCT).sqrt();
    let rvol_term = W_RVOL * unit((1.0 + inp.rvol.max(0.0)).ln() / (1.0 + RVOL_SAT).ln());
    let liq_term = if inp.dollar_volume > LIQ_FLOOR_USD {
        W_LIQ * unit((inp.dollar_volume / LIQ_FLOOR_USD).log10() / (LIQ_SAT_USD / LIQ_FLOOR_USD).log10())
    } else {
        0.0
    };

    let mut catalyst = 0.0;
    if inp.has_news {
        catalyst += match inp.news_age_min {
            Some(a) if a <= NEWS_FRESH_MIN => W_NEWS_FRESH,
            Some(a) if a <= NEWS_RECENT_MIN => W_NEWS_RECENT,
            _ => W_NEWS_OLD,
        };
    }
    if inp.has_earnings {
        catalyst += W_EARNINGS;
    }
    let catalyst_term = catalyst.min(W_CATALYST_CAP);

    let mut penalty = 0.0;
    if inp.has_offering {
        penalty += PEN_OFFERING;
    }
    if inp.is_halted {
        penalty += PEN_HALT;
    }
    if inp.spread_bps > SPREAD_PEN_START_BPS {
        penalty += PEN_SPREAD_MAX
            * unit((inp.spread_bps - SPREAD_PEN_START_BPS) / (SPREAD_PEN_FULL_BPS - SPREAD_PEN_START_BPS));
    }

    (move_term + rvol_term + liq_term + catalyst_term - penalty).max(0.0)
}

/// Minutes since the newest item in `items` (Alpaca returns newest first;
/// we take the minimum age defensively). `None` when nothing parses.
pub fn newest_news_age_minutes(items: &[NewsItem], now: DateTime<Utc>) -> Option<f64> {
    items
        .iter()
        .filter_map(|n| DateTime::parse_from_rfc3339(&n.created_at).ok())
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0) as f64 / 60.0)
        .fold(None, |acc: Option<f64>, a| Some(acc.map_or(a, |b| b.min(a))))
}

/// Payload of the `scanner-ignition` Tauri event (one per new entrant).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IgnitionAlert {
    pub symbol: String,
    pub price: f64,
    pub change_percent: f64,
    pub rvol: f64,
    pub dollar_volume: f64,
    pub score: f64,
    pub news_headline: String,
    pub news_url: String,
    pub catalyst_pills: Vec<String>,
}

/// Symbols in `current` that score ≥ `alert_min_score` and were absent
/// from `previous`, top-N by score. Emitted once per entrant — the
/// auto-scan loop calls this with the old `scan_results` before it
/// overwrites them, so a name alerts exactly when it first ignites.
pub fn new_ignitions(
    previous: &[ScanResult],
    current: &[ScanResult],
    p: &IgnitionParams,
) -> Vec<IgnitionAlert> {
    let seen: HashSet<&str> = previous.iter().map(|r| r.symbol.as_str()).collect();
    let mut fresh: Vec<&ScanResult> = current
        .iter()
        .filter(|r| r.score >= p.alert_min_score && !seen.contains(r.symbol.as_str()))
        .collect();
    fresh.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    fresh
        .into_iter()
        .take(p.alert_max_per_scan)
        .map(|r| IgnitionAlert {
            symbol: r.symbol.clone(),
            price: r.price,
            change_percent: r.change_percent,
            rvol: r.rvol,
            dollar_volume: r.dollar_volume,
            score: r.score,
            news_headline: r.news_headline.clone(),
            news_url: r.news_url.clone(),
            catalyst_pills: r.catalyst_pills.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> IgnitionInput {
        IgnitionInput {
            change_pct: 20.0,
            gap_pct: 0.0,
            rvol: 10.0,
            dollar_volume: 1_000_000.0,
            spread_bps: 30.0,
            in_premarket: true,
            has_news: true,
            news_age_min: Some(60.0),
            has_earnings: false,
            has_offering: false,
            is_halted: false,
        }
    }

    #[test]
    fn score_components_match_doc_examples() {
        let mut i = base();
        i.has_news = false;
        i.rvol = 0.0;
        i.dollar_volume = 0.0;
        i.change_pct = 40.0;
        assert!((ignition_score(&i) - 35.0).abs() < 1e-6, "move saturates at 40%");
        i.change_pct = 8.0;
        assert!((ignition_score(&i) - 15.65).abs() < 0.1);
        i.change_pct = 0.0;
        i.rvol = 30.0;
        assert!((ignition_score(&i) - 20.0).abs() < 1e-6, "rvol saturates at 30x");
        i.rvol = 0.0;
        i.dollar_volume = 5_000_000.0;
        assert!((ignition_score(&i) - 20.0).abs() < 1e-6, "liquidity saturates at $5M");
        i.dollar_volume = 100_000.0;
        assert_eq!(ignition_score(&i), 0.0);
    }

    #[test]
    fn full_runner_scores_high_and_is_bounded() {
        let mut i = base();
        i.change_pct = 45.0;
        i.rvol = 40.0;
        i.dollar_volume = 8_000_000.0;
        i.has_earnings = true;
        let s = ignition_score(&i);
        assert!(s <= 100.0 + 1e-9, "score {s} must not exceed 100");
        assert!(s > 95.0);
    }

    #[test]
    fn catalyst_freshness_ordering_and_cap() {
        let mut i = base();
        i.news_age_min = Some(30.0);
        let fresh = ignition_score(&i);
        i.news_age_min = Some(600.0);
        let recent = ignition_score(&i);
        i.news_age_min = Some(2_000.0);
        let old = ignition_score(&i);
        i.has_news = false;
        i.news_age_min = None;
        let none = ignition_score(&i);
        assert!(fresh > recent && recent > old && old > none);
        assert!((fresh - none - W_NEWS_FRESH).abs() < 1e-6);
        // news + earnings capped at 25 (15 + 10 = 25, exactly at cap)
        i.has_news = true;
        i.news_age_min = Some(10.0);
        i.has_earnings = true;
        assert!((ignition_score(&i) - none - W_CATALYST_CAP).abs() < 1e-6);
    }

    #[test]
    fn offering_halt_and_spread_penalise() {
        let clean = ignition_score(&base());
        let mut i = base();
        i.has_offering = true;
        assert!((clean - ignition_score(&i) - PEN_OFFERING).abs() < 1e-6);
        let mut i = base();
        i.is_halted = true;
        assert!((clean - ignition_score(&i) - PEN_HALT).abs() < 1e-6);
        let mut i = base();
        i.spread_bps = 150.0; // halfway on the 50→250 ramp → −5
        assert!((clean - ignition_score(&i) - 5.0).abs() < 1e-6);
        i.spread_bps = 900.0;
        assert!((clean - ignition_score(&i) - PEN_SPREAD_MAX).abs() < 1e-6);
    }

    #[test]
    fn negative_move_gets_no_move_credit_and_never_negative_total() {
        let mut i = base();
        i.change_pct = -30.0;
        i.rvol = 0.0;
        i.dollar_volume = 0.0;
        i.has_news = false;
        i.has_offering = true;
        assert_eq!(ignition_score(&i), 0.0);
    }

    #[test]
    fn rth_blends_gap_into_move() {
        let mut i = base();
        i.in_premarket = false;
        i.change_pct = 10.0;
        i.gap_pct = 30.0;
        let blended = ignition_score(&i);
        i.gap_pct = 0.0;
        assert!(blended > ignition_score(&i));
    }

    #[test]
    fn reject_reasons() {
        let p = IgnitionParams::default();
        let mut i = base();
        assert_eq!(ignition_reject(&i, &p), None);
        i.dollar_volume = 250_000.0;
        assert_eq!(ignition_reject(&i, &p), Some("dollar_volume"));
        i.dollar_volume = 250_000.0 + 1e6;
        i.spread_bps = 350.0;
        assert_eq!(ignition_reject(&i, &p), Some("spread"));
        i.spread_bps = 0.0; // no quote → not rejected
        assert_eq!(ignition_reject(&i, &p), None);
        let strict = IgnitionParams { require_catalyst: true, ..p.clone() };
        i.has_news = false;
        assert_eq!(ignition_reject(&i, &strict), Some("no_catalyst"));
        i.has_earnings = true;
        assert_eq!(ignition_reject(&i, &strict), None);
    }

    #[test]
    fn params_default_and_legacy_json() {
        let p: IgnitionParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p, IgnitionParams::default());
        let p: IgnitionParams = serde_json::from_str(r#"{"require_catalyst":true}"#).unwrap();
        assert!(p.require_catalyst);
        assert!((p.min_dollar_volume - 300_000.0).abs() < 1e-9);
    }

    fn news(created: &str) -> NewsItem {
        NewsItem {
            id: 1,
            headline: "h".into(),
            author: String::new(),
            created_at: created.into(),
            updated_at: String::new(),
            symbols: vec![],
            source: String::new(),
            url: String::new(),
        }
    }

    #[test]
    fn news_age_takes_newest_and_ignores_garbage() {
        let now = DateTime::parse_from_rfc3339("2026-09-03T11:00:00Z").unwrap().with_timezone(&Utc);
        let items = vec![news("2026-09-03T08:00:00Z"), news("not-a-date"), news("2026-09-03T10:30:00Z")];
        let age = newest_news_age_minutes(&items, now).unwrap();
        assert!((age - 30.0).abs() < 1e-6);
        assert!(newest_news_age_minutes(&[news("x")], now).is_none());
        assert!(newest_news_age_minutes(&[], now).is_none());
    }

    fn row(sym: &str, score: f64) -> ScanResult {
        ScanResult { symbol: sym.into(), score, ..Default::default() }
    }

    #[test]
    fn new_ignitions_only_new_high_scorers_top_n() {
        let p = IgnitionParams { alert_min_score: 50.0, alert_max_per_scan: 2, ..Default::default() };
        let prev = vec![row("OLD", 90.0)];
        let cur = vec![
            row("OLD", 95.0),
            row("A", 80.0),
            row("B", 60.0),
            row("C", 70.0),
            row("LOW", 40.0),
        ];
        let alerts = new_ignitions(&prev, &cur, &p);
        let syms: Vec<&str> = alerts.iter().map(|a| a.symbol.as_str()).collect();
        assert_eq!(syms, vec!["A", "C"], "OLD already seen, LOW below floor, B cut by top-2");
        assert!(new_ignitions(&cur, &cur, &p).is_empty());
    }

    #[test]
    fn alert_carries_row_fields() {
        let p = IgnitionParams::default();
        let mut r = row("X", 77.0);
        r.news_headline = "X wins".into();
        r.catalyst_pills = vec!["NEWS".into()];
        r.dollar_volume = 2.0e6;
        let a = &new_ignitions(&[], &[r], &p)[0];
        assert_eq!(a.news_headline, "X wins");
        assert_eq!(a.catalyst_pills, vec!["NEWS".to_string()]);
        assert!((a.dollar_volume - 2.0e6).abs() < 1e-9);
    }
}
