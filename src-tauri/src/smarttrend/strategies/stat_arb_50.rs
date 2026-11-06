//! Stat Arb 50 — cross-sector ETF pair stat-arb.
//!
//! ## Edge thesis
//!
//! Five hand-picked sector pairs with historically tight cointegration:
//!   - XLK / XLF (tech vs financials)
//!   - XLE / XLK (energy vs tech, anti-correlation)
//!   - XLY / XLP (consumer discretionary vs staples)
//!   - XLV / XLU (healthcare vs utilities)
//!   - XLI / XLB (industrials vs materials)
//!
//! Each pair runs the same Z-score logic as `etf_pair`, but on a
//! cross-sector basis. The bot's pair coordinator picks the leg with
//! the *higher* RVOL (more sensitivity) as the primary trade and
//! opens a synthetic offset on the lower-RVOL leg.
//!
//! ## Backtest evidence (Phase 1.1 clone)
//!
//! 721 trades, WR 55.8 %, expectancy -$0.16/trade — break-even on the
//! naive clone. Most failure cases were trend days where ratios stayed
//! stretched for hours; same Rust-impl improvements as `etf_pair`
//! (Z>2.5, ratio_sd floor, regime gating) lift this above the bar.
//!
//! ## Failure modes
//!
//! - Sector-specific shock (e.g. bank run on XLF) → ratio breaks
//!   structurally; force-close at |Z|>3.5
//! - Volume dries up on a single small-cap leg → spread_bps gate

use crate::state::{Direction, Position};

use super::{ctx_is_sane, pure_atr, risk_based_size, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};

#[derive(Debug, Clone)]
pub struct StatArb50 {
    pub min_z: f64,
    pub force_close_z: f64,
    pub revert_z_threshold: f64,
    pub min_sd: f64,
    pub max_spread_bps: f64,
    pub max_hold_minutes: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
}

impl Default for StatArb50 {
    fn default() -> Self {
        Self {
            min_z: 2.5,
            force_close_z: 3.5,
            revert_z_threshold: 0.5,
            min_sd: 0.0008,
            max_spread_bps: 30.0,
            max_hold_minutes: 60,
            risk_dollars: 40.0,
            sub_wallet_cap_pct: 0.30,
        }
    }
}

/// Recognised pair primaries — the bot only considers these symbols
/// for stat-arb entries; they all are deeply liquid sector ETFs that
/// can be paired with their sibling without slippage concerns.
pub const PRIMARY_SYMBOLS: &[&str] = &["XLK", "XLE", "XLY", "XLV", "XLI"];

impl Strategy for StatArb50 {
    fn id(&self) -> StrategyId {
        StrategyId::StatArb50
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 50) {
            return None;
        }
        let min_z = ctx.get_param(StrategyId::StatArb50, "min_z", self.min_z);
        let max_spread_bps = ctx.get_param(StrategyId::StatArb50, "max_spread_bps", self.max_spread_bps);
        let max_hold_minutes = ctx.get_param(StrategyId::StatArb50, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if !PRIMARY_SYMBOLS.iter().any(|s| s.eq_ignore_ascii_case(&ctx.symbol)) {
            return None;
        }
        if ctx.is_halted {
            return None;
        }
        if ctx.spread_bps > max_spread_bps {
            return None;
        }
        let comp = ctx.companion.as_ref()?;
        if comp.ratio_sd < self.min_sd || comp.ratio_z.abs() < min_z {
            return None;
        }
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
        let score = (comp.ratio_z.abs() * 18.0).min(70.0)
            + (max_spread_bps - ctx.spread_bps.min(max_spread_bps));
        let score = score.clamp(0.0, 100.0);
        Some(EntrySignal {
            strategy: StrategyId::StatArb50,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score,
            reason: "stat_arb_pair",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let force_close_z = ctx.get_param(StrategyId::StatArb50, "force_close_z", self.force_close_z);
        let max_hold_minutes = ctx.get_param(StrategyId::StatArb50, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if let Some(comp) = ctx.companion.as_ref() {
            if comp.ratio_z.abs() >= force_close_z {
                return Some(ExitSignal {
                    strategy: StrategyId::StatArb50,
                    limit_price: None,
                    reason: "stat_arb_force_close",
                });
            }
            if comp.ratio_z.abs() < self.revert_z_threshold {
                return Some(ExitSignal {
                    strategy: StrategyId::StatArb50,
                    limit_price: None,
                    reason: "stat_arb_revert",
                });
            }
        }
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::StatArb50,
                    limit_price: None,
                    reason: "stat_arb_max_hold",
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

    fn max_spread_bps(&self) -> f64 {
        self.max_spread_bps
    }
}
