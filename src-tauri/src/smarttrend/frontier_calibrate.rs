//! F4 — Frontier threshold-calibration engine (propose-only).
//!
//! Pure analysis over the condition vectors persisted in
//! `decisions.sqlite`: given the observed candidate rate vs the plan's
//! target band (3–8 candidates/day), propose threshold moves with the
//! evidence attached. The engine NEVER applies anything — the operator
//! reviews and clicks APPLY (Weights Lab pattern, human-in-the-loop).
//!
//! Loosening uses *sole-blocker* near-misses only: rows where exactly
//! one condition failed. Those are the candidates a single threshold
//! move can convert, so the projection is honest — no compounding
//! guesses about multi-condition rows.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::smarttrend::strategies::apex_calibration::APEX_CALIBRATION_BOUNDS;
use crate::smarttrend::strategies::frontier::CALIBRATION_BOUNDS;

/// Plan target band: calibrate toward 3–8 candidates/day.
pub const TARGET_LO: f64 = 3.0;
pub const TARGET_HI: f64 = 8.0;

/// One observed condition from a logged decision row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CondObs {
    pub name: String,
    pub passed: bool,
    pub value: f64,
    #[serde(default)]
    pub threshold: f64,
}

/// One Frontier decision row (candidate or near-miss).
#[derive(Debug, Clone)]
pub struct CalRow {
    /// ET date bucket, `YYYY-MM-DD`.
    pub day: String,
    pub setup: String,
    pub conditions: Vec<CondObs>,
    /// All conditions passed (GO / shadow candidate).
    pub is_candidate: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    /// Override key (`FrontierThresholds` field name).
    pub key: String,
    pub setup: String,
    pub condition: String,
    pub current: f64,
    pub suggested: f64,
    /// Near-misses blocked by this condition alone in the window.
    pub sole_blocked: u32,
    /// Rows the move would convert to candidates, per day.
    pub projected_extra_per_day: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationReport {
    pub days_observed: u32,
    pub decisions: u32,
    pub candidates: u32,
    pub candidates_per_day: f64,
    /// `(day, candidates)` ascending — the panel sparkline.
    pub per_day: Vec<(String, u32)>,
    pub in_target: bool,
    pub target_lo: f64,
    pub target_hi: f64,
    pub proposals: Vec<Proposal>,
}

/// `min`: value must be ≥ threshold (loosen = lower). `max`: value must
/// be ≤ threshold (loosen = raise). `abs`: compare |value|.
struct CondMap {
    setup: &'static str,
    condition: &'static str,
    key: &'static str,
    is_min: bool,
    use_abs: bool,
}

/// (setup, condition) → overridable threshold. Structural conditions
/// (`once_per_day`, `et_window`, `reclaim`, `first_prints`,
/// `microprice_lead`, `baseline_burst`, `pre_halt_move_pct`) are not
/// mapped — they encode setup identity, not calibration knobs.
const COND_MAP: &[CondMap] = &[
    CondMap { setup: "tape_ignition", condition: "pps_15s", key: "tape_min_pps_15s", is_min: true, use_abs: false },
    CondMap { setup: "tape_ignition", condition: "signed_notional_15s", key: "tape_min_signed_notional_15s", is_min: true, use_abs: true },
    CondMap { setup: "tape_ignition", condition: "spread_bps", key: "tape_max_spread_bps", is_min: false, use_abs: false },
    CondMap { setup: "tape_ignition", condition: "imbalance", key: "tape_min_imbalance", is_min: true, use_abs: true },
    CondMap { setup: "tape_ignition", condition: "micro_warmup", key: "tape_min_micro_age_secs", is_min: true, use_abs: false },
    CondMap { setup: "true_resume", condition: "resume_age_secs", key: "resume_max_age_secs", is_min: false, use_abs: false },
    CondMap { setup: "true_resume", condition: "spread_bps", key: "resume_max_spread_bps", is_min: false, use_abs: false },
    CondMap { setup: "true_resume", condition: "resume_print_notional", key: "resume_min_print_notional", is_min: true, use_abs: false },
    CondMap { setup: "open_sweep", condition: "gap_pct", key: "sweep_min_gap_pct", is_min: true, use_abs: true },
    CondMap { setup: "open_sweep", condition: "spread_bps", key: "sweep_max_spread_bps", is_min: false, use_abs: false },
    CondMap { setup: "premarket_ignition", condition: "catalyst", key: "pm_min_gap_pct", is_min: true, use_abs: true },
    CondMap { setup: "premarket_ignition", condition: "pps_15s", key: "pm_min_pps", is_min: true, use_abs: false },
    CondMap { setup: "premarket_ignition", condition: "signed_notional_15s", key: "pm_min_notional", is_min: true, use_abs: true },
    CondMap { setup: "premarket_ignition", condition: "spread_bps", key: "pm_max_spread_bps", is_min: false, use_abs: false },
    // F4-APEX — the house strategy joins the same protocol. Structural
    // conditions (`et_window`, `price_floor`, `evidence`, `short_regime`,
    // `aligned`) encode setup identity and are not mapped.
    CondMap { setup: "apex", condition: "rvol_trending", key: "apex_rvol_trending", is_min: true, use_abs: false },
    CondMap { setup: "apex", condition: "rvol_calm", key: "apex_rvol_calm", is_min: true, use_abs: false },
    CondMap { setup: "apex", condition: "rvol_volatile", key: "apex_rvol_volatile", is_min: true, use_abs: false },
    CondMap { setup: "apex", condition: "spread_bps", key: "apex_max_spread_bps", is_min: false, use_abs: false },
    CondMap { setup: "apex", condition: "ignition_range", key: "apex_ignition_range_atr", is_min: true, use_abs: false },
    CondMap { setup: "apex", condition: "ignition_close_pos", key: "apex_ignition_close_pos", is_min: true, use_abs: false },
    CondMap { setup: "apex", condition: "ignition_vol", key: "apex_ignition_vol_mult", is_min: true, use_abs: false },
    CondMap { setup: "apex", condition: "extension_atr", key: "apex_max_extension_atr", is_min: false, use_abs: false },
];

/// Calibration band for a key — Frontier and APEX share the namespace
/// (APEX keys are `apex_`-prefixed, so no collisions).
pub fn bounds_for(key: &str) -> Option<(f64, f64)> {
    CALIBRATION_BOUNDS
        .iter()
        .chain(APEX_CALIBRATION_BOUNDS.iter())
        .find(|(k, _, _)| *k == key)
        .map(|(_, lo, hi)| (*lo, *hi))
}

/// Whole-second keys on `FrontierThresholds` (`round() as u64`). A
/// 0.5s proposal is a no-op after snap — do not surface it.
pub(crate) fn snap_threshold(key: &str, v: f64) -> f64 {
    match key {
        "tape_min_micro_age_secs" | "resume_max_age_secs" => v.round(),
        _ => v,
    }
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Analyse the window and build the report. `rows` must already be
/// restricted to strategy-protocol decisions (the command layer accepts
/// `frontier:*` and `apex` sources). `current` maps every calibratable
/// key to its effective value (defaults + operator overrides) — built
/// by the command layer for Frontier and APEX together.
pub fn calibrate(rows: &[CalRow], current: &HashMap<String, f64>) -> CalibrationReport {
    let mut per_day: HashMap<String, u32> = HashMap::new();
    let mut days: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut candidates = 0u32;
    for r in rows {
        days.insert(r.day.clone());
        if r.is_candidate {
            candidates += 1;
            *per_day.entry(r.day.clone()).or_insert(0) += 1;
        } else {
            per_day.entry(r.day.clone()).or_insert(0);
        }
    }
    let days_observed = days.len().max(1) as u32;
    let candidates_per_day = f64::from(candidates) / f64::from(days_observed);
    let in_target = (TARGET_LO..=TARGET_HI).contains(&candidates_per_day);

    let mut proposals = Vec::new();
    if !in_target && !rows.is_empty() {
        if candidates_per_day < TARGET_LO {
            proposals = propose_loosening(rows, current, days_observed);
        } else {
            proposals = propose_tightening(rows, current, candidates_per_day);
        }
    }

    let per_day: Vec<(String, u32)> = days
        .into_iter()
        .map(|d| {
            let n = per_day.get(&d).copied().unwrap_or(0);
            (d, n)
        })
        .collect();

    CalibrationReport {
        days_observed,
        decisions: rows.len() as u32,
        candidates,
        candidates_per_day,
        per_day,
        in_target,
        target_lo: TARGET_LO,
        target_hi: TARGET_HI,
        proposals,
    }
}

/// Rate below target — for each mapped condition, look at near-misses
/// where it was the SOLE blocker and propose the median of the
/// blocking values (converts ~half), clamped to the calibration band.
fn propose_loosening(rows: &[CalRow], current: &HashMap<String, f64>, days: u32) -> Vec<Proposal> {
    let mut out = Vec::new();
    for m in COND_MAP {
        let mut blocked_vals: Vec<f64> = Vec::new();
        for r in rows {
            if r.is_candidate || r.setup != m.setup {
                continue;
            }
            let failed: Vec<&CondObs> = r.conditions.iter().filter(|c| !c.passed).collect();
            if failed.len() != 1 || failed[0].name != m.condition {
                continue;
            }
            let v = if m.use_abs { failed[0].value.abs() } else { failed[0].value };
            if v.is_finite() {
                blocked_vals.push(v);
            }
        }
        if blocked_vals.is_empty() {
            continue;
        }
        blocked_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = median(&blocked_vals);
        let Some(current) = current.get(m.key).copied() else { continue };
        let Some((lo, hi)) = bounds_for(m.key) else { continue };
        let suggested = snap_threshold(m.key, med.clamp(lo, hi));
        // Loosening must actually move the right way; a clamp / snap
        // back to the current value (or past it) means the band is
        // exhausted — including sub-second no-ops on u64 keys.
        let moves = if m.is_min { suggested < current } else { suggested > current };
        if !moves {
            continue;
        }
        let converted = blocked_vals
            .iter()
            .filter(|v| if m.is_min { **v >= suggested } else { **v <= suggested })
            .count() as u32;
        if converted == 0 {
            continue;
        }
        out.push(Proposal {
            key: m.key.into(),
            setup: m.setup.into(),
            condition: m.condition.into(),
            current,
            suggested,
            sole_blocked: blocked_vals.len() as u32,
            projected_extra_per_day: f64::from(converted) / f64::from(days.max(1)),
        });
    }
    out.sort_by(|a, b| {
        b.projected_extra_per_day
            .partial_cmp(&a.projected_extra_per_day)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(5);
    out
}

/// Rate above target — tighten toward keeping roughly
/// `TARGET_HI / rate` of today's candidates: per condition, propose the
/// value quantile that would keep that fraction, clamped to the band.
fn propose_tightening(rows: &[CalRow], current: &HashMap<String, f64>, rate: f64) -> Vec<Proposal> {
    let keep = (TARGET_HI / rate).clamp(0.2, 0.95);
    let mut out = Vec::new();
    for m in COND_MAP {
        let mut vals: Vec<f64> = rows
            .iter()
            .filter(|r| r.is_candidate && r.setup == m.setup)
            .flat_map(|r| r.conditions.iter())
            .filter(|c| c.name == m.condition)
            .map(|c| if m.use_abs { c.value.abs() } else { c.value })
            .filter(|v| v.is_finite())
            .collect();
        if vals.len() < 5 {
            continue;
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // min-type: keep the top `keep` fraction → threshold at the
        // (1-keep) quantile. max-type: keep the bottom `keep` fraction.
        let idx = if m.is_min {
            (((1.0 - keep) * vals.len() as f64) as usize).min(vals.len() - 1)
        } else {
            ((keep * vals.len() as f64) as usize).min(vals.len() - 1)
        };
        let Some(current) = current.get(m.key).copied() else { continue };
        let Some((lo, hi)) = bounds_for(m.key) else { continue };
        let suggested = snap_threshold(m.key, vals[idx].clamp(lo, hi));
        let moves = if m.is_min { suggested > current } else { suggested < current };
        if !moves {
            continue;
        }
        let kept = vals
            .iter()
            .filter(|v| if m.is_min { **v >= suggested } else { **v <= suggested })
            .count();
        out.push(Proposal {
            key: m.key.into(),
            setup: m.setup.into(),
            condition: m.condition.into(),
            current,
            suggested,
            sole_blocked: 0,
            projected_extra_per_day: -((vals.len() - kept) as f64),
        });
    }
    out.truncate(3);
    out
}

#[cfg(test)]
#[path = "frontier_calibrate_tests.rs"]
mod tests;
