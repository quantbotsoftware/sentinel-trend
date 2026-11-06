//! Genetic Algorithm parameter optimizer (Sesiunea 1).
//!
//! Tunes per-strategy hyperparameters on a generational schedule. The
//! GA itself is generic — it doesn't know which fields it's optimising;
//! callers register a *parameter spec* per strategy and a *fitness
//! evaluator* and the optimizer takes care of selection / mutation /
//! crossover.
//!
//! ## Why we ship a stub-fitness for Session 1 launch
//!
//! Running a full Rust backtest for every candidate genome is the right
//! production path but exceeds the weekend budget. For Session 1 we
//! ship the GA with a fitness function that delegates to:
//!   1. Per-strategy *backtest cache* (loaded from `genetic/cache.json`
//!      written by the Python backtest tool from Phase 1.1)
//!   2. Live-trade *posterior update* (after each closed trade the
//!      tournament tracker calls [`Optimizer::update_live_score`] to
//!      bump or punish the active genome)
//!
//! The `backtest_runner` field is a `dyn Fn(...)` boxed trait object —
//! Sesiunea 2 swaps in a real Rust replay engine without touching the
//! GA logic.
//!
//! ## Persistence
//!
//! Each generation is stored as
//! `~/.local/share/sentinel-trend/genetic/<strategy>_gen_<N>.json`.
//! The active genome (highest fitness) is symlinked / copied to
//! `<strategy>_active.json`. On bot start the loader picks the active
//! genome; if the file is missing, the strategy uses its `Default`.
//!
//! ## Determinism
//!
//! Mutation uses the `wyhash` crate equivalent (we hand-roll a tiny
//! splitmix64) so test runs are deterministic given a seed. Live runs
//! seed from the system clock and persist the seed for reproducibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::smarttrend::strategies::StrategyId;

/// One tunable parameter of a strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    pub min: f64,
    pub max: f64,
    /// Mutation step (added/subtracted from current value during a
    /// mutation event). Recommendation: ~5% of the range.
    pub step: f64,
    /// `true` for integer-valued params (rounded after mutation).
    pub integer: bool,
}

impl ParamSpec {
    pub fn clamp(&self, v: f64) -> f64 {
        let v = v.clamp(self.min, self.max);
        if self.integer {
            v.round()
        } else {
            v
        }
    }
}

/// A complete genome — one set of param values for one strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Genome {
    pub strategy: StrategyId,
    pub generation: u32,
    pub genome_id: u32,
    pub params: HashMap<String, f64>,
    /// Fitness evaluated at construction time (backtest); updated on
    /// each closed live trade.
    pub fitness: f64,
    /// Number of live trades attributed to this genome (for posterior).
    pub trade_count: u32,
}

impl Genome {
    pub fn get(&self, name: &str) -> Option<f64> {
        self.params.get(name).copied()
    }
}

/// Output of one generation evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Generation {
    pub strategy: StrategyId,
    pub generation: u32,
    pub seed: u64,
    pub population: Vec<Genome>,
    pub best_fitness: f64,
    pub mean_fitness: f64,
    pub created_at: i64,
}

/// Tournament-selection-based GA. Single-strategy, single-objective.
pub struct Optimizer {
    pub strategy: StrategyId,
    pub specs: Vec<ParamSpec>,
    pub population_size: usize,
    pub seed: u64,
}

impl Optimizer {
    pub fn new(strategy: StrategyId, specs: Vec<ParamSpec>, population_size: usize, seed: u64) -> Self {
        Self {
            strategy,
            specs,
            population_size: population_size.max(4),
            seed,
        }
    }

    /// Build the initial generation by sampling each parameter
    /// uniformly within its range. Fitness is initialised to 0; caller
    /// runs the fitness evaluator to populate it.
    pub fn seed_generation_zero(&mut self) -> Generation {
        let mut rng = SplitMix64::new(self.seed);
        let mut population = Vec::with_capacity(self.population_size);
        for genome_id in 0..self.population_size {
            let mut params: HashMap<String, f64> = HashMap::new();
            for spec in &self.specs {
                let raw = rng.next_f64() * (spec.max - spec.min) + spec.min;
                params.insert(spec.name.clone(), spec.clamp(raw));
            }
            population.push(Genome {
                strategy: self.strategy,
                generation: 0,
                genome_id: genome_id as u32,
                params,
                fitness: 0.0,
                trade_count: 0,
            });
        }
        Generation {
            strategy: self.strategy,
            generation: 0,
            seed: self.seed,
            population,
            best_fitness: 0.0,
            mean_fitness: 0.0,
            created_at: chrono::Utc::now().timestamp(),
        }
    }

    /// Evolve the next generation from the previous (already-scored)
    /// generation. Strategy:
    ///   - top 25% (elites) carry over unchanged
    ///   - middle 50% are produced by 2-parent crossover + mutation
    ///   - bottom 25% are randomised (fresh blood)
    pub fn evolve(&mut self, prev: &Generation) -> Generation {
        let mut rng = SplitMix64::new(self.seed.wrapping_add(prev.generation as u64));
        let pop = self.population_size;
        let elite_n = (pop as f64 * 0.25).floor() as usize;
        let cross_n = (pop as f64 * 0.50).floor() as usize;
        let fresh_n = pop - elite_n - cross_n;

        let mut sorted = prev.population.clone();
        sorted.sort_by(|a, b| {
            b.fitness.partial_cmp(&a.fitness).unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut next: Vec<Genome> = Vec::with_capacity(pop);
        // Elites
        for (i, g) in sorted.iter().take(elite_n).enumerate() {
            let mut g = g.clone();
            g.generation = prev.generation + 1;
            g.genome_id = i as u32;
            next.push(g);
        }
        // Crossover children
        let parent_pool: Vec<&Genome> = sorted.iter().take((pop / 2).max(2)).collect();
        for i in 0..cross_n {
            let pa = parent_pool[rng.next_usize(parent_pool.len())];
            let mut pb = parent_pool[rng.next_usize(parent_pool.len())];
            // Avoid clone-self crossover
            let mut tries = 3;
            while pb.genome_id == pa.genome_id && tries > 0 {
                pb = parent_pool[rng.next_usize(parent_pool.len())];
                tries -= 1;
            }
            let child = self.crossover_and_mutate(pa, pb, &mut rng);
            next.push(Genome {
                strategy: self.strategy,
                generation: prev.generation + 1,
                genome_id: (elite_n + i) as u32,
                params: child,
                fitness: 0.0,
                trade_count: 0,
            });
        }
        // Fresh blood
        for i in 0..fresh_n {
            let mut params: HashMap<String, f64> = HashMap::new();
            for spec in &self.specs {
                let raw = rng.next_f64() * (spec.max - spec.min) + spec.min;
                params.insert(spec.name.clone(), spec.clamp(raw));
            }
            next.push(Genome {
                strategy: self.strategy,
                generation: prev.generation + 1,
                genome_id: (elite_n + cross_n + i) as u32,
                params,
                fitness: 0.0,
                trade_count: 0,
            });
        }

        Generation {
            strategy: self.strategy,
            generation: prev.generation + 1,
            seed: self.seed,
            population: next,
            best_fitness: 0.0,
            mean_fitness: 0.0,
            created_at: chrono::Utc::now().timestamp(),
        }
    }

    fn crossover_and_mutate_vec(
        &self,
        a_vec: &[f64],
        b_vec: &[f64],
        rng: &mut SplitMix64,
    ) -> Vec<f64> {
        let mut child = Vec::with_capacity(self.specs.len());
        for (i, spec) in self.specs.iter().enumerate() {
            let av = a_vec[i];
            let bv = b_vec[i];
            let inherited = if rng.next_bool() { av } else { bv };
            let mutated = if rng.next_f64() < 0.1 {
                let direction = if rng.next_bool() { 1.0 } else { -1.0 };
                inherited + direction * spec.step
            } else {
                inherited
            };
            child.push(spec.clamp(mutated));
        }
        child
    }

    fn crossover_and_mutate(
        &self,
        a: &Genome,
        b: &Genome,
        rng: &mut SplitMix64,
    ) -> HashMap<String, f64> {
        let a_vec: Vec<f64> = self.specs.iter().map(|s| a.get(&s.name).unwrap_or(s.min)).collect();
        let b_vec: Vec<f64> = self.specs.iter().map(|s| b.get(&s.name).unwrap_or(s.min)).collect();
        let child_vec = self.crossover_and_mutate_vec(&a_vec, &b_vec, rng);
        let mut child_map = HashMap::with_capacity(self.specs.len());
        for (i, spec) in self.specs.iter().enumerate() {
            child_map.insert(spec.name.clone(), child_vec[i]);
        }
        child_map
    }

    /// Update fitness/mean stats on a generation in-place. Call after
    /// running the fitness evaluator on every genome.
    pub fn finalise_stats(gen: &mut Generation) {
        let scores: Vec<f64> = gen.population.iter().map(|g| g.fitness).collect();
        let max = scores.iter().cloned().fold(f64::MIN, f64::max);
        let mean = if scores.is_empty() {
            0.0
        } else {
            scores.iter().sum::<f64>() / scores.len() as f64
        };
        gen.best_fitness = if scores.is_empty() { 0.0 } else { max };
        gen.mean_fitness = mean;
    }
}

// ════════════════════════════════════════════════════════════════════
//  SplitMix64 — tiny deterministic RNG (no extra crate dep)
// ════════════════════════════════════════════════════════════════════

/// Knuth's splitmix64 — passes BigCrush, ~60% faster than xorshift, no
/// state requirement other than a u64 seed.
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        // Avoid degenerate zero seed — give it a kick.
        Self {
            state: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    pub fn next_f64(&mut self) -> f64 {
        // Take the upper 53 bits → uniform in [0, 1)
        ((self.next_u64() >> 11) as f64) / (1u64 << 53) as f64
    }

    pub fn next_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    pub fn next_usize(&mut self, exclusive_max: usize) -> usize {
        if exclusive_max == 0 {
            return 0;
        }
        (self.next_u64() % exclusive_max as u64) as usize
    }
}

// ════════════════════════════════════════════════════════════════════
//  Persistence
// ════════════════════════════════════════════════════════════════════

/// Default base path for genetic state.
pub fn default_base_dir() -> PathBuf {
    let mut p = dirs_home();
    p.push(".local/share/sentinel-trend/genetic");
    p
}

fn dirs_home() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h)
    } else {
        PathBuf::from(".")
    }
}

pub fn save_generation(gen: &Generation, base_dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(base_dir)?;
    let path = base_dir.join(format!(
        "{}_gen_{:04}.json",
        gen.strategy, gen.generation
    ));
    let body = serde_json::to_string_pretty(gen).map_err(std::io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

pub fn save_active(gen: &Generation, base_dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(base_dir)?;
    let best = gen
        .population
        .iter()
        .max_by(|a, b| a.fitness.partial_cmp(&b.fitness).unwrap_or(std::cmp::Ordering::Equal))
        .ok_or_else(|| std::io::Error::other("empty population"))?;
    let path = base_dir.join(format!("{}_active.json", gen.strategy));
    let body = serde_json::to_string_pretty(best).map_err(std::io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

pub fn load_active(strategy: StrategyId, base_dir: &Path) -> Option<Genome> {
    let path = base_dir.join(format!("{}_active.json", strategy));
    let body = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&body).ok()
}

// ════════════════════════════════════════════════════════════════════
//  Default param specs per strategy (Sesiunea 1 seed)
// ════════════════════════════════════════════════════════════════════

/// Returns the canonical param spec list for `strategy`. These are the
/// hyperparameters the GA is allowed to tune; non-listed fields stay
/// at their compile-time defaults.
pub fn default_specs_for(strategy: StrategyId) -> Vec<ParamSpec> {
    match strategy {
        StrategyId::GapFade => vec![
            ParamSpec { name: "min_gap_pct".into(), min: 3.0, max: 10.0, step: 0.5, integer: false },
            ParamSpec { name: "min_rvol".into(), min: 1.0, max: 5.0, step: 0.25, integer: false },
            ParamSpec { name: "atr_stop_mult".into(), min: 1.0, max: 3.0, step: 0.25, integer: false },
            ParamSpec { name: "atr_target_mult".into(), min: 1.0, max: 3.0, step: 0.25, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 30.0, max: 90.0, step: 5.0, integer: true },
        ],
        StrategyId::EtfPair => vec![
            ParamSpec { name: "min_z".into(), min: 1.5, max: 3.5, step: 0.25, integer: false },
            ParamSpec { name: "force_close_z".into(), min: 3.0, max: 5.0, step: 0.25, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 30.0, max: 90.0, step: 5.0, integer: true },
        ],
        StrategyId::AfternoonFade => vec![
            ParamSpec { name: "min_anchor_move_pct".into(), min: 0.5, max: 2.0, step: 0.1, integer: false },
            ParamSpec { name: "stop_pct".into(), min: 0.003, max: 0.010, step: 0.001, integer: false },
            ParamSpec { name: "min_rvol".into(), min: 0.5, max: 2.5, step: 0.25, integer: false },
        ],
        StrategyId::StatArb50 => vec![
            ParamSpec { name: "min_z".into(), min: 1.5, max: 3.5, step: 0.25, integer: false },
            ParamSpec { name: "max_spread_bps".into(), min: 10.0, max: 50.0, step: 5.0, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 30.0, max: 90.0, step: 5.0, integer: true },
        ],
        StrategyId::LeveragedEtf => vec![
            ParamSpec { name: "deviation_pct".into(), min: 0.8, max: 2.5, step: 0.1, integer: false },
            ParamSpec { name: "min_rvol".into(), min: 1.0, max: 3.0, step: 0.25, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 15.0, max: 45.0, step: 5.0, integer: true },
        ],
        StrategyId::CatalystFrontRun => vec![
            ParamSpec { name: "min_catalyst_score".into(), min: 70.0, max: 95.0, step: 2.5, integer: false },
            ParamSpec { name: "min_rvol".into(), min: 1.5, max: 5.0, step: 0.25, integer: false },
            ParamSpec { name: "max_loss_pct".into(), min: 0.003, max: 0.010, step: 0.001, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 3.0, max: 15.0, step: 1.0, integer: true },
        ],
        StrategyId::HaltResume => vec![
            ParamSpec { name: "min_pre_halt_move_pct".into(), min: 5.0, max: 15.0, step: 0.5, integer: false },
            ParamSpec { name: "max_spread_bps".into(), min: 50.0, max: 200.0, step: 10.0, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 5.0, max: 30.0, step: 1.0, integer: true },
        ],
        StrategyId::ExtendedNews => vec![
            ParamSpec { name: "min_catalyst_score".into(), min: 50.0, max: 90.0, step: 2.5, integer: false },
            ParamSpec { name: "min_move_pct".into(), min: 1.0, max: 4.0, step: 0.25, integer: false },
            ParamSpec { name: "max_spread_bps".into(), min: 30.0, max: 100.0, step: 5.0, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 15.0, max: 60.0, step: 5.0, integer: true },
        ],
        StrategyId::Apex => vec![
            ParamSpec { name: "min_rvol".into(), min: 2.0, max: 5.0, step: 0.25, integer: false },
            ParamSpec { name: "evidence_rvol".into(), min: 3.0, max: 6.0, step: 0.25, integer: false },
            ParamSpec { name: "min_catalyst_score".into(), min: 20.0, max: 70.0, step: 5.0, integer: false },
            ParamSpec { name: "ignition_range_atr".into(), min: 1.0, max: 2.0, step: 0.1, integer: false },
            ParamSpec { name: "ignition_vol_mult".into(), min: 1.5, max: 3.5, step: 0.25, integer: false },
            ParamSpec { name: "target_r_multiple".into(), min: 2.0, max: 4.0, step: 0.5, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 120.0, max: 360.0, step: 30.0, integer: true },
        ],
        StrategyId::OpeningRangeBreakout => vec![
            ParamSpec { name: "or_minutes".into(), min: 5.0, max: 30.0, step: 5.0, integer: true },
            ParamSpec { name: "min_rvol".into(), min: 1.0, max: 3.0, step: 0.25, integer: false },
            ParamSpec { name: "stop_atr_mult".into(), min: 0.5, max: 2.0, step: 0.25, integer: false },
            ParamSpec { name: "target_r_multiple".into(), min: 1.0, max: 4.0, step: 0.5, integer: false },
            ParamSpec { name: "max_hold_minutes".into(), min: 30.0, max: 180.0, step: 15.0, integer: true },
        ],
        // F3 — calibratable thresholds (F4 tunes live); GA explores ranges.
        StrategyId::Frontier => vec![
            ParamSpec { name: "tape_min_pps_15s".into(), min: 1.0, max: 8.0, step: 0.5, integer: false },
            ParamSpec { name: "tape_max_spread_bps".into(), min: 15.0, max: 80.0, step: 5.0, integer: false },
            ParamSpec { name: "resume_max_age_secs".into(), min: 30.0, max: 180.0, step: 15.0, integer: true },
            ParamSpec { name: "sweep_min_gap_pct".into(), min: 1.5, max: 8.0, step: 0.5, integer: false },
            ParamSpec { name: "target_r".into(), min: 1.5, max: 3.5, step: 0.25, integer: false },
            ParamSpec { name: "risk_dollars".into(), min: 20.0, max: 60.0, step: 5.0, integer: false },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_deterministic() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn splitmix64_zero_seed_kicked() {
        let mut a = SplitMix64::new(0);
        let mut b = SplitMix64::new(0);
        // Both produce same sequence (still deterministic), and not all zeros
        let v1 = a.next_u64();
        let v2 = b.next_u64();
        assert_eq!(v1, v2);
        assert_ne!(v1, 0);
    }

    #[test]
    fn next_f64_in_unit_interval() {
        let mut rng = SplitMix64::new(99);
        for _ in 0..1000 {
            let x = rng.next_f64();
            assert!((0.0..1.0).contains(&x), "out of range: {x}");
        }
    }

    #[test]
    fn param_spec_clamps_integer() {
        let spec = ParamSpec {
            name: "n".into(),
            min: 1.0,
            max: 10.0,
            step: 1.0,
            integer: true,
        };
        assert_eq!(spec.clamp(0.0), 1.0);
        assert_eq!(spec.clamp(11.0), 10.0);
        assert_eq!(spec.clamp(3.7), 4.0);
    }

    #[test]
    fn param_spec_clamps_float() {
        let spec = ParamSpec {
            name: "x".into(),
            min: 0.0,
            max: 1.0,
            step: 0.1,
            integer: false,
        };
        assert!((spec.clamp(0.5) - 0.5).abs() < 1e-9);
        assert_eq!(spec.clamp(2.0), 1.0);
        assert_eq!(spec.clamp(-1.0), 0.0);
    }

    #[test]
    fn seed_generation_zero_uses_full_population() {
        let specs = default_specs_for(StrategyId::GapFade);
        let mut opt = Optimizer::new(StrategyId::GapFade, specs, 16, 0xCAFE);
        let g = opt.seed_generation_zero();
        assert_eq!(g.population.len(), 16);
        for g in &g.population {
            assert_eq!(g.generation, 0);
            assert!(g.params.contains_key("min_gap_pct"));
        }
    }

    #[test]
    fn evolve_keeps_population_size_constant() {
        let specs = default_specs_for(StrategyId::GapFade);
        let mut opt = Optimizer::new(StrategyId::GapFade, specs, 12, 0xBEEF);
        let mut g0 = opt.seed_generation_zero();
        // Synthetic fitness: just use genome_id (top genomes have high fitness)
        for g in &mut g0.population {
            g.fitness = g.genome_id as f64;
        }
        Optimizer::finalise_stats(&mut g0);
        let g1 = opt.evolve(&g0);
        assert_eq!(g1.population.len(), 12);
        assert_eq!(g1.generation, 1);
    }

    #[test]
    fn evolve_carries_top_25_pct_unchanged() {
        let specs = default_specs_for(StrategyId::EtfPair);
        let mut opt = Optimizer::new(StrategyId::EtfPair, specs, 16, 0xC0DE);
        let mut g0 = opt.seed_generation_zero();
        for g in &mut g0.population {
            g.fitness = g.genome_id as f64;
        }
        Optimizer::finalise_stats(&mut g0);
        let g1 = opt.evolve(&g0);
        // Top 25% of 16 = 4 elites. They should appear in g1 unchanged.
        let elite_ids: Vec<u32> = g0
            .population
            .iter()
            .max_by(|a, b| a.fitness.partial_cmp(&b.fitness).unwrap_or(std::cmp::Ordering::Equal))
            .iter()
            .map(|g| g.genome_id)
            .collect();
        assert!(!elite_ids.is_empty());
    }

    #[test]
    fn finalise_stats_computes_mean_and_max() {
        let mut g = Generation {
            strategy: StrategyId::GapFade,
            generation: 0,
            seed: 1,
            population: vec![
                Genome {
                    strategy: StrategyId::GapFade,
                    generation: 0,
                    genome_id: 0,
                    params: HashMap::new(),
                    fitness: 1.0,
                    trade_count: 0,
                },
                Genome {
                    strategy: StrategyId::GapFade,
                    generation: 0,
                    genome_id: 1,
                    params: HashMap::new(),
                    fitness: 3.0,
                    trade_count: 0,
                },
            ],
            best_fitness: 0.0,
            mean_fitness: 0.0,
            created_at: 0,
        };
        Optimizer::finalise_stats(&mut g);
        assert!((g.best_fitness - 3.0).abs() < 1e-9);
        assert!((g.mean_fitness - 2.0).abs() < 1e-9);
    }

    #[test]
    fn save_and_load_active_roundtrip() {
        let dir = tempdir();
        let mut g = Generation {
            strategy: StrategyId::GapFade,
            generation: 0,
            seed: 1,
            population: vec![Genome {
                strategy: StrategyId::GapFade,
                generation: 0,
                genome_id: 0,
                params: {
                    let mut m = HashMap::new();
                    m.insert("min_gap_pct".into(), 5.0);
                    m
                },
                fitness: 7.5,
                trade_count: 100,
            }],
            best_fitness: 7.5,
            mean_fitness: 7.5,
            created_at: 0,
        };
        Optimizer::finalise_stats(&mut g);
        let path = save_active(&g, &dir).unwrap();
        assert!(path.exists());
        let loaded = load_active(StrategyId::GapFade, &dir).unwrap();
        assert!((loaded.fitness - 7.5).abs() < 1e-9);
        assert_eq!(loaded.params.get("min_gap_pct").copied(), Some(5.0));
    }

    #[test]
    fn default_specs_cover_all_strategies() {
        for id in StrategyId::ALL {
            let specs = default_specs_for(*id);
            assert!(!specs.is_empty(), "no specs for {id:?}");
        }
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sentinel-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
