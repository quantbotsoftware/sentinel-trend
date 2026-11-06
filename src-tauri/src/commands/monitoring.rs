use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::Utc;
use serde::Serialize;

use std::collections::HashMap;

use crate::market::market_state::MarketDataState;
use crate::ops::journal::compute_performance_stats;
use crate::state::{BreakerLevel, FsmPhase, MarkerKind, SymbolFsm, TitanState};

const LOOP_STALE_WARN_SECS: i64 = 60;
const LOOP_STALE_DEAD_SECS: i64 = 120;

// S36: with per-`(symbol, tf)` FSM keying, the monitoring HUD wants
// the "strongest" phase across all TFs for a symbol so the user sees
// `Active` the moment any TF transitions, not the lowest-cadence one.
fn fsm_phase_rank(p: FsmPhase) -> u8 {
    match p {
        FsmPhase::Active => 5,
        FsmPhase::Confirming => 4,
        FsmPhase::Pullback => 3,
        FsmPhase::Stalking => 2,
        FsmPhase::Exit => 1,
        FsmPhase::Idle => 0,
    }
}

pub(crate) fn pick_best_fsm_for_symbol(
    fsm_map: &HashMap<String, SymbolFsm>,
    symbol: &str,
) -> Option<SymbolFsm> {
    let prefix = format!("{}:", symbol.trim().to_uppercase());
    let mut best: Option<&SymbolFsm> = None;
    for (k, fsm) in fsm_map {
        let upper = k.to_uppercase();
        let matches = upper == symbol.trim().to_uppercase() || upper.starts_with(&prefix);
        if !matches {
            continue;
        }
        match best {
            None => best = Some(fsm),
            Some(cur) if fsm_phase_rank(fsm.phase) > fsm_phase_rank(cur.phase) => {
                best = Some(fsm);
            }
            _ => {}
        }
    }
    best.cloned()
}
const ENTRY_WINDOW_15M_SECS: i64 = 15 * 60;
const ENTRY_WINDOW_60M_SECS: i64 = 60 * 60;

// ═══════════════════════════════════════════════════════════════════════
//  ST Engine Details — pillar scores + FSM + safety status
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct PillarDetail {
    pub score: f64,
    pub direction: String,
    pub confidence: f64,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SafetyCheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryFrequencyMetrics {
    pub total_entries: usize,
    pub entries_last_15m: usize,
    pub entries_last_60m: usize,
    pub entries_per_hour_session: f64,
    pub entry_markers_total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WinrateMetrics {
    pub closed_trades: usize,
    pub winners: usize,
    pub losers: usize,
    pub winrate_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LateralNoiseMetrics {
    pub has_data: bool,
    pub stale_pillars: u8,
    pub low_confidence_pillars: u8,
    pub near_mid_band: bool,
    pub noisy_now: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockageMetrics {
    pub active_blockages: usize,
    pub failing_checks: Vec<String>,
    pub breaker_level: String,
    pub positions_open: usize,
    pub max_positions: usize,
    pub pending_orders: usize,
    pub stream_stale: bool,
    /// S05: cumulative bar-drop count from MarketDataState (atomic, no lock).
    #[serde(default)]
    pub bars_dropped: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrawdownMetrics {
    pub smarttrend_drawdown_pct: f64,
    pub smarttrend_max_drawdown_pct: f64,
    pub risk_drawdown_pct: f64,
    pub configured_limit_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryLoopObservation {
    pub status: String,
    pub stale_secs: i64,
    pub running: bool,
    pub tick_count: u64,
    pub persistence_watch: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationMetrics {
    pub entry_frequency: EntryFrequencyMetrics,
    pub winrate: WinrateMetrics,
    pub lateral_noise: LateralNoiseMetrics,
    pub blockages: BlockageMetrics,
    pub drawdown: DrawdownMetrics,
    pub discovery_loop: DiscoveryLoopObservation,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineDetailsResponse {
    pub has_data: bool,
    pub momentum_pillar: Option<PillarDetail>,
    pub combined_score: f64,
    pub combined_direction: String,
    pub strength: String,
    pub phase: String,
    pub confirm_count: u32,
    pub confirm_target: u32,
    pub max_hold_minutes: u32,
    pub safety_checks: Vec<SafetyCheckResult>,
    pub last_eval_ts: Option<i64>,
    pub evals_total: u64,
    pub validation_metrics: ValidationMetrics,
}

fn pillar_to_detail(ps: &crate::smarttrend::engine::PillarScore) -> PillarDetail {
    PillarDetail {
        score: ps.score,
        direction: format!("{:?}", ps.direction),
        confidence: ps.confidence,
        stale: ps.stale,
    }
}

fn stream_freshness_snapshot(state: &TitanState) -> (String, bool, Option<i64>, i64) {
    // S05: non-overlapping acquisitions — config(1) dropped before market(2)
    let stale_data_secs = {
        let cfg = state.config.lock().unwrap();
        cfg.market.stale_data_secs as i64
    };
    let (active_symbol, last_quote_ts) = {
        let market = state.market.lock().unwrap();
        let symbol = market.active_symbol.clone();
        let ts = market.last_quote_ts.get(&symbol).copied();
        (symbol, ts)
    };
    let now_ts = Utc::now().timestamp();
    let quote_age_secs = last_quote_ts.map(|ts| (now_ts - ts).max(0));
    let is_fresh = quote_age_secs
        .map(|age| age <= stale_data_secs)
        .unwrap_or(false);
    (active_symbol, is_fresh, quote_age_secs, stale_data_secs)
}

fn build_safety_checks(state: &TitanState) -> Vec<SafetyCheckResult> {
    // S05: non-overlapping acquisitions — 1,2,3,4,5 each dropped before next
    let (max_pos, max_dd, allow_short) = {
        let cfg = state.config.lock().unwrap();
        (cfg.execution.max_positions as usize, cfg.smarttrend.max_drawdown_pct, cfg.execution.allow_short)
    };
    let (stream_symbol, stream_fresh, quote_age_secs, stale_data_secs) = stream_freshness_snapshot(state);
    let dd = state.smarttrend.lock().unwrap().account.drawdown_pct();
    let pos_count = state.trading.lock().unwrap().positions.len();
    let (breaker_ok, breaker_level) = {
        let risk = state.risk.lock().unwrap();
        (risk.breaker == BreakerLevel::Normal, risk.breaker)
    };
    let stream_detail = match quote_age_secs {
        Some(age) => format!(
            "{stream_symbol}: {age}s <= {stale_data_secs}s (monitoring-only status; policy unchanged)"
        ),
        None => format!(
            "{stream_symbol}: no quote timestamp (monitoring-only status; policy unchanged)"
        ),
    };

    vec![
        SafetyCheckResult {
            name: "ATR Bracket Stop".into(),
            passed: true,
            detail: "checked at order time".into(),
        },
        SafetyCheckResult {
            name: "Stream Not Stale".into(),
            passed: stream_fresh,
            detail: stream_detail,
        },
        SafetyCheckResult {
            name: "Breaker Normal".into(),
            passed: breaker_ok,
            detail: format!("{:?}", breaker_level),
        },
        SafetyCheckResult {
            name: "Max Positions".into(),
            passed: pos_count < max_pos,
            detail: format!("{}/{}", pos_count, max_pos),
        },
        SafetyCheckResult {
            name: "Drawdown Limit".into(),
            passed: dd < max_dd,
            detail: format!("{:.1}%/{:.0}%", dd, max_dd),
        },
        SafetyCheckResult {
            name: "Short Allowed".into(),
            passed: true,
            detail: if allow_short { "yes" } else { "long only" }.into(),
        },
        SafetyCheckResult {
            name: "No Duplicate Pos".into(),
            passed: true,
            detail: "checked at order time".into(),
        },
    ]
}

fn classify_loop_status(age_secs: i64) -> &'static str {
    if age_secs >= LOOP_STALE_DEAD_SECS {
        "DEAD"
    } else if age_secs >= LOOP_STALE_WARN_SECS {
        "STALE"
    } else {
        "OK"
    }
}

fn build_discovery_loop_observation(state: &TitanState) -> (i64, DiscoveryLoopObservation) {
    let now = Utc::now();
    let sys = state.system.lock().unwrap();
    let uptime_secs = sys.uptime_secs();
    let obs = if let Some(health) = sys.loops.get("discovery") {
        let age_secs = (now - health.last_tick).num_seconds().max(0);
        let status = if !health.running || health.tick_count == 0 {
            if age_secs >= LOOP_STALE_DEAD_SECS {
                "DEAD".to_string()
            } else {
                "STARTING".to_string()
            }
        } else {
            classify_loop_status(age_secs).to_string()
        };
        let persistence_watch = status == "STALE";
        let note = if status == "DEAD" {
            "Discovery loop is DEAD; investigate immediately.".to_string()
        } else if persistence_watch {
            "Loop stale observed on discovery (operational only); track if it becomes persistent."
                .to_string()
        } else {
            "Discovery loop observation is operational only; no execution policy impact.".to_string()
        };
        DiscoveryLoopObservation {
            status,
            stale_secs: age_secs,
            running: health.running,
            tick_count: health.tick_count,
            persistence_watch,
            note,
        }
    } else {
        DiscoveryLoopObservation {
            status: "MISSING".into(),
            stale_secs: -1,
            running: false,
            tick_count: 0,
            persistence_watch: false,
            note: "Discovery loop not registered in system health map.".into(),
        }
    };
    (uptime_secs, obs)
}

fn build_entry_frequency_metrics(
    entries: &[crate::state::JournalEntry],
    entry_markers_total: usize,
    session_uptime_secs: i64,
) -> EntryFrequencyMetrics {
    let now_ts = Utc::now().timestamp();
    let entries_last_15m = entries
        .iter()
        .filter(|e| e.entry_time >= now_ts - ENTRY_WINDOW_15M_SECS)
        .count();
    let entries_last_60m = entries
        .iter()
        .filter(|e| e.entry_time >= now_ts - ENTRY_WINDOW_60M_SECS)
        .count();
    let entries_per_hour_session = if session_uptime_secs > 0 {
        entries.len() as f64 * 3600.0 / session_uptime_secs as f64
    } else {
        0.0
    };
    EntryFrequencyMetrics {
        total_entries: entries.len(),
        entries_last_15m,
        entries_last_60m,
        entries_per_hour_session,
        entry_markers_total,
    }
}

fn build_winrate_metrics(entries: &[crate::state::JournalEntry]) -> WinrateMetrics {
    let perf = compute_performance_stats(entries);
    WinrateMetrics {
        closed_trades: perf.closed_trades,
        winners: perf.winners,
        losers: perf.losers,
        winrate_pct: perf.win_rate,
    }
}

fn build_lateral_noise_metrics(
    diag: Option<&crate::smarttrend::engine::PillarDiagnostics>,
) -> LateralNoiseMetrics {
    if let Some(d) = diag {
        let stale_pillars = if d.momentum_score.stale { 1u8 } else { 0u8 };
        let low_confidence_pillars =
            if d.momentum_score.confidence < 0.35 { 1u8 } else { 0u8 };
        let near_mid_band = (d.combined_score - 50.0).abs() <= 7.5;
        let noisy_now = stale_pillars >= 1 || (near_mid_band && low_confidence_pillars >= 1);
        LateralNoiseMetrics {
            has_data: true,
            stale_pillars,
            low_confidence_pillars,
            near_mid_band,
            noisy_now,
        }
    } else {
        LateralNoiseMetrics {
            has_data: false,
            stale_pillars: 0,
            low_confidence_pillars: 0,
            near_mid_band: false,
            noisy_now: false,
        }
    }
}

fn build_blockage_metrics(
    safety_checks: &[SafetyCheckResult],
    breaker_level: BreakerLevel,
    positions_open: usize,
    max_positions: usize,
    pending_orders: usize,
    bars_dropped: u64,
) -> BlockageMetrics {
    let failing_checks: Vec<String> = safety_checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.clone())
        .collect();
    let stream_stale = failing_checks.iter().any(|name| name == "Stream Not Stale");
    BlockageMetrics {
        active_blockages: failing_checks.len(),
        failing_checks,
        breaker_level: format!("{:?}", breaker_level),
        positions_open,
        max_positions,
        pending_orders,
        stream_stale,
        bars_dropped,
    }
}

fn build_drawdown_metrics(
    smarttrend_drawdown_pct: f64,
    smarttrend_max_drawdown_pct: f64,
    risk_drawdown_pct: f64,
    configured_limit_pct: f64,
) -> DrawdownMetrics {
    DrawdownMetrics {
        smarttrend_drawdown_pct,
        smarttrend_max_drawdown_pct,
        risk_drawdown_pct,
        configured_limit_pct,
    }
}

#[tauri::command]
pub fn get_engine_details(
    state: tauri::State<'_, Arc<TitanState>>,
    market_data: tauri::State<'_, Arc<MarketDataState>>,
) -> EngineDetailsResponse {
    // S05: non-overlapping serialised acquisitions — each lock is acquired
    // in a scoped block and dropped before the next: 1, 2, 3, 4, 5, 7, 6.
    // Never two simultaneously.
    let (max_hold, max_positions, configured_limit_pct) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.smarttrend.max_hold_minutes,
            cfg.execution.max_positions as usize,
            cfg.smarttrend.max_drawdown_pct,
        )
    };
    let active_symbol = state.market.lock().unwrap().active_symbol.clone();
    let (phase, confirm_count, has_data, combined_score, combined_dir,
         strength, momentum_pillar,
         last_eval_ts, evals_total, last_diag, entry_markers_total,
         smarttrend_drawdown_pct, smarttrend_max_drawdown_pct) = {
        let st = state.smarttrend.lock().unwrap();
        // S36 (2026-05-28 18:46 ET) — FSM is keyed by `(symbol, tf)`.
        // For the top-of-screen monitoring HUD, surface the FSM that
        // represents the most-advanced phase across all TFs of the
        // active symbol — Active > Confirming > Stalking > Exit > Idle.
        // This way Tony's status panel shows "Active" the moment any
        // TF's FSM reaches Active, instead of the lowest-cadence-only
        // view he'd get from picking a single TF arbitrarily.
        let fsm = pick_best_fsm_for_symbol(&st.fsm, &active_symbol);
        let phase = fsm
            .as_ref()
            .map(|f| format!("{:?}", f.phase))
            .unwrap_or("Idle".into());
        let confirm_count = fsm.as_ref().map(|f| f.confirm_count).unwrap_or(0);
        let sentinel = &st.sentinel_status;
        let diag = st.last_diagnostics.clone();
        let has_data = diag.is_some();
        let (cs, cd, s) = if let Some(d) = diag.as_ref() {
            (d.combined_score, format!("{:?}", d.combined_direction),
             format!("{:?}", d.strength))
        } else {
            (0.0, "---".into(), "---".into())
        };
        let mom = diag.as_ref().map(|d| pillar_to_detail(&d.momentum_score));
        let entry_markers_total = st
            .markers
            .iter()
            .filter(|m| m.kind == MarkerKind::Entry)
            .count();
        (phase, confirm_count, has_data, cs, cd, s, mom,
         sentinel.last_eval_ts, sentinel.evals_total, diag, entry_markers_total,
         st.account.drawdown_pct(), st.account.max_drawdown_pct)
    };
    let safety_checks = build_safety_checks(&state);
    let (positions_open, pending_orders) = {
        let trading = state.trading.lock().unwrap();
        (trading.positions.len(), trading.pending_orders.len())
    };
    let (risk_drawdown_pct, breaker_level) = {
        let risk = state.risk.lock().unwrap();
        (risk.current_drawdown_pct, risk.breaker)
    };
    let (session_uptime_secs, discovery_loop) = build_discovery_loop_observation(&state);
    let (entry_frequency, winrate) = {
        let journal = state.journal.lock().unwrap();
        (
            build_entry_frequency_metrics(
                &journal.entries,
                entry_markers_total,
                session_uptime_secs,
            ),
            build_winrate_metrics(&journal.entries),
        )
    };
    let lateral_noise = build_lateral_noise_metrics(last_diag.as_ref());
    // S09: bars_dropped wired via MarketDataState atomic (lock-free, no ordering risk).
    let bars_dropped = market_data.bars_dropped.load(Ordering::Relaxed);
    let blockages = build_blockage_metrics(
        &safety_checks,
        breaker_level,
        positions_open,
        max_positions,
        pending_orders,
        bars_dropped,
    );
    let drawdown = build_drawdown_metrics(
        smarttrend_drawdown_pct,
        smarttrend_max_drawdown_pct,
        risk_drawdown_pct,
        configured_limit_pct,
    );

    EngineDetailsResponse {
        has_data,
        momentum_pillar,
        combined_score,
        combined_direction: combined_dir,
        strength,
        phase,
        confirm_count,
        confirm_target: crate::smarttrend::engine::ENTRY_CONFIRM_TARGET,
        max_hold_minutes: max_hold,
        safety_checks,
        last_eval_ts,
        evals_total,
        validation_metrics: ValidationMetrics {
            entry_frequency,
            winrate,
            lateral_noise,
            blockages,
            drawdown,
            discovery_loop,
        },
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_details_response_serializes() {
        let resp = EngineDetailsResponse {
            has_data: false,
            momentum_pillar: None,
            combined_score: 0.0,
            combined_direction: "---".into(),
            strength: "---".into(),
            phase: "Idle".into(),
            confirm_count: 0,
            confirm_target: crate::smarttrend::engine::ENTRY_CONFIRM_TARGET,
            max_hold_minutes: 120,
            safety_checks: vec![],
            last_eval_ts: None,
            evals_total: 0,
            validation_metrics: ValidationMetrics {
                entry_frequency: EntryFrequencyMetrics {
                    total_entries: 0,
                    entries_last_15m: 0,
                    entries_last_60m: 0,
                    entries_per_hour_session: 0.0,
                    entry_markers_total: 0,
                },
                winrate: WinrateMetrics {
                    closed_trades: 0,
                    winners: 0,
                    losers: 0,
                    winrate_pct: 0.0,
                },
                lateral_noise: LateralNoiseMetrics {
                    has_data: false,
                    stale_pillars: 0,
                    low_confidence_pillars: 0,
                    near_mid_band: false,
                    noisy_now: false,
                },
                blockages: BlockageMetrics {
                    active_blockages: 0,
                    failing_checks: vec![],
                    breaker_level: "Normal".into(),
                    positions_open: 0,
                    max_positions: 3,
                    pending_orders: 0,
                    stream_stale: false,
                    bars_dropped: 0,
                },
                drawdown: DrawdownMetrics {
                    smarttrend_drawdown_pct: 0.0,
                    smarttrend_max_drawdown_pct: 0.0,
                    risk_drawdown_pct: 0.0,
                    configured_limit_pct: 5.0,
                },
                discovery_loop: DiscoveryLoopObservation {
                    status: "OK".into(),
                    stale_secs: 15,
                    running: true,
                    tick_count: 42,
                    persistence_watch: false,
                    note: "Discovery loop observation is operational only; no execution policy impact."
                        .into(),
                },
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Idle"));
        assert!(json.contains("winrate"));
    }

    #[test]
    fn loop_status_classification_uses_watchdog_thresholds() {
        assert_eq!(classify_loop_status(0), "OK");
        assert_eq!(classify_loop_status(59), "OK");
        assert_eq!(classify_loop_status(60), "STALE");
        assert_eq!(classify_loop_status(119), "STALE");
        assert_eq!(classify_loop_status(120), "DEAD");
    }

}
