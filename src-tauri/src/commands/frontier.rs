//! FRONTIER (Sesiunea 4) — Tauri command surface.
//!
//! Read-only endpoints serving the FRONTIER UI tab. The commands
//! deliberately mirror the OMEGA command shape so the front-end
//! has a uniform pattern: every endpoint returns plain serde-friendly
//! types, never holds a state lock across an `await`, and errors
//! consistently when the supporting state isn't initialised.

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::ml::bandit::{ArmStats, ARM_MULTIPLIERS, COLD_START_TRADES_PER_BUCKET, NUM_ARMS};
use crate::ml::monte_carlo::{
    build_inputs_from_entry, simulate, GbmConfig, McGate, MAX_HORIZON_BARS, MAX_N_PATHS,
};
use crate::smarttrend::strategies::MarketRegime;
use crate::state::TitanState;

// ════════════════════════════════════════════════════════════════════
//  get_bandit_state
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct BanditArmRow {
    pub arm_idx: usize,
    pub multiplier: f64,
    pub successes: u32,
    pub failures: u32,
    pub success_rate: f64,
}

impl BanditArmRow {
    fn from_stats(idx: usize, stats: &ArmStats) -> Self {
        let multiplier = if idx < 4 {
            ARM_MULTIPLIERS[idx]
        } else {
            2.50
        };
        Self {
            arm_idx: idx,
            multiplier,
            successes: stats.successes,
            failures: stats.failures,
            success_rate: stats.success_rate(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BanditBucketRow {
    pub key: String,
    pub regime: MarketRegime,
    pub high_confidence: bool,
    pub total_trades: u32,
    pub activated: bool,
    pub arms: [BanditArmRow; NUM_ARMS],
}

#[derive(Debug, Clone, Serialize)]
pub struct BanditState {
    pub enabled: bool,
    pub activation_threshold: u32,
    pub total_observations: u64,
    pub buckets: Vec<BanditBucketRow>,
}

const ALL_REGIMES: [MarketRegime; 4] = [
    MarketRegime::Calm,
    MarketRegime::Trending,
    MarketRegime::Volatile,
    MarketRegime::Panic,
];

const HIGH_CONF_VARIANTS: [bool; 2] = [true, false];

/// Core impl factored out so unit tests can exercise the logic
/// without constructing a `tauri::State`.
fn collect_bandit_state(state: &TitanState) -> Result<BanditState, String> {
    let bandit = state.bandit.lock().map_err(|e| e.to_string())?;
    let activation_threshold = bandit.activation_threshold();
    let total_observations = bandit.total_observations();
    let enabled = bandit.enabled();

    let mut buckets: Vec<BanditBucketRow> = Vec::with_capacity(8);
    for &regime in &ALL_REGIMES {
        for &high in &HIGH_CONF_VARIANTS {
            let confidence_proxy = if high { 0.9 } else { 0.3 };
            let summary = bandit.bucket_summary(regime, confidence_proxy);
            let arms: [BanditArmRow; NUM_ARMS] = [
                BanditArmRow::from_stats(0, &summary.arms[0]),
                BanditArmRow::from_stats(1, &summary.arms[1]),
                BanditArmRow::from_stats(2, &summary.arms[2]),
                BanditArmRow::from_stats(3, &summary.arms[3]),
                BanditArmRow::from_stats(4, &summary.arms[4]),
            ];
            buckets.push(BanditBucketRow {
                key: summary.key,
                regime,
                high_confidence: high,
                total_trades: summary.total,
                activated: summary.total >= activation_threshold,
                arms,
            });
        }
    }
    drop(bandit);
    Ok(BanditState {
        enabled,
        activation_threshold,
        total_observations,
        buckets,
    })
}

#[tauri::command]
pub fn get_bandit_state(state: State<'_, Arc<TitanState>>) -> Result<BanditState, String> {
    collect_bandit_state(state.inner())
}

// ════════════════════════════════════════════════════════════════════
//  preview_mc_simulation
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct McPreviewResult {
    pub n_paths: usize,
    pub horizon_bars: usize,
    pub mean_return: f64,
    pub median_return: f64,
    pub std_return: f64,
    pub p_loss: f64,
    pub p_hit_target: f64,
    pub p_hit_stop: f64,
    pub sharpe: f64,
    pub elapsed_us: u64,
    pub decision: String,
    pub decision_reason: Option<String>,
    pub size_factor: f64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct McPreviewRequest {
    pub current_price: f64,
    pub atr: f64,
    pub target_price: f64,
    pub stop_price: f64,
    pub side: f64,
    /// Optional override for `n_paths` and `horizon_bars`. When
    /// `None`, defaults from `GbmConfig::default()`.
    pub n_paths: Option<usize>,
    pub horizon_bars: Option<usize>,
    pub seed: Option<u64>,
}

fn run_mc_preview(state: &TitanState, req: &McPreviewRequest) -> Result<McPreviewResult, String> {
    let (gate, n_paths, horizon, seed) = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        let ep = &cfg.evolution_protocol;
        let gate = McGate {
            min_expected_pct: ep.mc_min_expected_pct,
            max_p_loss: ep.mc_max_p_loss,
            shrink_p_hit_target: Some(ep.mc_shrink_p_hit_target),
            shrink_floor: ep.mc_shrink_floor,
        };
        let default_cfg = GbmConfig::default();
        let n_paths = req
            .n_paths
            .unwrap_or(default_cfg.n_paths)
            .min(MAX_N_PATHS);
        let horizon = req
            .horizon_bars
            .unwrap_or(default_cfg.horizon_bars)
            .min(MAX_HORIZON_BARS);
        let seed = req.seed.unwrap_or(default_cfg.seed);
        (gate, n_paths, horizon, seed)
    };
    let inputs = build_inputs_from_entry(
        req.current_price,
        req.atr,
        req.target_price,
        req.stop_price,
        req.side,
        crate::ml::monte_carlo::DEFAULT_ROUND_TRIP_BPS,
    );
    let res = simulate(
        &inputs,
        &GbmConfig {
            n_paths,
            horizon_bars: horizon,
            seed,
        },
    );
    let decision = gate.evaluate(&res);
    let (decision_kind, reason, factor) = match &decision {
        crate::ml::monte_carlo::McDecision::Accept => ("accept", None, 1.0),
        crate::ml::monte_carlo::McDecision::Shrink { factor, reason } => {
            ("shrink", Some(reason.clone()), *factor)
        }
        crate::ml::monte_carlo::McDecision::Veto { reason } => {
            ("veto", Some(reason.clone()), 0.0)
        }
    };
    Ok(McPreviewResult {
        n_paths: res.n_paths,
        horizon_bars: res.horizon_bars,
        mean_return: res.mean_return,
        median_return: res.median_return,
        std_return: res.std_return,
        p_loss: res.p_loss,
        p_hit_target: res.p_hit_target,
        p_hit_stop: res.p_hit_stop,
        sharpe: res.sharpe,
        elapsed_us: res.elapsed_us,
        decision: decision_kind.to_string(),
        decision_reason: reason,
        size_factor: factor,
    })
}

#[tauri::command]
pub fn preview_mc_simulation(
    state: State<'_, Arc<TitanState>>,
    req: McPreviewRequest,
) -> Result<McPreviewResult, String> {
    run_mc_preview(state.inner(), &req)
}

// ════════════════════════════════════════════════════════════════════
//  get_frontier_status
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct FrontierStatus {
    /// Bandit summary (mirrors `get_bandit_state` totals).
    pub bandit_enabled: bool,
    pub bandit_total_observations: u64,
    pub bandit_buckets_total: usize,
    pub bandit_buckets_activated: usize,
    pub bandit_activation_threshold: u32,
    /// Monte-Carlo gate config snapshot (read from runtime config).
    pub mc_enabled: bool,
    pub mc_min_expected_pct: f64,
    pub mc_max_p_loss: f64,
    pub mc_shrink_p_hit_target: f64,
    pub mc_shrink_floor: f64,
    /// Bootstrap progress — total ML samples staged in MemoryDb.
    /// `None` when D-PLUS isn't initialised (Evolution Protocol off).
    pub ml_samples_count: Option<u64>,
    /// Cold-start activation threshold for the predictors. Mirrors
    /// `crate::ml::COLD_START_MIN_SAMPLES` so the UI doesn't have to
    /// hard-code the same constant.
    pub omega_cold_start_threshold: usize,
    /// F3/F4/F5 — `off` | `shadow` | `live`
    #[serde(default)]
    pub frontier_mode: String,
    /// F4 — open shadow trades currently tracked.
    #[serde(default)]
    pub shadow_open: usize,
    /// F4 — resolved shadow trades retained.
    #[serde(default)]
    pub shadow_resolved: usize,
    /// F5 go-live gate snapshot from journal frontier closes.
    #[serde(default)]
    pub frontier_closes: u32,
    #[serde(default)]
    pub frontier_pf: f64,
    /// F5 — true when PF≥1.2 and closes≥30.
    #[serde(default)]
    pub frontier_gate_ok: bool,
}

fn collect_frontier_status(state: &TitanState) -> Result<FrontierStatus, String> {
    let cfg_snap = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.evolution_protocol.clone()
    };
    let (
        bandit_enabled,
        bandit_total,
        bandit_threshold,
        bandit_buckets_total,
        bandit_buckets_activated,
    ) = {
        let bandit = state.bandit.lock().map_err(|e| e.to_string())?;
        let total = bandit.total_observations();
        let threshold = bandit.activation_threshold();
        let mut buckets_seen = 0usize;
        let mut buckets_activated = 0usize;
        for &regime in &ALL_REGIMES {
            for &high in &HIGH_CONF_VARIANTS {
                let confidence_proxy = if high { 0.9 } else { 0.3 };
                let summary = bandit.bucket_summary(regime, confidence_proxy);
                if summary.total > 0 {
                    buckets_seen += 1;
                }
                if summary.total >= threshold {
                    buckets_activated += 1;
                }
            }
        }
        (
            bandit.enabled(),
            total,
            threshold,
            buckets_seen,
            buckets_activated,
        )
    };

    let ml_samples_count = state
        .dplus()
        .and_then(|d| d.memory_db.count_ml_samples().ok())
        .map(|n| n as u64);

    let (shadow_open, shadow_resolved) = state
        .dplus()
        .and_then(|dp| {
            let book = dp.shadow.lock().ok()?;
            Some((book.open_count(), book.resolved_count()))
        })
        .unwrap_or((0, 0));

    // F5 gate: profit factor + closes for strategy_id=frontier in journal.
    let (frontier_closes, frontier_pf) = {
        let journal = state.journal.lock().map_err(|e| e.to_string())?;
        let mut wins = 0.0;
        let mut losses = 0.0;
        let mut n = 0u32;
        for e in journal.entries.iter().rev() {
            if e.strategy_id.as_deref() != Some("frontier") {
                continue;
            }
            let Some(pnl) = e.pnl else { continue };
            n += 1;
            if pnl >= 0.0 {
                wins += pnl;
            } else {
                losses += pnl.abs();
            }
            if n >= 500 {
                break;
            }
        }
        let pf = if losses > 1e-9 {
            wins / losses
        } else if wins > 0.0 {
            99.0
        } else {
            0.0
        };
        (n, pf)
    };
    let frontier_gate_ok = frontier_closes >= 30 && frontier_pf >= 1.2;

    Ok(FrontierStatus {
        bandit_enabled,
        bandit_total_observations: bandit_total,
        bandit_buckets_total,
        bandit_buckets_activated,
        bandit_activation_threshold: bandit_threshold,
        mc_enabled: cfg_snap.monte_carlo_gate_enabled,
        mc_min_expected_pct: cfg_snap.mc_min_expected_pct,
        mc_max_p_loss: cfg_snap.mc_max_p_loss,
        mc_shrink_p_hit_target: cfg_snap.mc_shrink_p_hit_target,
        mc_shrink_floor: cfg_snap.mc_shrink_floor,
        ml_samples_count,
        omega_cold_start_threshold: crate::ml::COLD_START_MIN_SAMPLES,
        frontier_mode: cfg_snap.frontier_mode,
        shadow_open,
        shadow_resolved,
        frontier_closes,
        frontier_pf,
        frontier_gate_ok,
    })
}

#[tauri::command]
pub fn get_frontier_status(state: State<'_, Arc<TitanState>>) -> Result<FrontierStatus, String> {
    collect_frontier_status(state.inner())
}

// ════════════════════════════════════════════════════════════════════
//  toggle_bandit_enabled
// ════════════════════════════════════════════════════════════════════

fn set_bandit_enabled(state: &TitanState, enabled: bool) -> Result<bool, String> {
    let mut bandit = state.bandit.lock().map_err(|e| e.to_string())?;
    bandit.set_enabled(enabled);
    tracing::info!(
        component = "bandit",
        enabled,
        "Bandit toggle (runtime; not persisted to settings.json)"
    );
    Ok(enabled)
}

#[tauri::command]
pub fn toggle_bandit_enabled(
    state: State<'_, Arc<TitanState>>,
    enabled: bool,
) -> Result<bool, String> {
    set_bandit_enabled(state.inner(), enabled)
}

// ════════════════════════════════════════════════════════════════════
//  Constants exposed to the UI
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct FrontierConstants {
    pub bandit_arm_multipliers: [f64; NUM_ARMS],
    pub bandit_default_activation_threshold: u32,
    pub mc_default_n_paths: usize,
    pub mc_default_horizon_bars: usize,
    pub mc_max_n_paths: usize,
    pub mc_max_horizon_bars: usize,
}

#[tauri::command]
pub fn get_frontier_constants() -> Result<FrontierConstants, String> {
    let default_cfg = GbmConfig::default();
    let mut bandit_arm_multipliers = [0.0; NUM_ARMS];
    bandit_arm_multipliers[..4].copy_from_slice(&ARM_MULTIPLIERS);
    bandit_arm_multipliers[4] = 2.50;
    Ok(FrontierConstants {
        bandit_arm_multipliers,
        bandit_default_activation_threshold: COLD_START_TRADES_PER_BUCKET,
        mc_default_n_paths: default_cfg.n_paths,
        mc_default_horizon_bars: default_cfg.horizon_bars,
        mc_max_n_paths: MAX_N_PATHS,
        mc_max_horizon_bars: MAX_HORIZON_BARS,
    })
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;

    fn mk_state() -> TitanState {
        TitanState::new(RuntimeConfig::default())
    }

    #[test]
    fn get_frontier_constants_returns_sane_defaults() {
        let c = get_frontier_constants().expect("ok");
        let mut expected = [0.0; NUM_ARMS];
        expected[..4].copy_from_slice(&ARM_MULTIPLIERS);
        expected[4] = 2.50;
        assert_eq!(c.bandit_arm_multipliers, expected);
        assert!(c.mc_default_n_paths > 0 && c.mc_default_n_paths <= c.mc_max_n_paths);
        assert!(c.mc_default_horizon_bars > 0 && c.mc_default_horizon_bars <= c.mc_max_horizon_bars);
    }

    #[test]
    fn collect_bandit_state_returns_eight_buckets_on_cold_start() {
        let _ = std::fs::remove_file(crate::ml::bandit::default_state_path());
        let state = mk_state();
        let result = collect_bandit_state(&state).expect("ok");
        assert_eq!(result.buckets.len(), 8);
        for bucket in &result.buckets {
            assert!(!bucket.activated);
            assert_eq!(bucket.total_trades, 0);
            // Each bucket carries 5 arm rows in the right order.
            assert_eq!(bucket.arms[0].arm_idx, 0);
            assert_eq!(bucket.arms[1].arm_idx, 1);
            assert_eq!(bucket.arms[2].arm_idx, 2);
            assert_eq!(bucket.arms[3].arm_idx, 3);
            assert_eq!(bucket.arms[4].arm_idx, 4);
        }
    }

    #[test]
    fn collect_bandit_state_keys_unique() {
        let state = mk_state();
        let result = collect_bandit_state(&state).expect("ok");
        let mut seen = std::collections::HashSet::new();
        for bucket in &result.buckets {
            assert!(seen.insert(bucket.key.clone()), "duplicate key {}", bucket.key);
        }
    }

    #[test]
    fn run_mc_preview_returns_valid_decision() {
        let state = mk_state();
        let req = McPreviewRequest {
            current_price: 50.0,
            atr: 0.25,
            target_price: 51.0,
            stop_price: 49.5,
            side: 1.0,
            n_paths: Some(256),
            horizon_bars: Some(10),
            seed: Some(42),
        };
        let res = run_mc_preview(&state, &req).expect("ok");
        assert_eq!(res.n_paths, 256);
        assert_eq!(res.horizon_bars, 10);
        assert!((0.0..=1.0).contains(&res.p_loss));
        assert!(["accept", "shrink", "veto"].contains(&res.decision.as_str()));
        assert!((0.0..=1.0).contains(&res.size_factor));
    }

    #[test]
    fn run_mc_preview_caps_at_max_paths() {
        let state = mk_state();
        let req = McPreviewRequest {
            current_price: 50.0,
            atr: 0.25,
            target_price: 51.0,
            stop_price: 49.5,
            side: 1.0,
            n_paths: Some(MAX_N_PATHS + 1_000),
            horizon_bars: Some(5),
            seed: Some(42),
        };
        let res = run_mc_preview(&state, &req).expect("ok");
        assert_eq!(res.n_paths, MAX_N_PATHS);
    }

    #[test]
    fn run_mc_preview_default_seed_repeatable() {
        let state = mk_state();
        let req = McPreviewRequest {
            current_price: 50.0,
            atr: 0.25,
            target_price: 51.0,
            stop_price: 49.5,
            side: 1.0,
            n_paths: Some(128),
            horizon_bars: Some(5),
            seed: Some(7),
        };
        let r1 = run_mc_preview(&state, &req).expect("ok");
        let r2 = run_mc_preview(&state, &req).expect("ok");
        assert_eq!(r1.mean_return.to_bits(), r2.mean_return.to_bits());
        assert_eq!(r1.p_loss.to_bits(), r2.p_loss.to_bits());
    }

    #[test]
    fn collect_frontier_status_known_fields() {
        let state = mk_state();
        let status = collect_frontier_status(&state).expect("ok");
        assert!(status.bandit_activation_threshold > 0);
        assert_eq!(status.omega_cold_start_threshold, crate::ml::COLD_START_MIN_SAMPLES);
    }

    #[test]
    fn collect_frontier_status_zero_buckets_active_on_cold_start() {
        let _ = std::fs::remove_file(crate::ml::bandit::default_state_path());
        let state = mk_state();
        let status = collect_frontier_status(&state).expect("ok");
        assert_eq!(status.bandit_buckets_total, 0);
        assert_eq!(status.bandit_buckets_activated, 0);
    }

    #[test]
    fn set_bandit_enabled_round_trip() {
        let state = mk_state();
        let r1 = set_bandit_enabled(&state, false).expect("ok");
        assert!(!r1);
        {
            let b = state.bandit.lock().expect("lock");
            assert!(!b.enabled());
        }
        let r2 = set_bandit_enabled(&state, true).expect("ok");
        assert!(r2);
        {
            let b = state.bandit.lock().expect("lock");
            assert!(b.enabled());
        }
    }

    #[test]
    fn frontier_status_reflects_mc_config_defaults() {
        let state = mk_state();
        let status = collect_frontier_status(&state).expect("ok");
        // The default RuntimeConfig has mc gate enabled and the
        // documented thresholds.
        assert!(status.mc_enabled);
        assert!(status.mc_max_p_loss > 0.0 && status.mc_max_p_loss <= 1.0);
        assert!(
            status.mc_shrink_p_hit_target > 0.0 && status.mc_shrink_p_hit_target <= 1.0
        );
    }
}
