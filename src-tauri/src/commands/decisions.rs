//! PRIME F1 — Tauri surface for the decision journal.
//!
//! Read-only queries over `ops::decision_log`. Writes happen inside
//! the sentinel / bot pipelines; the UI only inspects.

use serde::Serialize;

use crate::ops::decision_log::{self, DecisionRecord, VetoSummaryRow};

#[derive(Debug, Serialize)]
pub struct DecisionsPayload {
    pub decisions: Vec<DecisionRecord>,
    pub total_returned: usize,
}

/// Most-recent decisions for `symbol` (empty string = all symbols)
/// in `[from_ts, to_ts]` (unix seconds; `0/0` = last 24 h). Capped at
/// 500 rows.
#[tauri::command]
pub async fn get_decisions(
    symbol: String,
    from_ts: i64,
    to_ts: i64,
) -> Result<DecisionsPayload, String> {
    let log = decision_log::global().ok_or("decision journal not initialised")?;
    let now = chrono::Utc::now().timestamp();
    let (from, to) = if from_ts == 0 && to_ts == 0 {
        (now - 86_400, now)
    } else {
        (from_ts, to_ts.max(from_ts))
    };
    let decisions = log
        .query(&symbol, from, to, 500)
        .map_err(|e| format!("decision query failed: {e}"))?;
    let total_returned = decisions.len();
    Ok(DecisionsPayload {
        decisions,
        total_returned,
    })
}

/// Veto counts grouped by (stage, reason) over the trailing
/// `hours` window (default 24 when 0).
#[tauri::command]
pub async fn get_veto_summary(hours: i64) -> Result<Vec<VetoSummaryRow>, String> {
    let log = decision_log::global().ok_or("decision journal not initialised")?;
    let window_h = if hours <= 0 { 24 } else { hours };
    let from = chrono::Utc::now().timestamp() - window_h * 3600;
    log.veto_summary(from)
        .map_err(|e| format!("veto summary failed: {e}"))
}

/// PRIME F5a — counterfactual outcome of the vetoed entries.
#[derive(Debug, Serialize)]
pub struct ShadowPayload {
    pub summary: Vec<crate::smarttrend::prime::shadow::ShadowSummaryRow>,
    pub recent: Vec<crate::smarttrend::prime::shadow::ResolvedShadow>,
    pub open_count: usize,
}

#[tauri::command]
pub async fn get_shadow_summary(
    state: tauri::State<'_, std::sync::Arc<crate::state::TitanState>>,
) -> Result<ShadowPayload, String> {
    let dp = state.dplus().ok_or("dplus not initialised")?;
    let book = dp.shadow.lock().map_err(|e| format!("lock: {e}"))?;
    Ok(ShadowPayload {
        summary: book.summary(),
        recent: book.recent_resolved(50),
        open_count: book.open_count(),
    })
}

/// P5 — realized execution slippage (decision price vs fill price).
#[derive(Debug, Serialize)]
pub struct SlippagePayload {
    pub summary: Vec<crate::ops::slippage::SlipSummaryRow>,
    pub recent: Vec<crate::ops::slippage::FillSlip>,
}

#[tauri::command]
pub async fn get_slippage_ledger() -> Result<SlippagePayload, String> {
    Ok(SlippagePayload {
        summary: crate::ops::slippage::summary(),
        recent: crate::ops::slippage::recent(50),
    })
}

/// P6 — end-of-day debrief report.
#[tauri::command]
pub async fn get_debrief(
    state: tauri::State<'_, std::sync::Arc<crate::state::TitanState>>,
) -> Result<crate::ops::debrief::Debrief, String> {
    Ok(crate::ops::debrief::build_debrief(&state))
}

/// P1 — Strategy Hospital status for the PRIME tab.
#[derive(Debug, Serialize)]
pub struct HospitalPayload {
    pub quarantined: Vec<QuarantinedPlaybook>,
    pub banned_symbols: Vec<String>,
    pub policy: crate::smarttrend::prime::hospital::HospitalPolicy,
}

#[derive(Debug, Serialize)]
pub struct QuarantinedPlaybook {
    pub source: String,
    pub since_ts: i64,
    pub reliability_at_admit: f64,
}

/// P3 — current fusion weights + latest Counterfactual Lab suggestion.
#[derive(Debug, Serialize)]
pub struct PrimeWeightsPayload {
    pub current: crate::smarttrend::prime::ConvictionWeights,
    pub suggested: Option<crate::smarttrend::prime::counterfactual::SuggestedWeights>,
    pub last_calibration_ts: i64,
}

#[tauri::command]
pub async fn get_prime_weights(
    state: tauri::State<'_, std::sync::Arc<crate::state::TitanState>>,
) -> Result<PrimeWeightsPayload, String> {
    let dp = state.dplus().ok_or("dplus not initialised")?;
    let engine = dp.prime.lock().map_err(|e| format!("lock: {e}"))?;
    Ok(PrimeWeightsPayload {
        current: engine.weights.clone(),
        suggested: engine.suggested_weights.clone(),
        last_calibration_ts: engine.last_calibration_ts,
    })
}

/// P3 — operator applies the lab's suggested weights. Human-in-the-
/// loop by design: the nightly search only ever *proposes*. Returns
/// the new active weights.
#[tauri::command]
pub async fn apply_suggested_weights(
    state: tauri::State<'_, std::sync::Arc<crate::state::TitanState>>,
) -> Result<crate::smarttrend::prime::ConvictionWeights, String> {
    let dp = state.dplus().ok_or("dplus not initialised")?;
    let snapshot = {
        let mut engine = dp.prime.lock().map_err(|e| format!("lock: {e}"))?;
        let s = engine
            .suggested_weights
            .take()
            .ok_or("no suggestion pending")?;
        tracing::warn!(
            component = "prime",
            baseline_r = format!("{:.2}", s.baseline_r),
            suggested_r = format!("{:.2}", s.suggested_r),
            sample = s.sample,
            "P3 — operator APPLIED counterfactual weight suggestion"
        );
        engine.weights = s.weights;
        engine.clone()
    };
    // Persist outside the guard (S05-adjacent discipline).
    if let Err(e) = snapshot.save(&crate::smarttrend::prime::default_engine_path()) {
        tracing::warn!(component = "prime", error = %e, "prime engine save failed");
    }
    Ok(snapshot.weights)
}

#[tauri::command]
pub async fn get_hospital_status(
    state: tauri::State<'_, std::sync::Arc<crate::state::TitanState>>,
) -> Result<HospitalPayload, String> {
    let dp = state.dplus().ok_or("dplus not initialised")?;
    // Standalone prime guard — snapshot, then drop before Tier A.
    let (quarantined, policy) = {
        let engine = dp.prime.lock().map_err(|e| format!("lock: {e}"))?;
        let mut q: Vec<QuarantinedPlaybook> = engine
            .hospital
            .quarantined
            .iter()
            .map(|(source, rec)| QuarantinedPlaybook {
                source: source.clone(),
                since_ts: rec.since_ts,
                reliability_at_admit: rec.reliability_at_admit,
            })
            .collect();
        q.sort_by(|a, b| a.source.cmp(&b.source));
        (q, engine.hospital.policy)
    };

    // Per-symbol daily breaker — re-derive today's banned set from the
    // journal (same pure logic as the dispatch gate).
    let day_start_ts = {
        let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        now_et
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|dt| dt.and_local_timezone(chrono_tz::US::Eastern).single())
            .map(|dt| dt.timestamp())
            .unwrap_or(0)
    };
    let banned_symbols = {
        use std::collections::HashMap;
        let journal = state.journal.lock().map_err(|e| format!("lock: {e}"))?;
        let mut per_symbol: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
        for e in &journal.entries {
            if let (Some(t), Some(p)) = (e.exit_time, e.pnl) {
                if t >= day_start_ts {
                    per_symbol
                        .entry(e.symbol.to_uppercase())
                        .or_default()
                        .push((t, p));
                }
            }
        }
        let mut banned: Vec<String> = per_symbol
            .into_iter()
            .filter(|(_, closes)| {
                let mut closes = closes.clone();
                closes.sort_by_key(|(t, _)| *t);
                let pnls: Vec<f64> = closes.into_iter().map(|(_, p)| p).collect();
                crate::smarttrend::prime::hospital::symbol_banned_today(
                    &pnls,
                    policy.symbol_max_consecutive_losses,
                )
            })
            .map(|(s, _)| s)
            .collect();
        banned.sort();
        banned
    };

    Ok(HospitalPayload {
        quarantined,
        banned_symbols,
        policy,
    })
}

// ════════════════════════════════════════════════════════════════════
//  F4 — Frontier per-condition kill-rate report
// ════════════════════════════════════════════════════════════════════

/// One (setup, condition) aggregate over the window.
#[derive(Debug, Serialize)]
pub struct KillRateRow {
    pub setup: String,
    pub condition: String,
    pub total: u32,
    pub killed: u32,
    pub kill_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct FrontierKillRatePayload {
    pub window_hours: i64,
    /// Frontier decision rows inspected (candidates + near-misses).
    pub decisions: u32,
    /// Rows that passed all conditions (GO / shadow candidates).
    pub candidates: u32,
    pub rows: Vec<KillRateRow>,
}

/// F4 — which condition kills the most Frontier candidates. Reads the
/// condition vectors persisted in `decisions.sqlite` `components`.
#[tauri::command]
pub async fn get_frontier_killrate(hours: i64) -> Result<FrontierKillRatePayload, String> {
    let window_hours = if hours <= 0 { 24 } else { hours.min(24 * 14) };
    let log = decision_log::global().ok_or("decision journal not initialised")?;
    let now = chrono::Utc::now().timestamp();
    let recs = log
        .query("", now - window_hours * 3600, now, 5_000)
        .map_err(|e| format!("killrate query failed: {e}"))?;

    #[derive(serde::Deserialize)]
    struct Cond {
        name: String,
        passed: bool,
    }

    let mut agg: std::collections::HashMap<(String, String), (u32, u32)> =
        std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut decisions = 0u32;
    let mut candidates = 0u32;
    for r in &recs {
        // F4-APEX — the house strategy shares the condition-vector
        // protocol under `source = "apex"`.
        let setup = match r.source.strip_prefix("frontier:") {
            Some(s) => s,
            None if r.source == "apex" => "apex",
            None => continue,
        };
        let Some(cjson) = r.components.as_deref() else {
            continue;
        };
        let Ok(conds) = serde_json::from_str::<Vec<Cond>>(cjson) else {
            continue;
        };
        let is_candidate = conds.iter().all(|c| c.passed);
        let day = chrono::DateTime::<chrono::Utc>::from_timestamp(r.ts, 0)
            .map(|dt| {
                dt.with_timezone(&chrono_tz::US::Eastern)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".into());
        let state_key = if is_candidate {
            "candidate".to_string()
        } else {
            let failed: Vec<&str> = conds
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.name.as_str())
                .collect();
            format!("miss:{}", failed.join("+"))
        };
        // S34 — collapse historical repeated GO rows (pre-fix Frontier
        // dedup included moving conviction) so the dashboard reflects
        // distinct symbol/setup states, not one record per price tick.
        if !seen.insert(format!("{day}|{}|{setup}|{state_key}", r.symbol)) {
            continue;
        }
        decisions += 1;
        if is_candidate {
            candidates += 1;
        }
        for c in conds {
            let e = agg.entry((setup.to_string(), c.name)).or_insert((0, 0));
            e.0 += 1;
            if !c.passed {
                e.1 += 1;
            }
        }
    }

    let mut rows: Vec<KillRateRow> = agg
        .into_iter()
        .map(|((setup, condition), (total, killed))| KillRateRow {
            setup,
            condition,
            total,
            killed,
            kill_rate: if total > 0 {
                killed as f64 / total as f64
            } else {
                0.0
            },
        })
        .collect();
    rows.sort_by(|a, b| b.killed.cmp(&a.killed).then(b.total.cmp(&a.total)));

    Ok(FrontierKillRatePayload {
        window_hours,
        decisions,
        candidates,
        rows,
    })
}
