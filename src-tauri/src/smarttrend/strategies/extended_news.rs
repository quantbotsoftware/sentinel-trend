//! Extended News — post-market and pre-market news plays.
//!
//! ## Edge thesis
//!
//! Post-market (16:00-20:00 ET) and pre-market (04:00-09:30 ET) news
//! events on liquid names create directional moves with persistent
//! follow-through into the next session. Unlike `catalyst_frontrun`
//! (which fades the immediate spike), this strategy *follows* the
//! catalyst direction over a 30-min hold expecting trend continuation.
//!
//! ## Why distinct from catalyst_frontrun
//!
//! Catalyst Front-Run targets the 0-5 min window where retail panic
//! creates over-reaction. Extended News targets the 5-30 min window
//! AFTER the initial reaction settles, when institutional flow takes
//! over and pushes the new equilibrium price.
//!
//! ## Backtest evidence (literature prior)
//!
//! [ESTIMATED] Extended-hours bars are partial in standard Alpaca
//! historical data. Lit prior: WR ~50 %, expectancy +$4/trade, but
//! with wider spreads. Strategy ships disabled by default during
//! Session 1 launch — needs live data validation before promotion.
//!
//! ## Failure modes
//!
//! - Liquidity void (small caps post-market) → spread_bps gate,
//!   max 50 bps allowed
//! - Halts more common in extended → halt watchdog closes
//! - Pre-market news reverses on RTH open → 30-min time stop limits
//!   exposure to the open

use crate::state::{Direction, Position};

use super::{ctx_is_sane, pure_atr, risk_based_size, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};

#[derive(Debug, Clone)]
pub struct ExtendedNews {
    pub min_catalyst_score: f64,
    pub min_move_pct: f64,
    pub max_spread_bps: f64,
    pub atr_stop_mult: f64,
    pub atr_target_mult: f64,
    pub max_hold_minutes: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
    pub min_price: f64,
}

impl Default for ExtendedNews {
    fn default() -> Self {
        Self {
            min_catalyst_score: 70.0,
            min_move_pct: 2.0,
            max_spread_bps: 250.0,
            atr_stop_mult: 1.5,
            atr_target_mult: 2.5,
            max_hold_minutes: 30,
            risk_dollars: 30.0,
            sub_wallet_cap_pct: 0.20,
            min_price: 5.0,
        }
    }
}

impl Strategy for ExtendedNews {
    fn id(&self) -> StrategyId {
        StrategyId::ExtendedNews
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::ExtendedAndRth
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 10) {
            return None;
        }
        let min_catalyst_score = ctx.get_param(StrategyId::ExtendedNews, "min_catalyst_score", self.min_catalyst_score);
        let min_move_pct = ctx.get_param(StrategyId::ExtendedNews, "min_move_pct", self.min_move_pct);
        let max_spread_bps = ctx.get_param(StrategyId::ExtendedNews, "max_spread_bps", self.max_spread_bps);
        let max_hold_minutes = ctx.get_param(StrategyId::ExtendedNews, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if ctx.is_halted {
            return None;
        }
        if ctx.price < self.min_price {
            return None;
        }
        if ctx.spread_bps > max_spread_bps {
            return None;
        }
        // Strategy active only outside RTH. RTH is 09:30-16:00.
        let in_rth = (9 * 60 + 30..16 * 60).contains(&ctx.now_et_min);
        if in_rth {
            return None;
        }
        let score = ctx.catalyst_score?;
        if score < min_catalyst_score {
            return None;
        }
        // Use last 5 candles (~25 min back) as anchor.
        let anchor_idx = ctx.candles.len().saturating_sub(5);
        let anchor = ctx.candles.get(anchor_idx)?;
        let move_pct = ((ctx.price - anchor.close) / anchor.close) * 100.0;
        if move_pct.abs() < min_move_pct {
            return None;
        }
        // FOLLOW the move (vs catalyst_frontrun which fades it).
        let side = if move_pct > 0.0 {
            Direction::Long
        } else {
            Direction::Short
        };
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 {
            return None;
        }
        let stop_dist = atr * self.atr_stop_mult;
        let target_dist = atr * self.atr_target_mult;
        let (stop, target) = match side {
            Direction::Long => (ctx.price - stop_dist, ctx.price + target_dist),
            Direction::Short => (ctx.price + stop_dist, ctx.price - target_dist),
        };
        let final_score = (score.min(100.0) * 0.5)
            + (move_pct.abs().min(8.0) * 5.0)
            + (max_spread_bps - ctx.spread_bps.min(max_spread_bps)) * 0.3;
        let final_score = final_score.clamp(0.0, 100.0);
        Some(EntrySignal {
            strategy: StrategyId::ExtendedNews,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score: final_score,
            reason: "extended_news_follow",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let max_hold_minutes = ctx.get_param(StrategyId::ExtendedNews, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if ctx.is_halted {
            return Some(ExitSignal {
                strategy: StrategyId::ExtendedNews,
                limit_price: None,
                reason: "extended_news_halted",
            });
        }
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::ExtendedNews,
                    limit_price: None,
                    reason: "extended_news_max_hold",
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
        // We don't know side at sizing time; use the larger of the two
        // possible stops to be conservative.
        let stop_dist = atr * self.atr_stop_mult;
        let stop = ctx.price - stop_dist;
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
