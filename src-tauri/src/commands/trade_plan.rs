//! APEX trade plan for the active symbol (2026-07-04).
//!
//! "Give me a message on the chart where I enter and exit — with the
//! calculations." This command runs the EXACT same gate chain the APEX
//! strategy trades through (`Apex::evaluate_entry_explained`) against
//! the live candle buffer, and returns either:
//!
//!   * a full trade plan — entry / stop / target prices, risk per
//!     share, share count for the strategy's risk budget, R:R, score —
//!     ready to be drawn as price lines on the chart; or
//!   * the FIRST failing gate, verbatim, so the operator knows exactly
//!     which condition is missing right now.
//!
//! Lock discipline: Tier-A (`state.market` for regime) is read alone
//! and dropped; dplus mutexes are standalone; Tier-B guards are then
//! acquired ascending (`last_prices` B1 → `candle_buffers` B3 →
//! `halts` B5 → `watchlist_metrics` B6), each dropped before the next.
//! The two tiers are never held simultaneously.

use std::sync::Arc;

use chrono::{Datelike, Timelike};
use serde::Serialize;

use crate::market::market_state::MarketDataState;
use crate::smarttrend::strategies::apex::{rolling_vwap, Apex};
use crate::smarttrend::strategies::MarketContext;
use crate::state::TitanState;

#[derive(Debug, Clone, Serialize)]
pub struct TradePlanPayload {
    pub symbol: String,
    pub timeframe: String,
    pub has_plan: bool,
    /// "long" / "short" when a plan exists.
    pub side: Option<&'static str>,
    pub entry: Option<f64>,
    pub stop: Option<f64>,
    pub target: Option<f64>,
    /// |entry − stop| — the dollar risk on ONE share.
    pub risk_per_share: Option<f64>,
    /// Shares sized so total risk ≈ `risk_dollars` (APEX budget).
    pub shares: Option<u32>,
    pub risk_dollars: Option<f64>,
    pub reward_dollars: Option<f64>,
    pub rr: Option<f64>,
    pub score: Option<f64>,
    pub max_hold_minutes: Option<u32>,
    /// First failing gate when there is no plan.
    pub block_reason: Option<String>,
    // Live inputs the plan (or the refusal) was computed from.
    pub price: f64,
    pub vwap: f64,
    pub atr: f64,
    pub rvol: f64,
    pub spread_bps: f64,
    pub catalyst: Option<f64>,
    pub generated_at: i64,
}

/// Aggregate 5m candles into 15m bars (ET-aligned on :00/:15/:30/:45).
/// The still-forming final bucket is dropped so the "last closed
/// candle" the APEX anatomy inspects is genuinely closed.
///
/// Why resample instead of a native 15m buffer: `apex-tf` (60d, 16
/// symbols) measured 15m as the strategy's best manual timeframe
/// (PF 1.49, 47.6% win vs 1.22/36.9% on 5m), but the live streamer
/// only maintains 1m/5m buffers. OHLCV aggregation of SIP 5m bars is
/// equivalent to native 15m bars.
fn resample_15m(five: &[titan_types::Candle], now_ts: i64) -> Vec<titan_types::Candle> {
    let mut out: Vec<titan_types::Candle> = Vec::with_capacity(five.len() / 3 + 1);
    for c in five {
        let bucket = c.time - c.time.rem_euclid(900);
        match out.last_mut() {
            Some(last) if last.time == bucket => {
                last.high = last.high.max(c.high);
                last.low = last.low.min(c.low);
                last.close = c.close;
                last.volume += c.volume;
            }
            _ => out.push(titan_types::Candle {
                time: bucket,
                open: c.open,
                high: c.high,
                low: c.low,
                close: c.close,
                volume: c.volume,
            }),
        }
    }
    // Drop the bucket whose 15-minute window hasn't elapsed yet.
    if out.last().map(|l| l.time + 900 > now_ts).unwrap_or(false) {
        out.pop();
    }
    out
}

/// First RTH bar open of *today* (ET) from the buffer, falling back to
/// `prev_close` — mirrors `tournament_dispatch::build_market_context`.
fn session_open_from(candles: &[titan_types::Candle], prev_close: f64) -> f64 {
    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    candles
        .iter()
        .find(|c| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(c.time, 0)
                .map(|d| d.with_timezone(&chrono_tz::US::Eastern))
                .map(|d| {
                    d.year() == now_et.year()
                        && d.ordinal() == now_et.ordinal()
                        && (d.hour() * 60 + d.minute()) >= 9 * 60 + 30
                })
                .unwrap_or(false)
        })
        .map(|c| c.open)
        .unwrap_or(prev_close)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(time: i64, open: f64, high: f64, low: f64, close: f64, volume: f64) -> titan_types::Candle {
        titan_types::Candle { time, open, high, low, close, volume }
    }

    #[test]
    fn resample_aggregates_ohlcv_into_quarter_buckets() {
        // Three 5m bars in the 10:00 bucket + one in the (open) 10:15.
        let five = vec![
            c(36000, 10.0, 11.0, 9.5, 10.5, 100.0),
            c(36300, 10.5, 12.0, 10.4, 11.8, 200.0),
            c(36600, 11.8, 11.9, 11.0, 11.2, 150.0),
            c(36900, 11.2, 11.4, 11.1, 11.3, 50.0),
        ];
        // now = 37200 → 10:15 bucket (36900) not elapsed yet → dropped.
        let out = resample_15m(&five, 37200);
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.time, 36000);
        assert_eq!(b.open, 10.0);
        assert_eq!(b.high, 12.0);
        assert_eq!(b.low, 9.5);
        assert_eq!(b.close, 11.2);
        assert_eq!(b.volume, 450.0);
    }

    #[test]
    fn resample_keeps_elapsed_final_bucket() {
        let five = vec![
            c(36000, 10.0, 11.0, 9.5, 10.5, 100.0),
            c(36300, 10.5, 12.0, 10.4, 11.8, 200.0),
            c(36600, 11.8, 11.9, 11.0, 11.2, 150.0),
        ];
        // now = 36900 → the 36000 bucket has fully elapsed → kept.
        let out = resample_15m(&five, 36900);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].close, 11.2);
    }
}

#[tauri::command]
pub fn get_trade_plan(
    state: tauri::State<'_, Arc<TitanState>>,
    market: tauri::State<'_, Arc<MarketDataState>>,
    symbol: String,
    timeframe: String,
) -> Result<TradePlanPayload, String> {
    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err("Symbol cannot be empty".to_string());
    }
    let tf: &'static str = match timeframe.as_str() {
        "1m" => "1m",
        "15m" => "15m",
        _ => "5m",
    };
    Ok(compute_plan(state.inner(), market.inner(), symbol, tf))
}

/// Full plan computation for one symbol × timeframe. Shared by
/// `get_trade_plan` (active chart symbol) and the APEX radar sweep
/// (`commands::apex_radar`) so both surfaces are guaranteed to agree.
pub(crate) fn compute_plan(
    state: &TitanState,
    market: &MarketDataState,
    symbol: String,
    tf: &'static str,
) -> TradePlanPayload {
    // ── Tier A: regime snapshot, taken alone and dropped. ──
    let regime = state.market.lock().map(|m| m.regime).ok();

    // dplus overrides (standalone mutexes — outside both tiers).
    let mut overrides = state
        .dplus()
        .map(|dp| dp.overrides.snapshot())
        .unwrap_or_default();
    // F4-APEX — operator calibration overrides, same merge as the live
    // dispatcher so radar/plan and live agree on effective thresholds.
    {
        let cfg_over = state
            .config
            .lock()
            .map(|c| c.evolution_protocol.frontier_overrides.clone())
            .unwrap_or_default();
        crate::smarttrend::strategies::apex_calibration::merge_apex_overrides(&mut overrides, &cfg_over);
    }

    // ── Tier B, ascending. ──
    let live_price: Option<f64> = {
        let lp = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
        lp.get(&symbol).copied()
    };
    let candles = {
        let buffers = market
            .candle_buffers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // 15m has no native buffer — resampled from 5m (see resample_15m).
        let source_tf = if tf == "15m" { "5m" } else { tf };
        buffers
            .get(&format!("{symbol}:{source_tf}"))
            .map(|b| b.snapshot())
            .unwrap_or_default()
    };
    let candles = if tf == "15m" {
        resample_15m(&candles, chrono::Utc::now().timestamp())
    } else {
        candles
    };
    let is_halted = {
        let halts = market.halts.lock().unwrap_or_else(|p| p.into_inner());
        // F2: resumed rows stay in the map — only active halts count.
        halts.get(&symbol).map(|h| h.resumed_at.is_none()).unwrap_or(false)
    };
    let wm = {
        let wms = market
            .watchlist_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        wms.get(&symbol).cloned()
    };

    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let generated_at = chrono::Utc::now().timestamp();

    let last_close = candles.last().map(|c| c.close).unwrap_or(0.0);
    let price = live_price.filter(|p| p.is_finite() && *p > 0.0).unwrap_or(last_close);

    if candles.is_empty() || price <= 0.0 {
        return TradePlanPayload {
            symbol,
            timeframe: tf.to_string(),
            has_plan: false,
            side: None,
            entry: None,
            stop: None,
            target: None,
            risk_per_share: None,
            shares: None,
            risk_dollars: None,
            reward_dollars: None,
            rr: None,
            score: None,
            max_hold_minutes: None,
            block_reason: Some("no candle data for this symbol/timeframe".to_string()),
            price,
            vwap: 0.0,
            atr: 0.0,
            rvol: 0.0,
            spread_bps: 0.0,
            catalyst: None,
            generated_at,
        };
    }

    let prev_close = if candles.len() >= 2 {
        candles[candles.len() - 2].close
    } else {
        price
    };
    let session_open = session_open_from(&candles, prev_close);
    let gap_pct = if prev_close > 0.0 {
        (session_open - prev_close) / prev_close * 100.0
    } else {
        0.0
    };

    // Metrics from the watchlist refresh loop; local fallback for rvol
    // when the symbol is off the hot list (same policy as dispatch).
    let (rvol, spread_bps, catalyst, news_sentiment) = match &wm {
        Some(m) => (
            m.rvol,
            m.spread_bps,
            Some(m.catalyst_flags.score),
            m.news_sentiment,
        ),
        None => {
            let n = candles.len();
            let local_rvol = if n >= 5 {
                let current = candles[n - 1].volume.max(1.0);
                let lookback = std::cmp::min(20, n - 1);
                let avg: f64 = candles[(n - 1 - lookback)..(n - 1)]
                    .iter()
                    .map(|c| c.volume)
                    .sum::<f64>()
                    / lookback as f64;
                if avg > 0.0 { (current / avg).clamp(0.1, 20.0) } else { 1.0 }
            } else {
                1.0
            };
            (local_rvol, 0.0, None, None)
        }
    };

    let atr = crate::smarttrend::strategies::pure_atr(&candles, 14);
    let vwap = rolling_vwap(&candles, 30);

    let ctx = MarketContext {
        symbol: symbol.clone(),
        timeframe: tf,
        price,
        prev_close,
        session_open,
        candles,
        atr,
        rvol,
        gap_pct,
        spread_bps,
        catalyst_score: catalyst,
        now_et_min: now_et.hour() * 60 + now_et.minute(),
        now_et_hour: now_et.hour(),
        now_et_minute: now_et.minute(),
        regime,
        is_halted,
        companion: None,
        float_shares: None,
        news_sentiment,
        overrides,
    };

    let apex = Apex::default();
    match apex.evaluate_entry_explained(&ctx) {
        Ok(sig) => {
            let risk_per_share = (ctx.price - sig.stop_loss).abs();
            let shares = if risk_per_share > 0.0 {
                (apex.risk_dollars / risk_per_share).floor().max(0.0) as u32
            } else {
                0
            };
            let reward_per_share = (sig.take_profit - ctx.price).abs();
            let rr = if risk_per_share > 0.0 {
                reward_per_share / risk_per_share
            } else {
                0.0
            };
            TradePlanPayload {
                symbol,
                timeframe: tf.to_string(),
                has_plan: true,
                side: Some(if sig.side == crate::state::Direction::Long {
                    "long"
                } else {
                    "short"
                }),
                entry: Some(ctx.price),
                stop: Some(sig.stop_loss),
                target: Some(sig.take_profit),
                risk_per_share: Some(risk_per_share),
                shares: Some(shares),
                risk_dollars: Some(shares as f64 * risk_per_share),
                reward_dollars: Some(shares as f64 * reward_per_share),
                rr: Some(rr),
                score: Some(sig.score),
                max_hold_minutes: Some(sig.max_hold_minutes),
                block_reason: None,
                price: ctx.price,
                vwap,
                atr,
                rvol,
                spread_bps,
                catalyst,
                generated_at,
            }
        }
        Err(reason) => TradePlanPayload {
            symbol,
            timeframe: tf.to_string(),
            has_plan: false,
            side: None,
            entry: None,
            stop: None,
            target: None,
            risk_per_share: None,
            shares: None,
            risk_dollars: None,
            reward_dollars: None,
            rr: None,
            score: None,
            max_hold_minutes: None,
            block_reason: Some(reason.to_string()),
            price: ctx.price,
            vwap,
            atr,
            rvol,
            spread_bps,
            catalyst,
            generated_at,
        },
    }
}
