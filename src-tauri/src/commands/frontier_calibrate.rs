//! F4 — Tauri surface for the Frontier calibration engine.
//!
//! `get_frontier_calibration` is read-only analysis; the engine only
//! ever *proposes*. `apply_frontier_calibration` is the explicit
//! operator action (Weights Lab pattern) — it merges clamped overrides
//! into `evolution_protocol.frontier_overrides` and persists
//! settings.json. The 1 Hz loop picks the new thresholds up on its
//! next tick.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::ops::decision_log;
use crate::smarttrend::frontier_calibrate::{
    bounds_for, calibrate, snap_threshold, CalRow, CalibrationReport, CondObs,
};
use crate::smarttrend::strategies::apex_calibration::{apex_default_for, APEX_CALIBRATION_BOUNDS};
use crate::smarttrend::strategies::frontier::{FrontierThresholds, CALIBRATION_BOUNDS};
use crate::state::TitanState;

#[derive(Debug, Serialize)]
pub struct CalibrationPayload {
    pub report: CalibrationReport,
    /// Effective thresholds after overrides (key → value).
    pub effective: HashMap<String, f64>,
    /// Raw operator overrides currently persisted.
    pub overrides: HashMap<String, f64>,
}

/// Effective thresholds — Frontier (defaults + overrides via
/// `FrontierThresholds`) plus APEX (`apex_*` keys: default from
/// `Apex::default()` unless overridden, clamped into the band).
fn effective_map(
    th: &FrontierThresholds,
    overrides: &HashMap<String, f64>,
) -> HashMap<String, f64> {
    let mut map: HashMap<String, f64> = CALIBRATION_BOUNDS
        .iter()
        .filter_map(|(k, _, _)| th.get(k).map(|v| ((*k).to_string(), v)))
        .collect();
    for (k, lo, hi) in APEX_CALIBRATION_BOUNDS {
        let base = apex_default_for(k).unwrap_or(0.0);
        let v = overrides.get(*k).copied().unwrap_or(base).clamp(*lo, *hi);
        map.insert((*k).to_string(), v);
    }
    map
}

fn load_rows(window_days: i64) -> Result<Vec<CalRow>, String> {
    let log = decision_log::global().ok_or("decision journal not initialised")?;
    let now = chrono::Utc::now().timestamp();
    let recs = log
        .query("", now - window_days * 86_400, now, 5_000)
        .map_err(|e| format!("calibration query failed: {e}"))?;
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for r in &recs {
        // F4-APEX — the house strategy logs `source = "apex"` rows with
        // the same condition-vector components as `frontier:*` setups.
        let setup = match r.source.strip_prefix("frontier:") {
            Some(s) => s,
            None if r.source == "apex" => "apex",
            None => continue,
        };
        let Some(cjson) = r.components.as_deref() else {
            continue;
        };
        let Ok(conditions) = serde_json::from_str::<Vec<CondObs>>(cjson) else {
            continue;
        };
        let day = chrono::DateTime::<chrono::Utc>::from_timestamp(r.ts, 0)
            .map(|dt| {
                dt.with_timezone(&chrono_tz::US::Eastern)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".into());
        let is_candidate = conditions.iter().all(|c| c.passed);
        // S34 — a setup is one candidate per symbol/setup/day. Older
        // Frontier rows may contain repeated GO records because the
        // pre-fix dedup key included moving conviction; collapsing
        // those protects proposals from historical log amplification.
        // Near-misses remain distinct by their failed-condition
        // signature, which preserves useful sole-blocker evidence.
        let state_key = if is_candidate {
            "candidate".to_string()
        } else {
            let failed: Vec<&str> = conditions
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.name.as_str())
                .collect();
            format!("miss:{}", failed.join("+"))
        };
        let dedup_key = format!("{day}|{}|{setup}|{state_key}", r.symbol);
        if !seen.insert(dedup_key) {
            continue;
        }
        rows.push(CalRow {
            day,
            setup: setup.to_string(),
            conditions,
            is_candidate,
        });
    }
    Ok(rows)
}

#[tauri::command]
pub async fn get_frontier_calibration(
    state: State<'_, Arc<TitanState>>,
    days: Option<i64>,
) -> Result<CalibrationPayload, String> {
    let window_days = days.unwrap_or(7).clamp(1, 30);
    let overrides = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.evolution_protocol.frontier_overrides.clone()
    };
    let th = FrontierThresholds::with_overrides(&overrides);
    let effective = effective_map(&th, &overrides);
    let rows = load_rows(window_days)?;
    Ok(CalibrationPayload {
        report: calibrate(&rows, &effective),
        effective,
        overrides,
    })
}

/// Operator applies a set of proposals. Unknown keys are rejected
/// (defensive: the UI only sends engine-produced keys); values are
/// clamped into the calibration band before persisting.
#[tauri::command]
pub async fn apply_frontier_calibration(
    state: State<'_, Arc<TitanState>>,
    overrides: HashMap<String, f64>,
) -> Result<HashMap<String, f64>, String> {
    for (k, v) in &overrides {
        if bounds_for(k).is_none() {
            return Err(format!("unknown threshold key: {k}"));
        }
        if !v.is_finite() {
            return Err(format!("non-finite value for {k}"));
        }
    }
    let (th, persisted) = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        for (k, v) in overrides {
            let (lo, hi) = bounds_for(&k).expect("validated above");
            let snapped = snap_threshold(&k, v.clamp(lo, hi));
            cfg.evolution_protocol
                .frontier_overrides
                .insert(k, snapped);
        }
        (
            FrontierThresholds::with_overrides(&cfg.evolution_protocol.frontier_overrides),
            cfg.evolution_protocol.frontier_overrides.clone(),
        )
    };
    if let Err(e) = crate::ops::persistence::save_config(&state) {
        tracing::error!(component = "frontier", error = %e, "calibration persist failed");
        return Err(format!("persist failed: {e}"));
    }
    tracing::warn!(
        component = "frontier",
        "F4 — operator APPLIED calibration overrides (settings.json updated)"
    );
    Ok(effective_map(&th, &persisted))
}
