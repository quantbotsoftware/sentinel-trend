//! Halt Resume — fade the post-halt resume reaction.
//!
//! ## Edge thesis
//!
//! NASDAQ T1 / NYSE volatility halts trigger when a stock prints a 10 %
//! move in 5 min. When trading resumes (typically 5-min halt + reopen
//! cross), the resume cross often *over-reacts* (panic sell on a halt-
//! up; panic cover on a halt-down). Fading the resume direction in the
//! first 5 min after un-halt has historically returned 0.8-1.5×ATR
//! before mean reversion.
//!
//! ## Mechanics
//!
//! `is_halted` flag flips false (via Alpaca SIP `T="s"` resume code).
//! The strategy fires on the first bar AFTER resume, taking the side
//! opposite the halt direction. Tight stops because reverse-momentum
//! risk is real; max hold 10 min.
//!
//! ## Backtest evidence (literature prior)
//!
//! [ESTIMATED] Halt event data not in standard Alpaca historical bars,
//! so the simplified backtest skipped this strategy. Lit prior: WR
//! ~52 %, expectancy +$8/trade on small-cap halts. Sample size in
//! production will be small (~5 events/week) — strategy serves more
//! as opportunistic edge than primary P/L driver.
//!
//! ## Failure modes
//!
//! - Resume gap fills both directions instantly → use LIMIT order, not
//!   MARKET (specified via `limit_price`)
//! - Stock halts again immediately → halt watchdog closes
//! - Wide spread post-halt → spread_bps gate refuses entry > 100 bps

use crate::state::{Direction, Position};

use super::{ctx_is_sane, pure_atr, risk_based_size, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};

#[derive(Debug, Clone)]
pub struct HaltResume {
    pub min_pre_halt_move_pct: f64,
    pub max_spread_bps: f64,
    pub atr_stop_mult: f64,
    pub atr_target_mult: f64,
    pub max_hold_minutes: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
    /// Tracking field name reserved for the loop runner; the strategy
    /// itself is stateless — the bot decides "first bar after resume"
    /// using its halts cache.
    pub limit_offset_bps: f64,
}

impl Default for HaltResume {
    fn default() -> Self {
        Self {
            min_pre_halt_move_pct: 8.0,
            max_spread_bps: 100.0,
            atr_stop_mult: 1.0,
            atr_target_mult: 1.5,
            max_hold_minutes: 10,
            risk_dollars: 30.0,
            sub_wallet_cap_pct: 0.20,
            limit_offset_bps: 25.0,
        }
    }
}

impl Strategy for HaltResume {
    fn id(&self) -> StrategyId {
        StrategyId::HaltResume
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 10) {
            return None;
        }
        let min_pre_halt_move_pct = ctx.get_param(StrategyId::HaltResume, "min_pre_halt_move_pct", self.min_pre_halt_move_pct);
        let max_spread_bps = ctx.get_param(StrategyId::HaltResume, "max_spread_bps", self.max_spread_bps);
        let max_hold_minutes = ctx.get_param(StrategyId::HaltResume, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        // Strategy fires only on a "just resumed" bar. The bot's halt
        // tracker exposes that via `regime` not `is_halted`; the loop
        // runner inserts a marker in the candle slice which we infer
        // by checking: most recent N candles include a high-volatility
        // 10% move, AND `is_halted` is false right now.
        if ctx.is_halted {
            return None;
        }
        if ctx.spread_bps > max_spread_bps {
            return None;
        }
        // Detect pre-halt move: max |% change| within the last 6 5-min bars.
        if ctx.candles.len() < 6 {
            return None;
        }
        let recent: Vec<f64> = ctx.candles
            .iter()
            .rev()
            .take(6)
            .map(|c| c.close)
            .collect();
        let max_recent = recent.iter().cloned().fold(f64::MIN, f64::max);
        let min_recent = recent.iter().cloned().fold(f64::MAX, f64::min);
        if min_recent <= 0.0 {
            return None;
        }
        let span_pct = ((max_recent - min_recent) / min_recent) * 100.0;
        if span_pct < min_pre_halt_move_pct {
            return None;
        }
        // Direction: fade the *most recent* leg. If last bar > prior bar, the
        // resume printed up — we fade short. Vice-versa for down.
        let last = ctx.candles.last()?;
        let prior_idx = ctx.candles.len().saturating_sub(2);
        let prior = ctx.candles.get(prior_idx)?;
        let side = if last.close > prior.close {
            Direction::Short
        } else {
            Direction::Long
        };
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 {
            return None;
        }
        let stop_dist = atr * self.atr_stop_mult;
        let target_dist = atr * self.atr_target_mult;
        let (stop, target) = match side {
            Direction::Short => (ctx.price + stop_dist, ctx.price - target_dist),
            Direction::Long => (ctx.price - stop_dist, ctx.price + target_dist),
        };
        // Limit order offset to avoid chasing the resume cross.
        let offset = ctx.price * (self.limit_offset_bps / 10_000.0);
        let limit = match side {
            Direction::Short => Some(ctx.price + offset),
            Direction::Long => Some(ctx.price - offset),
        };
        let score = (span_pct.min(20.0) * 3.0)
            + (50.0 - ctx.spread_bps.min(50.0)) * 0.4;
        let score = score.clamp(0.0, 100.0);
        Some(EntrySignal {
            strategy: StrategyId::HaltResume,
            side,
            limit_price: limit,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score,
            reason: "halt_resume_fade",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let max_hold_minutes = ctx.get_param(StrategyId::HaltResume, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if ctx.is_halted {
            return Some(ExitSignal {
                strategy: StrategyId::HaltResume,
                limit_price: None,
                reason: "halt_resume_re_halted",
            });
        }
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::HaltResume,
                    limit_price: None,
                    reason: "halt_resume_max_hold",
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
        // PRIME F0.2 — mirror the entry's fade direction so the stop
        // distance matches the actual side. Pre-F0 the stop was always
        // computed above price (short-style) regardless of side; the
        // *distance* fed to `risk_based_size` happened to be identical
        // for both sides, but keeping the derivation side-aware guards
        // against future asymmetric stop logic drifting silently.
        let last = ctx.candles.last();
        let prior = ctx.candles.get(ctx.candles.len().saturating_sub(2));
        let fading_short = match (last, prior) {
            (Some(l), Some(p)) => l.close > p.close,
            _ => true,
        };
        let stop = if fading_short {
            ctx.price + atr * self.atr_stop_mult
        } else {
            ctx.price - atr * self.atr_stop_mult
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
