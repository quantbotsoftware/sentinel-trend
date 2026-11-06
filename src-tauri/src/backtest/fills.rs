//! Realistic fill model for the backtest simulator.
//!
//! Three concerns:
//!
//! 1. **Slippage + commission** — apply at the entry/exit price level
//!    (`apply_slippage`, `apply_commission`).
//! 2. **Intrabar SL/TP resolution** — given the next bar's OHLC, did
//!    the position's stop-loss or take-profit get hit, and at what
//!    fill price? The model is intentionally **pessimistic** for the
//!    common ambiguity case (both SL and TP are in-range): assume SL
//!    fires first. This biases reported P/L *down*, which is the
//!    right direction for any forecast we feed back to the operator.
//! 3. **Limit-cross check** — a limit entry only fills if the bar's
//!    [low, high] straddles the limit price; market orders fill at
//!    bar open with slippage applied. The strategies in this codebase
//!    emit `limit_price=None` (market) almost exclusively; the limit
//!    path is included for symmetry and the one strategy (Halt
//!    Resume) that uses it.

use crate::state::Direction;

/// Tunable fill model — every numeric an operator might want to dial
/// without touching the strategy code lives here.
#[derive(Debug, Clone, Copy)]
pub struct FillModel {
    /// Slippage applied to market orders, in basis points of price.
    /// 5 bps is realistic for SP500 names at peak liquidity; widen to
    /// 15-25 bps when backtesting small-caps. Applied symmetrically:
    /// long entries pay +slip, short entries receive -slip.
    pub slippage_bps: f64,
    /// Per-trade commission in dollars. Default 0 (Alpaca paper/live
    /// is commission-free on equities) — kept as a knob for clarity
    /// if the operator ever wants to model a different broker.
    pub commission: f64,
    /// Per-share fee (e.g. SEC/TAF round-trip on shorts). 0 by default.
    pub per_share_fee: f64,
}

impl Default for FillModel {
    fn default() -> Self {
        Self {
            slippage_bps: 5.0,
            commission: 0.0,
            per_share_fee: 0.0,
        }
    }
}

impl FillModel {
    /// Apply slippage to a market price in the direction of the trade.
    /// Long buys pay UP (worse), longs sell DOWN (worse). Symmetric
    /// for shorts.
    pub fn slipped(&self, price: f64, side: Direction, is_entry: bool) -> f64 {
        let bps = self.slippage_bps / 10_000.0;
        let pay_up = matches!(
            (side, is_entry),
            (Direction::Long, true) | (Direction::Short, false)
        );
        if pay_up {
            price * (1.0 + bps)
        } else {
            price * (1.0 - bps)
        }
    }

    /// Total dollar cost of a fill: commission + per-share fee × qty.
    pub fn fee(&self, qty: f64) -> f64 {
        self.commission + self.per_share_fee * qty.abs()
    }
}

/// Outcome of bracket evaluation against a single subsequent bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BracketFill {
    /// Neither bracket triggered on this bar.
    None,
    /// Stop-loss fired. Returns the fill price (after slippage).
    StopLoss(f64),
    /// Take-profit fired. Returns the fill price (after slippage).
    TakeProfit(f64),
}

/// Resolve whether SL or TP filled inside the given bar's range.
/// Pessimistic ambiguity policy: if both SL and TP fall inside
/// `[bar.low, bar.high]`, assume SL fires first.
///
/// Slippage is applied to the trigger price (the SL/TP level), since
/// once triggered the order becomes a market order against the next
/// available print.
pub fn resolve_bracket(
    side: Direction,
    sl: f64,
    tp: f64,
    bar_low: f64,
    bar_high: f64,
    model: &FillModel,
) -> BracketFill {
    if bar_high <= 0.0 || bar_low <= 0.0 || bar_high < bar_low {
        return BracketFill::None;
    }
    let sl_hit = match side {
        Direction::Long => bar_low <= sl,
        Direction::Short => bar_high >= sl,
    };
    let tp_hit = match side {
        Direction::Long => bar_high >= tp,
        Direction::Short => bar_low <= tp,
    };
    match (sl_hit, tp_hit) {
        (true, _) => BracketFill::StopLoss(model.slipped(sl, side, false)),
        (false, true) => BracketFill::TakeProfit(model.slipped(tp, side, false)),
        (false, false) => BracketFill::None,
    }
}

/// Check whether a limit order priced at `limit` would cross during
/// the bar — i.e. the bar's range includes the price. Returns
/// `Some(fill_price)` if filled. Limit orders are assumed to fill
/// AT the limit price with no slippage benefit (queue-position
/// uncertainty).
///
/// Kept on the public surface even though the current strategy set
/// emits mostly market orders — Halt Resume uses a limit offset and
/// the engine path will route through this helper when wired up.
#[allow(dead_code)]
pub fn limit_crosses(limit: f64, bar_low: f64, bar_high: f64) -> Option<f64> {
    if limit >= bar_low && limit <= bar_high {
        Some(limit)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_long_entry_pays_up() {
        let m = FillModel { slippage_bps: 10.0, ..Default::default() };
        // 10 bps = 0.10% → 100 * 1.001 = 100.10
        let p = m.slipped(100.0, Direction::Long, true);
        assert!((p - 100.10).abs() < 1e-6);
    }

    #[test]
    fn slippage_long_exit_pays_down() {
        let m = FillModel { slippage_bps: 10.0, ..Default::default() };
        let p = m.slipped(100.0, Direction::Long, false);
        assert!((p - 99.90).abs() < 1e-6);
    }

    #[test]
    fn slippage_short_entry_pays_down() {
        // Short entry = SELL → worse price for us = LOWER price.
        let m = FillModel { slippage_bps: 10.0, ..Default::default() };
        let p = m.slipped(100.0, Direction::Short, true);
        assert!((p - 99.90).abs() < 1e-6);
    }

    #[test]
    fn slippage_short_exit_pays_up() {
        // Short exit = BUY → worse price = HIGHER price.
        let m = FillModel { slippage_bps: 10.0, ..Default::default() };
        let p = m.slipped(100.0, Direction::Short, false);
        assert!((p - 100.10).abs() < 1e-6);
    }

    #[test]
    fn bracket_sl_fires_when_long_low_below() {
        let m = FillModel::default();
        let r = resolve_bracket(Direction::Long, 99.0, 102.0, 98.5, 100.5, &m);
        match r {
            BracketFill::StopLoss(p) => assert!(p < 99.0, "p={p}"),
            _ => panic!("expected SL fill, got {r:?}"),
        }
    }

    #[test]
    fn bracket_tp_fires_when_long_high_above() {
        let m = FillModel::default();
        let r = resolve_bracket(Direction::Long, 99.0, 102.0, 100.0, 102.5, &m);
        match r {
            BracketFill::TakeProfit(p) => assert!(p < 102.0, "tp slip down for long exit"),
            _ => panic!("expected TP fill, got {r:?}"),
        }
    }

    #[test]
    fn bracket_pessimistic_when_both_in_range() {
        // Long position; bar engulfs both SL and TP → assume SL.
        let m = FillModel::default();
        let r = resolve_bracket(Direction::Long, 99.0, 102.0, 98.0, 103.0, &m);
        assert!(matches!(r, BracketFill::StopLoss(_)));
    }

    #[test]
    fn bracket_none_when_inside_range() {
        let m = FillModel::default();
        let r = resolve_bracket(Direction::Long, 99.0, 102.0, 100.0, 101.5, &m);
        assert_eq!(r, BracketFill::None);
    }

    #[test]
    fn bracket_short_sl_when_high_above() {
        let m = FillModel::default();
        let r = resolve_bracket(Direction::Short, 102.0, 99.0, 101.5, 102.5, &m);
        match r {
            BracketFill::StopLoss(p) => assert!(p > 102.0, "short SL exit pays up"),
            _ => panic!("expected short SL, got {r:?}"),
        }
    }

    #[test]
    fn limit_crosses_inside_range() {
        assert_eq!(limit_crosses(100.0, 99.5, 100.5), Some(100.0));
    }

    #[test]
    fn limit_outside_range_no_fill() {
        assert!(limit_crosses(100.0, 100.5, 101.0).is_none());
        assert!(limit_crosses(100.0, 98.0, 99.0).is_none());
    }

    #[test]
    fn fee_includes_per_share_and_commission() {
        let m = FillModel { commission: 1.0, per_share_fee: 0.005, ..Default::default() };
        assert!((m.fee(100.0) - (1.0 + 0.5)).abs() < 1e-9);
    }
}
