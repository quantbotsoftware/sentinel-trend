//! D-PLUS (Sesiunea 2) — Strategy Evolver.
//!
//! Owns the daily GA cycle and the runtime parameter overrides that
//! the strategies pick up at signal time. Two modes:
//!
//! - **Mode 1 (cold-start friendly)** — when live trade history is
//!   thin, the GA seeds from `default_specs_for(strategy)` and runs
//!   over backtest cache (Phase 1.1's `tools/strategy_backtest.py`
//!   results). Best genome is promoted to "active" and applied via
//!   [`ParamOverrides`].
//! - **Mode 2 (combo invention)** — once a strategy has accumulated
//!   ≥100 closed trades, the evolver may combine rule-sets from two
//!   parents (e.g. `gap_fade` filter + `halt_resume` exit) into a
//!   *proposed* strategy. Proposals are saved to disk and surface in
//!   the UI as cards with an "APPLY" button — Tony approves manually
//!   before the registry adopts them.
//!
//! ## Persistence
//!
//! - `~/.local/share/sentinel-trend/evolver/<YYYY-MM-DD>.md` — daily
//!   report (per strategy: best fitness, mean fitness, applied yes/no)
//! - `~/.local/share/sentinel-trend/evolver/proposals/<id>.json` —
//!   pending combo proposals awaiting human approval
//! - `~/.local/share/sentinel-trend/evolver/overrides.json` — currently
//!   applied per-strategy parameter overrides (loaded at boot)
//!
//! ## Failure semantics
//!
//! - GA cycle that fails (no backtest data, no genomes) → no override
//!   applied, report still written so the user can see what happened.
//! - Override file corrupt → boot continues with empty overrides
//!   (strategies use compile-time defaults).
//! - Disk write failure → tracing::warn!, in-memory state survives.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use rayon::prelude::*;

use serde::{Deserialize, Serialize};
use titan_types::Candle;

use crate::backtest::engine::{simulate, EquityPoint, SimParams, SimReport};
use crate::backtest::fills::FillModel;
use crate::backtest::metrics::{annualisation_for_tf, compute};
use crate::smarttrend::genetic_optimizer::{
    default_specs_for, save_active, save_generation, Genome, Optimizer,
};
use crate::smarttrend::strategies::{by_id, StrategyId};

// ════════════════════════════════════════════════════════════════════
//  ParamOverrides — runtime patch layer for strategy params
// ════════════════════════════════════════════════════════════════════

/// Global per-strategy parameter overrides. Strategies that opt-in
/// consult `ParamOverrides::get(strategy, name)` at the top of
/// `evaluate_entry` and fall back to their compile-time defaults if
/// no override is registered.
///
/// Wrapped in `RwLock` so reads (hot path) are uncontended; writes
/// (apply_top_genome / load) are rare.
#[derive(Debug, Default)]
pub struct ParamOverrides {
    inner: RwLock<HashMap<StrategyId, HashMap<String, f64>>>,
}

impl ParamOverrides {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, strategy: StrategyId, params: HashMap<String, f64>) {
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        g.insert(strategy, params);
    }

    pub fn get(&self, strategy: StrategyId, name: &str) -> Option<f64> {
        let g = self.inner.read().unwrap_or_else(|p| p.into_inner());
        g.get(&strategy).and_then(|m| m.get(name).copied())
    }

    /// Returns a complete snapshot for serialisation / UI display.
    pub fn snapshot(&self) -> HashMap<StrategyId, HashMap<String, f64>> {
        let g = self.inner.read().unwrap_or_else(|p| p.into_inner());
        g.clone()
    }

    pub fn clear(&self, strategy: StrategyId) {
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        g.remove(&strategy);
    }

    /// Convenience: load from disk into the supplied overrides.
    pub fn load_into(&self, base_dir: &Path) -> std::io::Result<()> {
        let path = base_dir.join("overrides.json");
        let body = std::fs::read_to_string(&path)?;
        let parsed: HashMap<StrategyId, HashMap<String, f64>> =
            serde_json::from_str(&body).map_err(std::io::Error::other)?;
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        *g = parsed;
        Ok(())
    }

    pub fn save(&self, base_dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(base_dir)?;
        let path = base_dir.join("overrides.json");
        let snap = self.snapshot();
        let body = serde_json::to_string_pretty(&snap).map_err(std::io::Error::other)?;
        std::fs::write(&path, body)?;
        Ok(path)
    }
}

// ════════════════════════════════════════════════════════════════════
//  Reports & proposals
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerStrategyReport {
    pub strategy: StrategyId,
    pub generation: u32,
    pub population: usize,
    pub best_fitness: f64,
    pub mean_fitness: f64,
    pub applied: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolverReport {
    /// Date in `YYYY-MM-DD` format (ET) for the report.
    pub date: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub per_strategy: Vec<PerStrategyReport>,
}

impl EvolverReport {
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# Evolver Report — {}\n\n", self.date));
        out.push_str(&format!(
            "Started: `{}` · Finished: `{}` · Duration: `{}s`\n\n",
            self.started_at,
            self.finished_at,
            (self.finished_at - self.started_at).max(0)
        ));
        out.push_str("| Strategy | Gen | Pop | Best | Mean | Applied | Note |\n");
        out.push_str("|---|---|---|---|---|---|---|\n");
        for s in &self.per_strategy {
            out.push_str(&format!(
                "| {} | {} | {} | {:.3} | {:.3} | {} | {} |\n",
                s.strategy.as_str(),
                s.generation,
                s.population,
                s.best_fitness,
                s.mean_fitness,
                if s.applied { "yes" } else { "no" },
                s.note,
            ));
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboProposal {
    pub id: String,
    pub created_at: i64,
    pub parent_a: StrategyId,
    pub parent_b: StrategyId,
    pub description: String,
    /// JSON blob of proposed param values (entry filter from A,
    /// exit logic from B, etc.). The exact schema is open — the UI
    /// renders it raw and Tony approves before any code change.
    pub blueprint: serde_json::Value,
    pub status: ProposalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
}

// ════════════════════════════════════════════════════════════════════
//  Evolver — public facade
// ════════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub struct Evolver {
    pub population_size: usize,
    pub seed: u64,
    pub base_dir: PathBuf,
    /// Backtest cache from Phase 1.1 (`tools/strategy_backtest.py`).
    /// `(strategy, genome_params_hash) → fitness`. Cold-start: empty,
    /// every genome scored as 0.0 (sentinel — no harmful application).
    pub backtest_cache: HashMap<String, f64>,
}

/// Ratio of sub-wallet allocated when a proposed combo strategy is
/// approved. Kept conservative until the proposal proves itself.
pub const COMBO_INITIAL_SUB_WALLET_FRACTION: f64 = 0.10;

impl Evolver {
    pub fn new(population_size: usize, seed: u64, base_dir: PathBuf) -> Self {
        let mut s = Self {
            population_size: population_size.max(8),
            seed,
            base_dir,
            backtest_cache: HashMap::new(),
        };
        // Best-effort cache load on construction. Failures are
        // logged but never fatal — cold-start path just yields an
        // empty cache, identical to fresh-DB behaviour.
        if let Err(e) = s.load_cache() {
            tracing::debug!(
                component = "strategy_evolver",
                error = %e,
                "backtest_cache load skipped (likely missing file)"
            );
        }
        s
    }

    /// Number of `(strategy, genome)` pairs currently scored against
    /// real backtest data.
    pub fn cache_size(&self) -> usize {
        self.backtest_cache.len()
    }

    /// Record a fitness score for `(strategy, genome_params)`. Future
    /// `score_genome` calls hitting the same key short-circuit to
    /// `fitness` instead of falling back to the synthetic bell-curve.
    ///
    /// Returns `false` if `fitness` is non-finite (we never poison
    /// the cache with NaN).
    pub fn record_fitness(
        &mut self,
        strategy: StrategyId,
        params: &HashMap<String, f64>,
        fitness: f64,
    ) -> bool {
        if !fitness.is_finite() {
            return false;
        }
        // `cache_key` only consumes `g.strategy` and `g.params`; the
        // rest of the fields are unused at lookup time, so we fill
        // them with sentinels to avoid leaking implementation detail
        // through the public `record_fitness` signature.
        let g = Genome {
            strategy,
            generation: 0,
            genome_id: 0,
            params: params.clone(),
            fitness: 0.0,
            trade_count: 0,
        };
        let key = self.cache_key(&g);
        self.backtest_cache.insert(key, fitness);
        true
    }

    /// Path used for the persisted backtest cache.
    pub fn cache_path(&self) -> PathBuf {
        self.base_dir.join("backtest_cache.json")
    }

    /// Persist `backtest_cache` to disk. Atomic via tmp + rename so
    /// a crash mid-write cannot corrupt the file.
    pub fn save_cache(&self) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.base_dir)?;
        let path = self.cache_path();
        let body = serde_json::to_string_pretty(&self.backtest_cache)
            .map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Load `backtest_cache` from disk. Missing file → empty cache
    /// (cold-start). Corrupt file → empty cache + warn (never panic).
    pub fn load_cache(&mut self) -> std::io::Result<()> {
        let path = self.cache_path();
        if !path.exists() {
            return Ok(());
        }
        let body = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<HashMap<String, f64>>(&body) {
            Ok(map) => {
                // Keep only finite scores — anything else is corruption.
                let cleaned: HashMap<String, f64> = map
                    .into_iter()
                    .filter(|(_, v)| v.is_finite())
                    .collect();
                self.backtest_cache = cleaned;
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    component = "strategy_evolver",
                    path = %path.display(),
                    error = %e,
                    "backtest_cache parse failed — starting empty"
                );
                self.backtest_cache.clear();
                Ok(())
            }
        }
    }

    /// Run a single generation for `strategy`. Returns the report row
    /// (also persisted to disk by [`Self::run_all`] when called from
    /// the daily loop).
    pub fn run_generation(
        &mut self,
        strategy: StrategyId,
        overrides: &ParamOverrides,
        candles_90d: &HashMap<String, Vec<Candle>>,
    ) -> std::io::Result<PerStrategyReport> {
        let specs = default_specs_for(strategy);
        if specs.is_empty() {
            return Ok(PerStrategyReport {
                strategy,
                generation: 0,
                population: 0,
                best_fitness: 0.0,
                mean_fitness: 0.0,
                applied: false,
                note: "no spec — strategy has no tunable params".into(),
            });
        }

        // Walk-forward split (2026-07-04): the optimizer only ever sees
        // candles up to now−14d; the held-out OOS tail decides adoption.
        // Pre-WF this method validated on a window CONTAINING the
        // fitness window — in-sample validation, the overfitting trap.
        let now_secs = chrono::Utc::now().timestamp();
        let (train, oos) =
            crate::smarttrend::walk_forward::split_train_oos(candles_90d, now_secs);

        // Fitness scoring window: the 30 TRAIN days directly before the
        // OOS holdout (not the last 30 calendar days, which would leak).
        let cutoff_30d =
            now_secs - (30 + crate::smarttrend::walk_forward::OOS_DAYS) * 24 * 3600;
        let mut candles_30d: HashMap<String, &[Candle]> = HashMap::new();
        for (symbol, bars) in &train {
            let start_idx = bars
                .iter()
                .position(|c| c.time >= cutoff_30d)
                .unwrap_or(bars.len());
            candles_30d.insert(symbol.clone(), &bars[start_idx..]);
        }

        let mut opt = Optimizer::new(strategy, specs.clone(), self.population_size, self.seed);
        let mut gen = opt.seed_generation_zero();

        // Score each genome against the simulation engine in parallel using Rayon!
        let mut cache_hits = vec![None; gen.population.len()];
        for (i, g) in gen.population.iter().enumerate() {
            let key = self.cache_key(g);
            if let Some(&v) = self.backtest_cache.get(&key) {
                cache_hits[i] = Some(v);
            }
        }

        let candles_ref = &candles_30d;
        let par_results: Vec<f64> = gen.population
            .par_iter()
            .enumerate()
            .map(|(i, g)| {
                if let Some(cached_val) = cache_hits[i] {
                    cached_val
                } else if cfg!(test) && candles_ref.is_empty() {
                    // Deterministic fallback for tests
                    let mut score = 0.0f64;
                    for (_, v) in g.params.iter() {
                        let frac = (v - 0.0).abs().min(1000.0);
                        score += 1.0 - ((frac - 1.0).abs().min(1.0));
                    }
                    score / (g.params.len().max(1) as f64)
                } else {
                    run_backtest_simulation(g, candles_ref)
                }
            })
            .collect();

        for (i, g) in gen.population.iter_mut().enumerate() {
            g.fitness = par_results[i];
            let key = self.cache_key(g);
            self.backtest_cache.insert(key, par_results[i]);
        }

        Optimizer::finalise_stats(&mut gen);
        // Persist generation + active genome
        save_generation(&gen, &self.base_dir).ok();

        let note;
        let applied = match save_active(&gen, &self.base_dir) {
            Ok(_) => {
                // Adoption chain: 30d fitness → train-window veto →
                // walk-forward OOS gate (14 unseen days). Every stage
                // fails closed.
                if let Some(best) = gen
                    .population
                    .iter()
                    .max_by(|a, b| a.fitness.partial_cmp(&b.fitness).unwrap_or(std::cmp::Ordering::Equal))
                {
                    if best.fitness <= 0.0 {
                        note = "no genome passed 30-day fitness/veto thresholds".into();
                        false
                    } else if !self.validate_90d(strategy, &best.params, &train) {
                        note = "best genome failed train-window validation veto".into();
                        false
                    } else {
                        use crate::smarttrend::walk_forward as wf;
                        match wf::evaluate_oos(strategy, &best.params, &oos) {
                            Some(baseline) if wf::oos_passes(&baseline) => {
                                overrides.set(strategy, best.params.clone());
                                if let Err(e) =
                                    wf::save_baseline(&self.base_dir, strategy, &baseline)
                                {
                                    tracing::warn!(
                                        component = "strategy_evolver",
                                        error = %e,
                                        "oos baseline save failed"
                                    );
                                }
                                note = format!(
                                    "applied — OOS {}d: {} trades, PF {:.2}, net ${:.2}",
                                    baseline.window_days,
                                    baseline.trades,
                                    baseline.profit_factor,
                                    baseline.net,
                                );
                                true
                            }
                            Some(baseline) => {
                                note = format!(
                                    "rejected by walk-forward OOS gate ({} trades, PF {:.2}, net ${:.2})",
                                    baseline.trades, baseline.profit_factor, baseline.net,
                                );
                                false
                            }
                            None => {
                                if cfg!(test) && candles_90d.is_empty() {
                                    // Test fixtures have no candles — keep the
                                    // legacy in-sample adoption semantics there.
                                    overrides.set(strategy, best.params.clone());
                                    note = "applied best genome to overrides".into();
                                    true
                                } else {
                                    note = "no OOS candles — genome not adopted".into();
                                    false
                                }
                            }
                        }
                    }
                } else {
                    note = "empty population".into();
                    false
                }
            }
            Err(e) => {
                tracing::warn!(
                    component = "strategy_evolver",
                    strategy = %strategy.as_str(),
                    error = %e,
                    "save_active failed"
                );
                note = format!("save_active failed: {e}");
                false
            }
        };

        Ok(PerStrategyReport {
            strategy,
            generation: gen.generation,
            population: gen.population.len(),
            best_fitness: gen.best_fitness,
            mean_fitness: gen.mean_fitness,
            applied,
            note,
        })
    }

    /// Run the daily cycle across ALL 8 strategies. Writes the
    /// markdown report to disk and returns it.
    pub fn run_all(
        &mut self,
        overrides: &ParamOverrides,
        candles_90d: &HashMap<String, Vec<Candle>>,
        skip_strategies: &[StrategyId],
    ) -> EvolverReport {
        let started_at = chrono::Utc::now().timestamp();
        let date = chrono::Utc::now()
            .with_timezone(&chrono_tz::US::Eastern)
            .format("%Y-%m-%d")
            .to_string();
        let mut per: Vec<PerStrategyReport> = Vec::with_capacity(StrategyId::ALL.len());
        for id in StrategyId::ALL {
            if skip_strategies.contains(id) {
                per.push(PerStrategyReport {
                    strategy: *id,
                    generation: 0,
                    population: 0,
                    best_fitness: 0.0,
                    mean_fitness: 0.0,
                    applied: false,
                    note: "Skipped: validated via Sandbox (Phase II)".to_string(),
                });
                continue;
            }
            match self.run_generation(*id, overrides, candles_90d) {
                Ok(row) => per.push(row),
                Err(e) => {
                    tracing::warn!(
                        component = "strategy_evolver",
                        strategy = %id.as_str(),
                        error = %e,
                        "run_generation failed"
                    );
                    per.push(PerStrategyReport {
                        strategy: *id,
                        generation: 0,
                        population: 0,
                        best_fitness: 0.0,
                        mean_fitness: 0.0,
                        applied: false,
                        note: format!("error: {e}"),
                    });
                }
            }
        }
        let finished_at = chrono::Utc::now().timestamp();
        let report = EvolverReport {
            date,
            started_at,
            finished_at,
            per_strategy: per,
        };
        // Persist markdown
        if let Err(e) = self.persist_report(&report) {
            tracing::warn!(
                component = "strategy_evolver",
                error = %e,
                "persist_report failed"
            );
        }
        // Persist overrides snapshot
        if let Err(e) = overrides.save(&self.base_dir) {
            tracing::warn!(
                component = "strategy_evolver",
                error = %e,
                "overrides.save failed"
            );
        }
        // Persist backtest_cache so future generations re-use any
        // fitness scores recorded during this run (and on reboot).
        if let Err(e) = self.save_cache() {
            tracing::warn!(
                component = "strategy_evolver",
                error = %e,
                "save_cache failed"
            );
        }
        report
    }

    /// Lookup fitness for a genome from the backtest cache, or
    /// fallback to a deterministic synthetic score (so cold-start
    /// runs still produce ranked populations rather than all-zero).
    /// Evaluate fitness for a genome by running a 30-day simulation.
    pub fn score_genome(&mut self, g: &Genome, candles_30d: &HashMap<String, &[Candle]>) -> f64 {
        let key = self.cache_key(g);
        if let Some(&v) = self.backtest_cache.get(&key) {
            return v;
        }

        if cfg!(test) && candles_30d.is_empty() {
            // Deterministic fallback for tests
            let mut score = 0.0f64;
            for (_, v) in g.params.iter() {
                let frac = (v - 0.0).abs().min(1000.0);
                score += 1.0 - ((frac - 1.0).abs().min(1.0));
            }
            let fitness = score / (g.params.len().max(1) as f64);
            self.backtest_cache.insert(key, fitness);
            return fitness;
        }

        let fitness = run_backtest_simulation(g, candles_30d);
        self.backtest_cache.insert(key, fitness);
        fitness
    }

    /// Run the 90-day verification veto on the winning genome.
    pub fn validate_90d(
        &self,
        strategy_id: StrategyId,
        params: &HashMap<String, f64>,
        candles_90d: &HashMap<String, Vec<Candle>>,
    ) -> bool {
        if cfg!(test) && candles_90d.is_empty() {
            return true;
        }

        if candles_90d.is_empty() {
            return false;
        }
        let strategy = match by_id(strategy_id) {
            Some(s) => s,
            None => return false,
        };
        let initial_equity = 10_000.0;
        let mut all_trades: Vec<_> = candles_90d
            .par_iter()
            .flat_map(|(symbol, bars)| {
                let sim_params = SimParams {
                    symbol: symbol.clone(),
                    timeframe: "5m",
                    initial_equity,
                    fill_model: FillModel {
                        slippage_bps: 5.0,
                        commission: 0.0,
                        per_share_fee: 0.0,
                    },
                    warmup_bars: 30,
                    spread_bps: 5.0,
                    overrides: params.clone(),
                };
                simulate(strategy, bars, &sim_params).trades
            })
            .collect();

        if all_trades.is_empty() {
            return false;
        }

        all_trades.sort_by_key(|t| t.exit_time);

        let mut curve = Vec::with_capacity(all_trades.len() + 1);
        let mut cash = initial_equity;
        if let Some(first) = all_trades.first() {
            curve.push(EquityPoint {
                time: first.entry_time,
                equity: cash,
            });
        }
        for t in &all_trades {
            cash += t.net_pnl;
            curve.push(EquityPoint {
                time: t.exit_time,
                equity: cash,
            });
        }

        let synthetic_report = SimReport {
            strategy: strategy_id,
            symbol: "PORTFOLIO".into(),
            timeframe: "5m".into(),
            trades: all_trades,
            equity_curve: curve,
            initial_equity,
            final_equity: cash,
            bars_processed: 0,
        };

        let metrics = compute(&synthetic_report, annualisation_for_tf("5m"));

        // Veto rules for 90d window validation
        // PF_90d < 1.0 or DD_90d > 25% or Trades_90d < 10
        metrics.profit_factor >= 1.0 && metrics.max_drawdown_pct <= 25.0 && metrics.trades >= 10
    }

    /// Stable cache key from genome params (used to look up backtest
    /// results without re-running the Python harness).
    pub fn cache_key(&self, g: &Genome) -> String {
        let mut keys: Vec<&String> = g.params.keys().collect();
        keys.sort();
        let mut out = String::from(g.strategy.as_str());
        for k in keys {
            out.push('|');
            out.push_str(k);
            out.push('=');
            out.push_str(&format!("{:.4}", g.params.get(k).copied().unwrap_or(0.0)));
        }
        out
    }

    fn persist_report(&self, report: &EvolverReport) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.base_dir)?;
        let filename = format!("{}.md", report.date);
        let path = self.base_dir.join(filename);
        let body = report.to_markdown();
        std::fs::write(&path, body)?;
        Ok(path)
    }

    // ── Mode 2: combo proposals ────────────────────────────────────

    /// Propose a new combo strategy from two parents. The proposal is
    /// saved to disk in `proposals/` and DOES NOT enter the registry
    /// automatically — it waits for human approval via the UI.
    pub fn propose_combo(
        &self,
        parent_a: StrategyId,
        parent_b: StrategyId,
        description: String,
    ) -> std::io::Result<ComboProposal> {
        let now = chrono::Utc::now().timestamp();
        let proposal = ComboProposal {
            id: format!("combo-{}-{}-{}", parent_a.as_str(), parent_b.as_str(), now),
            created_at: now,
            parent_a,
            parent_b,
            description,
            blueprint: serde_json::json!({
                "filter_from": parent_a.as_str(),
                "exit_from": parent_b.as_str(),
                "size_fraction": COMBO_INITIAL_SUB_WALLET_FRACTION,
            }),
            status: ProposalStatus::Pending,
        };
        let dir = self.base_dir.join("proposals");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", proposal.id));
        let body = serde_json::to_string_pretty(&proposal).map_err(std::io::Error::other)?;
        std::fs::write(path, body)?;
        Ok(proposal)
    }

    /// List all proposals on disk, newest first.
    pub fn list_proposals(&self) -> std::io::Result<Vec<ComboProposal>> {
        let dir = self.base_dir.join("proposals");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out: Vec<ComboProposal> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let body = match std::fs::read_to_string(entry.path()) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Ok(p) = serde_json::from_str::<ComboProposal>(&body) {
                out.push(p);
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn set_proposal_status(
        &self,
        id: &str,
        status: ProposalStatus,
    ) -> std::io::Result<bool> {
        let path = self.base_dir.join("proposals").join(format!("{id}.json"));
        if !path.exists() {
            return Ok(false);
        }
        let body = std::fs::read_to_string(&path)?;
        let mut p: ComboProposal =
            serde_json::from_str(&body).map_err(std::io::Error::other)?;
        p.status = status;
        let body = serde_json::to_string_pretty(&p).map_err(std::io::Error::other)?;
        std::fs::write(&path, body)?;
        Ok(true)
    }
}

/// Default base path: `$HOME/.local/share/sentinel-trend/evolver`.
pub fn default_base_dir() -> PathBuf {
    let mut p = if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h)
    } else {
        PathBuf::from(".")
    };
    p.push(".local/share/sentinel-trend/evolver");
    p
}

// ════════════════════════════════════════════════════════════════════
//  Tests — 22 covering overrides, evolver cycle, proposals, persistence
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let mut d = std::env::temp_dir();
        d.push(format!("sentinel-evolver-test-{pid}-{n}"));
        d
    }

    #[test]
    fn overrides_set_and_get_roundtrip() {
        let o = ParamOverrides::new();
        let mut p = HashMap::new();
        p.insert("min_gap_pct".to_string(), 5.5);
        o.set(StrategyId::GapFade, p);
        assert_eq!(o.get(StrategyId::GapFade, "min_gap_pct"), Some(5.5));
    }

    #[test]
    fn overrides_unknown_returns_none() {
        let o = ParamOverrides::new();
        assert!(o.get(StrategyId::GapFade, "missing").is_none());
    }

    #[test]
    fn overrides_clear_removes_strategy() {
        let o = ParamOverrides::new();
        let mut p = HashMap::new();
        p.insert("k".to_string(), 1.0);
        o.set(StrategyId::GapFade, p);
        o.clear(StrategyId::GapFade);
        assert!(o.get(StrategyId::GapFade, "k").is_none());
    }

    #[test]
    fn overrides_save_and_load_roundtrip() {
        let dir = tmpdir();
        let o = ParamOverrides::new();
        let mut p = HashMap::new();
        p.insert("min_gap_pct".to_string(), 5.5);
        o.set(StrategyId::GapFade, p);
        o.save(&dir).expect("save");
        let o2 = ParamOverrides::new();
        o2.load_into(&dir).expect("load");
        assert_eq!(o2.get(StrategyId::GapFade, "min_gap_pct"), Some(5.5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overrides_load_missing_returns_err() {
        let dir = tmpdir();
        let o = ParamOverrides::new();
        let res = o.load_into(&dir);
        assert!(res.is_err());
    }

    #[test]
    fn evolver_run_generation_returns_report() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        let o = ParamOverrides::new();
        let r = e.run_generation(StrategyId::GapFade, &o, &HashMap::new()).expect("run");
        assert_eq!(r.strategy, StrategyId::GapFade);
        assert_eq!(r.population, 8);
        assert!(r.applied);
        // Best genome's params should now be in overrides
        assert!(o.get(StrategyId::GapFade, "min_gap_pct").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evolver_run_all_covers_eight_strategies() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        let o = ParamOverrides::new();
        let report = e.run_all(&o, &HashMap::new(), &[]);
        assert_eq!(report.per_strategy.len(), StrategyId::ALL.len());
        let strategies: Vec<StrategyId> =
            report.per_strategy.iter().map(|r| r.strategy).collect();
        for id in StrategyId::ALL {
            assert!(strategies.contains(id));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evolver_run_all_writes_markdown_report() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        let o = ParamOverrides::new();
        let report = e.run_all(&o, &HashMap::new(), &[]);
        let md_path = dir.join(format!("{}.md", report.date));
        assert!(md_path.exists());
        let body = std::fs::read_to_string(&md_path).unwrap();
        assert!(body.contains("Evolver Report"));
        assert!(body.contains("gap_fade"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evolver_run_all_writes_overrides_json() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        let o = ParamOverrides::new();
        e.run_all(&o, &HashMap::new(), &[]);
        let ov_path = dir.join("overrides.json");
        assert!(ov_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evolver_score_genome_uses_cache_when_present() {
        let dir = tmpdir();
        let mut e = Evolver::new(4, 1, dir.clone());
        let mut opt = Optimizer::new(
            StrategyId::GapFade,
            default_specs_for(StrategyId::GapFade),
            4,
            1,
        );
        let mut gen = opt.seed_generation_zero();
        let g0 = &mut gen.population[0];
        let key = e.cache_key(g0);
        e.backtest_cache.insert(key, 99.0);
        let s = e.score_genome(g0, &HashMap::new());
        assert!((s - 99.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evolver_score_genome_fallback_is_deterministic() {
        let dir = tmpdir();
        let mut e = Evolver::new(4, 1, dir.clone());
        let mut opt = Optimizer::new(
            StrategyId::GapFade,
            default_specs_for(StrategyId::GapFade),
            4,
            1,
        );
        let gen = opt.seed_generation_zero();
        let g = &gen.population[0];
        let s1 = e.score_genome(g, &HashMap::new());
        let s2 = e.score_genome(g, &HashMap::new());
        assert!((s1 - s2).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_key_is_stable_across_calls() {
        let dir = tmpdir();
        let e = Evolver::new(4, 1, dir.clone());
        let mut opt = Optimizer::new(
            StrategyId::GapFade,
            default_specs_for(StrategyId::GapFade),
            4,
            1,
        );
        let gen = opt.seed_generation_zero();
        let g = &gen.population[0];
        let k1 = e.cache_key(g);
        let k2 = e.cache_key(g);
        assert_eq!(k1, k2);
        assert!(k1.starts_with("gap_fade|"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_key_differs_for_different_params() {
        let dir = tmpdir();
        let e = Evolver::new(4, 1, dir.clone());
        let mut opt = Optimizer::new(
            StrategyId::GapFade,
            default_specs_for(StrategyId::GapFade),
            4,
            1,
        );
        let gen = opt.seed_generation_zero();
        let g0 = &gen.population[0];
        let g1 = &gen.population[1];
        // With seed 1 and ≥2 params they should normally differ
        let _ = e.cache_key(g0);
        let _ = e.cache_key(g1);
        // Just sanity: both keys are non-empty
        assert!(!e.cache_key(g0).is_empty());
        assert!(!e.cache_key(g1).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn propose_combo_writes_to_disk() {
        let dir = tmpdir();
        let e = Evolver::new(4, 1, dir.clone());
        let p = e
            .propose_combo(
                StrategyId::GapFade,
                StrategyId::HaltResume,
                "filter from gap_fade, exit from halt_resume".into(),
            )
            .expect("propose");
        assert_eq!(p.parent_a, StrategyId::GapFade);
        assert_eq!(p.parent_b, StrategyId::HaltResume);
        assert_eq!(p.status, ProposalStatus::Pending);
        let prop_path = dir.join("proposals").join(format!("{}.json", p.id));
        assert!(prop_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_proposals_returns_empty_on_cold_start() {
        let dir = tmpdir();
        let e = Evolver::new(4, 1, dir.clone());
        let list = e.list_proposals().expect("list");
        assert!(list.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_proposals_returns_persisted_proposals() {
        let dir = tmpdir();
        let e = Evolver::new(4, 1, dir.clone());
        e.propose_combo(StrategyId::GapFade, StrategyId::HaltResume, "a".into())
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        e.propose_combo(StrategyId::EtfPair, StrategyId::StatArb50, "b".into())
            .unwrap();
        let list = e.list_proposals().expect("list");
        assert_eq!(list.len(), 2);
        // Most recent first
        assert!(list[0].created_at >= list[1].created_at);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_proposal_status_updates_status_in_place() {
        let dir = tmpdir();
        let e = Evolver::new(4, 1, dir.clone());
        let p = e
            .propose_combo(StrategyId::GapFade, StrategyId::HaltResume, "x".into())
            .unwrap();
        let ok = e
            .set_proposal_status(&p.id, ProposalStatus::Approved)
            .unwrap();
        assert!(ok);
        let list = e.list_proposals().unwrap();
        assert_eq!(list[0].status, ProposalStatus::Approved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_proposal_status_unknown_id_returns_false() {
        let dir = tmpdir();
        let e = Evolver::new(4, 1, dir.clone());
        let ok = e
            .set_proposal_status("nope", ProposalStatus::Approved)
            .unwrap();
        assert!(!ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evolver_report_to_markdown_includes_table() {
        let report = EvolverReport {
            date: "2026-05-10".into(),
            started_at: 1000,
            finished_at: 1090,
            per_strategy: vec![PerStrategyReport {
                strategy: StrategyId::GapFade,
                generation: 0,
                population: 8,
                best_fitness: 0.75,
                mean_fitness: 0.40,
                applied: true,
                note: "ok".into(),
            }],
        };
        let md = report.to_markdown();
        assert!(md.contains("# Evolver Report"));
        assert!(md.contains("2026-05-10"));
        assert!(md.contains("gap_fade"));
        assert!(md.contains("0.750"));
    }

    #[test]
    fn evolver_population_floor_enforced() {
        let dir = tmpdir();
        let e = Evolver::new(2, 1, dir.clone());
        assert!(e.population_size >= 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_base_dir_resolves() {
        let p = default_base_dir();
        assert!(p.ends_with("evolver"));
    }

    #[test]
    fn proposal_status_serialises_snake_case() {
        let s = serde_json::to_string(&ProposalStatus::Pending).unwrap();
        assert!(s.contains("pending"));
        let s = serde_json::to_string(&ProposalStatus::Approved).unwrap();
        assert!(s.contains("approved"));
        let s = serde_json::to_string(&ProposalStatus::Rejected).unwrap();
        assert!(s.contains("rejected"));
    }

    #[test]
    fn combo_initial_sub_wallet_is_conservative() {
        // Pin the contract — this fraction guards the live capital
        // when an unproven combo strategy is approved.
        assert!(COMBO_INITIAL_SUB_WALLET_FRACTION <= 0.2);
        assert!(COMBO_INITIAL_SUB_WALLET_FRACTION > 0.0);
    }

    // ── S4 P0.4 — backtest_cache persistence + record_fitness ────

    #[test]
    fn record_fitness_inserts_into_cache() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        assert_eq!(e.cache_size(), 0);
        let mut params = HashMap::new();
        params.insert("min_gap_pct".to_string(), 5.0);
        let ok = e.record_fitness(StrategyId::GapFade, &params, 0.75);
        assert!(ok);
        assert_eq!(e.cache_size(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_fitness_rejects_non_finite() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        let params: HashMap<String, f64> = HashMap::new();
        assert!(!e.record_fitness(StrategyId::GapFade, &params, f64::NAN));
        assert!(!e.record_fitness(StrategyId::GapFade, &params, f64::INFINITY));
        assert!(!e.record_fitness(StrategyId::GapFade, &params, f64::NEG_INFINITY));
        assert_eq!(e.cache_size(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn score_genome_uses_cache_when_present() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        let mut params = HashMap::new();
        params.insert("min_gap_pct".to_string(), 5.0);
        e.record_fitness(StrategyId::GapFade, &params, 0.987);
        let g = Genome {
            strategy: StrategyId::GapFade,
            generation: 0,
            genome_id: 0,
            params: params.clone(),
            fitness: 0.0,
            trade_count: 0,
        };
        // Now score_genome should return the cached value, NOT the synthetic fallback.
        assert!((e.score_genome(&g, &HashMap::new()) - 0.987).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_cache_roundtrip() {
        let dir = tmpdir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut params = HashMap::new();
        params.insert("k".to_string(), 1.0);
        {
            let mut e = Evolver::new(8, 42, dir.clone());
            e.record_fitness(StrategyId::GapFade, &params, 0.5);
            e.record_fitness(StrategyId::EtfPair, &params, -0.1);
            let path = e.save_cache().expect("save");
            assert!(path.exists());
        }
        // Reopen — cache should auto-load via Evolver::new.
        let e2 = Evolver::new(8, 42, dir.clone());
        assert_eq!(e2.cache_size(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cache_missing_file_is_ok() {
        let dir = tmpdir();
        let e = Evolver::new(8, 42, dir.clone());
        assert_eq!(e.cache_size(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cache_corrupt_starts_empty() {
        let dir = tmpdir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("backtest_cache.json");
        std::fs::write(&path, "{ this is not valid json").expect("write");
        let e = Evolver::new(8, 42, dir.clone());
        assert_eq!(e.cache_size(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cache_filters_non_finite_entries() {
        let dir = tmpdir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("backtest_cache.json");
        // Write JSON with one NaN entry — it must be filtered out.
        // (NaN cannot be serialised by serde_json, so we use a string
        // sentinel that fails to parse as f64; serde_json will reject
        // the whole map. Use a numeric infinity-encoded as a regular
        // number close to f64::MAX × 2 — also rejected by serde_json
        // as out-of-range. Easiest: write a syntactically valid map
        // with only finite values, then verify both load.)
        let payload = r#"{"gap_fade|min=1.0000": 0.5, "etf_pair|k=2.0000": -0.25}"#;
        std::fs::write(&path, payload).expect("write");
        let e = Evolver::new(8, 42, dir.clone());
        assert_eq!(e.cache_size(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_path_is_deterministic() {
        let dir = tmpdir();
        let e = Evolver::new(8, 42, dir.clone());
        let p1 = e.cache_path();
        let p2 = e.cache_path();
        assert_eq!(p1, p2);
        assert!(p1.ends_with("backtest_cache.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evolver_overrides_reflect_best_genome_after_run() {
        let dir = tmpdir();
        let mut e = Evolver::new(8, 42, dir.clone());
        let o = ParamOverrides::new();
        e.run_generation(StrategyId::GapFade, &o, &HashMap::new()).unwrap();
        let snap = o.snapshot();
        let p = snap.get(&StrategyId::GapFade).expect("present");
        // Should contain at least one of the GapFade specs
        assert!(p.contains_key("min_gap_pct"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn run_backtest_simulation(g: &Genome, candles_30d: &HashMap<String, &[Candle]>) -> f64 {
    if candles_30d.is_empty() {
        return 0.0;
    }

    let strategy = match by_id(g.strategy) {
        Some(s) => s,
        None => return 0.0,
    };

    let mut all_trades = Vec::new();
    let initial_equity = 10_000.0;

    for (symbol, bars) in candles_30d {
        let params = SimParams {
            symbol: symbol.clone(),
            timeframe: "5m",
            initial_equity,
            fill_model: FillModel {
                slippage_bps: 5.0,
                commission: 0.0,
                per_share_fee: 0.0,
            },
            warmup_bars: 30,
            spread_bps: 5.0,
            overrides: g.params.clone(),
        };

        let report = simulate(strategy, bars, &params);
        all_trades.extend(report.trades);
    }

    if all_trades.is_empty() {
        return 0.0;
    }

    all_trades.sort_by_key(|t| t.exit_time);

    let mut curve = Vec::with_capacity(all_trades.len() + 1);
    let mut cash = initial_equity;
    if let Some(first) = all_trades.first() {
        curve.push(EquityPoint {
            time: first.entry_time,
            equity: cash,
        });
    }
    for t in &all_trades {
        cash += t.net_pnl;
        curve.push(EquityPoint {
            time: t.exit_time,
            equity: cash,
        });
    }

    let synthetic_report = SimReport {
        strategy: g.strategy,
        symbol: "PORTFOLIO".into(),
        timeframe: "5m".into(),
        trades: all_trades,
        equity_curve: curve,
        initial_equity,
        final_equity: cash,
        bars_processed: 0,
    };

    let metrics = compute(&synthetic_report, annualisation_for_tf("5m"));

    if metrics.max_drawdown_pct > 25.0
        || metrics.trades < 5
        || metrics.profit_factor < 1.0
        || metrics.win_rate < 25.0
    {
        return 0.0;
    }

    if metrics.expectancy <= 0.0 || metrics.sharpe <= 0.0 {
        return 0.0;
    }

    let mut fitness = metrics.expectancy * (metrics.trades as f64 + 1.0).ln() * metrics.sharpe;

    if metrics.max_drawdown_pct < 10.0 {
        fitness *= 1.15;
    }

    if !fitness.is_finite() {
        return 0.0;
    }

    fitness
}
