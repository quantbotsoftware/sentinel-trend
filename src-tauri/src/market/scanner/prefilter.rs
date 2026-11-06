use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use chrono_tz::Tz;

use super::premarket::{
    ignition_reject, ignition_score, is_premarket_ignition, newest_news_age_minutes,
    IgnitionInput,
};
use super::rvol::{time_adjusted_rvol, RvolMethod};
use super::scoring::{
    compute_candidate_score, extract_bar_metrics, gap_scanner_alignment_multiplier,
    is_gap_scanner_profile, score_weights_for_preset,
};
use super::sessions::{in_premarket, session_date_et_str};
use super::types::{AlpacaAsset, ScanParams, ScanResult, ScoreInput, ScoreWeights, SnapshotData};
use crate::market::alpaca_news::NewsItem;
use crate::market::catalyst::{is_halted_combined, CatalystFlags, CatalystInput, CatalystWeights};
use crate::market::market_state::HaltState;

/// Quantitative pre-filter — produces ranked `ScanResult` candidates.
///
/// S11 additions:
/// - Compliance gate (Issue #14): symbols with no asset metadata, with
///   `tradable=false`, or with `shortable=false` when `allow_short=true`
///   are rejected before scoring (saves work and prevents the bot from
///   tripping its own Gate 0).
/// - Real-time pricing via `extract_bar_metrics` fallback chain.
/// - Populates `bid`/`ask`/`spread_bps`/`last_trade_age_seconds`/
///   `is_stale` on every result.
///
/// S12 additions:
/// - `is_stale` symbols are now excluded from scoring (carry-forward of
///   the SCANNER_ROADMAP "Risk-off default" decision) immediately after
///   `extract_bar_metrics` and before any price/volume/change filter.
/// - RVOL is computed via [`RvolMethod`] dispatch on `params.rvol_method`:
///   `Naive` keeps the legacy `volume / prev_volume` formula; `TimeAdjusted`
///   (default) projects against `compute_session_progress(now_et)`;
///   `Historical` is API-defined but currently maps to `TimeAdjusted`
///   internally (S12.5 will introduce the minute-volume cache).
///
/// S13 additions (merged in S14):
/// - `halts` and `news_map` parameters drive the catalyst engine. Both
///   may be empty maps (no halts known, no news cached) — empty inputs
///   produce `catalyst_flags = Default` with `pills = []`.
/// - Per-row catalyst computation: `is_halted_combined` (status sub OR
///   RTH heuristic) → `CatalystFlags::compute` → pills.
/// - Halt is metadata, NOT a hard exclusion gate. The strategy layer
///   may opt to skip halted candidates downstream; surfacing them keeps
///   visibility for the UI (S14 dims halted rows).
#[allow(clippy::too_many_arguments)]
pub fn quant_prefilter(
    snapshots: &HashMap<String, SnapshotData>,
    params: &ScanParams,
    earnings: &HashSet<String>,
    assets: &HashMap<String, AlpacaAsset>,
    allow_short: bool,
    now_et: DateTime<Tz>,
    halts: &HashMap<String, HaltState>,
    news_map: &HashMap<String, Vec<NewsItem>>,
    catalyst_weights: &CatalystWeights,
) -> Vec<ScanResult> {
    let (w_change, w_rvol, w_gap, w_volume) = score_weights_for_preset(&params.score_preset);
    let mut candidates: Vec<ScanResult> = Vec::new();
    let session_date = session_date_et_str(now_et);
    let premarket = in_premarket(now_et);
    // Premarket Ignition profile: $-volume floor + ignition score replace
    // the share-count floor / legacy composite (see `scanner::premarket`).
    let ignition = is_premarket_ignition(&params.score_preset);
    let now_utc = now_et.with_timezone(&chrono::Utc);
    // Premarket absolute volume is thin vs RTH — cap the floor so
    // catalyst gappers with real prints are not killed by a 500k/1M
    // RTH-oriented volume_min. User can still set a lower floor.
    let volume_min = if premarket {
        params.volume_min.min(100_000.0)
    } else {
        params.volume_min
    };

    for (sym, snap) in snapshots {
        // S11 compliance gate (Issue #14). Asset map keys are uppercase;
        // snapshot keys come from Alpaca already uppercase, but normalise
        // defensively.
        let key = sym.to_uppercase();
        let asset = match assets.get(&key) {
            Some(a) => a,
            None => continue,
        };
        if !asset.tradable {
            continue;
        }
        if allow_short && !asset.shortable {
            continue;
        }

        let m = extract_bar_metrics(snap, now_et);

        // S12 carry-forward of the SCANNER_ROADMAP §Risk-off default,
        // extended in S15.5: stale candidates (per session-aware
        // thresholds in `sessions::compute_is_stale`) never reach
        // scoring. RTH > 60s, premarket/extended > 300s, dead > 1h.
        if m.is_stale {
            continue;
        }
        // SC11 TODAY-ONLY: require a consolidated print dated today ET.
        // dailyBar/minuteBar cannot prove "today" — fail closed.
        if !m.has_today_trade {
            continue;
        }

        if !m.current_price.is_finite() || m.current_price <= 0.0 || m.prev_close <= 0.0 {
            continue;
        }
        if !(params.price_min..=params.price_max).contains(&m.current_price) {
            continue;
        }
        if !ignition && m.volume_today < volume_min {
            continue;
        }
        if m.change_pct < params.change_min {
            continue;
        }

        // S12 RVOL dispatch. `Historical` falls through to `TimeAdjusted`
        // until the minute-volume cache lands in S12.5.
        let rvol = match params.rvol_method {
            RvolMethod::Naive => {
                if m.prev_volume > 0.0 {
                    m.volume_today / m.prev_volume
                } else {
                    1.0
                }
            }
            RvolMethod::TimeAdjusted | RvolMethod::Historical => {
                time_adjusted_rvol(m.volume_today, m.prev_volume, now_et)
            }
        };
        if rvol < params.rvol_min {
            continue;
        }

        let gap_pct = if m.prev_close > 0.0 {
            ((m.open_price - m.prev_close) / m.prev_close) * 100.0
        } else {
            0.0
        };
        // Ignition ignores `gap_min`: in premarket `dailyBar.o` is the
        // 04:00 print, so gap% is noise there; `change_min` is the gate.
        if !ignition && params.gap_min > 0.0 && gap_pct < params.gap_min {
            continue;
        }

        if params.exclude_earnings && earnings.contains(sym) {
            continue;
        }

        // S13: catalyst inputs — pure, no locks. `is_halted_combined`
        // is the single source of truth (status sub OR heuristic).
        // Resolved before scoring so the ignition profile can use them.
        let key_upper = sym.to_uppercase();
        let is_halted = is_halted_combined(
            halts,
            &key_upper,
            m.last_trade_age_seconds,
            now_et,
        );
        let news_items = news_map.get(&key_upper);
        let has_recent_news = news_items.is_some();
        let has_earnings = earnings.contains(sym) || earnings.contains(&key_upper);
        let has_offering = crate::market::edgar::has_recent_offering(sym);

        let score = if ignition {
            let inp = IgnitionInput {
                change_pct: m.change_pct,
                gap_pct,
                rvol,
                dollar_volume: m.current_price * m.volume_today,
                spread_bps: m.spread_bps,
                in_premarket: premarket,
                has_news: has_recent_news,
                news_age_min: news_items
                    .and_then(|items| newest_news_age_minutes(items, now_utc)),
                has_earnings,
                has_offering,
                is_halted,
            };
            if ignition_reject(&inp, &params.ignition).is_some() {
                continue;
            }
            ignition_score(&inp)
        } else {
            let mut score = compute_candidate_score(
                &ScoreInput {
                    change_pct: m.change_pct,
                    rvol,
                    gap_pct,
                    volume: m.volume_today,
                    prev_volume: m.prev_volume,
                },
                &ScoreWeights { w_change, w_rvol, w_gap, w_volume },
            );
            if is_gap_scanner_profile(params) {
                let Some(multiplier) = gap_scanner_alignment_multiplier(
                    params,
                    m.current_price,
                    m.open_price,
                    m.prev_close,
                    m.change_pct,
                    gap_pct,
                    rvol,
                ) else {
                    continue;
                };
                score *= multiplier;
            }
            // Premarket: float catalyst-backed names (news/earnings) so
            // exploding headlines outrank quiet gap noise.
            if premarket && (has_recent_news || has_earnings) {
                score *= 1.35;
            }
            score
        };
        if !score.is_finite() {
            continue;
        }

        let catalyst_flags = CatalystFlags::compute(
            &CatalystInput {
                symbol: sym,
                change_pct: m.change_pct,
                gap_pct,
                rvol,
                has_earnings,
                has_recent_news,
                is_halted,
                has_offering,
            },
            catalyst_weights,
        );
        let catalyst_pills = catalyst_flags.pills_owned();

        candidates.push(ScanResult {
            symbol: sym.clone(),
            price: m.current_price,
            change_percent: m.change_pct,
            volume: m.volume_today,
            rvol,
            gap_percent: gap_pct,
            score,
            dollar_volume: m.current_price * m.volume_today,
            bid: m.bid,
            ask: m.ask,
            spread_bps: m.spread_bps,
            last_trade_age_seconds: m.last_trade_age_seconds,
            is_stale: m.is_stale,
            catalyst_flags,
            catalyst_pills,
            news_headline: String::new(),
            news_url: String::new(),
            session_date_et: session_date.clone(),
        });
    }

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.symbol.cmp(&b.symbol))
    });

    candidates.truncate(params.max_candidates);
    candidates
}

// S15 split — tests live in three sibling files (`prefilter_basic_tests.rs`,
// `prefilter_compliance_tests.rs`, `prefilter_catalyst_tests.rs`) chained
// via `#[path]` so production `prefilter.rs` stays under the R2 500-LOC
// cap and each test sibling stays under cap as well. Helpers are
// duplicated across the three siblings (each is its own `mod`); this is
// intentional for cohesion-driven reading. Predecessor:
// S12 introduced the single-file `prefilter_tests.rs`; S14 (merge) kept
// it; S15 split it after the file crossed 500 LOC at S14 close.
#[cfg(test)]
#[path = "prefilter_basic_tests.rs"]
mod basic_tests;

#[cfg(test)]
#[path = "prefilter_compliance_tests.rs"]
mod compliance_tests;

#[cfg(test)]
#[path = "prefilter_catalyst_tests.rs"]
mod catalyst_tests;

#[cfg(test)]
#[path = "prefilter_ignition_tests.rs"]
mod ignition_tests;

