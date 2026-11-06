//! Opening Range Breakout (ORB) — event-anchored intraday breakout.
//!
//! ## Edge thesis
//!
//! The first 15 minutes of US equity RTH (09:30–09:45 ET) produce a
//! tight high/low range under heavy participation. A bar that closes
//! *outside* that range during the second hour of trading (≤11:30 ET)
//! signals a continuation move driven by trapped liquidity on the
//! wrong side of the range plus institutions rebalancing into the
//! direction of least resistance. The edge is:
//!
//! * **Event-anchored** — fires once per symbol per day, around a
//!   structural event (the open), so it does *not* compete with the
//!   continuous SmartTrend momentum FSM or the mean-reversion fades.
//! * **Trend-confirmed** — entry requires price on the correct side
//!   of session VWAP, ruling out fake breakouts that immediately
//!   collapse back through the range.
//! * **Volume-confirmed** — RVOL ≥ 1.5 ensures the breakout has
//!   participation, not a thin-tape drift through the range.
//! * **Range-quality gated** — the OR width must be at least ~0.6×
//!   ATR(14) (genuine compression) but not blown out beyond ~3× ATR
//!   (where the "breakout" is really already exhausted).
//!
//! ## Risk geometry
//!
//! Stop: the **tighter** of (a) the opposite side of the OR and
//! (b) `k × ATR` from entry. Target: `R` multiples of risk distance
//! (default 2R, configurable). Max-hold: 90 minutes by default
//! (truncates losers that drift sideways past the thesis horizon).
//! Force-flat at 15:55 ET regardless.
//!
//! ## Failure modes
//!
//! - Range never forms (very thin tape) → `evaluate_entry` refuses
//!   when OR-high == OR-low.
//! - Whipsaw breakout that immediately fails → tight ATR stop +
//!   VWAP exit hook in `evaluate_exit`.
//! - Halt mid-breakout → `is_halted` guard refuses; live bot's halt
//!   watchdog closes any open position separately.

use chrono::Timelike;
use titan_types::Candle;

use crate::state::{Direction, Position};

use super::{ctx_is_sane, pure_atr, risk_based_size, EntrySignal, ExitSignal,
            MarketContext, Strategy, StrategyId, TradingWindow};

const RTH_OPEN_MIN: u32 = 9 * 60 + 30;
const RTH_CLOSE_MIN: u32 = 16 * 60;

#[derive(Debug, Clone)]
pub struct OpeningRangeBreakout {
    /// Length of the opening range window in minutes from 09:30 ET.
    pub or_minutes: u32,
    /// Latest minute-of-day (ET) the strategy is allowed to ENTER.
    /// 11:30 ET = 690. After this, even a valid breakout is ignored
    /// because the thesis horizon collapses by mid-day.
    pub entry_cutoff_min: u32,
    /// Force-flat for any open position at/after this minute (ET).
    /// 15:55 ET = 955 — leaves 5 min for safe exit before close.
    pub flat_by_min: u32,
    /// Maximum hold minutes from entry (per-trade time stop).
    pub max_hold_minutes: u32,
    /// Minimum relative volume on the breakout bar.
    pub min_rvol: f64,
    /// Maximum bid-ask spread allowed (basis points).
    pub max_spread_bps: f64,
    /// OR width must be >= this × ATR(14) (compression floor).
    pub min_or_width_atr_mult: f64,
    /// OR width must be <= this × ATR(14) (exhaustion ceiling).
    pub max_or_width_atr_mult: f64,
    /// Stop distance multiplier on ATR when the OR opposite side
    /// is farther than `k × ATR`. Use whichever is tighter.
    pub stop_atr_mult: f64,
    /// Risk-multiple for target (R:R = 1 : target_r_multiple).
    pub target_r_multiple: f64,
    /// Allow short side (downside breakouts). Honours global
    /// `allow_short` policy at the bot layer too.
    pub allow_short: bool,
    /// Dollars at risk per trade (sizing).
    pub risk_dollars: f64,
    /// Fraction of strategy sub-wallet a single trade may consume.
    pub sub_wallet_cap_pct: f64,
}

impl Default for OpeningRangeBreakout {
    fn default() -> Self {
        Self {
            or_minutes: 15,
            entry_cutoff_min: 11 * 60 + 30,
            flat_by_min: 15 * 60 + 55,
            max_hold_minutes: 90,
            // Tuned via parameter sweep on the traded small-cap universe.
            // The LIVE tournament evaluates strategies on 5m bars
            // (`sentinel/tournament_dispatch.rs`), so we optimise for 5m
            // robustness — NOT the 30m sweet spot. A tight 0.75×ATR stop
            // wins on 30m but is the worst config on 5m (noise stop-outs,
            // ~28% win). 3.0R / 1.0×ATR / rvol≥1.5 is best on 5m
            // (PF 1.09, +$440 vs old 2.0R default PF 1.01, +$49) and
            // still solid on 30m (PF 1.07, +$415).
            min_rvol: 1.5,
            max_spread_bps: 30.0,
            min_or_width_atr_mult: 0.6,
            max_or_width_atr_mult: 3.0,
            stop_atr_mult: 1.0,
            target_r_multiple: 3.0,
            allow_short: true,
            risk_dollars: 50.0,
            sub_wallet_cap_pct: 0.30,
        }
    }
}

/// Output of the opening-range scan over a candle slice — public so
/// unit tests can pin the exact math.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpeningRange {
    pub high: f64,
    pub low: f64,
    /// Last bar timestamp included in the OR (UTC seconds).
    pub last_time: i64,
    pub bars: usize,
}

/// Compute the opening range from a candle slice. The OR is built
/// from bars whose ET minute-of-day is in `[09:30, 09:30 + or_minutes)`.
/// Returns `None` if no qualifying bars exist or the range is degenerate
/// (high == low) — the strategy refuses to fire on those.
pub fn compute_opening_range(candles: &[Candle], or_minutes: u32) -> Option<OpeningRange> {
    if candles.is_empty() || or_minutes == 0 {
        return None;
    }
    use chrono::TimeZone;
    let last_ts = candles.last()?.time;
    let last_et = chrono_tz::US::Eastern.timestamp_opt(last_ts, 0).single()?;
    let session_date = last_et.date_naive();

    let mut high = f64::NEG_INFINITY;
    let mut low = f64::INFINITY;
    let mut last_time = 0i64;
    let mut bars = 0usize;
    let end_min = RTH_OPEN_MIN + or_minutes;

    for c in candles {
        let et = match chrono_tz::US::Eastern.timestamp_opt(c.time, 0).single() {
            Some(t) => t,
            None => continue,
        };
        if et.date_naive() != session_date {
            continue;
        }
        let mod_ = et.hour() * 60 + et.minute();
        if mod_ < RTH_OPEN_MIN || mod_ >= end_min {
            continue;
        }
        high = high.max(c.high);
        low = low.min(c.low);
        last_time = c.time.max(last_time);
        bars += 1;
    }
    if bars == 0 || !high.is_finite() || !low.is_finite() || high <= low {
        return None;
    }
    Some(OpeningRange { high, low, last_time, bars })
}

/// Compute today-only session VWAP from the candle slice using the
/// same ET date as the last bar. Used as the trend gate at entry.
/// Returns `None` when no qualifying bars are present or the volume
/// total is zero.
pub fn compute_session_vwap(candles: &[Candle]) -> Option<f64> {
    if candles.is_empty() {
        return None;
    }
    use chrono::TimeZone;
    let last_ts = candles.last()?.time;
    let last_et = chrono_tz::US::Eastern.timestamp_opt(last_ts, 0).single()?;
    let session_date = last_et.date_naive();

    let mut num = 0.0;
    let mut den = 0.0;
    for c in candles {
        let et = match chrono_tz::US::Eastern.timestamp_opt(c.time, 0).single() {
            Some(t) => t,
            None => continue,
        };
        if et.date_naive() != session_date {
            continue;
        }
        let typical = (c.high + c.low + c.close) / 3.0;
        let v = c.volume.max(0.0);
        num += typical * v;
        den += v;
    }
    if den <= 0.0 || !num.is_finite() {
        return None;
    }
    Some(num / den)
}

impl Strategy for OpeningRangeBreakout {
    fn id(&self) -> StrategyId {
        StrategyId::OpeningRangeBreakout
    }

    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::RthOnly
    }

    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if !ctx_is_sane(ctx, 14) {
            return None;
        }
        let or_minutes = ctx.get_param(StrategyId::OpeningRangeBreakout, "or_minutes", self.or_minutes as f64) as u32;
        let min_rvol = ctx.get_param(StrategyId::OpeningRangeBreakout, "min_rvol", self.min_rvol);
        let stop_atr_mult = ctx.get_param(StrategyId::OpeningRangeBreakout, "stop_atr_mult", self.stop_atr_mult);
        let target_r_multiple = ctx.get_param(StrategyId::OpeningRangeBreakout, "target_r_multiple", self.target_r_multiple);
        let max_hold_minutes = ctx.get_param(StrategyId::OpeningRangeBreakout, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        if ctx.is_halted {
            return None;
        }
        if ctx.spread_bps > self.max_spread_bps {
            return None;
        }
        if ctx.rvol < min_rvol {
            return None;
        }
        // Must be after the OR window forms, before the cutoff.
        let now = ctx.now_et_min;
        if now < RTH_OPEN_MIN + or_minutes || now > self.entry_cutoff_min {
            return None;
        }

        let or = compute_opening_range(&ctx.candles, or_minutes)?;
        let or_width = or.high - or.low;
        let atr = if ctx.atr > 0.0 { ctx.atr } else { pure_atr(&ctx.candles, 14) };
        if atr <= 0.0 {
            return None;
        }
        let width_in_atr = or_width / atr;
        if width_in_atr < self.min_or_width_atr_mult
            || width_in_atr > self.max_or_width_atr_mult
        {
            return None;
        }

        let vwap = compute_session_vwap(&ctx.candles)?;

        // Breakout direction.
        let (side, stop_or_side, target_dir) = if ctx.price > or.high && ctx.price >= vwap {
            (Direction::Long, or.low, 1.0)
        } else if self.allow_short && ctx.price < or.low && ctx.price <= vwap {
            (Direction::Short, or.high, -1.0)
        } else {
            return None;
        };

        // Stop = tighter of (opposite OR side) and (k × ATR from price).
        let atr_stop_dist = stop_atr_mult * atr;
        let stop = match side {
            Direction::Long => {
                let atr_stop = ctx.price - atr_stop_dist;
                stop_or_side.max(atr_stop)
            }
            Direction::Short => {
                let atr_stop = ctx.price + atr_stop_dist;
                stop_or_side.min(atr_stop)
            }
        };
        let risk = (ctx.price - stop).abs();
        if !risk.is_finite() || risk <= 0.0 {
            return None;
        }
        let target = ctx.price + target_dir * (target_r_multiple * risk);

        // Score: emphasize breakout strength, RVOL, and tight spread.
        let extension = match side {
            Direction::Long => (ctx.price - or.high) / atr.max(1e-9),
            Direction::Short => (or.low - ctx.price) / atr.max(1e-9),
        }
        .clamp(0.0, 3.0);
        let score = (extension * 20.0)
            + (ctx.rvol.min(8.0) * 5.0)
            + (self.max_spread_bps - ctx.spread_bps).max(0.0) * 0.4;
        let score = score.clamp(0.0, 100.0);

        Some(EntrySignal {
            strategy: StrategyId::OpeningRangeBreakout,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes,
            score,
            reason: "orb_breakout",
        })
    }

    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal> {
        let max_hold_minutes = ctx.get_param(StrategyId::OpeningRangeBreakout, "max_hold_minutes", self.max_hold_minutes as f64) as u32;

        // Hard flat-by-close.
        if ctx.now_et_min >= self.flat_by_min && ctx.now_et_min < RTH_CLOSE_MIN {
            return Some(ExitSignal {
                strategy: StrategyId::OpeningRangeBreakout,
                limit_price: None,
                reason: "orb_flat_by_close",
            });
        }
        // VWAP-lost exit: long positions exit if price drops below
        // VWAP; shorts exit if price rises above VWAP. Acts as a
        // soft trend break before the bracket SL fires.
        if let Some(vwap) = compute_session_vwap(&ctx.candles) {
            match pos.side {
                Direction::Long if ctx.price < vwap => {
                    return Some(ExitSignal {
                        strategy: StrategyId::OpeningRangeBreakout,
                        limit_price: None,
                        reason: "orb_vwap_lost",
                    });
                }
                Direction::Short if ctx.price > vwap => {
                    return Some(ExitSignal {
                        strategy: StrategyId::OpeningRangeBreakout,
                        limit_price: None,
                        reason: "orb_vwap_lost",
                    });
                }
                _ => {}
            }
        }
        // Per-position max hold.
        if let Some(opened_at) = pos.opened_at {
            let now_secs = chrono::Utc::now().timestamp();
            let held_min = (now_secs - opened_at).max(0) / 60;
            if held_min as u32 >= max_hold_minutes {
                return Some(ExitSignal {
                    strategy: StrategyId::OpeningRangeBreakout,
                    limit_price: None,
                    reason: "orb_max_hold",
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
        let stop_atr_mult = ctx.get_param(StrategyId::OpeningRangeBreakout, "stop_atr_mult", self.stop_atr_mult);

        // Reproduce the stop distance the entry would have used (k × ATR);
        // the OR-anchored side will only tighten this, so this is a
        // conservative upper bound on the risk-per-share.
        let stop = ctx.price - stop_atr_mult * atr;
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
