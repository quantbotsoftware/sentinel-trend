//! PRIME F2b — Conviction-scaled capital allocation (pure).
//!
//! Replaces the fragmented per-strategy sub-wallet sizing with one
//! consolidated policy: risk budget scales with conviction, hard caps
//! on per-position and total exposure. The tournament sub-wallets stay
//! alive for *attribution* (leaderboard, bandit) but no longer starve
//! good signals of capital.
//!
//! Empirical driver (audit 2026-07-02): avg win $4.20 vs avg loss
//! $9.20 with flat sizing — the book paid full risk on low-quality
//! signals. Conviction scaling puts more on the fat part of the
//! distribution and less on the marginal entries.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CapitalPolicy {
    /// Risk per trade at conviction = threshold, as fraction of equity.
    pub base_risk_pct: f64,
    /// Hard cap on one position's notional, as fraction of equity.
    pub max_position_pct: f64,
    /// Hard cap on total open notional, as fraction of equity.
    pub max_total_exposure_pct: f64,
}

impl Default for CapitalPolicy {
    fn default() -> Self {
        Self {
            base_risk_pct: 0.01,
            max_position_pct: 0.25,
            max_total_exposure_pct: 0.90,
        }
    }
}

/// Maps conviction to a risk multiplier. Below `threshold` → 0 (no
/// trade); at threshold → 0.5×; linear up to 1.5× at conviction 1.0.
pub fn conviction_multiplier(conviction: f64, threshold: f64) -> f64 {
    if conviction < threshold {
        return 0.0;
    }
    let span = (1.0 - threshold).max(1e-9);
    let t = ((conviction - threshold) / span).clamp(0.0, 1.0);
    0.5 + t
}

/// P4 — correlation haircut band edges. Below `CORR_FREE` the new
/// position is treated as independent (no haircut); above
/// `CORR_FULL` the haircut bottoms out at `CORR_MIN_MULT`.
pub const CORR_FREE: f64 = 0.40;
pub const CORR_FULL: f64 = 0.85;
pub const CORR_MIN_MULT: f64 = 0.40;

/// P4 — portfolio-heat haircut. Maps the strongest |lead-lag
/// correlation| between the incoming symbol and any open position to
/// a size multiplier in `[CORR_MIN_MULT, 1.0]`.
///
/// Empirical driver (audit 2026-07-03): the top-of-loss book was
/// dominated by *clusters* — same-day entries in tickers moving on
/// the same catalyst (each individually within risk limits, jointly a
/// single concentrated bet). The lead-lag graph already measures that
/// co-movement; sizing now respects it. Pure function; NaN or
/// out-of-range inputs are treated as "no correlation evidence" (1.0
/// — sizing must never be zeroed by a data gap).
pub fn correlation_haircut(max_abs_corr_with_open: f64) -> f64 {
    if !max_abs_corr_with_open.is_finite() {
        return 1.0;
    }
    let c = max_abs_corr_with_open.abs();
    if c <= CORR_FREE {
        return 1.0;
    }
    if c >= CORR_FULL {
        return CORR_MIN_MULT;
    }
    let t = (c - CORR_FREE) / (CORR_FULL - CORR_FREE);
    1.0 - t * (1.0 - CORR_MIN_MULT)
}

/// Sized notional (USD) for one entry. Pure.
///
/// * `equity` — account equity in USD.
/// * `conviction` / `threshold` — fusion output + dispatch threshold.
/// * `price` — entry price; `stop_dist` — |entry − stop| in USD.
/// * `open_exposure` — current total open notional in USD.
///
/// Returns 0.0 when the trade should not be sized (below threshold,
/// degenerate stop, exhausted exposure budget).
pub fn position_dollars(
    policy: &CapitalPolicy,
    equity: f64,
    conviction: f64,
    threshold: f64,
    price: f64,
    stop_dist: f64,
    open_exposure: f64,
) -> f64 {
    if equity <= 0.0 || price <= 0.0 || !price.is_finite() {
        return 0.0;
    }
    let mult = conviction_multiplier(conviction, threshold);
    if mult <= 0.0 {
        return 0.0;
    }
    // Degenerate stop distance → fall back to 1% of price so a missing
    // stop cannot buy an unbounded position.
    let stop = if stop_dist.is_finite() && stop_dist > price * 1e-4 {
        stop_dist
    } else {
        price * 0.01
    };
    let risk_dollars = equity * policy.base_risk_pct * mult;
    let shares = risk_dollars / stop;
    let notional = shares * price;

    let position_cap = equity * policy.max_position_pct;
    let exposure_room = (equity * policy.max_total_exposure_pct - open_exposure).max(0.0);

    notional.min(position_cap).min(exposure_room)
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    const EQ: f64 = 10_000.0;
    const TH: f64 = 0.55;

    #[test]
    fn below_threshold_sizes_zero() {
        let p = CapitalPolicy::default();
        assert_eq!(position_dollars(&p, EQ, 0.40, TH, 10.0, 0.2, 0.0), 0.0);
    }

    #[test]
    fn multiplier_ramp() {
        assert_eq!(conviction_multiplier(0.54, TH), 0.0);
        assert!((conviction_multiplier(TH, TH) - 0.5).abs() < 1e-9);
        assert!((conviction_multiplier(1.0, TH) - 1.5).abs() < 1e-9);
        let mid = conviction_multiplier(0.775, TH);
        assert!(mid > 0.5 && mid < 1.5);
    }

    #[test]
    fn higher_conviction_sizes_bigger() {
        let p = CapitalPolicy::default();
        // Wide stop (2.0) keeps both notionals under the position cap
        // so the conviction ramp is what differentiates them.
        let low = position_dollars(&p, EQ, 0.60, TH, 10.0, 2.0, 0.0);
        let high = position_dollars(&p, EQ, 0.90, TH, 10.0, 2.0, 0.0);
        assert!(high > low && low > 0.0, "high {high} low {low}");
        assert!(high < EQ * p.max_position_pct);
    }

    #[test]
    fn position_cap_enforced() {
        let p = CapitalPolicy::default();
        // Tiny stop distance would imply a huge notional — cap kicks in.
        let sized = position_dollars(&p, EQ, 1.0, TH, 100.0, 0.01, 0.0);
        assert!(sized <= EQ * p.max_position_pct + 1e-9);
    }

    #[test]
    fn exposure_budget_enforced() {
        let p = CapitalPolicy::default();
        // 89% already deployed of a 90% budget → at most 1% left.
        let sized = position_dollars(&p, EQ, 1.0, TH, 100.0, 0.01, EQ * 0.89);
        assert!(sized <= EQ * 0.01 + 1e-9);
        // Budget fully consumed → zero.
        let none = position_dollars(&p, EQ, 1.0, TH, 100.0, 0.01, EQ * 0.95);
        assert_eq!(none, 0.0);
    }

    #[test]
    fn degenerate_stop_falls_back() {
        let p = CapitalPolicy::default();
        let sized = position_dollars(&p, EQ, 0.80, TH, 10.0, 0.0, 0.0);
        assert!(sized > 0.0);
        let nan_stop = position_dollars(&p, EQ, 0.80, TH, 10.0, f64::NAN, 0.0);
        assert!((sized - nan_stop).abs() < 1e-9);
    }

    #[test]
    fn correlation_haircut_bands() {
        // Independent / weakly correlated → full size.
        assert_eq!(correlation_haircut(0.0), 1.0);
        assert_eq!(correlation_haircut(0.40), 1.0);
        // Fully clustered → floor.
        assert_eq!(correlation_haircut(0.85), CORR_MIN_MULT);
        assert_eq!(correlation_haircut(1.0), CORR_MIN_MULT);
        // Monotone decreasing in between; sign-agnostic.
        let mid = correlation_haircut(0.60);
        assert!(mid < 1.0 && mid > CORR_MIN_MULT);
        assert!(correlation_haircut(0.70) < mid);
        assert_eq!(correlation_haircut(-0.60), mid);
        // Data gaps never zero the size.
        assert_eq!(correlation_haircut(f64::NAN), 1.0);
    }

    #[test]
    fn garbage_inputs_size_zero() {
        let p = CapitalPolicy::default();
        assert_eq!(position_dollars(&p, 0.0, 1.0, TH, 10.0, 0.2, 0.0), 0.0);
        assert_eq!(position_dollars(&p, EQ, 1.0, TH, 0.0, 0.2, 0.0), 0.0);
        assert_eq!(position_dollars(&p, EQ, 1.0, TH, f64::NAN, 0.2, 0.0), 0.0);
    }
}
