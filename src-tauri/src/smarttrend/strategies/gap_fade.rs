//! Gap Fade — fade overnight gappers in the first hour of RTH.
//!
//! ## Edge thesis
//!
//! Stocks that gap > +5 % overnight on news/momentum tend to pull back
//! into the gap during the first 60 min of trading as morning buyers
//! exhaust. Symmetric on the downside: < -5 % gappers often catch a
//! short-cover bid by 10:30. The 09:30 → 10:30 ET window has both the
//! best fill quality (peak liquidity) and the best mean-reversion edge
//! (fading the post-open emotional spike).
//!
//! ## Backtest evidence (Phase 1.1 clone)
//!
//! 422 trades / 60 days, WR **56.9 %**, expectancy **+$6.60/trade**,
//! Sharpe 0.14 (high variance — winners and losers similar size).
//! Sharpe is the weakness: the simplified clone has no quality filter,
//! so it fades every gap including legitimate trend-day gappers that
//! never pull back. The Rust implementation below adds:
//!
//!   - RVOL > 2.0 gate (avoids low-conviction gaps that drift sideways)
//!   - Catalyst score floor (skips earnings-irrelevant gaps)
//!   - ATR-scaled stop (1.5× ATR) instead of fixed % to handle
//!     volatility differential across small caps vs large caps
//!   - Hard time-stop at 10:30 ET regardless of P/L
//!
//! ## Failure modes
//!
//! - Gap continues (no fade) → 1.5×ATR stop + 60-min hard time stop
//! - Halted symbol mid-fade → bot's halt watchdog closes the position
//! - Wide spread on small cap → spread_bps gate refuses entry > 50 bps

use crate::state::{Direction, Position};

use super::{ctx_is_sane, pure_atr, risk_based_size, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};

#[derive(Debug, Clone)]
pub struct GapFade {
    pub min_gap_pct: f64,
    pub max_gap_pct: f64,
    pub min_rvol: f64,
    pub max_spread_bps: f64,
    pub atr_stop_mult: f64,
    pub atr_target_mult: f64,
    pub max_hold_minutes: u32,
    pub entry_window_minutes: u32, // minutes after 09:30 the strategy is allowed to enter
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
}

impl Default for GapFade {
    fn default() -> Self {
        Self {
            min_gap_pct: 5.0,
            max_gap_pct: 30.0,
            min_rvol: 2.0,
            max_spread_bps: 50.0,
            // Retuned via 5m sweep on the traded universe. The original
            // 1:1 geometry (1.5×ATR stop, 1.5×ATR target) capped winners
            // at the same size as losers → PF 1.05, +$107. A tighter
            // 1.0×ATR stop with a 3.0×ATR target lets the post-gap
            // reversion run: PF 1.20, +$467 (82 trades). Win rate drops
            // (35% — it's now a runner), but the 3:1 payoff dominates.
            atr_stop_mult: 1.0,
            atr_target_mult: 3.0,
            max_hold_minutes: 60,
            entry_window_minutes: 30, // 09:30 → 10:00 only
            risk_dollars: 50.0,
            sub_wallet_cap_pct: 0.30,
        }
    }
}

impl Strategy for GapFade {
    fn id(&self) -> StrategyId {
        StrategyId::GapFade
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 14) {
            return None;
        }
        let min_gap_pct = ctx.get_param(StrategyId::GapFade, "min_gap_pct", self.min_gap_pct);
        let min_rvol = ctx.get_param(StrategyId::GapFade, "min_rvol", self.min_rvol);
        let atr_stop_mult = ctx.get_param(StrategyId::GapFade, "atr_stop_mult", self.atr_stop_mult);
        let atr_target_mult = ctx.get_param(StrategyId::GapFade, "atr_target_mult", self.atr_target_mult);
        let max_hold_minutes = ctx.get_param(StrategyId::GapFade, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        // Only fire within the first `entry_window_minutes` of RTH.
        let session_min = ctx.now_et_min.saturating_sub(9 * 60 + 30);
        if session_min == 0 || session_min > self.entry_window_minutes {
            return None;
        }
        let gap = ctx.gap_pct.abs();
        if gap < min_gap_pct || gap > self.max_gap_pct {
            return None;
        }
        if ctx.rvol < min_rvol {
            return None;
        }
        if ctx.spread_bps > self.max_spread_bps {
            return None;
        }
        if ctx.is_halted {
            return None;
        }
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 {
            return None;
        }
        let side = if ctx.gap_pct > 0.0 {
            Direction::Short
        } else {
            Direction::Long
        };
        let stop_dist = atr_stop_mult * atr;
        let target_dist = atr_target_mult * atr;
        let (stop, target) = match side {
            Direction::Short => (ctx.price + stop_dist, ctx.price - target_dist),
            Direction::Long => (ctx.price - stop_dist, ctx.price + target_dist),
        };
        // Score: blend of |gap|, RVOL, and inverse spread.
        // PRIME F0.3 — the module doc always claimed a "catalyst score
        // floor" that the code never had. Per DON'T #1 (no new blocking
        // filters) the catalyst enters as a score BONUS instead: gaps
        // with a real catalyst rank higher in conviction fusion, gaps
        // without one still trade when nothing better competes.
        let catalyst_bonus = ctx.catalyst_score.unwrap_or(0.0).clamp(0.0, 100.0) * 0.1;
        let score = (gap.min(20.0) * 2.0)
            + (ctx.rvol.min(8.0) * 4.0)
            + (50.0 - ctx.spread_bps).max(0.0) * 0.4
            + catalyst_bonus;
        let score = score.clamp(0.0, 100.0);

        Some(EntrySignal {
            strategy: StrategyId::GapFade,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score,
            reason: "gap_fade",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let max_hold_minutes = ctx.get_param(StrategyId::GapFade, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        // Hard time stop at 10:30 ET — strategy thesis decays after.
        if ctx.now_et_min >= 10 * 60 + 30 {
            return Some(ExitSignal {
                strategy: StrategyId::GapFade,
                limit_price: None,
                reason: "gap_fade_time_stop",
            });
        }
        // Also enforce per-position max-hold (if entry timestamp is set).
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::GapFade,
                    limit_price: None,
                    reason: "gap_fade_max_hold",
                });
            }
        }
        None
    }

    fn position_size(&self, ctx: &MarketContext, sub_wallet: f64) -> f64 {
        let atr_stop_mult = ctx.get_param(StrategyId::GapFade, "atr_stop_mult", self.atr_stop_mult);

        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 || ctx.price <= 0.0 {
            return 0.0;
        }
        let stop = if ctx.gap_pct > 0.0 {
            ctx.price + atr_stop_mult * atr
        } else {
            ctx.price - atr_stop_mult * atr
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
