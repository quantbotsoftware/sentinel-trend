//! Afternoon Fade — fade the 14:00-15:00 trend on liquid large caps.
//!
//! ## Edge thesis
//!
//! Between 15:00 and 15:45 ET, market-maker rebalancing flow tends to
//! fade the prevailing afternoon trend on the most liquid SP500 names.
//! Mechanism: order books skew imbalance reverses as MMs hedge their
//! delta into the close, making 1 % moves in the 14:00-15:00 window
//! revert ~55 % of the time over the next 55 min.
//!
//! ## Backtest evidence (Phase 1.1 clone)
//!
//! 63 trades, WR 55.6 %, expectancy +$0.94/trade. Marginally positive
//! but well below GO threshold. Low N (only ~1 trade/day across 42
//! large caps) — deliberately tight gates here keep the trade count
//! statistically meaningful rather than noise.
//!
//! Rust impl improvements:
//!   - Stricter trend gate (1.0 % → 1.2 %)
//!   - RVOL filter on the entry candle (don't fade dead tape)
//!   - Closed by 15:55 ET regardless (no late-rally risk)
//!
//! ## Failure modes
//!
//! - Trend continues into close on news days (FOMC, CPI surprise) →
//!   tight 0.5 % stop, hard close at 15:55
//! - Stock halted at 15:55 → bot's halt watchdog cleans up

use crate::state::{Direction, Position};

use super::{ctx_is_sane, EntrySignal, ExitSignal, MarketContext,
            Strategy, StrategyId, TradingWindow};

#[derive(Debug, Clone)]
pub struct AfternoonFade {
    pub min_anchor_move_pct: f64,
    pub min_rvol: f64,
    pub stop_pct: f64,
    pub force_close_minute: u32, // 935 = 15:35 ET, 955 = 15:55 ET
    pub max_hold_minutes: u32,
    pub entry_minute_start: u32,
    pub entry_minute_end: u32,
    pub risk_dollars: f64,
    pub sub_wallet_cap_pct: f64,
}

impl Default for AfternoonFade {
    fn default() -> Self {
        Self {
            min_anchor_move_pct: 1.2,
            // Retuned via 5m sweep on the traded small-cap universe. The
            // original large-cap params (stop 0.5%, rvol 0.8) were the
            // WORST config on small-caps (PF 0.88, −$656) — a 0.5% stop
            // is inside the intraday noise band on volatile names, so
            // every fade gets stopped before it reverts. Widening to a
            // 2.0% stop + rvol≥1.5 gate flips it to PF 1.18, +$560
            // (205 trades, 52.7% win) — a +$1.2k swing.
            min_rvol: 1.5,
            stop_pct: 0.02,
            force_close_minute: 15 * 60 + 55, // 15:55 ET
            max_hold_minutes: 55,
            entry_minute_start: 15 * 60,    // 15:00 ET
            entry_minute_end: 15 * 60 + 30, // 15:30 ET
            risk_dollars: 50.0,
            sub_wallet_cap_pct: 0.30,
        }
    }
}

impl Strategy for AfternoonFade {
    fn id(&self) -> StrategyId {
        StrategyId::AfternoonFade
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 24) {
            return None;
        }
        let min_anchor_move_pct = ctx.get_param(StrategyId::AfternoonFade, "min_anchor_move_pct", self.min_anchor_move_pct);
        let min_rvol = ctx.get_param(StrategyId::AfternoonFade, "min_rvol", self.min_rvol);
        let stop_pct = ctx.get_param(StrategyId::AfternoonFade, "stop_pct", self.stop_pct);

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
        // Use last 12 5-min candles (~60 min back) as anchor window.
        let anchor_idx = ctx.candles.len().saturating_sub(12);
        let anchor = ctx.candles.get(anchor_idx)?;
        let move_pct = ((ctx.price - anchor.close) / anchor.close) * 100.0;
        if move_pct.abs() < min_anchor_move_pct {
            return None;
        }
        let side = if move_pct > 0.0 {
            Direction::Short
        } else {
            Direction::Long
        };
        let stop = if side == Direction::Short {
            ctx.price * (1.0 + stop_pct)
        } else {
            ctx.price * (1.0 - stop_pct)
        };
        let target = if side == Direction::Short {
            ctx.price * (1.0 - stop_pct * 1.5)
        } else {
            ctx.price * (1.0 + stop_pct * 1.5)
        };
        let score = (move_pct.abs().min(5.0) * 12.0)
            + (ctx.rvol.min(3.0) * 8.0);
        let score = score.clamp(0.0, 100.0);
        Some(EntrySignal {
            strategy: StrategyId::AfternoonFade,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes: self.max_hold_minutes,
            score,
            reason: "afternoon_fade",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        // Pre-emptive close at 15:55 ET — the strategy thesis evaporates
        // and stock-halts-at-close are a known failure mode.
        if ctx.now_et_min >= self.force_close_minute {
            return Some(ExitSignal {
                strategy: StrategyId::AfternoonFade,
                limit_price: None,
                reason: "afternoon_fade_close",
            });
        }
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= self.max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::AfternoonFade,
                    limit_price: None,
                    reason: "afternoon_fade_max_hold",
                });
            }
        }
        None
    }

    fn position_size(&self, ctx: &MarketContext, sub_wallet: f64) -> f64 {
        if ctx.price <= 0.0 {
            return 0.0;
        }
        let stop_pct = ctx.get_param(StrategyId::AfternoonFade, "stop_pct", self.stop_pct);

        // Stop is fixed % so distance is well-defined regardless of side.
        let distance = ctx.price * stop_pct;
        let shares = (self.risk_dollars / distance).max(1.0).floor();
        let dollars = shares * ctx.price;
        let cap = sub_wallet * self.sub_wallet_cap_pct;
        dollars.min(cap).max(0.0)
    }
}
