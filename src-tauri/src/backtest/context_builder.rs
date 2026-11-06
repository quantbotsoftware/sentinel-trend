//! Reconstructs a `MarketContext` (the input the `Strategy` trait
//! consumes) from a slice of historical bars. This is the single piece
//! that guarantees PARITY between the offline backtest and the live
//! sentinel/bot pipeline — both feed the same trait method, so the
//! only thing that can drift is the inputs.
//!
//! ## Inputs derived from candles only
//!
//! - `price` / `prev_close` — direct from the latest bar.
//! - `session_open` — open of the first bar on the same ET date.
//! - `gap_pct` — `(session_open - prev_close_yesterday) / prev_close_yesterday * 100`.
//!   The "previous session close" is the close of the last bar on the
//!   PRIOR ET date.
//! - `atr` — Wilder ATR(14) via `titan_indicators::calculate_atr` if
//!   ≥15 bars; otherwise the strategy helper `pure_atr` (already used
//!   by the live engine when the indicator cache is cold).
//! - `rvol` — last-bar volume / 20-bar SMA volume, identical to
//!   `assemble_momentum_input` in the live sentinel.
//! - `now_et_*` — ET clock derived from the latest bar timestamp
//!   via `chrono_tz::US::Eastern` (S04 invariant).
//! - `is_halted` — always `false` in the backtest (halt feed not in
//!   historical bars; live path supplies it from Alpaca status sub).
//!
//! ## Inputs supplied externally (caller-provided)
//!
//! - `symbol`, `timeframe` — passed in.
//! - `spread_bps` — fill model constant (default 5 bps for liquid
//!   names). Realistic enough for swing/intraday equities; pair-trade
//!   strategies that read this are tested with a configurable value.
//! - `catalyst_score`, `regime`, `companion`, `float_shares` — `None`
//!   in the default builder. Catalyst-anchored strategies
//!   (Catalyst Front-Run, Extended News) will refuse to fire without
//!   them, which is the correct behaviour — historical news feeds are
//!   out of scope for this iteration.

use chrono::{TimeZone, Timelike};
use titan_indicators::calculate_atr;
use titan_types::Candle;

use crate::smarttrend::strategies::{
    pure_atr, MarketContext, MarketRegime,
};

/// Static parameters that don't change per-bar. Provided by the
/// caller and reused for every context built during a simulation run.
#[derive(Debug, Clone)]
pub struct ContextDefaults {
    pub symbol: String,
    pub timeframe: &'static str,
    /// Constant bid-ask spread assumption (basis points). Realistic
    /// for SP500 liquid names; widen for small-caps if the universe
    /// extends in that direction.
    pub spread_bps: f64,
    /// Optional regime tag forced onto every context. None = leave
    /// the field unset (strategies that gate on regime will refuse).
    pub regime: Option<MarketRegime>,
}

impl Default for ContextDefaults {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            timeframe: "5m",
            spread_bps: 5.0,
            regime: Some(MarketRegime::Calm),
        }
    }
}

/// Build a `MarketContext` from a candle window. `bars` is the full
/// history available to the simulator up to and including the bar
/// being evaluated; `bars.last()` is the bar whose close drives the
/// decision. Returns `None` when the window is empty or the latest
/// timestamp cannot be converted to ET.
pub fn build_context(bars: &[Candle], defaults: &ContextDefaults) -> Option<MarketContext> {
    let last = bars.last()?;
    let prev_close = if bars.len() >= 2 {
        bars[bars.len() - 2].close
    } else {
        last.close
    };

    let last_et = chrono_tz::US::Eastern
        .timestamp_opt(last.time, 0)
        .single()?;
    let last_date = last_et.date_naive();

    // Session open: first bar on the same ET date.
    let session_open = bars
        .iter()
        .find_map(|c| {
            let et = chrono_tz::US::Eastern.timestamp_opt(c.time, 0).single()?;
            if et.date_naive() == last_date {
                Some(c.open)
            } else {
                None
            }
        })
        .unwrap_or(last.open);

    // Previous session close = last bar on the prior ET date.
    let prev_session_close = bars
        .iter()
        .rev()
        .find_map(|c| {
            let et = chrono_tz::US::Eastern.timestamp_opt(c.time, 0).single()?;
            if et.date_naive() < last_date {
                Some(c.close)
            } else {
                None
            }
        });

    let gap_pct = match prev_session_close {
        Some(pc) if pc > 0.0 => (session_open - pc) / pc * 100.0,
        _ => 0.0,
    };

    let atr = if bars.len() >= 15 {
        calculate_atr(bars, 14).unwrap_or_else(|| pure_atr(bars, 14))
    } else {
        pure_atr(bars, 14)
    };

    let rvol = compute_rvol(bars, 20);

    let now_et_min = last_et.hour() * 60 + last_et.minute();
    let now_et_hour = last_et.hour();
    let now_et_minute = last_et.minute();

    Some(MarketContext {
        symbol: defaults.symbol.clone(),
        timeframe: defaults.timeframe,
        price: last.close,
        prev_close,
        session_open,
        candles: bars.to_vec(),
        atr,
        rvol,
        gap_pct,
        spread_bps: defaults.spread_bps,
        catalyst_score: None,
        now_et_min,
        now_et_hour,
        now_et_minute,
        regime: defaults.regime,
        is_halted: false,
        companion: None,
        float_shares: None,
        news_sentiment: None,
        overrides: std::collections::HashMap::new(),
    })
}

/// last_bar.volume / SMA(volume, n) over the prior `n` bars. Returns
/// 0.0 if insufficient history or zero baseline (matches the live
/// sentinel's `assemble_momentum_input` semantics).
pub fn compute_rvol(bars: &[Candle], n: usize) -> f64 {
    if bars.len() < n + 1 {
        return 0.0;
    }
    let last = bars[bars.len() - 1].volume;
    let start = bars.len() - n - 1;
    let end = bars.len() - 1;
    let mut sum = 0.0;
    for b in &bars[start..end] {
        sum += b.volume;
    }
    let avg = sum / n as f64;
    if avg <= 0.0 {
        return 0.0;
    }
    last / avg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn et_ts(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    fn make_bar(time: i64, price: f64, volume: f64) -> Candle {
        Candle {
            time,
            open: price,
            high: price * 1.002,
            low: price * 0.998,
            close: price,
            volume,
        }
    }

    #[test]
    fn build_context_basic_session_open_and_now_et() {
        // 09:30, 09:35, 09:40 ET on a fixed Monday.
        let bars = vec![
            make_bar(et_ts(2026, 6, 15, 9, 30), 100.0, 10_000.0),
            make_bar(et_ts(2026, 6, 15, 9, 35), 100.5, 12_000.0),
            make_bar(et_ts(2026, 6, 15, 9, 40), 101.0, 14_000.0),
        ];
        let d = ContextDefaults {
            symbol: "TEST".into(),
            ..ContextDefaults::default()
        };
        let ctx = build_context(&bars, &d).expect("context");
        assert_eq!(ctx.symbol, "TEST");
        assert!((ctx.session_open - 100.0).abs() < 1e-9);
        assert!((ctx.price - 101.0).abs() < 1e-9);
        assert!((ctx.prev_close - 100.5).abs() < 1e-9);
        assert_eq!(ctx.now_et_hour, 9);
        assert_eq!(ctx.now_et_minute, 40);
        assert_eq!(ctx.now_et_min, 9 * 60 + 40);
    }

    #[test]
    fn gap_pct_computed_against_prior_session() {
        let bars = vec![
            // Prior session: Friday 2026-06-12 15:55 close.
            make_bar(et_ts(2026, 6, 12, 15, 55), 100.0, 10_000.0),
            // New session: Monday 2026-06-15 09:30 open = 102.0
            make_bar(et_ts(2026, 6, 15, 9, 30), 102.0, 11_000.0),
            make_bar(et_ts(2026, 6, 15, 9, 35), 102.5, 12_000.0),
        ];
        let d = ContextDefaults::default();
        let ctx = build_context(&bars, &d).expect("context");
        assert!((ctx.gap_pct - 2.0).abs() < 1e-6, "gap_pct = {}", ctx.gap_pct);
    }

    #[test]
    fn rvol_matches_last_over_sma() {
        // 21 bars: first 20 at vol=10k, last at vol=20k → rvol = 2.0
        let mut bars: Vec<Candle> = (0..20)
            .map(|i| make_bar(et_ts(2026, 6, 15, 9, 30) + (i as i64) * 60, 100.0, 10_000.0))
            .collect();
        bars.push(make_bar(
            et_ts(2026, 6, 15, 9, 30) + 21 * 60,
            100.5,
            20_000.0,
        ));
        let d = ContextDefaults::default();
        let ctx = build_context(&bars, &d).expect("context");
        assert!((ctx.rvol - 2.0).abs() < 1e-9, "rvol = {}", ctx.rvol);
    }

    #[test]
    fn atr_finite_with_enough_history() {
        let bars: Vec<Candle> = (0..30)
            .map(|i| {
                let p = 100.0 + (i as f64) * 0.5;
                make_bar(et_ts(2026, 6, 15, 9, 30) + (i as i64) * 60, p, 10_000.0)
            })
            .collect();
        let d = ContextDefaults::default();
        let ctx = build_context(&bars, &d).expect("context");
        assert!(ctx.atr.is_finite() && ctx.atr > 0.0, "atr={}", ctx.atr);
    }

    #[test]
    fn build_context_returns_none_on_empty() {
        let bars: Vec<Candle> = vec![];
        let d = ContextDefaults::default();
        assert!(build_context(&bars, &d).is_none());
    }
}
