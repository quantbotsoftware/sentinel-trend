//! ETF Pair — XLK / SPY mean-reversion pair trade.
//!
//! ## Edge thesis
//!
//! XLK (technology select sector ETF) and SPY (S&P 500) are tightly
//! correlated (r ≈ 0.92 over the trailing year), but XLK is ~30 %
//! more volatile because tech has higher beta. When the *ratio*
//! XLK/SPY drifts > 2σ from its 50-bar rolling mean, intraday
//! mean-reversion is statistically observable in a 60-min window.
//!
//! ## Backtest evidence (Phase 1.1 clone)
//!
//! 97 trades, WR 48.5 %, expectancy -$1.03 — sub-par. Two reasons:
//!   1. Naive clone enters every Z > 2 violation, including legitimate
//!      sector rotation days where the ratio stays elevated for hours.
//!   2. Doesn't filter for intraday liquidity windows.
//!
//! Rust impl improvements: only fire 10:30-15:00 (steady state of
//! sector rotation), require Z > 2.5 (tighter), and require ratio_sd
//! > 0.0005 (skip flat days where the trade has no edge).
//!
//! ## Failure modes
//!
//! - Correlation break (e.g. earnings shock to a single XLK constituent
//!   like AAPL post-FOMC) → Z keeps drifting, position bleeds.
//!   Defense: |Z| > 3.5 force-close, plus 60-min hard time stop.
//! - Companion data missing / stale → ctx.companion is None, refuse.

use crate::state::{Direction, Position};

use super::{ctx_is_sane, pure_atr, risk_based_size, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};

#[derive(Debug, Clone)]
pub struct EtfPair {
    pub min_z: f64,
    pub force_close_z: f64,
    pub revert_z_threshold: f64,
    pub min_sd: f64,
    pub entry_window_start_min: u32,
    pub entry_window_end_min: u32,
    pub max_hold_minutes: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
}

impl Default for EtfPair {
    fn default() -> Self {
        Self {
            min_z: 2.5,
            force_close_z: 3.5,
            revert_z_threshold: 0.5,
            min_sd: 0.0005,
            entry_window_start_min: 10 * 60 + 30, // 10:30 ET
            entry_window_end_min: 15 * 60,         // 15:00 ET
            max_hold_minutes: 60,
            risk_dollars: 50.0,
            sub_wallet_cap_pct: 0.40,
        }
    }
}

impl Strategy for EtfPair {
    fn id(&self) -> StrategyId {
        StrategyId::EtfPair
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 50) {
            return None;
        }
        let min_z = ctx.get_param(StrategyId::EtfPair, "min_z", self.min_z);
        let max_hold_minutes = ctx.get_param(StrategyId::EtfPair, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if ctx.now_et_min < self.entry_window_start_min
            || ctx.now_et_min >= self.entry_window_end_min
        {
            return None;
        }
        if ctx.is_halted {
            return None;
        }
        let comp = ctx.companion.as_ref()?;
        if comp.ratio_sd < self.min_sd {
            return None;
        }
        if comp.ratio_z.abs() < min_z {
            return None;
        }
        // Mean-revert: if Z>0 (XLK rich vs SPY) → short XLK (this leg).
        // The companion (SPY) leg is opened by the bot's pair coordinator;
        // this strategy returns the signal for the *primary* (XLK) only.
        let side = if comp.ratio_z > 0.0 {
            Direction::Short
        } else {
            Direction::Long
        };
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 {
            return None;
        }
        let stop_dist = atr * 2.0;
        let target_dist = atr * 1.5;
        let (stop, target) = match side {
            Direction::Short => (ctx.price + stop_dist, ctx.price - target_dist),
            Direction::Long => (ctx.price - stop_dist, ctx.price + target_dist),
        };
        let score = (comp.ratio_z.abs() * 20.0).min(80.0)
            + (10.0 - ctx.spread_bps.min(10.0));
        let score = score.clamp(0.0, 100.0);
        Some(EntrySignal {
            strategy: StrategyId::EtfPair,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score,
            reason: "etf_pair_zscore",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let force_close_z = ctx.get_param(StrategyId::EtfPair, "force_close_z", self.force_close_z);
        let max_hold_minutes = ctx.get_param(StrategyId::EtfPair, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        // Force-close on extreme divergence (correlation break risk).
        if let Some(comp) = ctx.companion.as_ref() {
            if comp.ratio_z.abs() >= force_close_z {
                return Some(ExitSignal {
                    strategy: StrategyId::EtfPair,
                    limit_price: None,
                    reason: "etf_pair_force_close",
                });
            }
            if comp.ratio_z.abs() < self.revert_z_threshold {
                return Some(ExitSignal {
                    strategy: StrategyId::EtfPair,
                    limit_price: None,
                    reason: "etf_pair_revert",
                });
            }
        }
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::EtfPair,
                    limit_price: None,
                    reason: "etf_pair_max_hold",
                });
            }
        }
        None
    }

    fn position_size(&self, ctx: &MarketContext, sub_wallet: f64) -> f64 {
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 || ctx.price <= 0.0 {
            return 0.0;
        }
        let z_dir = ctx.companion.as_ref().map(|c| c.ratio_z).unwrap_or(0.0);
        let stop = if z_dir > 0.0 {
            ctx.price + 2.0 * atr
        } else {
            ctx.price - 2.0 * atr
        };
        risk_based_size(
            self.risk_dollars,
            ctx.price,
            stop,
            self.sub_wallet_cap_pct,
            sub_wallet,
        )
    }
}
