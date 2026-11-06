use std::sync::Arc;

use serde::Serialize;
use titan_types::Candle;

use crate::config::SmartTrendConfig;
use crate::error::TitanError;
use crate::smarttrend::engine::{evaluate, MomentumInput, SmartTrendInput, Timeframe};
use crate::state::{MarkerKind, SymbolFsm, TitanState};
use crate::smarttrend::strategies::MarketRegime;

use super::market_data::fetch_candles_internal;

// ─── Trend veto helpers (mirrors sentinel.rs logic) ──────────────────

fn bt_ema(data: &[f64], period: usize) -> f64 {
    if data.is_empty() || period == 0 {
        return 0.0;
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut val = data[0];
    for &price in &data[1..] {
        val = price * k + val * (1.0 - k);
    }
    val
}

fn bt_efficiency_ratio(closes: &[f64], lookback: usize) -> f64 {
    if closes.len() <= lookback || lookback == 0 {
        return 0.0;
    }
    let start = closes.len() - lookback - 1;
    let end = closes.len() - 1;
    let direction = (closes[end] - closes[start]).abs();
    let mut volatility = 0.0;
    for i in (start + 1)..=end {
        volatility += (closes[i] - closes[i - 1]).abs();
    }
    if volatility > 0.0 {
        (direction / volatility).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn bt_atr_from_slice(candles: &[Candle], period: usize) -> f64 {
    if period == 0 || candles.len() < period + 1 {
        return 0.0;
    }
    let start = candles.len() - period;
    let mut sum_tr = 0.0;
    for idx in start..candles.len() {
        let bar = &candles[idx];
        let prev_close = candles[idx - 1].close;
        let tr = (bar.high - bar.low)
            .max((bar.high - prev_close).abs())
            .max((bar.low - prev_close).abs());
        sum_tr += tr;
    }
    let atr = sum_tr / period as f64;
    if atr.is_finite() && atr > 0.0 { atr } else { 0.0 }
}

const TREND_VETO_REASONS: &[&str] = &[
    "bearish_trend",
    "sideways_regime",
    "overextended_after_spike",
];

fn bt_should_veto_entry(candles: &[Candle], end_idx: usize, current_price: f64, cfg: &SmartTrendConfig, timeframe: &str) -> bool {
    if timeframe == "1m" {
        return false;
    }
    if end_idx < 40 {
        return false;
    }
    let window = &candles[..=end_idx];

    let closes: Vec<f64> = window.iter().map(|c| c.close).collect();
    let ema_fast = bt_ema(&closes, 8);
    let ema_slow = bt_ema(&closes, 21);
    if ema_slow <= 0.0 || !ema_slow.is_finite() {
        return false;
    }

    let ema_slow_prev = if closes.len() > 25 {
        bt_ema(&closes[..closes.len() - 5], 21)
    } else {
        ema_slow
    };
    let slope_bps = if ema_slow_prev > 0.0 {
        ((ema_slow - ema_slow_prev) / ema_slow_prev) * 10_000.0
    } else {
        0.0
    };

    let efficiency_ratio = bt_efficiency_ratio(&closes, 20);
    let atr = bt_atr_from_slice(window, 14);

    let n = window.len();
    let recent_start = n.saturating_sub(20);
    let recent = &window[recent_start..];
    let recent_high = recent.iter().map(|c| c.high).fold(f64::NEG_INFINITY, f64::max);
    let recent_low = recent.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);

    let ema_spread_bps = ((ema_fast - ema_slow).abs() / ema_slow) * 10_000.0;
    let slope_ok = slope_bps >= cfg.trend_min_slope_bps;
    let efficiency_ok = efficiency_ratio >= cfg.trend_min_efficiency;
    let is_bullish_trend = ema_fast > ema_slow && current_price >= ema_slow && slope_ok && efficiency_ok;

    let low_slope = slope_bps.abs() < cfg.trend_min_slope_bps;
    let low_efficiency = efficiency_ratio < cfg.trend_min_efficiency;
    let tight_range = atr > 0.0 && ((recent_high - recent_low) / atr) < 2.2;
    let compressed_emas = ema_spread_bps < cfg.trend_min_slope_bps;
    let is_sideways = (low_slope && low_efficiency) || (compressed_emas && tight_range);

    let pullback_depth_atr = if atr > 0.0 { (recent_high - current_price) / atr } else { 0.0 };
    let overextended_after_spike = pullback_depth_atr > cfg.pullback_max_atr;

    let bearish_trend = slope_bps <= -cfg.trend_min_slope_bps && efficiency_ratio >= cfg.trend_min_efficiency;

    let reason = if bearish_trend {
        Some("bearish_trend")
    } else if is_sideways {
        Some("sideways_regime")
    } else if overextended_after_spike {
        Some("overextended_after_spike")
    } else if !is_bullish_trend {
        Some("trend_not_bullish")
    } else {
        None
    };

    match reason {
        Some(r) => TREND_VETO_REASONS.contains(&r),
        None => false,
    }
}

fn bt_should_veto_quality(atr: f64, price: f64, avg_vol: f64, candles: &[Candle], end_idx: usize) -> bool {
    if price <= 0.0 || atr <= 0.0 {
        return false;
    }
    if (atr / price) < 0.005 {
        return true;
    }
    if avg_vol < 5000.0 {
        return true;
    }
    let er_lookback = 10usize;
    if end_idx >= er_lookback {
        let start = end_idx - er_lookback;
        let direction = (candles[end_idx].close - candles[start].close).abs();
        let mut volatility = 0.0;
        for j in (start + 1)..=end_idx {
            volatility += (candles[j].close - candles[j - 1].close).abs();
        }
        let er = if volatility > 0.0 { direction / volatility } else { 0.0 };
        if er < 0.15 {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestMarker {
    pub kind: String,
    pub bar_time: i64,
    pub price: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestStats {
    pub timeframe: String,
    pub total_trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    pub avg_gain_pct: f64,
    pub avg_loss_pct: f64,
    pub profit_factor: f64,
    pub total_pnl_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestResult {
    pub markers: Vec<BacktestMarker>,
    pub stats: BacktestStats,
    /// S03: disclaimer banner surfaced in UI — backtest uses simplified
    /// fills (no slippage, instant execution, no partial fills).
    pub disclaimer: String,
}

fn run_backtest_on_candles(
    candles: &[Candle],
    timeframe: &str,
    symbol: &str,
    cfg: &SmartTrendConfig,
) -> BacktestResult {
    let mut fsm = SymbolFsm::default();
    let mut bt_markers = Vec::new();

    for (i, candle) in candles.iter().enumerate() {
        let prev_close = if fsm.prev_close > 0.0 {
            fsm.prev_close
        } else if i > 0 {
            candles[i - 1].close
        } else {
            candle.close
        };

        let roc_lookback: usize = match timeframe {
            "1m" => 10,
            "5m" => 5,
            _ => 3,
        };
        let roc_base_idx = i.saturating_sub(roc_lookback);
        let roc_base_price = candles[roc_base_idx].close;
        let roc = if roc_base_price > 0.0 {
            (candle.close - roc_base_price) / roc_base_price * 100.0
        } else {
            0.0
        };

        let vol_window = 20usize;
        let vol_start = i.saturating_sub(vol_window);
        let avg_vol = if i > 0 {
            let slice = &candles[vol_start..i];
            let sum: f64 = slice.iter().map(|c| c.volume).sum();
            if slice.is_empty() { 1.0 } else { (sum / slice.len() as f64).max(1.0) }
        } else {
            1.0
        };
        let rvol = (candle.volume / avg_vol).max(0.01);

        let atr = if i >= 15 {
            let atr_period = 14usize;
            let start = i - atr_period;
            let mut sum_tr = 0.0;
            for idx in start..i {
                let bar = &candles[idx + 1];
                let pc = candles[idx].close;
                let tr = (bar.high - bar.low)
                    .max((bar.high - pc).abs())
                    .max((bar.low - pc).abs());
                sum_tr += tr;
            }
            let val = sum_tr / atr_period as f64;
            if val.is_finite() && val > 0.0 { val } else { 0.0 }
        } else {
            0.0
        };

        let closes: Vec<f64> = candles[..=i].iter().map(|c| c.close).collect();
        let ema_fast = bt_ema(&closes, 8);
        let ema_slow = bt_ema(&closes, 21);
        let above_ema_fast = candle.close >= ema_fast;
        let above_ema_slow = candle.close >= ema_slow;

        let tf_enum = Timeframe::from_str(timeframe).unwrap_or(Timeframe::M5);
        let input = SmartTrendInput {
            current_price: candle.close,
            prev_close,
            momentum_input: MomentumInput { rvol, roc },
            bar_time: candle.time,
            timeframe: tf_enum,
            symbol,
            atr,
            regime: MarketRegime::Calm,
            macro_trend_bullish: true,
            above_ema_fast,
            above_ema_slow,
            news_sentiment: None,
        };

        let result = evaluate(&input, &fsm, cfg);
        fsm = result.new_fsm;

        #[allow(for_loops_over_fallibles)]
        for m in &result.marker {
            let kind_str = match m.kind {
                MarkerKind::Entry => {
                    let vetoed_trend = bt_should_veto_entry(candles, i, candle.close, cfg, timeframe);
                    let vetoed_quality = bt_should_veto_quality(atr, candle.close, avg_vol, candles, i);
                    if vetoed_trend || vetoed_quality {
                        // S03 fix: match live sentinel.rs — vetoed entry suppresses
                        // the marker but does NOT reset FSM state. The FSM remains
                        // in whatever phase evaluate() returned.
                        continue;
                    }
                    "Entry"
                }
                MarkerKind::Exit => "Exit",
                _ => continue,
            };
            bt_markers.push(BacktestMarker {
                kind: kind_str.to_string(),
                bar_time: m.bar_time,
                price: m.price,
            });
        }
    }

    let stats = compute_stats(&bt_markers, timeframe);

    let mut entry_p: Option<(f64, i64)> = None;
    for m in &bt_markers {
        match m.kind.as_str() {
            "Entry" => {
                entry_p = Some((m.price, m.bar_time));
            }
            "Exit" => {
                if let Some((ep, et)) = entry_p.take() {
                    let pnl_pct = if ep > 0.0 { (m.price - ep) / ep * 100.0 } else { 0.0 };
                    tracing::info!(
                        "  BT-TRADE {symbol} {timeframe}: entry={ep:.2}@{et} exit={:.2}@{} pnl={pnl_pct:+.2}%",
                        m.price, m.bar_time
                    );
                }
            }
            _ => {}
        }
    }

    BacktestResult {
        markers: bt_markers,
        stats,
        disclaimer: "Backtest uses simplified fills: no slippage, instant execution, \
            no partial fills, no commission. Live results will differ."
            .to_string(),
    }
}

fn compute_stats(markers: &[BacktestMarker], timeframe: &str) -> BacktestStats {
    let mut trades_pnl: Vec<f64> = Vec::new();
    let mut entry_price: Option<f64> = None;

    for m in markers {
        match m.kind.as_str() {
            "Entry" => {
                entry_price = Some(m.price);
            }
            "Exit" => {
                if let Some(ep) = entry_price.take() {
                    if ep > 0.0 {
                        let pnl = (m.price - ep) / ep * 100.0;
                        trades_pnl.push(pnl);
                    }
                }
            }
            _ => {}
        }
    }

    let total_trades = trades_pnl.len();
    let wins = trades_pnl.iter().filter(|&&p| p > 0.0).count();
    let losses = trades_pnl.iter().filter(|&&p| p <= 0.0).count();
    let win_rate = if total_trades > 0 {
        wins as f64 / total_trades as f64 * 100.0
    } else {
        0.0
    };

    let gains: Vec<f64> = trades_pnl.iter().copied().filter(|&p| p > 0.0).collect();
    let loss_vals: Vec<f64> = trades_pnl.iter().copied().filter(|&p| p <= 0.0).collect();

    let avg_gain_pct = if gains.is_empty() {
        0.0
    } else {
        gains.iter().sum::<f64>() / gains.len() as f64
    };
    let avg_loss_pct = if loss_vals.is_empty() {
        0.0
    } else {
        loss_vals.iter().sum::<f64>() / loss_vals.len() as f64
    };

    let total_gains = gains.iter().sum::<f64>();
    let total_losses = loss_vals.iter().map(|v| v.abs()).sum::<f64>();
    let profit_factor = if total_losses > 0.0 {
        total_gains / total_losses
    } else if total_gains > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };

    let total_pnl_pct = trades_pnl.iter().sum::<f64>();

    BacktestStats {
        timeframe: timeframe.to_string(),
        total_trades,
        wins,
        losses,
        win_rate,
        avg_gain_pct,
        avg_loss_pct,
        profit_factor,
        total_pnl_pct,
    }
}

#[tauri::command]
pub async fn run_backtest(
    symbol: String,
    timeframe: String,
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<BacktestResult, TitanError> {
    let (key, secret, cfg) = {
        let config = state
            .config
            .lock()
            .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
        (
            config.market.alpaca_key_id.clone(),
            config.market.alpaca_secret.clone(),
            config.smarttrend.clone(),
        )
    };

    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation(
            "Alpaca API keys not configured".into(),
        ));
    }

    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(TitanError::Validation("Symbol cannot be empty".into()));
    }

    let candles =
        fetch_candles_internal(&symbol, &timeframe, &state.http_client, &key, &secret).await?;

    let result = run_backtest_on_candles(&candles, &timeframe, &symbol, &cfg);

    tracing::info!(
        "Backtest {symbol} {timeframe}: {} candles, {} trades, {:.1}% win rate, {:.2}% PnL",
        candles.len(),
        result.stats.total_trades,
        result.stats.win_rate,
        result.stats.total_pnl_pct
    );

    Ok(result)
}

#[tauri::command]
pub async fn run_backtest_all(
    symbol: String,
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<Vec<BacktestResult>, TitanError> {
    let (key, secret, cfg) = {
        let config = state
            .config
            .lock()
            .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
        (
            config.market.alpaca_key_id.clone(),
            config.market.alpaca_secret.clone(),
            config.smarttrend.clone(),
        )
    };

    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation(
            "Alpaca API keys not configured".into(),
        ));
    }

    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(TitanError::Validation("Symbol cannot be empty".into()));
    }

    let timeframes = ["1m", "5m", "15m", "30m", "1h", "4h", "1D"];
    let mut results = Vec::with_capacity(timeframes.len());

    for tf in &timeframes {
        let candles =
            fetch_candles_internal(&symbol, tf, &state.http_client, &key, &secret).await?;
        let result = run_backtest_on_candles(&candles, tf, &symbol, &cfg);
        results.push(result);
    }

    tracing::info!(
        "Backtest ALL {symbol}: {} timeframes completed",
        results.len()
    );

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_candle(time: i64, close: f64) -> Candle {
        Candle {
            time,
            open: close,
            high: close + 0.5,
            low: close - 0.5,
            close,
            volume: 1000.0,
        }
    }

    #[test]
    fn backtest_entry_exit_cycle() {
        // S36 (2026-05-28): 5m gained `confirm_target_for_tf=1` so the
        // legacy two-bar-confirm scenario only holds on 1m or ≥15m. We
        // pin this test to 15m so the original Idle → Confirming →
        // Active progression and 0.8% trailing math stay valid.
        let cfg = SmartTrendConfig::default();
        // 15m trailing stop = 0.8% from peak.
        // Entry @ 102, peak=102, drop to 101 = 0.98% > 0.8% => trailing exit at 101
        let candles = vec![
            make_candle(1000, 100.0),
            make_candle(1300, 101.0),
            make_candle(1600, 102.0), // Entry @ 102, peak=102
            make_candle(1900, 101.0), // drop 0.98% from peak -> trailing exit @ 101
        ];

        let result = run_backtest_on_candles(&candles, "15m", "TEST", &cfg);

        let entries: Vec<_> = result.markers.iter().filter(|m| m.kind == "Entry").collect();
        let exits: Vec<_> = result.markers.iter().filter(|m| m.kind == "Exit").collect();

        assert_eq!(entries.len(), 1, "Should have exactly 1 entry");
        assert_eq!(exits.len(), 1, "Should have exactly 1 exit");
        assert!((entries[0].price - 102.0).abs() < f64::EPSILON);
        assert!((exits[0].price - 101.0).abs() < f64::EPSILON);

        assert_eq!(result.stats.total_trades, 1);
        assert_eq!(result.stats.losses, 1);
        assert!(result.stats.total_pnl_pct < 0.0);
    }

    #[test]
    fn backtest_winning_trade() {
        // S36: pinned to 15m for the same reason as
        // `backtest_entry_exit_cycle` above.
        let cfg = SmartTrendConfig::default();
        // 15m trailing = 0.8%. Entry @ 102, peak rises to 105.
        // 104.5: drop 0.48% < 0.8% (OK). 104.0: drop 0.95% > 0.8% -> trailing exit @ 104
        let candles = vec![
            make_candle(1000, 100.0),
            make_candle(1300, 101.0),
            make_candle(1600, 102.0), // Entry @ 102, peak=102
            make_candle(1900, 105.0), // peak=105
            make_candle(2200, 104.5), // drop 0.48% -- hold
            make_candle(2500, 104.0), // drop 0.95% -- trailing exit @ 104
        ];

        let result = run_backtest_on_candles(&candles, "15m", "TEST", &cfg);

        assert_eq!(result.stats.total_trades, 1);
        assert_eq!(result.stats.wins, 1);
        assert!(result.stats.total_pnl_pct > 0.0);
        let expected_pnl = (104.0 - 102.0) / 102.0 * 100.0;
        assert!((result.stats.total_pnl_pct - expected_pnl).abs() < 0.01);
    }

    #[test]
    fn backtest_no_candles_returns_empty() {
        let cfg = SmartTrendConfig::default();
        let result = run_backtest_on_candles(&[], "5m", "TEST", &cfg);

        assert!(result.markers.is_empty());
        assert_eq!(result.stats.total_trades, 0);
        assert_eq!(result.stats.win_rate, 0.0);
    }

    #[test]
    fn backtest_stats_profit_factor() {
        let cfg = SmartTrendConfig::default();
        // 15m: trailing=0.8%, cooldown=base(3)+1=4, reentry_momentum=55+3=58
        let candles = vec![
            make_candle(1000, 100.0),  // seed
            make_candle(1300, 101.0),  // green 1
            make_candle(1600, 102.0),  // Entry @ 102, peak=102
            make_candle(1900, 101.0),  // drop 0.98% > 0.8% -> trailing exit @ 101 (loss)
            // cooldown: 4 bars (flat)
            make_candle(2200, 101.0),
            make_candle(2500, 101.0),
            make_candle(2800, 101.0),
            make_candle(3100, 101.0),
            // re-entry: strong green
            make_candle(3400, 103.0),  // cooldown expired + green -> Confirming
            make_candle(3700, 105.0),  // green 2 -> Active + Entry @ 105, peak=105
            make_candle(4000, 104.0),  // drop 0.95% > 0.8% -> trailing exit @ 104 (loss)
        ];

        let result = run_backtest_on_candles(&candles, "15m", "TEST", &cfg);
        assert_eq!(result.stats.total_trades, 2);
        assert_eq!(result.stats.wins, 0);
        assert_eq!(result.stats.losses, 2);
        assert!(result.stats.profit_factor == 0.0);
    }
}
