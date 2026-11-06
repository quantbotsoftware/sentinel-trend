//! FRONTIER (Sesiunea 4) — Monte Carlo P&L gate.
//!
//! Forward-simulates a Geometric Brownian Motion (GBM) on the
//! candidate entry to estimate per-trade expectancy *after costs*
//! and probability of hitting the planned target before the
//! planned stop. Used as a final risk gate in `bot/entry.rs` after
//! OMEGA, before `qty` is locked in.
//!
//! ## GBM model
//!
//! For each path `i`, simulate `T` bars:
//!
//! ```text
//! S_{i,t+1} = S_{i,t} * exp((μ - 0.5 σ²) Δt + σ √Δt · Z_{i,t})
//! ```
//!
//! with `Z ~ N(0, 1)`, `Δt = 1 bar`. Per-bar drift `μ` defaults to
//! 0 (martingale assumption). Per-bar volatility `σ` is derived from
//! the recent ATR / price ratio so the simulation is calibrated to
//! the symbol's actual realised volatility, not a generic prior.
//!
//! ## Determinism
//!
//! All randomness is sourced from [`SplitMix64`] with the seed
//! supplied in [`GbmConfig`]. Same seed + same inputs → byte-for-byte
//! identical [`McResult`]. This is critical for replay tests and for
//! reproducing flagged trades during post-mortem.
//!
//! ## Latency budget
//!
//! Default 1024 paths × 20 bars = ~20 000 floats per simulation.
//! On CPU this completes in ~200 µs (release build); on GPU via
//! candle the speedup is mostly noise at this scale, so we stay
//! CPU-only on the entry hot path for predictable latency. The
//! [`simulate_paths_dense`] helper is exposed for ad-hoc GPU runs
//! (research mode, not used by the bot).
//!
//! ## Gate semantics
//!
//! [`McGate::evaluate`] returns one of three [`McDecision`] values:
//!
//! - `Accept` — expected return after costs ≥ `min_expected_pct`
//!   and `P[loss] ≤ max_p_loss`.
//! - `Shrink { factor }` — borderline. The bot scales `size_usd` by
//!   `factor ∈ [0.25, 1.0]` instead of vetoing.
//! - `Veto { reason }` — hard refuse. Bot logs and emits
//!   `bot-blocked` with `reason`.
//!
//! Cold-start: when `current_price`, `target_price`, or `vol_per_bar`
//! is non-finite or non-positive, the gate returns `Accept` to avoid
//! breaking entries on edge data. The intent is "fail-open for the
//! check itself, fail-closed for the *outcome*" — the gate cannot
//! create new entries, only filter ones already approved.

use crate::smarttrend::genetic_optimizer::SplitMix64;

/// Hard caps used by the Tauri command boundary so a misconfigured
/// frontend can't ask the simulator to allocate gigabytes.
pub const MAX_N_PATHS: usize = 65_536;
pub const MAX_HORIZON_BARS: usize = 1_024;
/// Default round-trip cost in bps. Alpaca commission-free, so this
/// is purely slippage allowance — 5 bps ≈ a quarter cent on a $50
/// stock, which matches what we see on filled orders historically.
pub const DEFAULT_ROUND_TRIP_BPS: f64 = 5.0;

/// Hard upper bound on simulation latency for the entry hot path.
/// Enforced by a regression test.
#[allow(dead_code)] // referenced by `#[cfg(test)]` regression test only
pub const SIMULATION_LATENCY_BUDGET_MS: u64 = 5;

/// Inputs for a single Monte Carlo run. All prices in dollars; all
/// drifts and vols are *per-bar* (not annualised — the bar cadence
/// is whatever the entry is using, typically 5 min).
#[derive(Debug, Clone)]
pub struct GbmInputs {
    /// Current best estimate of the price the order will fill at.
    pub current_price: f64,
    /// Per-bar log-return drift. Default `0.0` (martingale).
    pub drift_per_bar: f64,
    /// Per-bar log-return std-dev. Must be `> 0` and finite, else
    /// the gate fail-opens (see module docs).
    pub vol_per_bar: f64,
    /// Take-profit price in absolute dollars (long: `> current`,
    /// short: `< current`).
    pub target_price: f64,
    /// Stop-loss price in absolute dollars (long: `< current`,
    /// short: `> current`).
    pub stop_price: f64,
    /// Round-trip cost in basis points (entry slip + exit slip +
    /// commission). Subtracted from final return before computing
    /// `mean_return` and `p_loss`.
    pub round_trip_bps: f64,
    /// `+1.0` for long, `-1.0` for short. Used to flip the
    /// hit-target / hit-stop comparisons consistently.
    pub side: f64,
}

/// Tunable simulation knobs. Defaults match the v2.3.9 plan.
#[derive(Debug, Clone)]
pub struct GbmConfig {
    pub n_paths: usize,
    pub horizon_bars: usize,
    pub seed: u64,
}

impl Default for GbmConfig {
    fn default() -> Self {
        Self {
            n_paths: 1024,
            horizon_bars: 20,
            seed: 0xA1B2C3D4_E5F60718,
        }
    }
}

/// Outcome of a single Monte Carlo simulation. All probabilities
/// are in `[0, 1]`; all returns are unitless fractions
/// (`+0.01` = 1 % up). `mean_return` is *after* `round_trip_bps`.
#[derive(Debug, Clone, PartialEq)]
pub struct McResult {
    pub n_paths: usize,
    pub horizon_bars: usize,
    /// `E[r_T] - cost`, where `r_T` = log-return at horizon
    /// terminal converted to simple return.
    pub mean_return: f64,
    /// Median of the per-path simple returns (after costs). Useful
    /// when the distribution is fat-tailed and `mean` is misleading.
    pub median_return: f64,
    /// Standard deviation of the per-path simple returns.
    pub std_return: f64,
    /// `P[final_simple_return < 0]` after costs.
    pub p_loss: f64,
    /// `P[max_t price_t crosses target before min_t price_t crosses
    /// stop]`. Path-dependent — tracks per-step intra-path moves.
    pub p_hit_target: f64,
    /// `P[min_t price_t crosses stop before max_t price_t crosses
    /// target]`. Mirror of `p_hit_target`.
    pub p_hit_stop: f64,
    /// Sharpe-like score: `mean_return / std_return`. Capped at
    /// `±10` to avoid divide-by-zero noise leaking into the gate.
    pub sharpe: f64,
    /// Wall-clock duration of the simulation in microseconds.
    pub elapsed_us: u64,
}

/// Outcome of [`McGate::evaluate`].
#[derive(Debug, Clone, PartialEq)]
pub enum McDecision {
    /// Simulation says the trade clears the gate. Continue.
    Accept,
    /// Borderline. Multiply intended size by `factor` and continue.
    Shrink { factor: f64, reason: String },
    /// Hard refuse. Bot emits `bot-blocked` with `reason`.
    Veto { reason: String },
}

#[allow(dead_code)] // accessor sugar for UI / future telemetry consumers
impl McDecision {
    pub fn is_veto(&self) -> bool {
        matches!(self, Self::Veto { .. })
    }
    pub fn size_factor(&self) -> f64 {
        match self {
            Self::Accept => 1.0,
            Self::Shrink { factor, .. } => *factor,
            Self::Veto { .. } => 0.0,
        }
    }
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Accept => None,
            Self::Shrink { reason, .. } | Self::Veto { reason, .. } => Some(reason.as_str()),
        }
    }
}

/// Risk-gate thresholds. Loaded from config at boot.
#[derive(Debug, Clone)]
pub struct McGate {
    /// Veto if `mean_return` (after costs) < this. Default `0.0`
    /// — only veto trades with negative expected value.
    pub min_expected_pct: f64,
    /// Veto if `p_loss` exceeds this. Default `0.65` — refuse only
    /// trades where 2/3 of simulated paths end down.
    pub max_p_loss: f64,
    /// Below this `p_hit_target` value, shrink size instead of
    /// outright vetoing. `None` disables the shrink path.
    pub shrink_p_hit_target: Option<f64>,
    /// Floor for the shrink factor (clamped to `[shrink_floor, 1.0]`).
    pub shrink_floor: f64,
}

impl Default for McGate {
    fn default() -> Self {
        Self {
            min_expected_pct: 0.0,
            max_p_loss: 0.65,
            shrink_p_hit_target: Some(0.40),
            shrink_floor: 0.50,
        }
    }
}

impl McGate {
    /// Apply the gate to a simulation outcome.
    pub fn evaluate(&self, r: &McResult) -> McDecision {
        if r.mean_return < self.min_expected_pct {
            return McDecision::Veto {
                reason: format!(
                    "MC: E[r]={:.4} < min_expected={:.4}",
                    r.mean_return, self.min_expected_pct
                ),
            };
        }
        if r.p_loss > self.max_p_loss {
            return McDecision::Veto {
                reason: format!(
                    "MC: P[loss]={:.3} > max={:.3}",
                    r.p_loss, self.max_p_loss
                ),
            };
        }
        if let Some(thr) = self.shrink_p_hit_target {
            if r.p_hit_target < thr {
                let factor = (r.p_hit_target / thr).clamp(self.shrink_floor, 1.0);
                return McDecision::Shrink {
                    factor,
                    reason: format!(
                        "MC: P[target]={:.3} < {:.3} → shrink ×{:.2}",
                        r.p_hit_target, thr, factor
                    ),
                };
            }
        }
        McDecision::Accept
    }
}

// ─── Core simulation ────────────────────────────────────────────────

/// Sample two independent `N(0, 1)` draws via the Box–Muller
/// transform on top of a [`SplitMix64`]. Returns `(z1, z2)`.
fn box_muller_pair(rng: &mut SplitMix64) -> (f64, f64) {
    // Avoid `ln(0)` — clamp `u1` away from zero. The probability of
    // observing a zero from `next_f64()` (which uses 53 mantissa
    // bits) is below 2⁻⁵³ but we belt-and-braces it anyway.
    let u1 = rng.next_f64().max(f64::MIN_POSITIVE);
    let u2 = rng.next_f64();
    let r = (-2.0 * u1.ln()).sqrt();
    let theta = 2.0 * std::f64::consts::PI * u2;
    (r * theta.cos(), r * theta.sin())
}

/// Generates `n` independent `N(0, 1)` samples deterministically.
pub fn rand_normals(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let (a, b) = box_muller_pair(&mut rng);
        out.push(a);
        if out.len() < n {
            out.push(b);
        }
    }
    out
}

/// Returns `true` if the long target was hit before the long stop
/// along this single price path. Caller flips comparisons for short.
fn long_target_hit_first(path: &[f64], target: f64, stop: f64) -> bool {
    for &p in path {
        if p >= target {
            return true;
        }
        if p <= stop {
            return false;
        }
    }
    false
}

/// Same as [`long_target_hit_first`] but for short trades.
fn short_target_hit_first(path: &[f64], target: f64, stop: f64) -> bool {
    for &p in path {
        if p <= target {
            return true;
        }
        if p >= stop {
            return false;
        }
    }
    false
}

/// Returns whether the stop was hit *anywhere* along the path
/// (independent of target order). Used to compute `p_hit_stop`.
fn stop_hit(path: &[f64], stop: f64, side: f64) -> bool {
    if side > 0.0 {
        path.iter().any(|&p| p <= stop)
    } else {
        path.iter().any(|&p| p >= stop)
    }
}

/// Run the full simulation and return aggregate statistics.
///
/// Implementation note: we generate the random matrix once upfront
/// (for cache locality and determinism), then walk each path
/// step-by-step on the stack. `n_paths × horizon_bars × 8 bytes`
/// of heap is the only allocation. For the default 1024 × 20 that's
/// 160 KB, comfortably below L2 on every supported platform.
pub fn simulate(inputs: &GbmInputs, cfg: &GbmConfig) -> McResult {
    let t0 = std::time::Instant::now();
    let n_paths = inputs_n_paths(cfg);
    let horizon = inputs_horizon(cfg);

    // Edge cases: degenerate inputs short-circuit to a "no-info"
    // result that the gate will accept (fail-open semantics — see
    // module docs).
    if n_paths == 0
        || horizon == 0
        || !inputs.current_price.is_finite()
        || inputs.current_price <= 0.0
        || !inputs.vol_per_bar.is_finite()
        || inputs.vol_per_bar <= 0.0
    {
        return McResult {
            n_paths,
            horizon_bars: horizon,
            mean_return: 0.0,
            median_return: 0.0,
            std_return: 0.0,
            p_loss: 0.5,
            p_hit_target: 0.0,
            p_hit_stop: 0.0,
            sharpe: 0.0,
            elapsed_us: t0.elapsed().as_micros() as u64,
        };
    }

    let total = n_paths.saturating_mul(horizon);
    let z = rand_normals(total, cfg.seed);

    let s0 = inputs.current_price;
    let mu = inputs.drift_per_bar;
    let sigma = inputs.vol_per_bar;
    let dt = 1.0_f64; // each step is one bar; the calibration is per-bar
    let drift_term = (mu - 0.5 * sigma * sigma) * dt;
    let vol_step = sigma * dt.sqrt();

    let cost = inputs.round_trip_bps / 10_000.0;
    let mut returns: Vec<f64> = Vec::with_capacity(n_paths);
    let mut path_buf: Vec<f64> = Vec::with_capacity(horizon);
    let mut hit_target_count = 0usize;
    let mut hit_stop_count = 0usize;

    for p in 0..n_paths {
        path_buf.clear();
        let mut s = s0;
        let path_offset = p * horizon;
        for t in 0..horizon {
            let zt = z[path_offset + t];
            s *= (drift_term + vol_step * zt).exp();
            path_buf.push(s);
        }
        // Final simple return (after costs).
        let final_simple = path_buf
            .last()
            .copied()
            .map(|p| (p - s0) / s0)
            .unwrap_or(0.0)
            * inputs.side
            - cost;
        returns.push(final_simple);

        // Target-vs-stop ordering. We track BOTH whether target was
        // hit before stop (p_hit_target) and whether stop was hit at
        // all (p_hit_stop). The two are not exclusive: a path can
        // touch the stop after first hitting the target.
        if inputs.side > 0.0 {
            if long_target_hit_first(&path_buf, inputs.target_price, inputs.stop_price) {
                hit_target_count += 1;
            }
        } else if short_target_hit_first(&path_buf, inputs.target_price, inputs.stop_price) {
            hit_target_count += 1;
        }
        if stop_hit(&path_buf, inputs.stop_price, inputs.side) {
            hit_stop_count += 1;
        }
    }

    let n = returns.len() as f64;
    let mean_return = if n > 0.0 { returns.iter().sum::<f64>() / n } else { 0.0 };
    let var = if n > 1.0 {
        returns.iter().map(|x| (x - mean_return).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    let std_return = var.sqrt();
    let p_loss = if n > 0.0 {
        returns.iter().filter(|r| **r < 0.0).count() as f64 / n
    } else {
        0.5
    };
    let p_hit_target = hit_target_count as f64 / n.max(1.0);
    let p_hit_stop = hit_stop_count as f64 / n.max(1.0);

    let mut sorted = returns.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_return = if sorted.is_empty() {
        0.0
    } else {
        sorted[sorted.len() / 2]
    };

    let sharpe = if std_return > 1e-12 {
        (mean_return / std_return).clamp(-10.0, 10.0)
    } else {
        0.0
    };

    McResult {
        n_paths,
        horizon_bars: horizon,
        mean_return,
        median_return,
        std_return,
        p_loss,
        p_hit_target,
        p_hit_stop,
        sharpe,
        elapsed_us: t0.elapsed().as_micros() as u64,
    }
}

#[inline]
fn inputs_n_paths(cfg: &GbmConfig) -> usize {
    cfg.n_paths.min(MAX_N_PATHS)
}

#[inline]
fn inputs_horizon(cfg: &GbmConfig) -> usize {
    cfg.horizon_bars.min(MAX_HORIZON_BARS)
}

/// Convenience — derive a sensible `GbmInputs` from the live entry
/// path inputs (price, ATR, target/stop, side). ATR is converted to
/// per-bar log-return std-dev via the standard `atr / price`
/// approximation.
pub fn build_inputs_from_entry(
    current_price: f64,
    atr_per_bar: f64,
    target_price: f64,
    stop_price: f64,
    side: f64,
    round_trip_bps: f64,
) -> GbmInputs {
    let vol_per_bar = if current_price > 0.0 {
        (atr_per_bar / current_price).max(1e-6)
    } else {
        1e-3
    };
    GbmInputs {
        current_price,
        drift_per_bar: 0.0,
        vol_per_bar,
        target_price,
        stop_price,
        round_trip_bps,
        side,
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn long_inputs(price: f64, vol: f64) -> GbmInputs {
        GbmInputs {
            current_price: price,
            drift_per_bar: 0.0,
            vol_per_bar: vol,
            target_price: price * 1.02,
            stop_price: price * 0.99,
            round_trip_bps: 5.0,
            side: 1.0,
        }
    }

    #[test]
    fn box_muller_produces_finite_pairs() {
        let mut rng = SplitMix64::new(123);
        for _ in 0..1_000 {
            let (a, b) = box_muller_pair(&mut rng);
            assert!(a.is_finite() && b.is_finite());
        }
    }

    #[test]
    fn rand_normals_deterministic_with_same_seed() {
        let a = rand_normals(256, 42);
        let b = rand_normals(256, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn rand_normals_differ_across_seeds() {
        let a = rand_normals(256, 42);
        let b = rand_normals(256, 43);
        assert_ne!(a, b);
    }

    #[test]
    fn rand_normals_zero_mean_unit_variance_approx() {
        let n = 10_000;
        let xs = rand_normals(n, 7);
        let mean = xs.iter().sum::<f64>() / n as f64;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
        // Central limit: |mean| should be ~1/sqrt(n) ≈ 0.01.
        assert!(mean.abs() < 0.05, "mean = {mean}");
        assert!((var - 1.0).abs() < 0.05, "var = {var}");
    }

    #[test]
    fn simulate_deterministic_with_same_seed() {
        let cfg = GbmConfig::default();
        let inp = long_inputs(50.0, 0.005);
        let r1 = simulate(&inp, &cfg);
        let r2 = simulate(&inp, &cfg);
        // Note: elapsed_us will differ; compare every other field.
        assert_eq!(r1.n_paths, r2.n_paths);
        assert_eq!(r1.horizon_bars, r2.horizon_bars);
        assert_eq!(r1.mean_return.to_bits(), r2.mean_return.to_bits());
        assert_eq!(r1.p_loss.to_bits(), r2.p_loss.to_bits());
        assert_eq!(r1.p_hit_target.to_bits(), r2.p_hit_target.to_bits());
        assert_eq!(r1.p_hit_stop.to_bits(), r2.p_hit_stop.to_bits());
    }

    #[test]
    fn zero_drift_zero_vol_returns_zero_minus_cost() {
        let inp = GbmInputs {
            current_price: 100.0,
            drift_per_bar: 0.0,
            vol_per_bar: 1e-9,
            target_price: 102.0,
            stop_price: 99.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig {
            n_paths: 256,
            horizon_bars: 5,
            seed: 1,
        };
        let r = simulate(&inp, &cfg);
        // With near-zero vol, every path ≈ current price ⇒ return ≈ -cost.
        let expected_cost = 5.0 / 10_000.0;
        assert!(
            (r.mean_return + expected_cost).abs() < 1e-3,
            "mean_return = {} (expected ≈ -{})",
            r.mean_return,
            expected_cost
        );
    }

    #[test]
    fn positive_drift_yields_positive_mean_return() {
        let inp = GbmInputs {
            current_price: 100.0,
            drift_per_bar: 0.005, // strong positive drift, long side
            vol_per_bar: 0.005,
            target_price: 110.0,
            stop_price: 90.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig {
            n_paths: 1024,
            horizon_bars: 20,
            seed: 11,
        };
        let r = simulate(&inp, &cfg);
        assert!(r.mean_return > 0.0, "expected positive E[r], got {}", r.mean_return);
    }

    #[test]
    fn negative_drift_yields_negative_mean_return() {
        let inp = GbmInputs {
            current_price: 100.0,
            drift_per_bar: -0.005,
            vol_per_bar: 0.005,
            target_price: 110.0,
            stop_price: 90.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig {
            n_paths: 1024,
            horizon_bars: 20,
            seed: 11,
        };
        let r = simulate(&inp, &cfg);
        assert!(r.mean_return < 0.0, "expected negative E[r], got {}", r.mean_return);
    }

    #[test]
    fn p_loss_in_unit_interval() {
        let cfg = GbmConfig::default();
        let inp = long_inputs(50.0, 0.01);
        let r = simulate(&inp, &cfg);
        assert!((0.0..=1.0).contains(&r.p_loss));
        assert!((0.0..=1.0).contains(&r.p_hit_target));
        assert!((0.0..=1.0).contains(&r.p_hit_stop));
    }

    #[test]
    fn long_far_target_low_hit_probability() {
        // Target 10 % up with tiny vol → should rarely hit.
        let inp = GbmInputs {
            current_price: 100.0,
            drift_per_bar: 0.0,
            vol_per_bar: 0.001,
            target_price: 110.0,
            stop_price: 99.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig {
            n_paths: 1024,
            horizon_bars: 20,
            seed: 7,
        };
        let r = simulate(&inp, &cfg);
        assert!(r.p_hit_target < 0.05, "P[target] = {} too high", r.p_hit_target);
    }

    #[test]
    fn long_close_target_high_hit_probability() {
        // Target 0.5 % up with 1 % per-bar vol → should hit often.
        let inp = GbmInputs {
            current_price: 100.0,
            drift_per_bar: 0.0,
            vol_per_bar: 0.01,
            target_price: 100.5,
            stop_price: 95.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig {
            n_paths: 1024,
            horizon_bars: 20,
            seed: 7,
        };
        let r = simulate(&inp, &cfg);
        assert!(r.p_hit_target > 0.5, "P[target] = {} too low", r.p_hit_target);
    }

    #[test]
    fn short_side_flips_hit_target_logic() {
        // For short, target is BELOW current; stop is ABOVE.
        let inp = GbmInputs {
            current_price: 100.0,
            drift_per_bar: 0.0,
            vol_per_bar: 0.01,
            target_price: 99.5,
            stop_price: 105.0,
            round_trip_bps: 5.0,
            side: -1.0,
        };
        let cfg = GbmConfig {
            n_paths: 1024,
            horizon_bars: 20,
            seed: 7,
        };
        let r = simulate(&inp, &cfg);
        assert!(r.p_hit_target > 0.5, "short P[target] = {} too low", r.p_hit_target);
    }

    #[test]
    fn nonpositive_price_returns_no_info_result() {
        let inp = GbmInputs {
            current_price: 0.0,
            drift_per_bar: 0.0,
            vol_per_bar: 0.01,
            target_price: 1.0,
            stop_price: -1.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig::default();
        let r = simulate(&inp, &cfg);
        assert_eq!(r.mean_return, 0.0);
        assert_eq!(r.p_loss, 0.5);
    }

    #[test]
    fn nonpositive_vol_returns_no_info_result() {
        let inp = GbmInputs {
            current_price: 100.0,
            drift_per_bar: 0.0,
            vol_per_bar: 0.0,
            target_price: 102.0,
            stop_price: 99.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig::default();
        let r = simulate(&inp, &cfg);
        assert_eq!(r.mean_return, 0.0);
        assert_eq!(r.p_loss, 0.5);
    }

    #[test]
    fn nan_price_returns_no_info_result() {
        let inp = GbmInputs {
            current_price: f64::NAN,
            drift_per_bar: 0.0,
            vol_per_bar: 0.01,
            target_price: 102.0,
            stop_price: 99.0,
            round_trip_bps: 5.0,
            side: 1.0,
        };
        let cfg = GbmConfig::default();
        let r = simulate(&inp, &cfg);
        assert_eq!(r.mean_return, 0.0);
    }

    #[test]
    fn n_paths_capped_at_max() {
        let cfg = GbmConfig {
            n_paths: MAX_N_PATHS + 1_000,
            horizon_bars: 5,
            seed: 1,
        };
        let inp = long_inputs(100.0, 0.005);
        let r = simulate(&inp, &cfg);
        assert_eq!(r.n_paths, MAX_N_PATHS);
    }

    #[test]
    fn horizon_capped_at_max() {
        let cfg = GbmConfig {
            n_paths: 16,
            horizon_bars: MAX_HORIZON_BARS + 100,
            seed: 1,
        };
        let inp = long_inputs(100.0, 0.005);
        let r = simulate(&inp, &cfg);
        assert_eq!(r.horizon_bars, MAX_HORIZON_BARS);
    }

    #[test]
    fn build_inputs_from_entry_uses_atr_over_price() {
        let inp = build_inputs_from_entry(50.0, 0.50, 51.0, 49.5, 1.0, 5.0);
        assert!((inp.vol_per_bar - 0.01).abs() < 1e-12);
        assert_eq!(inp.current_price, 50.0);
        assert_eq!(inp.target_price, 51.0);
        assert_eq!(inp.side, 1.0);
        assert_eq!(inp.round_trip_bps, 5.0);
    }

    #[test]
    fn build_inputs_from_entry_clamps_zero_atr() {
        let inp = build_inputs_from_entry(50.0, 0.0, 51.0, 49.5, 1.0, 5.0);
        assert!(inp.vol_per_bar >= 1e-6);
    }

    #[test]
    fn build_inputs_from_entry_handles_zero_price() {
        let inp = build_inputs_from_entry(0.0, 0.5, 1.0, -1.0, 1.0, 5.0);
        // Falls back to a non-zero default vol so the gate fail-opens.
        assert!(inp.vol_per_bar > 0.0);
    }

    // ─── Gate behaviour ──────────────────────────────────────────────

    #[test]
    fn gate_default_accepts_positive_expectancy() {
        let gate = McGate::default();
        let res = McResult {
            n_paths: 1024,
            horizon_bars: 20,
            mean_return: 0.005,
            median_return: 0.003,
            std_return: 0.01,
            p_loss: 0.45,
            p_hit_target: 0.50,
            p_hit_stop: 0.40,
            sharpe: 0.5,
            elapsed_us: 200,
        };
        assert_eq!(gate.evaluate(&res), McDecision::Accept);
    }

    #[test]
    fn gate_vetoes_negative_expectancy() {
        let gate = McGate {
            min_expected_pct: 0.0,
            ..Default::default()
        };
        let res = McResult {
            n_paths: 1024,
            horizon_bars: 20,
            mean_return: -0.001,
            median_return: -0.002,
            std_return: 0.01,
            p_loss: 0.55,
            p_hit_target: 0.50,
            p_hit_stop: 0.40,
            sharpe: -0.1,
            elapsed_us: 200,
        };
        let dec = gate.evaluate(&res);
        assert!(dec.is_veto(), "expected Veto, got {:?}", dec);
        assert!(dec.reason().unwrap().contains("E[r]"));
    }

    #[test]
    fn gate_vetoes_high_p_loss() {
        let gate = McGate {
            min_expected_pct: -1.0, // disable expectancy gate
            max_p_loss: 0.50,
            shrink_p_hit_target: None,
            shrink_floor: 0.5,
        };
        let res = McResult {
            n_paths: 1024,
            horizon_bars: 20,
            mean_return: 0.001,
            median_return: 0.0,
            std_return: 0.01,
            p_loss: 0.80,
            p_hit_target: 0.50,
            p_hit_stop: 0.60,
            sharpe: 0.1,
            elapsed_us: 200,
        };
        let dec = gate.evaluate(&res);
        assert!(dec.is_veto());
        assert!(dec.reason().unwrap().contains("P[loss]"));
    }

    #[test]
    fn gate_shrinks_when_p_hit_target_low() {
        let gate = McGate {
            min_expected_pct: -1.0,
            max_p_loss: 1.0,
            shrink_p_hit_target: Some(0.50),
            shrink_floor: 0.50,
        };
        let res = McResult {
            n_paths: 1024,
            horizon_bars: 20,
            mean_return: 0.001,
            median_return: 0.0,
            std_return: 0.01,
            p_loss: 0.45,
            p_hit_target: 0.30,
            p_hit_stop: 0.40,
            sharpe: 0.1,
            elapsed_us: 200,
        };
        let dec = gate.evaluate(&res);
        match dec {
            McDecision::Shrink { factor, reason } => {
                // factor = (0.30 / 0.50).clamp(0.5, 1.0) = 0.6
                assert!(
                    (factor - 0.6).abs() < 1e-6 || (factor - 0.5).abs() < 1e-6,
                    "factor = {factor}"
                );
                assert!(reason.contains("P[target]"));
            }
            other => panic!("expected Shrink, got {other:?}"),
        }
    }

    #[test]
    fn gate_shrink_factor_floor_respected() {
        let gate = McGate {
            min_expected_pct: -1.0,
            max_p_loss: 1.0,
            shrink_p_hit_target: Some(0.50),
            shrink_floor: 0.75,
        };
        let res = McResult {
            n_paths: 1024,
            horizon_bars: 20,
            mean_return: 0.001,
            median_return: 0.0,
            std_return: 0.01,
            p_loss: 0.45,
            p_hit_target: 0.05,
            p_hit_stop: 0.40,
            sharpe: 0.1,
            elapsed_us: 200,
        };
        match gate.evaluate(&res) {
            McDecision::Shrink { factor, .. } => assert!((factor - 0.75).abs() < 1e-6),
            other => panic!("expected Shrink, got {other:?}"),
        }
    }

    #[test]
    fn decision_size_factor_matches_kind() {
        assert_eq!(McDecision::Accept.size_factor(), 1.0);
        assert_eq!(
            McDecision::Veto {
                reason: "x".into()
            }
            .size_factor(),
            0.0
        );
        assert_eq!(
            McDecision::Shrink {
                factor: 0.7,
                reason: "x".into()
            }
            .size_factor(),
            0.7
        );
    }

    /// Regression guard: a single simulation at default params must
    /// stay well under the documented hot-path budget.
    #[test]
    fn simulate_latency_under_budget() {
        if cfg!(debug_assertions) {
            // Skip latency check in debug mode as unoptimized code runs much slower.
            return;
        }
        let inp = long_inputs(50.0, 0.005);
        let cfg = GbmConfig::default();
        // Warm-up.
        for _ in 0..3 {
            let _ = simulate(&inp, &cfg);
        }
        let mut samples_us: Vec<u128> = Vec::with_capacity(20);
        for _ in 0..20 {
            let r = simulate(&inp, &cfg);
            samples_us.push(r.elapsed_us as u128);
        }
        samples_us.sort_unstable();
        let median = samples_us[samples_us.len() / 2];
        let budget = (SIMULATION_LATENCY_BUDGET_MS as u128) * 1_000;
        assert!(
            median <= budget,
            "MC simulate too slow: median {median}us > budget {budget}us"
        );
    }

    #[test]
    fn long_target_hit_first_basic() {
        let path = vec![100.0, 101.0, 102.0, 99.5];
        assert!(long_target_hit_first(&path, 102.0, 99.0));
    }

    #[test]
    fn long_target_hit_first_stop_first_returns_false() {
        let path = vec![100.0, 99.0, 102.5];
        assert!(!long_target_hit_first(&path, 102.0, 99.0));
    }

    #[test]
    fn short_target_hit_first_basic() {
        let path = vec![100.0, 99.0, 98.0, 105.0];
        assert!(short_target_hit_first(&path, 98.0, 102.0));
    }

    #[test]
    fn stop_hit_long_detects_below() {
        let path = vec![100.0, 101.0, 99.0];
        assert!(stop_hit(&path, 99.5, 1.0));
    }

    #[test]
    fn stop_hit_short_detects_above() {
        let path = vec![100.0, 99.0, 105.0];
        assert!(stop_hit(&path, 104.0, -1.0));
    }
}
