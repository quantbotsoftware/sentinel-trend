//! Unit tests for the strategy framework. Tests focus on:
//!   - Pure helpers (rolling_mean, rolling_sd, pure_atr, risk_based_size)
//!   - Trait round-trip (StrategyId::ALL, registry, by_id)
//!   - TradingWindow time gating
//!   - Each strategy's entry / exit / sizing in isolated MarketContext
//!     fixtures
//!
//! Strategies are tested with deterministic synthetic candles. Real
//! market data is not used at the unit-test layer — the goal is to
//! pin algorithmic correctness and refusal semantics, not to replicate
//! the backtest. Phase 1.5 expands these tests to 50+ per strategy.

use rust_decimal_macros::dec;
use titan_types::Candle;

use crate::state::{Direction, Position};

use super::*;

fn synth_candles(prices: &[f64]) -> Vec<Candle> {
    prices
        .iter()
        .enumerate()
        .map(|(i, &p)| Candle {
            time: i as i64 * 300,
            open: p,
            high: p * 1.002,
            low: p * 0.998,
            close: p,
            volume: 100_000.0,
        })
        .collect()
}

fn synth_position(symbol: &str, side: Direction, opened_at: i64) -> Position {
    Position {
        symbol: symbol.into(),
        qty: dec!(10),
        avg_entry: dec!(100),
        current_price: 100.0,
        unrealized_pnl: 0.0,
        side,
        opened_at: Some(opened_at),
        source: "bot".into(),
        first_seen: opened_at,
        version: 0,
        trade_id: "test".into(),
        is_swing_intent: false,
        entry_timeframe: String::new(),
        stop_loss: 0.0,
        take_profit: 0.0,
    }
}

fn base_ctx() -> MarketContext {
    let candles = synth_candles(&[100.0; 60]);
    MarketContext {
        symbol: "TEST".into(),
        timeframe: "5m",
        price: 100.0,
        prev_close: 100.0,
        session_open: 100.0,
        candles,
        atr: 0.5,
        rvol: 2.5,
        gap_pct: 0.0,
        spread_bps: 5.0,
        catalyst_score: Some(75.0),
        now_et_min: 9 * 60 + 35, // 09:35 ET
        now_et_hour: 9,
        now_et_minute: 35,
        regime: Some(MarketRegime::Calm),
        is_halted: false,
        companion: None,
        float_shares: None,
        news_sentiment: None,
        overrides: std::collections::HashMap::new(),
    }
}

// ──────────────────────────────────────────────────────────────────
//  Helpers
// ──────────────────────────────────────────────────────────────────

#[test]
fn rolling_mean_constant_series() {
    let c = synth_candles(&[10.0; 20]);
    assert!((rolling_mean(&c, 10) - 10.0).abs() < 1e-9);
}

#[test]
fn rolling_mean_window_smaller_than_history() {
    let c = synth_candles(&[1.0, 2.0, 3.0, 4.0, 5.0]);
    // Last 3: (3+4+5)/3 = 4.0
    assert!((rolling_mean(&c, 3) - 4.0).abs() < 1e-9);
}

#[test]
fn rolling_mean_window_larger_than_history() {
    let c = synth_candles(&[1.0, 2.0, 3.0]);
    // Use all available
    assert!((rolling_mean(&c, 10) - 2.0).abs() < 1e-9);
}

#[test]
fn rolling_mean_empty() {
    let c: Vec<Candle> = vec![];
    assert_eq!(rolling_mean(&c, 5), 0.0);
}

#[test]
fn rolling_sd_constant_is_zero() {
    let c = synth_candles(&[10.0; 20]);
    assert!(rolling_sd(&c, 10) < 1e-9);
}

#[test]
fn rolling_sd_alternating() {
    let c = synth_candles(&[10.0, 12.0, 10.0, 12.0, 10.0, 12.0, 10.0, 12.0, 10.0, 12.0]);
    // Pop SD of 10/12 alternating = 1.0
    let sd = rolling_sd(&c, 10);
    assert!((sd - 1.0).abs() < 1e-3, "got {sd}");
}

#[test]
fn rolling_sd_too_short_returns_zero() {
    let c = synth_candles(&[1.0]);
    assert_eq!(rolling_sd(&c, 5), 0.0);
}

#[test]
fn pure_atr_basic() {
    let c = synth_candles(&[100.0, 101.0, 99.0, 100.0, 102.0]);
    let atr = pure_atr(&c, 4);
    // Each TR = high-low = ~ 0.4 from synth_candles, plus close gaps.
    assert!(atr > 0.0);
}

#[test]
fn pure_atr_empty() {
    let c: Vec<Candle> = vec![];
    assert_eq!(pure_atr(&c, 14), 0.0);
}

#[test]
fn risk_based_size_basic() {
    // $50 risk, entry 100, stop 99 → 50 shares. dollars = 5000.
    // sub_wallet 12500, cap 0.30 → 3750. min(5000, 3750) = 3750.
    let s = risk_based_size(50.0, 100.0, 99.0, 0.30, 12_500.0);
    assert!((s - 3750.0).abs() < 1.0);
}

#[test]
fn risk_based_size_capped_by_subwallet() {
    let s = risk_based_size(50.0, 100.0, 99.99, 0.30, 1_000.0);
    // tiny stop → many shares; capped by 1000*0.3 = 300
    assert!((s - 300.0).abs() < 1.0);
}

#[test]
fn risk_based_size_invalid_entry() {
    assert_eq!(risk_based_size(50.0, 0.0, 99.0, 0.30, 1000.0), 0.0);
    assert_eq!(risk_based_size(50.0, -1.0, 99.0, 0.30, 1000.0), 0.0);
    assert_eq!(risk_based_size(50.0, f64::NAN, 99.0, 0.30, 1000.0), 0.0);
}

// ──────────────────────────────────────────────────────────────────
//  StrategyId / TradingWindow
// ──────────────────────────────────────────────────────────────────

#[test]
fn strategy_id_all_lists_all_strategies() {
    assert_eq!(StrategyId::ALL.len(), 11);
}

#[test]
fn strategy_id_as_str_unique() {
    let names: std::collections::HashSet<_> =
        StrategyId::ALL.iter().map(|id| id.as_str()).collect();
    assert_eq!(names.len(), StrategyId::ALL.len());
}

#[test]
fn trading_window_rth_only() {
    let w = TradingWindow::RthOnly;
    assert!(!w.permits(9, 0));
    assert!(!w.permits(9, 29));
    assert!(w.permits(9, 30));
    assert!(w.permits(15, 59));
    assert!(!w.permits(16, 0));
}

#[test]
fn trading_window_extended() {
    let w = TradingWindow::ExtendedAndRth;
    assert!(!w.permits(3, 59));
    assert!(w.permits(4, 0));
    assert!(w.permits(19, 59));
    assert!(!w.permits(20, 0));
}

#[test]
fn trading_window_always_on() {
    let w = TradingWindow::AlwaysOn;
    assert!(w.permits(0, 0));
    assert!(w.permits(23, 59));
}

// ──────────────────────────────────────────────────────────────────
//  Registry
// ──────────────────────────────────────────────────────────────────

#[test]
fn registry_returns_all_strategies() {
    let r = registry();
    assert_eq!(r.len(), StrategyId::ALL.len());
}

#[test]
fn registry_each_id_is_unique() {
    let r = registry();
    let ids: std::collections::HashSet<_> = r.iter().map(|s| s.id()).collect();
    assert_eq!(ids.len(), StrategyId::ALL.len());
}

#[test]
fn by_id_finds_each_strategy() {
    for id in StrategyId::ALL {
        let s = by_id(*id);
        assert!(s.is_some(), "by_id({id:?}) returned None");
        assert_eq!(s.unwrap().id(), *id);
    }
}

// ──────────────────────────────────────────────────────────────────
//  Gap Fade
// ──────────────────────────────────────────────────────────────────

#[test]
fn gap_fade_fires_on_5pct_gap_up() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 7.0;
    let s = gap_fade::GapFade::default();
    let sig = s.evaluate_entry(&ctx).expect("gap_fade should fire");
    assert_eq!(sig.side, Direction::Short);
    assert!(sig.score > 0.0);
}

#[test]
fn gap_fade_fires_on_5pct_gap_down() {
    let mut ctx = base_ctx();
    ctx.gap_pct = -6.0;
    let s = gap_fade::GapFade::default();
    let sig = s.evaluate_entry(&ctx).expect("gap_fade should fire");
    assert_eq!(sig.side, Direction::Long);
}

#[test]
fn gap_fade_refuses_below_threshold() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 3.0;
    let s = gap_fade::GapFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn gap_fade_refuses_after_window() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 7.0;
    ctx.now_et_min = 11 * 60; // 11:00 ET, past 30-min window
    let s = gap_fade::GapFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn gap_fade_refuses_low_rvol() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 7.0;
    ctx.rvol = 1.0;
    let s = gap_fade::GapFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn gap_fade_refuses_halted() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 7.0;
    ctx.is_halted = true;
    let s = gap_fade::GapFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn gap_fade_exit_at_1030() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 10 * 60 + 30;
    let s = gap_fade::GapFade::default();
    let pos = synth_position("TEST", Direction::Short, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).expect("should exit at 10:30");
    assert_eq!(exit.reason, "gap_fade_time_stop");
}

// ──────────────────────────────────────────────────────────────────
//  ETF Pair
// ──────────────────────────────────────────────────────────────────

#[test]
fn etf_pair_refuses_no_companion() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 11 * 60;
    let s = etf_pair::EtfPair::default();
    assert!(s.evaluate_entry(&ctx).is_none(), "no companion → no signal");
}

#[test]
fn etf_pair_fires_on_high_z() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 11 * 60;
    ctx.companion = Some(CompanionSnapshot {
        symbol: "SPY".into(),
        price: 400.0,
        ratio_mean: 0.50,
        ratio_sd: 0.001,
        ratio_z: 3.0,
    });
    let s = etf_pair::EtfPair::default();
    let sig = s.evaluate_entry(&ctx).expect("z=3 should fire");
    assert_eq!(sig.side, Direction::Short);
}

#[test]
fn etf_pair_force_close_on_extreme_z() {
    let mut ctx = base_ctx();
    ctx.companion = Some(CompanionSnapshot {
        symbol: "SPY".into(),
        price: 400.0,
        ratio_mean: 0.50,
        ratio_sd: 0.001,
        ratio_z: 4.0,
    });
    let s = etf_pair::EtfPair::default();
    let pos = synth_position("XLK", Direction::Short, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).expect("force close");
    assert_eq!(exit.reason, "etf_pair_force_close");
}

#[test]
fn etf_pair_revert_exit() {
    let mut ctx = base_ctx();
    ctx.companion = Some(CompanionSnapshot {
        symbol: "SPY".into(),
        price: 400.0,
        ratio_mean: 0.50,
        ratio_sd: 0.001,
        ratio_z: 0.2,
    });
    let s = etf_pair::EtfPair::default();
    let pos = synth_position("XLK", Direction::Short, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).expect("revert exit");
    assert_eq!(exit.reason, "etf_pair_revert");
}

// ──────────────────────────────────────────────────────────────────
//  Afternoon Fade
// ──────────────────────────────────────────────────────────────────

#[test]
fn afternoon_fade_refuses_outside_window() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 10 * 60;
    let s = afternoon_fade::AfternoonFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn afternoon_fade_refuses_no_anchor_move() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 15 * 60 + 5;
    // candles all 100 → no move
    let s = afternoon_fade::AfternoonFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn afternoon_fade_fires_on_uptrend() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 15 * 60 + 5;
    let mut prices = vec![100.0; 24];
    prices[12] = 100.0;
    *prices.last_mut().unwrap() = 102.5; // last bar +2.5% above anchor (idx 12)
    ctx.candles = synth_candles(&prices);
    ctx.price = 102.5;
    let s = afternoon_fade::AfternoonFade::default();
    let sig = s.evaluate_entry(&ctx).expect("should fade up");
    assert_eq!(sig.side, Direction::Short);
}

#[test]
fn afternoon_fade_force_close_at_1555() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 15 * 60 + 55;
    let s = afternoon_fade::AfternoonFade::default();
    let pos = synth_position("AAPL", Direction::Short, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).expect("close at 15:55");
    assert_eq!(exit.reason, "afternoon_fade_close");
}

// ──────────────────────────────────────────────────────────────────
//  Stat Arb 50
// ──────────────────────────────────────────────────────────────────

#[test]
fn stat_arb_refuses_non_primary_symbol() {
    let mut ctx = base_ctx();
    ctx.symbol = "AAPL".into(); // not in PRIMARY_SYMBOLS
    ctx.companion = Some(CompanionSnapshot {
        symbol: "MSFT".into(),
        price: 400.0,
        ratio_mean: 0.50,
        ratio_sd: 0.002,
        ratio_z: 3.0,
    });
    let s = stat_arb_50::StatArb50::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn stat_arb_fires_on_xlk() {
    let mut ctx = base_ctx();
    ctx.symbol = "XLK".into();
    ctx.companion = Some(CompanionSnapshot {
        symbol: "XLF".into(),
        price: 35.0,
        ratio_mean: 0.50,
        ratio_sd: 0.002,
        ratio_z: 3.0,
    });
    let s = stat_arb_50::StatArb50::default();
    let sig = s.evaluate_entry(&ctx).expect("XLK z=3 should fire");
    assert_eq!(sig.side, Direction::Short);
}

#[test]
fn stat_arb_refuses_wide_spread() {
    let mut ctx = base_ctx();
    ctx.symbol = "XLK".into();
    ctx.spread_bps = 100.0; // > max 30
    ctx.companion = Some(CompanionSnapshot {
        symbol: "XLF".into(),
        price: 35.0,
        ratio_mean: 0.50,
        ratio_sd: 0.002,
        ratio_z: 3.0,
    });
    let s = stat_arb_50::StatArb50::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

// ──────────────────────────────────────────────────────────────────
//  Leveraged ETF
// ──────────────────────────────────────────────────────────────────

#[test]
fn leveraged_etf_refuses_non_target_symbol() {
    let mut ctx = base_ctx();
    ctx.symbol = "AAPL".into();
    ctx.now_et_min = 12 * 60;
    let s = leveraged_etf::LeveragedEtf::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn leveraged_etf_refuses_panic_regime() {
    let mut ctx = base_ctx();
    ctx.symbol = "TQQQ".into();
    ctx.now_et_min = 12 * 60;
    ctx.regime = Some(MarketRegime::Panic);
    let s = leveraged_etf::LeveragedEtf::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn leveraged_etf_fires_on_deviation() {
    let mut ctx = base_ctx();
    ctx.symbol = "TQQQ".into();
    ctx.now_et_min = 12 * 60;
    let mut prices = vec![100.0; 60];
    prices[59] = 102.0; // last is +2% above 50-bar mean (~100)
    ctx.candles = synth_candles(&prices);
    ctx.price = 102.0;
    ctx.rvol = 2.0;
    let s = leveraged_etf::LeveragedEtf::default();
    let sig = s.evaluate_entry(&ctx).expect("dev > 1.5% should fire");
    assert_eq!(sig.side, Direction::Short);
}

// ──────────────────────────────────────────────────────────────────
//  Catalyst Front-Run
// ──────────────────────────────────────────────────────────────────

#[test]
fn catalyst_frontrun_refuses_low_score() {
    let mut ctx = base_ctx();
    ctx.catalyst_score = Some(50.0);
    ctx.gap_pct = 3.0;
    let s = catalyst_frontrun::CatalystFrontRun::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn catalyst_frontrun_fires_on_high_score_gap_up() {
    let mut ctx = base_ctx();
    ctx.catalyst_score = Some(85.0);
    ctx.gap_pct = 4.0;
    let s = catalyst_frontrun::CatalystFrontRun::default();
    let sig = s.evaluate_entry(&ctx).expect("high score should fire");
    assert_eq!(sig.side, Direction::Long);
}

#[test]
fn catalyst_frontrun_5min_time_stop() {
    let ctx = base_ctx();
    let s = catalyst_frontrun::CatalystFrontRun::default();
    let now = chrono::Utc::now().timestamp();
    let pos = synth_position("AAPL", Direction::Long, now - 360); // 6 min ago
    let exit = s.evaluate_exit(&pos, &ctx).expect("should time-stop");
    assert_eq!(exit.reason, "catalyst_frontrun_max_hold");
}

// ──────────────────────────────────────────────────────────────────
//  Halt Resume
// ──────────────────────────────────────────────────────────────────

#[test]
fn halt_resume_refuses_when_currently_halted() {
    let mut ctx = base_ctx();
    ctx.is_halted = true;
    let s = halt_resume::HaltResume::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn halt_resume_refuses_no_pre_halt_move() {
    let mut ctx = base_ctx();
    ctx.is_halted = false;
    let s = halt_resume::HaltResume::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn halt_resume_fires_after_volatility_spike() {
    let mut ctx = base_ctx();
    // Need ≥ 10 bars for ctx_is_sane; pad calm prefix then volatility spike
    let prices = vec![
        100.0, 100.0, 100.0, 100.0, 100.0, 101.0, 100.0, 110.0, 113.0, 115.0,
    ];
    ctx.candles = synth_candles(&prices);
    ctx.price = 115.0;
    ctx.is_halted = false;
    let s = halt_resume::HaltResume::default();
    let sig = s.evaluate_entry(&ctx).expect("post-halt resume should fire");
    assert_eq!(sig.side, Direction::Short);
    assert!(sig.limit_price.is_some(), "halt resume uses limit orders");
}

// ──────────────────────────────────────────────────────────────────
//  Extended News
// ──────────────────────────────────────────────────────────────────

#[test]
fn extended_news_refuses_during_rth() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 12 * 60; // RTH
    let s = extended_news::ExtendedNews::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn extended_news_fires_premarket() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 8 * 60; // premarket
    ctx.catalyst_score = Some(80.0);
    let mut prices = vec![100.0; 10];
    prices[9] = 103.0; // +3% from 5 bars back
    ctx.candles = synth_candles(&prices);
    ctx.price = 103.0;
    let s = extended_news::ExtendedNews::default();
    let sig = s.evaluate_entry(&ctx).expect("premarket news should fire");
    assert_eq!(sig.side, Direction::Long);
}

#[test]
fn extended_news_refuses_low_price_stocks() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 8 * 60;
    ctx.price = 2.0; // < min_price 5
    ctx.catalyst_score = Some(80.0);
    let s = extended_news::ExtendedNews::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

// ──────────────────────────────────────────────────────────────────
//  Opening Range Breakout
// ──────────────────────────────────────────────────────────────────

/// Anchor candles to a real ET RTH day so `compute_opening_range` and
/// `compute_session_vwap` find the bars in the correct session bucket.
/// Returns (start_ts, candles). 2026-06-15 is a Monday in EDT (UTC-4).
fn orb_synth_session(prices: &[f64], step_secs: i64) -> (i64, Vec<Candle>) {
    use chrono::TimeZone;
    let start_et = chrono_tz::US::Eastern
        .with_ymd_and_hms(2026, 6, 15, 9, 30, 0)
        .single()
        .expect("valid ET datetime");
    let start_ts = start_et.timestamp();
    let candles: Vec<Candle> = prices
        .iter()
        .enumerate()
        .map(|(i, &p)| Candle {
            time: start_ts + (i as i64) * step_secs,
            open: p,
            high: p * 1.002,
            low: p * 0.998,
            close: p,
            volume: 100_000.0,
        })
        .collect();
    (start_ts, candles)
}

#[test]
fn orb_compute_opening_range_basic() {
    // 15 one-minute bars starting at 09:30 ET. Bars 0-14 (09:30-09:44)
    // are inside the OR; the 16th bar (09:45) is outside.
    let prices: Vec<f64> = (0..16).map(|i| 100.0 + (i as f64) * 0.1).collect();
    let (_, candles) = orb_synth_session(&prices, 60);
    let or = opening_range_breakout::compute_opening_range(&candles, 15)
        .expect("OR present");
    assert_eq!(or.bars, 15);
    // OR high == high of bar 14 = (100 + 14*0.1) * 1.002 = 101.4 * 1.002
    assert!((or.high - 101.4 * 1.002).abs() < 1e-6, "got {}", or.high);
    // OR low == low of bar 0 = 100.0 * 0.998
    assert!((or.low - 100.0 * 0.998).abs() < 1e-6, "got {}", or.low);
}

#[test]
fn orb_compute_opening_range_returns_none_outside_session() {
    // Synthetic candles with epoch-based time (not in any ET date).
    let candles = synth_candles(&[100.0; 30]);
    assert!(opening_range_breakout::compute_opening_range(&candles, 15).is_none());
}

#[test]
fn orb_session_vwap_matches_constant_close() {
    let prices = vec![100.0; 30];
    let (_, candles) = orb_synth_session(&prices, 60);
    let vwap = opening_range_breakout::compute_session_vwap(&candles)
        .expect("vwap present");
    // typical = (h+l+c)/3 = (100.2 + 99.8 + 100.0)/3 ≈ 100.0
    assert!((vwap - 100.0).abs() < 0.05, "vwap = {vwap}");
}

#[test]
fn orb_fires_on_long_breakout() {
    // Flat OR at 100, then post-OR price pushes to 101 with VWAP < price.
    let mut prices: Vec<f64> = (0..16).map(|_| 100.0).collect();
    prices.push(100.5); // 09:46
    prices.push(101.0); // 09:47 — breakout bar
    let (_, candles) = orb_synth_session(&prices, 60);

    let mut ctx = base_ctx();
    ctx.candles = candles;
    ctx.timeframe = "1m";
    ctx.price = 101.0;
    ctx.atr = 0.3; // OR width ≈ 0.4 → width/atr = 1.33 (within bounds)
    ctx.rvol = 2.5; // ≥ min_rvol = 2.0
    ctx.spread_bps = 5.0;
    ctx.now_et_min = 9 * 60 + 47;
    let s = opening_range_breakout::OpeningRangeBreakout::default();
    let sig = s.evaluate_entry(&ctx).expect("ORB should fire long");
    assert_eq!(sig.side, Direction::Long);
    assert!(sig.stop_loss < ctx.price, "stop below entry");
    assert!(sig.take_profit > ctx.price, "target above entry");
    let risk = ctx.price - sig.stop_loss;
    let reward = sig.take_profit - ctx.price;
    assert!((reward - 3.0 * risk).abs() < 1e-6, "R:R should be ~3.0");
}

#[test]
fn orb_refuses_low_rvol() {
    let mut prices: Vec<f64> = (0..16).map(|_| 100.0).collect();
    prices.push(101.0);
    let (_, candles) = orb_synth_session(&prices, 60);
    let mut ctx = base_ctx();
    ctx.candles = candles;
    ctx.timeframe = "1m";
    ctx.price = 101.0;
    ctx.atr = 0.3;
    ctx.rvol = 0.8; // < min_rvol = 1.5
    ctx.now_et_min = 9 * 60 + 47;
    let s = opening_range_breakout::OpeningRangeBreakout::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn orb_refuses_before_window_close() {
    let mut prices: Vec<f64> = (0..10).map(|_| 100.0).collect();
    prices.push(101.0);
    let (_, candles) = orb_synth_session(&prices, 60);
    let mut ctx = base_ctx();
    ctx.candles = candles;
    ctx.timeframe = "1m";
    ctx.price = 101.0;
    ctx.atr = 0.3;
    ctx.rvol = 2.5;
    // 09:40 — still inside the 15-min OR window
    ctx.now_et_min = 9 * 60 + 40;
    let s = opening_range_breakout::OpeningRangeBreakout::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn orb_refuses_after_cutoff() {
    let mut prices: Vec<f64> = (0..16).map(|_| 100.0).collect();
    prices.push(101.0);
    let (_, candles) = orb_synth_session(&prices, 60);
    let mut ctx = base_ctx();
    ctx.candles = candles;
    ctx.timeframe = "1m";
    ctx.price = 101.0;
    ctx.atr = 0.3;
    ctx.rvol = 2.5;
    // 12:00 ET — past the 11:30 cutoff
    ctx.now_et_min = 12 * 60;
    let s = opening_range_breakout::OpeningRangeBreakout::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn orb_exit_flat_by_close() {
    let prices = vec![100.0; 20];
    let (_, candles) = orb_synth_session(&prices, 60);
    let mut ctx = base_ctx();
    ctx.candles = candles;
    ctx.price = 100.0;
    ctx.now_et_min = 15 * 60 + 56; // past 15:55 force-flat
    let s = opening_range_breakout::OpeningRangeBreakout::default();
    let now = chrono::Utc::now().timestamp();
    let pos = synth_position("TEST", Direction::Long, now - 600);
    let exit = s.evaluate_exit(&pos, &ctx).expect("must flat by close");
    assert_eq!(exit.reason, "orb_flat_by_close");
}

#[test]
fn orb_exit_max_hold() {
    // Use a 09:35 ET context so neither flat-by-close nor VWAP-lost
    // fire — only the per-position max-hold rail should trip.
    let prices = vec![100.0; 20];
    let (_, candles) = orb_synth_session(&prices, 60);
    let mut ctx = base_ctx();
    ctx.candles = candles;
    ctx.price = 100.5; // above flat VWAP, so vwap-lost does not fire
    ctx.now_et_min = 9 * 60 + 35;
    let s = opening_range_breakout::OpeningRangeBreakout::default();
    let now = chrono::Utc::now().timestamp();
    // Position opened 4h ago — well past default 90-min max hold.
    let pos = synth_position("TEST", Direction::Long, now - 4 * 3600);
    let exit = s.evaluate_exit(&pos, &ctx).expect("must max-hold");
    assert_eq!(exit.reason, "orb_max_hold");
}
