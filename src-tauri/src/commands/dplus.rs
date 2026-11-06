//! D-PLUS (Sesiunea 2) — Tauri command surface.
//!
//! Read-mostly endpoints serving the D-PLUS UI tab plus the manual
//! actions (reset adaptive risk, approve/reject combo proposals,
//! apply genome overrides, force-recompute causal graph).
//!
//! Lock semantics:
//!   - `state.dplus()` returns `Option<&DPlusHandles>`. All commands
//!     short-circuit with a friendly error when the protocol is
//!     disabled.
//!   - Each command holds at most one mutex / lock at a time; never
//!     across an await (no `.await` inside lock guards).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::market::market_state::MarketDataState;
use crate::smarttrend::strategies::StrategyId;
use crate::smarttrend::strategy_evolver::ProposalStatus;
use crate::state::TitanState;

// ════════════════════════════════════════════════════════════════════
//  Common helper
// ════════════════════════════════════════════════════════════════════

fn dplus_or_err(
    state: &Arc<TitanState>,
) -> Result<&crate::smarttrend::dplus::DPlusHandles, String> {
    state
        .dplus()
        .ok_or_else(|| "D-PLUS disabled (set evolution_protocol.enabled = true)".to_string())
}

// ════════════════════════════════════════════════════════════════════
//  Adaptive Risk
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct AdaptiveRiskRow {
    pub strategy: String,
    pub consecutive_losses: u32,
    pub consecutive_wins: u32,
    pub current_size_mult: f64,
    pub cooldown_until: i64,
    pub throttle_count: u32,
    pub deep_throttle_count: u32,
    pub last_outcome_ts: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdaptiveRiskOut {
    pub rows: Vec<AdaptiveRiskRow>,
    pub last_saved_at: i64,
}

#[tauri::command]
pub fn get_adaptive_risk_state(
    state: State<'_, Arc<TitanState>>,
) -> Result<AdaptiveRiskOut, String> {
    let dplus = dplus_or_err(state.inner())?;
    let r = dplus
        .adaptive_risk
        .lock()
        .map_err(|e| format!("adaptive_risk lock: {e}"))?;
    let rows: Vec<AdaptiveRiskRow> = r
        .rows()
        .into_iter()
        .map(|s| AdaptiveRiskRow {
            strategy: s.strategy.map(|id| id.to_string()).unwrap_or_default(),
            consecutive_losses: s.consecutive_losses,
            consecutive_wins: s.consecutive_wins,
            current_size_mult: s.current_size_mult,
            cooldown_until: s.cooldown_until,
            throttle_count: s.throttle_count,
            deep_throttle_count: s.deep_throttle_count,
            last_outcome_ts: s.last_outcome_ts,
        })
        .collect();
    Ok(AdaptiveRiskOut {
        rows,
        last_saved_at: r.last_saved_at,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResetRiskPayload {
    pub strategy: StrategyId,
}

#[tauri::command]
pub fn reset_adaptive_risk(
    state: State<'_, Arc<TitanState>>,
    payload: ResetRiskPayload,
) -> Result<bool, String> {
    let dplus = dplus_or_err(state.inner())?;
    let mut r = dplus
        .adaptive_risk
        .lock()
        .map_err(|e| format!("adaptive_risk lock: {e}"))?;
    r.reset(payload.strategy);
    let _ = r.save(&crate::smarttrend::risk_adaptive::default_base_dir());
    Ok(true)
}

// ════════════════════════════════════════════════════════════════════
//  Memory DB
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct MemoryStatsOut {
    pub total_setups: i64,
    pub closed_setups: i64,
    pub posteriors_trained: i64,
    pub last_write_ts: i64,
    pub schema_version: i64,
}

#[tauri::command]
pub fn get_memory_db_stats(
    state: State<'_, Arc<TitanState>>,
) -> Result<MemoryStatsOut, String> {
    let dplus = dplus_or_err(state.inner())?;
    let s = dplus
        .memory_db
        .stats()
        .map_err(|e| format!("memory_db stats: {e}"))?;
    Ok(MemoryStatsOut {
        total_setups: s.total_setups,
        closed_setups: s.closed_setups,
        posteriors_trained: s.posteriors_trained,
        last_write_ts: s.last_write_ts,
        schema_version: s.schema_version,
    })
}

// ════════════════════════════════════════════════════════════════════
//  Causal Lead-Lag
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct LeadLagEdgeOut {
    pub leader: String,
    pub lagger: String,
    pub lag_secs: i64,
    pub corr: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeadLagSnapshotOut {
    pub edges: Vec<LeadLagEdgeOut>,
    pub recomputed_at: i64,
    pub samples_seen: usize,
}

#[tauri::command]
pub fn get_lead_lag_graph(
    state: State<'_, Arc<TitanState>>,
) -> Result<LeadLagSnapshotOut, String> {
    let dplus = dplus_or_err(state.inner())?;
    let snap = {
        let eng = dplus
            .causal
            .lock()
            .map_err(|e| format!("causal lock: {e}"))?;
        eng.snapshot()
    };
    Ok(LeadLagSnapshotOut {
        edges: snap
            .edges
            .into_iter()
            .map(|e| LeadLagEdgeOut {
                leader: e.leader,
                lagger: e.lagger,
                lag_secs: e.lag_secs,
                corr: e.corr,
            })
            .collect(),
        recomputed_at: snap.recomputed_at,
        samples_seen: snap.samples_seen,
    })
}

#[tauri::command]
pub fn force_recompute_causal_graph(
    state: State<'_, Arc<TitanState>>,
) -> Result<bool, String> {
    let dplus = dplus_or_err(state.inner())?;
    let now = chrono::Utc::now().timestamp();
    let eng = dplus
        .causal
        .lock()
        .map_err(|e| format!("causal lock: {e}"))?;
    eng.recompute_graph(now);
    Ok(true)
}

// ════════════════════════════════════════════════════════════════════
//  Strategy Evolver — reports + proposals + overrides
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct EvolverProposalOut {
    pub id: String,
    pub created_at: i64,
    pub parent_a: String,
    pub parent_b: String,
    pub description: String,
    pub status: String,
}

#[tauri::command]
pub async fn list_evolver_proposals(
    state: State<'_, Arc<TitanState>>,
) -> Result<Vec<EvolverProposalOut>, String> {
    let dplus = dplus_or_err(state.inner())?;
    let evolver = dplus.evolver.lock().await;
    let list = evolver
        .list_proposals()
        .map_err(|e| format!("list_proposals: {e}"))?;
    Ok(list
        .into_iter()
        .map(|p| EvolverProposalOut {
            id: p.id,
            created_at: p.created_at,
            parent_a: p.parent_a.to_string(),
            parent_b: p.parent_b.to_string(),
            description: p.description,
            status: format!("{:?}", p.status).to_lowercase(),
        })
        .collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProposalActionPayload {
    pub id: String,
}

#[tauri::command]
pub async fn approve_evolver_proposal(
    state: State<'_, Arc<TitanState>>,
    payload: ProposalActionPayload,
) -> Result<bool, String> {
    let dplus = dplus_or_err(state.inner())?;
    let evolver = dplus.evolver.lock().await;
    evolver
        .set_proposal_status(&payload.id, ProposalStatus::Approved)
        .map_err(|e| format!("approve: {e}"))
}

#[tauri::command]
pub async fn reject_evolver_proposal(
    state: State<'_, Arc<TitanState>>,
    payload: ProposalActionPayload,
) -> Result<bool, String> {
    let dplus = dplus_or_err(state.inner())?;
    let evolver = dplus.evolver.lock().await;
    evolver
        .set_proposal_status(&payload.id, ProposalStatus::Rejected)
        .map_err(|e| format!("reject: {e}"))
}

#[derive(Debug, Clone, Serialize)]
pub struct OverridesSnapshotOut {
    pub strategies: std::collections::HashMap<String, std::collections::HashMap<String, f64>>,
}

#[tauri::command]
pub fn get_param_overrides(
    state: State<'_, Arc<TitanState>>,
) -> Result<OverridesSnapshotOut, String> {
    let dplus = dplus_or_err(state.inner())?;
    let snap = dplus.overrides.snapshot();
    let mut out = std::collections::HashMap::new();
    for (id, p) in snap {
        out.insert(id.to_string(), p);
    }
    Ok(OverridesSnapshotOut { strategies: out })
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplyGenomePayload {
    pub strategy: StrategyId,
}

#[tauri::command]
pub async fn run_evolver_now(
    state: State<'_, Arc<TitanState>>,
    _market_data: State<'_, Arc<MarketDataState>>,
    payload: ApplyGenomePayload,
) -> Result<bool, String> {
    let dplus = dplus_or_err(state.inner())?;
    
    let mut symbols: Vec<String> = {
        let market = state.market.lock().unwrap();
        market.auto_universe.active_set().into_iter().collect()
    };
    if symbols.is_empty() {
        symbols = state.config.lock().unwrap().ui.favorites.clone();
    }
    if symbols.is_empty() {
        symbols = vec!["AAPL".to_string(), "MSFT".to_string(), "NVDA".to_string()];
    }

    let (key, secret) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };

    let end = chrono::Utc::now();
    let start = end - chrono::Duration::days(90);
    let cache_dir = Some(crate::paths::data_dir().join("backtest_cache"));

    let (candles_90d, _) = crate::backtest::runner::fetch_universe(
        &symbols,
        "5m",
        start,
        end,
        &state.http_client,
        &key,
        &secret,
        cache_dir.as_deref(),
    )
    .await;

    let overrides_clone = Arc::clone(&dplus.overrides);
    let candles_clone = candles_90d.clone();
    let strat = payload.strategy;
    let evolver_clone = Arc::clone(&dplus.evolver);
    let res = tokio::task::spawn_blocking(move || {
        let mut evolver = evolver_clone.blocking_lock();
        evolver.run_generation(strat, &overrides_clone, &candles_clone)
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(format!("Evolver task panicked: {e}"))));
    match res {
        Ok(_) => {
            let _ = dplus
                .overrides
                .save(&crate::smarttrend::strategy_evolver::default_base_dir());
            Ok(true)
        }
        Err(e) => Err(format!("run_generation: {e}")),
    }
}
