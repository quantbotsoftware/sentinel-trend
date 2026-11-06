//! Catalyst Front-Run — pre-positioning on news-driven catalysts.
//!
//! ## Edge thesis
//!
//! When the news stream surfaces a high-confidence catalyst on a
//! liquid name (catalyst_score ≥ 80) within 60 s of publication, the
//! price reaction is statistically front-runnable: 55-60 % of the time
//! the immediate move continues for 3-5 min before mean reversion. The
//! strategy:
//!   1. Confirms catalyst_score ≥ 80 AND news is < 60 s old
//!   2. Enters in the direction of the news polarity (bullish news →
//!      long; bearish → short)
//!   3. Time-stops at 5 min, ATR-stops at 1.0×
//!
//! ## Why "front-run" is OK in paper / equity (vs futures)
//!
//! Equities news goes through public regulatory feeds. Reading a SEC
//! filing 100 ms after publication is *not* market manipulation — it
//! is information processing. The strategy DOES NOT trade on
//! non-public material.
//!
//! ## Backtest evidence (literature prior)
//!
//! [ESTIMATED] Phase 1.1 marked this strategy as priors-only because we
//! lack a per-bar news label in the cached data. Estimated expectancy
//! +$5/trade, WR ~55 %, validated against published research. Live
//! collection (post-launch) will replace the prior with empirical
//! data.
//!
//! ## Failure modes
//!
//! - News false positive (typo, scraper noise) → confidence_floor + 0.5 %
//!   loss cap
//! - News already priced in → sub-1×ATR move → 5-min time stop
//! - Halt right after entry → halt watchdog closes

use crate::state::{Direction, Position};

use super::{ctx_is_sane, pure_atr, risk_based_size, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};

#[derive(Debug, Clone)]
pub struct CatalystFrontRun {
    pub min_catalyst_score: f64,
    pub min_rvol: f64,
    pub max_loss_pct: f64,
    pub atr_stop_mult: f64,
    pub atr_target_mult: f64,
    pub max_hold_minutes: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
}

impl Default for CatalystFrontRun {
    fn default() -> Self {
        // AUDIT RISK 6 (S30, 2026-05-28) — widened max_loss_pct from
        // 0.5% to 2.0% and atr_stop_mult from 1.0 to 1.5.
        //
        // Audit observed: small-cap bid-ask bounce alone routinely
        // exceeded the 0.5% loss cap, guaranteeing immediate stop-out
        // on the first tick. CatalystFrontRun was effectively losing
        // 100% of the time on noise.
        //
        // Spread study (paper trades 2026-05-18 → 05-26, n=27 entries):
        //   * Median spread on small-caps ($1-$50) at premarket open:
        //     85 bps. P75 = 150 bps. P95 = 320 bps.
        //   * Median 1-tick price oscillation (5s window) post-entry:
        //     ~0.6%-0.8% for $1-$10 plays, ~0.2%-0.3% for $20+.
        //   * 0.5% stop catches the noise in 80% of the $1-$10 cohort.
        //   * 2.0% stop covers spread + noise + 1-2 ATR while still
        //     keeping the loss bounded; combined with ATR-stop
        //     (atr_stop_mult * ATR ≈ 1.5x at typical 0.5%-1.5% ATR),
        //     the *effective* stop is `min(2.0%, atr_stop)` so trades
        //     in tight regimes still respect the ATR fence.
        //
        // Targets remain unchanged. The strategy is still a fast scalp
        // (`max_hold_minutes = 5`) — widening only the floor not the
        // ceiling.
        Self {
            min_catalyst_score: 80.0,
            min_rvol: 2.0,
            max_loss_pct: 0.02,
            atr_stop_mult: 1.5,
            atr_target_mult: 2.0,
            max_hold_minutes: 5,
            risk_dollars: 30.0,
            sub_wallet_cap_pct: 0.30,
        }
    }
}

impl Strategy for CatalystFrontRun {
    fn id(&self) -> StrategyId {
        StrategyId::CatalystFrontRun
    }

    fn allowed_window(&self) -> TradingWindow {
        // Extended hours allowed: ER calls + post-market 8K filings
        // are valid catalyst events.
        TradingWindow::ExtendedAndRth
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 5) {
            return None;
        }
        let min_catalyst_score = ctx.get_param(StrategyId::CatalystFrontRun, "min_catalyst_score", self.min_catalyst_score);
        let min_rvol = ctx.get_param(StrategyId::CatalystFrontRun, "min_rvol", self.min_rvol);
        let max_loss_pct = ctx.get_param(StrategyId::CatalystFrontRun, "max_loss_pct", self.max_loss_pct);
        let max_hold_minutes = ctx.get_param(StrategyId::CatalystFrontRun, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if ctx.is_halted {
            return None;
        }
        let score = ctx.catalyst_score?;
        if score < min_catalyst_score {
            return None;
        }
        if ctx.rvol < min_rvol {
            return None;
        }
        // PRIME F0.6 — direction from REAL news polarity, not the gap
        // sign. Empirical motivation: 83 live trades, -$233 total, 17%
        // win rate with the old `gap_pct >= 0.0 → Long` proxy (which
        // even mapped gap=0.0 to Long — a coin flip biased to the
        // wrong side on faded gaps). Priority:
        //   1. headline sentiment when it has clear polarity (|s|≥0.2)
        //   2. gap direction only when the gap is REAL (|gap|≥3%)
        //   3. otherwise stand aside — no thesis, no trade.
        let side = match ctx.news_sentiment {
            Some(s) if s >= 0.2 => Direction::Long,
            Some(s) if s <= -0.2 => Direction::Short,
            _ => {
                if ctx.gap_pct >= 3.0 {
                    Direction::Long
                } else if ctx.gap_pct <= -3.0 {
                    Direction::Short
                } else {
                    return None;
                }
            }
        };
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 {
            return None;
        }
        // Stop is the *tighter* of (1.0×ATR) and (max_loss_pct × price).
        let atr_stop = atr * self.atr_stop_mult;
        let pct_stop = ctx.price * max_loss_pct;
        let stop_dist = atr_stop.min(pct_stop);
        let target_dist = atr * self.atr_target_mult;
        let (stop, target) = match side {
            Direction::Long => (ctx.price - stop_dist, ctx.price + target_dist),
            Direction::Short => (ctx.price + stop_dist, ctx.price - target_dist),
        };
        let final_score = (score.min(100.0) * 0.6)
            + (ctx.rvol.min(8.0) * 5.0);
        let final_score = final_score.clamp(0.0, 100.0);
        Some(EntrySignal {
            strategy: StrategyId::CatalystFrontRun,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score: final_score,
            reason: "catalyst_frontrun",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let max_hold_minutes = ctx.get_param(StrategyId::CatalystFrontRun, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        // Strict 5-min time stop — strategy thesis decays fast.
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::CatalystFrontRun,
                    limit_price: None,
                    reason: "catalyst_frontrun_max_hold",
                });
            }
        }
        // Halt protection.
        if ctx.is_halted {
            return Some(ExitSignal {
                strategy: StrategyId::CatalystFrontRun,
                limit_price: None,
                reason: "catalyst_frontrun_halted",
            });
        }
        None
    }

    fn position_size(&self, ctx: &MarketContext, sub_wallet: f64) -> f64 {
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 || ctx.price <= 0.0 {
            return 0.0;
        }
        let max_loss_pct = ctx.get_param(StrategyId::CatalystFrontRun, "max_loss_pct", self.max_loss_pct);
        let stop_dist = (atr * self.atr_stop_mult).min(ctx.price * max_loss_pct);
        let stop = if ctx.gap_pct >= 0.0 {
            ctx.price - stop_dist
        } else {
            ctx.price + stop_dist
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
