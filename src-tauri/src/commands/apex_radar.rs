//! APEX radar — the whole watchlist × execution timeframes (2026-07-04).
//!
//! "From the entire watchlist the bot trades on, tell ME where to
//! trade and on which timeframe." This command sweeps every symbol in
//! `MarketDataState.watchlist_metrics` (SC7 — the same authoritative
//! source the bot's dispatcher sees) across the two execution
//! timeframes (5m, 1m), running the identical `compute_plan` gate
//! chain used by the chart overlay. It returns:
//!
//!   * `plans` — symbols with a VALID entry right now, ranked by
//!     ignition score (best first). Each row carries the full
//!     entry/stop/target/shares calculation.
//!   * `near` — symbols that pass the liquidity + evidence gates but
//!     are blocked at the trigger stage (no ignition bar yet, not
//!     aligned, already extended). These are "watch closely" names.
//!
//! Lock discipline: the symbol list is snapshotted from B6 taken
//! alone; `compute_plan` then acquires each Tier-B guard alone per
//! symbol. No guard is ever held across iterations, and Tier-A is
//! never overlapped with Tier-B.

use std::sync::Arc;

use serde::Serialize;

use crate::market::market_state::MarketDataState;
use crate::state::TitanState;

use crate::smarttrend::strategies::apex::Apex;

use super::trade_plan::{compute_plan, TradePlanPayload};

/// 15m first — `apex-tf` (60d) measured it as the best manual TF
/// (PF 1.49, 47.6% win); 5m/1m follow for faster triggers.
const RADAR_TFS: [&str; 3] = ["15m", "5m", "1m"];
const MAX_NEAR: usize = 12;

#[derive(Debug, Clone, Serialize)]
pub struct RadarPayload {
    pub generated_at: i64,
    /// symbols × timeframes evaluated this sweep.
    pub scanned: usize,
    /// Valid plans, best ignition score first.
    pub plans: Vec<TradePlanPayload>,
    /// Hot but not triggered — blocked at the ignition/alignment stage.
    pub near: Vec<TradePlanPayload>,
}

#[tauri::command]
pub fn get_apex_radar(
    state: tauri::State<'_, Arc<TitanState>>,
    market: tauri::State<'_, Arc<MarketDataState>>,
) -> Result<RadarPayload, String> {
    // B6 taken alone: snapshot the authoritative watchlist symbols.
    let symbols: Vec<String> = {
        let wms = market
            .watchlist_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        wms.keys().cloned().collect()
    };

    let mut plans: Vec<TradePlanPayload> = Vec::new();
    let mut near: Vec<TradePlanPayload> = Vec::new();
    let mut scanned = 0usize;

    for symbol in &symbols {
        for tf in RADAR_TFS {
            let plan = compute_plan(state.inner(), market.inner(), symbol.clone(), tf);
            scanned += 1;
            if plan.has_plan {
                plans.push(plan);
            } else if let Some(reason) = plan.block_reason.as_deref() {
                if Apex::NEAR_MISS_REASONS.contains(&reason) {
                    near.push(plan);
                }
            }
        }
    }

    plans.sort_by(|a, b| {
        b.score
            .unwrap_or(0.0)
            .partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    near.sort_by(|a, b| b.rvol.partial_cmp(&a.rvol).unwrap_or(std::cmp::Ordering::Equal));
    near.truncate(MAX_NEAR);

    Ok(RadarPayload {
        generated_at: chrono::Utc::now().timestamp(),
        scanned,
        plans,
        near,
    })
}
