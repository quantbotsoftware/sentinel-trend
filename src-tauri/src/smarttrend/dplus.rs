//! D-PLUS (Sesiunea 2) — Container module.
//!
//! Aggregates all four D-PLUS subsystems (Adaptive Risk, Memory DB,
//! Causal Lead-Lag, Strategy Evolver) plus parameter overrides and the
//! tournament tracker behind a single [`DPlusHandles`] facade. Stored
//! in [`crate::state::TitanState::dplus`] (a `OnceLock`) so the bot
//! loop, the order-fill handlers, and the Tauri commands all reach the
//! same instance without duplicating Arc clones across the codebase.
//!
//! ## Trade attribution
//!
//! Legacy SentinelTrend dispatches set `BotAction.strategy_id = None`
//! and the order's `Position.source` to `"bot"`. New strategy-framework
//! signals encode the strategy id in the source string with the prefix
//! `"s2:<id>"` (e.g. `"s2:gap_fade"`). The close-hook helper
//! [`parse_strategy_from_source`] decodes this. Legacy trades produce
//! `None` and are skipped by the D-PLUS hooks (Tournament + AdaptiveRisk
//! + MemoryDb close-time updates) — that's the design intent: D-PLUS
//!   only attributes data it can confidently link to a strategy.
//!
//! ## Side-table for setup ids
//!
//! When a strategy-attributed BotAction submits an order, we record the
//! `(symbol, strategy_id) -> setup_id` returned by `MemoryDb::record_open`
//! in `pending_setup_ids`. The close hook pops the entry and calls
//! `MemoryDb::record_close(setup_id, pnl, ts)`. For legacy trades the
//! map stays empty and the close hook becomes a no-op — same outcome
//! as not having a strategy attribution, by construction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

use crate::ml::PredictorRegistry;
use crate::smarttrend::causal::CausalEngine;
use crate::smarttrend::memory_db::MemoryDb;
use crate::smarttrend::risk_adaptive::AdaptiveRisk;
use crate::smarttrend::strategies::{StrategyId, MarketRegime};
use crate::smarttrend::strategy_evolver::{Evolver, ParamOverrides};
use crate::smarttrend::tournament::Tournament;
use rust_decimal::prelude::ToPrimitive;

/// Side-table tying live setups (symbol + strategy) to MemoryDb row ids.
pub type SetupAttributionKey = (String, StrategyId);

pub enum DPlusWriteOp {
    SaveTournament(Tournament),
    SaveAdaptiveRisk(AdaptiveRisk),
}

#[allow(clippy::type_complexity)]
pub struct DPlusHandles {
    pub adaptive_risk: Arc<Mutex<AdaptiveRisk>>,
    pub memory_db: Arc<MemoryDb>,
    pub causal: Arc<Mutex<CausalEngine>>,
    pub evolver: Arc<tokio::sync::Mutex<Evolver>>,
    pub overrides: Arc<ParamOverrides>,
    pub tournament: Arc<Mutex<Tournament>>,
    /// `(symbol, strategy_id) → setup_id` populated at entry time so
    /// the close hook can call `MemoryDb::record_close(setup_id, ...)`.
    pub pending_setup_ids: Arc<Mutex<HashMap<SetupAttributionKey, i64>>>,
    /// `setup_id → (entry_price, atr, qty, ml_sample_id)` stashed at entry time
    /// for live ML learning updates.
    pub pending_entry_details: Arc<Mutex<HashMap<i64, (f64, f64, f64, i64)>>>,
    /// OMEGA (Sesiunea 3) — predictor registry. Read by the bot
    /// loop on every entry decision; hot-swapped by `predictor_loop`
    /// after a successful retrain. Behind `ArcSwap` so reads are
    /// lock-free (critical for the entry hot path).
    pub omega: Arc<ArcSwap<PredictorRegistry>>,
    /// Per-model enable flags. Toggled from the OMEGA UI tab; the
    /// bot reads these before consulting `omega`.
    pub omega_enabled: Arc<Mutex<HashMap<String, bool>>>,
    /// Asynchronous database logger for low-jitter stashing.
    pub ml_logger: crate::ml::ml_logger::MlLogger,
    /// Sequential write queue channel to eliminate filesystem race conditions.
    pub write_tx: tokio::sync::mpsc::UnboundedSender<DPlusWriteOp>,
    /// PRIME F2 — unified conviction engine. Read on every entry
    /// arbitration; mutated only by the nightly calibration (F5b) and
    /// the settings commands. Standalone mutex — never held together
    /// with any Tier-A/Tier-B guard (callers extract plain numbers
    /// first).
    pub prime: Arc<Mutex<crate::smarttrend::prime::PrimeEngine>>,
    /// PRIME F5a — shadow book (counterfactual tracking of vetoed
    /// entries). In-memory, bounded (S06 caps live in `shadow.rs`).
    /// Standalone mutex; same non-overlap discipline as `prime`.
    pub shadow: Arc<Mutex<crate::smarttrend::prime::shadow::ShadowBook>>,
}

impl DPlusHandles {
    pub fn new(
        adaptive_risk: Arc<Mutex<AdaptiveRisk>>,
        memory_db: Arc<MemoryDb>,
        causal: Arc<Mutex<CausalEngine>>,
        evolver: Arc<tokio::sync::Mutex<Evolver>>,
        overrides: Arc<ParamOverrides>,
        tournament: Arc<Mutex<Tournament>>,
    ) -> Self {
        let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<DPlusWriteOp>();
        let ml_logger = crate::ml::ml_logger::MlLogger::new(Arc::clone(&memory_db));
        
        // Background queue processor task
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                while let Some(op) = write_rx.recv().await {
                    match op {
                        DPlusWriteOp::SaveTournament(mut t) => {
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Err(e) = t.save(&crate::smarttrend::tournament::default_base_dir()) {
                                    tracing::warn!(
                                        component = "dplus",
                                        error = %e,
                                        "Tournament save failed (worker queue path)"
                                    );
                                }
                            }).await;
                        }
                        DPlusWriteOp::SaveAdaptiveRisk(mut r) => {
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Err(e) = r.save(&crate::smarttrend::risk_adaptive::default_base_dir()) {
                                    tracing::warn!(
                                        component = "dplus",
                                        error = %e,
                                        "AdaptiveRisk save failed (worker queue path)"
                                    );
                                }
                            }).await;
                        }
                    }
                }
            });
        }

        Self {
            adaptive_risk,
            memory_db,
            causal,
            evolver,
            overrides,
            tournament,
            pending_setup_ids: Arc::new(Mutex::new(HashMap::new())),
            pending_entry_details: Arc::new(Mutex::new(HashMap::new())),
            omega: Arc::new(ArcSwap::from_pointee(PredictorRegistry::new())),
            omega_enabled: Arc::new(Mutex::new(HashMap::new())),
            ml_logger,
            write_tx,
            prime: Arc::new(Mutex::new(
                crate::smarttrend::prime::PrimeEngine::load_or_default(
                    &crate::smarttrend::prime::default_engine_path(),
                ),
            )),
            shadow: Arc::new(Mutex::new(
                crate::smarttrend::prime::shadow::ShadowBook::new(),
            )),
        }
    }

    /// Convenience: replaces the OMEGA registry atomically. Used by
    /// `predictor_loop` after a successful retrain.
    pub fn swap_omega(&self, new_registry: PredictorRegistry) {
        self.omega.store(Arc::new(new_registry));
    }

    /// Returns `true` iff the model is enabled (defaults to `true`
    /// when no toggle has been recorded yet).
    pub fn is_omega_enabled(&self, model_id: &str) -> bool {
        self.omega_enabled
            .lock()
            .ok()
            .and_then(|g| g.get(model_id).copied())
            .unwrap_or(true)
    }

    pub fn set_omega_enabled(&self, model_id: &str, enabled: bool) {
        if let Ok(mut g) = self.omega_enabled.lock() {
            g.insert(model_id.to_string(), enabled);
        }
    }

    /// Record the open of a strategy-attributed setup. Adds the
    /// MemoryDb row id to `pending_setup_ids` so the close hook can
    /// consume it. No-op if `strategy_id` is `None`.
    pub fn on_trade_open(
        &self,
        symbol: &str,
        strategy_id: Option<StrategyId>,
        setup_id: Option<i64>,
    ) {
        let (Some(sid), Some(id)) = (strategy_id, setup_id) else {
            return;
        };
        let mut map = self
            .pending_setup_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        map.insert((symbol.to_uppercase(), sid), id);
    }

    /// Hook fired from the canonical trade-close paths. Updates the
    /// Tournament, AdaptiveRisk, MemoryDb close, and emits a
    /// `dplus-trade-close` event for the UI.
    ///
    /// Returns the resolved [`StrategyId`] (if any) for caller diagnostic.
    pub fn on_trade_close(
        &self,
        symbol: &str,
        source: &str,
        pnl: rust_decimal::Decimal,
        closed_at: i64,
        current_regime: MarketRegime,
    ) -> Option<StrategyId> {
        let sid = parse_strategy_from_source(source)?;
        
        // Tournament — record_trade_close under mutex lock, then push snapshot to the write queue.
        let tournament_snapshot = if let Ok(mut t) = self.tournament.lock() {
            t.record_trade_close(sid, pnl, closed_at);
            t.run_bandit_allocation(current_regime);

            // Auto-mutation check: if degraded and not already pending sandbox, crossover/mutate with the leader
            let (is_degraded, is_pending) = t.stats.get(&sid)
                .map(|s| (s.is_degraded(), s.pending_sandbox))
                .unwrap_or((false, false));
            if is_degraded && !is_pending {
                let leader = t.leaderboard().into_iter()
                    .find(|(leader_id, leader_stats)| {
                        leader_stats.status == crate::smarttrend::tournament::StrategyStatus::Active
                            && *leader_id != sid
                    })
                    .map(|(leader_id, _)| leader_id);
                t.mutate_degraded_strategy(sid, leader, &self.overrides);
            }

            Some(t.clone())
        } else {
            None
        };
        if let Some(snapshot) = tournament_snapshot {
            let _ = self.write_tx.send(DPlusWriteOp::SaveTournament(snapshot));
        }

        let pnl_f = pnl.to_f64().unwrap_or(0.0);

        // Adaptive Risk — record outcome under mutex lock, then push snapshot to the write queue.
        let risk_snapshot = if let Ok(mut r) = self.adaptive_risk.lock() {
            r.record_outcome(sid, pnl_f, closed_at);
            Some(r.clone())
        } else {
            None
        };
        if let Some(snapshot) = risk_snapshot {
            let _ = self.write_tx.send(DPlusWriteOp::SaveAdaptiveRisk(snapshot));
        }

        // Memory DB — close the setup row if one was recorded at entry
        let key: SetupAttributionKey = (symbol.to_uppercase(), sid);
        let setup_id_opt = {
            let mut map = self
                .pending_setup_ids
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            map.remove(&key)
        };
        if let Some(id) = setup_id_opt {
            let _ = self.memory_db.record_close(id, pnl_f, closed_at);

            // OMEGA Live-Learning update
            if let Some((_entry_price, atr, qty, ml_sample_id)) = self
                .pending_entry_details
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id)
            {
                if qty > 0.0 && atr > 0.0 {
                    let return_norm = (pnl_f / (qty * atr)).clamp(-6.0, 6.0);
                    self.ml_logger.update_return(ml_sample_id, return_norm, closed_at);
                }
            }
        }
        Some(sid)
    }
}

/// Parses `Position.source` into a [`StrategyId`].
///
/// - `"s2:gap_fade"` → `Some(GapFade)`
/// - `"bot"` (legacy SentinelTrend) → `None`
/// - `"user"` (manual order) → `None`
/// - empty / unknown → `None`
pub fn parse_strategy_from_source(source: &str) -> Option<StrategyId> {
    let s = source.trim();
    let rest = s.strip_prefix("s2:")?;
    match rest {
        "apex" => Some(StrategyId::Apex),
        "gap_fade" => Some(StrategyId::GapFade),
        "etf_pair" => Some(StrategyId::EtfPair),
        "afternoon_fade" => Some(StrategyId::AfternoonFade),
        "stat_arb_50" => Some(StrategyId::StatArb50),
        "leveraged_etf" => Some(StrategyId::LeveragedEtf),
        "catalyst_frontrun" => Some(StrategyId::CatalystFrontRun),
        "halt_resume" => Some(StrategyId::HaltResume),
        "extended_news" => Some(StrategyId::ExtendedNews),
        "opening_range_breakout" => Some(StrategyId::OpeningRangeBreakout),
        "frontier" => Some(StrategyId::Frontier),
        _ => None,
    }
}

/// Reverse of [`parse_strategy_from_source`] — formats a [`StrategyId`]
/// into the canonical `Position.source` prefix used by D-PLUS hooks.
pub fn source_for_strategy(id: StrategyId) -> String {
    format!("s2:{}", id.as_str())
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strategy_from_known_prefix() {
        assert_eq!(
            parse_strategy_from_source("s2:gap_fade"),
            Some(StrategyId::GapFade)
        );
        assert_eq!(
            parse_strategy_from_source("s2:halt_resume"),
            Some(StrategyId::HaltResume)
        );
    }

    #[test]
    fn parse_strategy_legacy_returns_none() {
        assert!(parse_strategy_from_source("bot").is_none());
        assert!(parse_strategy_from_source("user").is_none());
        assert!(parse_strategy_from_source("").is_none());
    }

    #[test]
    fn parse_strategy_unknown_returns_none() {
        assert!(parse_strategy_from_source("s2:made_up").is_none());
        assert!(parse_strategy_from_source("s3:gap_fade").is_none());
    }

    #[test]
    fn parse_handles_whitespace() {
        assert_eq!(
            parse_strategy_from_source("  s2:gap_fade  "),
            Some(StrategyId::GapFade)
        );
    }

    #[test]
    fn source_for_strategy_roundtrip() {
        for id in StrategyId::ALL {
            let s = source_for_strategy(*id);
            assert_eq!(parse_strategy_from_source(&s), Some(*id));
        }
    }
}
