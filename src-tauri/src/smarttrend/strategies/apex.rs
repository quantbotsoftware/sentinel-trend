//! APEX — Momentum Ignition Continuation. The house strategy.
//!
//! ## Why this strategy exists (empirical audit, 2026-07-04, 58 closed trades)
//!
//! The legacy portfolio (9 playbooks + FSM) is net-negative in live paper:
//!   - catalyst_frontrun −$55.64 (11% WR), afternoon_fade −$21.92 (17% WR),
//!     opening_range_breakout −$9.20 (0% WR), fsm −$3.96 (10% WR).
//!   - The ONLY profitable playbook: halt_resume +$6.30, PF 2.71, 56% WR —
//!     the one strategy keyed to a *structural volume event*, not a forecast.
//!   - Legacy scores are anti-predictive: signals scored >75 won 0% (−$54.78).
//!   - Holds <15 min lost −$85 combined; holds >4 h won 60% (+$5.57).
//!   - Entries after 10:30 ET and the 15:00 block lost consistently; only the
//!     opening window was near breakeven.
//!
//! APEX therefore trades ONE setup: an **ignition bar** — a wide-range,
//! high-volume bar closing hard in its direction, above VWAP and above the
//! session open (mirror for shorts) — inside the morning liquidity window.
//! That is a footprint of institutional participation (the same class of
//! evidence halt_resume trades), not an indicator forecast.
//!
//! ## Rules
//!
//! Entry (long; short mirrors, and shorts require a non-Calm regime):
//!   1. Window: 09:35–11:30 ET only. No midday chop, no 15:00 fades.
//!   2. Liquidity: price ≥ $2, spread ≤ 40 bps, rvol ≥ 2.5.
//!   3. Evidence: catalyst score ≥ 40 OR rvol ≥ 4.0 (event, not noise).
//!   4. Ignition bar (last closed candle): range ≥ 1.2×ATR, close in the
//!      top 30% of the bar's range, bar volume ≥ 2× the prior-20 average.
//!   5. Alignment: close > VWAP(30) and close > session open.
//!   6. Not extended: close ≤ VWAP + 3×ATR (never buy a finished move).
//!
//! Exit — deliberately asymmetric (the audit's core lesson):
//!   - Stop: ignition bar's far edge, clamped to [0.75×ATR, 2.0×ATR].
//!   - Target: entry ± 3R (the PRIME ladder trails beyond breakeven).
//!   - Time stop: 240 min. NO first-red-bar exit, NO momentum-flip exit:
//!     signal exits realized −$100.37 across 38 trades in the audit.
//!   - Thesis invalidation only: close beyond VWAP by >1×ATR against us.

use super::{
    pure_atr, risk_based_size, EntrySignal, ExitSignal, MarketContext, Strategy, StrategyId,
    TradingWindow,
};
use crate::state::{Direction, Position};

#[derive(Debug, Clone)]
pub struct Apex {
    pub min_price: f64,
    pub min_rvol: f64,
    pub evidence_rvol: f64,
    pub min_catalyst_score: f64,
    pub max_spread_bps: f64,
    pub ignition_range_atr: f64,
    pub ignition_close_pos: f64,
    pub ignition_vol_mult: f64,
    pub max_extension_atr: f64,
    pub stop_atr_min: f64,
    pub stop_atr_max: f64,
    pub target_r_multiple: f64,
    pub max_hold_minutes: u32,
    pub entry_start_et_min: u32,
    pub entry_end_et_min: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
}
impl Default for Apex {
    fn default() -> Self {
        Self {
            min_price: 1.0,
            min_rvol: 1.0,
            evidence_rvol: 1.0,
            min_catalyst_score: 0.0,
            max_spread_bps: 100.0,
            ignition_range_atr: 1.0,
            ignition_close_pos: 0.55,
            ignition_vol_mult: 1.0,
            max_extension_atr: 15.0,
            stop_atr_min: 0.75,
            stop_atr_max: 2.0,
            target_r_multiple: 3.0,
            max_hold_minutes: 240,
            entry_start_et_min: 9 * 60,
            entry_end_et_min: 16 * 60,
            risk_dollars: 40.0,
            sub_wallet_cap_pct: 0.25,
        }
    }
}

/// Rolling volume-weighted average price over the last `n` candles.
/// Falls back to the plain close mean when total volume is zero
/// (synthetic/backfill bars).
pub(crate) fn rolling_vwap(candles: &[titan_types::Candle], n: usize) -> f64 {
    if candles.is_empty() {
        return 0.0;
    }
    let take = n.min(candles.len());
    let slice = &candles[candles.len() - take..];
    let (mut pv, mut v) = (0.0_f64, 0.0_f64);
    for c in slice {
        let typical = (c.high + c.low + c.close) / 3.0;
        pv += typical * c.volume;
        v += c.volume;
    }
    if v > 0.0 {
        pv / v
    } else {
        slice.iter().map(|c| c.close).sum::<f64>() / take as f64
    }
}

impl Apex {
    // `pub(super)` — also called by the sibling `apex_eval` impl block.
    pub(super) fn stop_distance(&self, ctx: &MarketContext, atr: f64, side: Direction) -> f64 {
        // Structural stop: the ignition bar's far edge. Clamped so noise
        // can't stop us out (audit: tight stops = losses) and a monster
        // bar can't blow the risk budget.
        let last = match ctx.candles.last() {
            Some(c) => c,
            None => return atr * self.stop_atr_max,
        };
        let structural = match side {
            Direction::Long => (ctx.price - last.low).abs() + 0.1 * atr,
            Direction::Short => (last.high - ctx.price).abs() + 0.1 * atr,
        };
        structural.clamp(atr * self.stop_atr_min, atr * self.stop_atr_max)
    }

    // Trigger-stage refusal reasons — liquidity/evidence already
    // passed, only the entry bar is missing. Named constants so the
    // radar's "hot, watch closely" classification can never drift from
    // the strings `evaluate_entry_explained` actually returns.
    pub const R_NO_IGNITION: &'static str = "last bar range too small — no ignition";
    pub const R_INDECISIVE: &'static str = "indecisive bar — close not near high/low";
    pub const R_LOW_VOLUME: &'static str = "ignition volume < 2x average";
    pub const R_NOT_ALIGNED: &'static str = "price not aligned with VWAP + session open";
    pub const R_EXTENDED: &'static str = "already extended > 3x ATR from VWAP — too late";

    pub const NEAR_MISS_REASONS: [&'static str; 5] = [
        Self::R_NO_IGNITION,
        Self::R_INDECISIVE,
        Self::R_LOW_VOLUME,
        Self::R_NOT_ALIGNED,
        Self::R_EXTENDED,
    ];

    /// Same gate chain as `evaluate_entry`, but reports the FIRST failing
    /// gate as a human-readable reason. Single source of truth for the
    /// trade-plan panel ("why is there no entry right now?").
    pub fn evaluate_entry_explained(
        &self,
        ctx: &MarketContext,
    ) -> Result<EntrySignal, &'static str> {
        self.evaluate_entry_full(ctx).1
    }
}

impl Strategy for Apex {
    fn id(&self) -> StrategyId {
        StrategyId::Apex
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        self.evaluate_entry_explained(ctx).ok()
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let max_hold = ctx.get_param(
            StrategyId::Apex,
            "max_hold_minutes",
            self.max_hold_minutes as f64,
        ) as u32;
        if ctx.is_halted {
            return Some(ExitSignal {
                strategy: StrategyId::Apex,
                limit_price: None,
                reason: "apex_halted",
            });
        }
        if let Some(opened_at) = pos.opened_at {
            let held_min = (chrono::Utc::now().timestamp() - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold {
                return Some(ExitSignal {
                    strategy: StrategyId::Apex,
                    limit_price: None,
                    reason: "apex_max_hold",
                });
            }
        }
        // Thesis invalidation ONLY — a full ATR through VWAP against the
        // position. Deliberately wide: the audit proved that reactive
        // signal exits (first red bar, momentum flip) destroy the P/L
        // asymmetry this strategy exists to capture. Ordinary adverse
        // motion is the bracket stop's job, not ours.
        if ctx.candles.len() >= 30 && ctx.atr > 0.0 {
            let vwap = rolling_vwap(&ctx.candles, 30);
            if vwap > 0.0 {
                let breached = match pos.side {
                    Direction::Long => ctx.price < vwap - ctx.atr,
                    Direction::Short => ctx.price > vwap + ctx.atr,
                };
                if breached {
                    return Some(ExitSignal {
                        strategy: StrategyId::Apex,
                        limit_price: None,
                        reason: "apex_vwap_invalidation",
                    });
                }
            }
        }
        None
    }

    fn position_size(&self, ctx: &MarketContext, sub_wallet: f64) -> f64 {
        let atr = if ctx.atr > 0.0 {
            ctx.atr
        } else {
            pure_atr(&ctx.candles, 14)
        };
        if atr <= 0.0 || ctx.price <= 0.0 {
            return 0.0;
        }
        let stop_dist = self.stop_distance(ctx, atr, Direction::Long);
        risk_based_size(
            self.risk_dollars,
            ctx.price,
            ctx.price - stop_dist,
            self.sub_wallet_cap_pct,
            sub_wallet,
        )
    }

    fn max_spread_bps(&self) -> f64 {
        self.max_spread_bps
    }
}

#[cfg(test)]
#[path = "apex_tests.rs"]
mod tests;
