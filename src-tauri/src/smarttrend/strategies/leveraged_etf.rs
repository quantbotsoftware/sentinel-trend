//! Leveraged ETF — intraday mean reversion on 3x ETFs.
//!
//! ## Edge thesis
//!
//! 3x leveraged ETFs (TQQQ, SQQQ, SOXL, SOXS, UPRO, SPXU, TNA, TZA)
//! exhibit strong intraday mean reversion because:
//!   1. Their compounding decay penalises directional drift
//!   2. Daily rebalance flow forces price snapping to NAV
//!   3. Retail momentum waves create overshoots that fade
//!
//! Strategy: when price drifts > 1.5 % from a 50-bar (250 min) mean,
//! fade the move with a hold ≤ 30 min. Never overnight (path
//! dependence destroys equity over weekends).
//!
//! ## Backtest evidence (Phase 1.1 clone)
//!
//! 7,817 trades, WR 45.1 %, expectancy -$0.73/trade — the worst of the
//! testable cohort. Cause: naive Z-score on the entire RTH window
//! over-trades. Rust impl restricts to:
//!   - Calm/Trending regimes (skip Volatile + Panic)
//!   - 11:00-15:00 ET (skip choppy open + late-day MOC volatility)
//!   - RVOL > 1.5 (only fade *meaningful* moves)
//!
//! ## Failure modes
//!
//! - Overnight decay: HARD CLOSE 15:55 ET — never carry overnight
//! - Volatile-regime breakouts (Panic regime regime) → strategy
//!   refuses entries
//! - MOC market impact at close → close at 15:55, not 15:59

use crate::state::{Direction, Position};

use super::{ctx_is_sane, rolling_mean, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};
use super::MarketRegime;

#[derive(Debug, Clone)]
pub struct LeveragedEtf {
    pub deviation_pct: f64,
    pub min_rvol: f64,
    pub max_hold_minutes: u32,
    pub force_close_minute: u32,
    pub entry_minute_start: u32,
    pub entry_minute_end: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
}

impl Default for LeveragedEtf {
    fn default() -> Self {
        Self {
            deviation_pct: 1.5,
            min_rvol: 1.5,
            max_hold_minutes: 30,
            force_close_minute: 15 * 60 + 55, // 15:55 ET
            entry_minute_start: 11 * 60,      // 11:00 ET
            entry_minute_end: 15 * 60,        // 15:00 ET
            risk_dollars: 40.0,
            sub_wallet_cap_pct: 0.20,
        }
    }
}

pub const TARGET_SYMBOLS: &[&str] = &[
    "TQQQ", "SQQQ", "SOXL", "SOXS", "UPRO", "SPXU", "TNA", "TZA",
];

impl Strategy for LeveragedEtf {
    fn id(&self) -> StrategyId {
        StrategyId::LeveragedEtf
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 50) {
            return None;
        }
        let deviation_pct = ctx.get_param(StrategyId::LeveragedEtf, "deviation_pct", self.deviation_pct);
        let min_rvol = ctx.get_param(StrategyId::LeveragedEtf, "min_rvol", self.min_rvol);
        let max_hold_minutes = ctx.get_param(StrategyId::LeveragedEtf, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if !TARGET_SYMBOLS.iter().any(|s| s.eq_ignore_ascii_case(&ctx.symbol)) {
            return None;
        }
        if ctx.now_et_min < self.entry_minute_start
            || ctx.now_et_min >= self.entry_minute_end
        {
            return None;
        }
        if ctx.is_halted {
            return None;
        }
        if ctx.rvol < min_rvol {
            return None;
        }
        // Skip volatile / panic regimes — leveraged ETFs become trend-
        // chasers in those regimes (decay accelerates the same direction).
        if matches!(ctx.regime, Some(MarketRegime::Volatile | MarketRegime::Panic)) {
            return None;
        }
        let mean = rolling_mean(&ctx.candles, 50);
        if mean <= 0.0 {
            return None;
        }
        let dev_pct = ((ctx.price - mean) / mean) * 100.0;
        if dev_pct.abs() < deviation_pct {
            return None;
        }
        let side = if dev_pct > 0.0 {
            Direction::Short
        } else {
            Direction::Long
        };
        // Use 1× std as stop, mean as target.
        let sd = super::rolling_sd(&ctx.candles, 50);
        if sd <= 0.0 {
            return None;
        }
        let stop = if side == Direction::Short {
            ctx.price + sd
        } else {
            ctx.price - sd
        };
        let target = mean;
        let score = (dev_pct.abs().min(5.0) * 12.0)
            + (ctx.rvol.min(4.0) * 8.0);
        let score = score.clamp(0.0, 100.0);
        Some(EntrySignal {
            strategy: StrategyId::LeveragedEtf,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score,
            reason: "leveraged_mean_revert",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let max_hold_minutes = ctx.get_param(StrategyId::LeveragedEtf, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if ctx.now_et_min >= self.force_close_minute {
            return Some(ExitSignal {
                strategy: StrategyId::LeveragedEtf,
                limit_price: None,
                reason: "leveraged_etf_close",
            });
        }
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::LeveragedEtf,
                    limit_price: None,
                    reason: "leveraged_etf_max_hold",
                });
            }
        }
        None
    }

    fn position_size(&self, ctx: &MarketContext, sub_wallet: f64) -> f64 {
        let sd = super::rolling_sd(&ctx.candles, 50);
        if sd <= 0.0 || ctx.price <= 0.0 {
            return 0.0;
        }
        let mean = rolling_mean(&ctx.candles, 50);
        let dev = ctx.price - mean;
        let stop = if dev > 0.0 {
            ctx.price + sd
        } else {
            ctx.price - sd
        };
        super::risk_based_size(
            self.risk_dollars,
            ctx.price,
            stop,
            self.sub_wallet_cap_pct,
            sub_wallet,
        )
    }
}
