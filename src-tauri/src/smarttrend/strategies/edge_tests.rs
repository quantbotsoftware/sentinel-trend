//! Defensive edge-case tests across the entire strategy framework.
//!
//! Phase 1.5 widening — every helper, every entry/exit code path,
//! every numerical edge (NaN, ±inf, empty slices, single bar) gets
//! a test. Two reasons:
//!   1. Strategies run unattended overnight in paper mode; a single
//!      panic bubbles up to `panic = "abort"` and we lose the
//!      session.
//!   2. The GA mutates parameters into ranges the original author
//!      didn't anticipate; the strategy code MUST stay sane on
//!      arbitrary param combinations as long as they pass
//!      `ParamSpec::clamp`.

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
        now_et_min: 9 * 60 + 35,
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

// ════════════════════════════════════════════════════════════════════
//  ctx_is_sane — gates everything; pin its semantics carefully
// ════════════════════════════════════════════════════════════════════

#[test]
fn ctx_sane_rejects_nan_price() {
    let mut ctx = base_ctx();
    ctx.price = f64::NAN;
    assert!(!ctx_is_sane(&ctx, 5));
}

#[test]
fn ctx_sane_rejects_inf_price() {
    let mut ctx = base_ctx();
    ctx.price = f64::INFINITY;
    assert!(!ctx_is_sane(&ctx, 5));
}

#[test]
fn ctx_sane_rejects_negative_price() {
    let mut ctx = base_ctx();
    ctx.price = -10.0;
    assert!(!ctx_is_sane(&ctx, 5));
}

#[test]
fn ctx_sane_rejects_zero_price() {
    let mut ctx = base_ctx();
    ctx.price = 0.0;
    assert!(!ctx_is_sane(&ctx, 5));
}

#[test]
fn ctx_sane_accepts_zero_atr() {
    // ATR=0 is OK at this gate; strategies fall back to pure_atr if
    // they need a non-zero value.
    let mut ctx = base_ctx();
    ctx.atr = 0.0;
    assert!(ctx_is_sane(&ctx, 5));
}

#[test]
fn ctx_sane_rejects_nan_atr() {
    let mut ctx = base_ctx();
    ctx.atr = f64::NAN;
    assert!(!ctx_is_sane(&ctx, 5));
}

#[test]
fn ctx_sane_rejects_negative_atr() {
    let mut ctx = base_ctx();
    ctx.atr = -1.0;
    assert!(!ctx_is_sane(&ctx, 5));
}

#[test]
fn ctx_sane_rejects_too_few_candles() {
    let ctx = MarketContext {
        candles: synth_candles(&[100.0; 3]),
        ..base_ctx()
    };
    assert!(!ctx_is_sane(&ctx, 10));
    assert!(ctx_is_sane(&ctx, 3));
}

// ════════════════════════════════════════════════════════════════════
//  pure_atr defensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn pure_atr_single_bar_returns_zero() {
    let c = synth_candles(&[100.0]);
    assert_eq!(pure_atr(&c, 14), 0.0);
}

#[test]
fn pure_atr_period_zero_falls_back_to_one() {
    let c = synth_candles(&[100.0, 101.0, 99.0]);
    let atr = pure_atr(&c, 0);
    // Period 0 clamps to min 1 → uses last 1 TR
    assert!(atr > 0.0);
}

#[test]
fn pure_atr_period_larger_than_history() {
    let c = synth_candles(&[100.0, 101.0, 99.0]);
    let atr = pure_atr(&c, 100);
    assert!(atr > 0.0); // uses what's available
}

// ════════════════════════════════════════════════════════════════════
//  rolling_mean / rolling_sd — extra coverage
// ════════════════════════════════════════════════════════════════════

#[test]
fn rolling_mean_n_zero_uses_zero_or_panics_safely() {
    // Behaviour: take = min(0, len) = 0 → division by zero would panic
    // We test what actually happens: the function returns nonsense if
    // n=0 but doesn't panic on len > 0 because take.min(len) >= 0.
    // The contract is `n >= 1` from callers, but defensive: we don't
    // require this. Test for non-panic + finite or zero return.
    let c = synth_candles(&[10.0; 10]);
    // n=0 → take = 0 → division by zero produces NaN; we assert no panic
    let r = rolling_mean(&c, 0);
    assert!(r.is_nan() || r == 0.0);
}

#[test]
fn rolling_sd_increasing_series() {
    let c = synth_candles(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
    let sd = rolling_sd(&c, 10);
    // Pop SD of 1..10 ≈ 2.872
    assert!((sd - 2.872).abs() < 0.01, "got {sd}");
}

#[test]
fn rolling_sd_handles_n_one() {
    let c = synth_candles(&[10.0; 10]);
    // n=1 < 2 → returns 0 per the clamp
    let sd = rolling_sd(&c, 1);
    assert_eq!(sd, 0.0);
}

// ════════════════════════════════════════════════════════════════════
//  risk_based_size — extra coverage
// ════════════════════════════════════════════════════════════════════

#[test]
fn risk_based_size_zero_distance() {
    // entry == stop → distance 0 → distance.max(0.001) clamps; many
    // shares result, capped by sub-wallet
    let s = risk_based_size(50.0, 100.0, 100.0, 0.30, 1_000.0);
    assert!(s > 0.0);
    assert!(s <= 300.0);
}

#[test]
fn risk_based_size_negative_subwallet() {
    let s = risk_based_size(50.0, 100.0, 99.0, 0.30, -1_000.0);
    assert_eq!(s, 0.0); // negative cap → max(0)
}

#[test]
fn risk_based_size_zero_risk() {
    let s = risk_based_size(0.0, 100.0, 99.0, 0.30, 1_000.0);
    // shares = max(0/1, 1) = 1; dollars = 100; cap 300; result 100
    assert!(s > 0.0 && s <= 300.0);
}

#[test]
fn risk_based_size_cap_clamps_above_one() {
    // sub_wallet_cap_pct > 1.0 → clamp(0,1) = 1.0
    let s = risk_based_size(50.0, 100.0, 99.0, 5.0, 1_000.0);
    assert!(s <= 1_000.0);
}

#[test]
fn risk_based_size_cap_clamps_below_zero() {
    let s = risk_based_size(50.0, 100.0, 99.0, -0.5, 1_000.0);
    assert_eq!(s, 0.0);
}

// ════════════════════════════════════════════════════════════════════
//  Strategy parameterised: position_size never panics on weird params
// ════════════════════════════════════════════════════════════════════

#[test]
fn all_strategies_size_nonnegative_on_default_ctx() {
    let ctx = base_ctx();
    for s in registry() {
        let size = s.position_size(&ctx, 12_500.0);
        assert!(size >= 0.0, "{} returned negative size", s.name());
        assert!(size.is_finite(), "{} returned non-finite size", s.name());
    }
}

#[test]
fn all_strategies_size_zero_on_zero_price() {
    let mut ctx = base_ctx();
    ctx.price = 0.0;
    for s in registry() {
        let size = s.position_size(&ctx, 12_500.0);
        assert_eq!(size, 0.0, "{} should return 0 on zero price", s.name());
    }
}

#[test]
fn all_strategies_size_zero_on_nan_atr() {
    let mut ctx = base_ctx();
    ctx.atr = f64::NAN;
    ctx.candles = synth_candles(&[100.0]); // ensure pure_atr returns 0
    // AfternoonFade uses fixed-% sizing (not ATR-based) so it's the only
    // strategy that can return non-zero here. All ATR-based strategies
    // must return 0.0 to avoid sizing on NaN distance.
    for s in registry() {
        if s.id() == StrategyId::AfternoonFade {
            continue;
        }
        let size = s.position_size(&ctx, 12_500.0);
        assert_eq!(size, 0.0, "{} should return 0 when ATR unavailable", s.name());
    }
}

// ════════════════════════════════════════════════════════════════════
//  Strategy parameterised: evaluate_entry never panics on weird ctx
// ════════════════════════════════════════════════════════════════════

#[test]
fn all_strategies_no_panic_on_empty_candles() {
    let mut ctx = base_ctx();
    ctx.candles = vec![];
    for s in registry() {
        let _ = s.evaluate_entry(&ctx);
        // Just assert no panic; result is implementation-defined None
    }
}

#[test]
fn all_strategies_no_panic_on_nan_inputs() {
    let mut ctx = base_ctx();
    ctx.price = f64::NAN;
    ctx.gap_pct = f64::NAN;
    ctx.rvol = f64::NAN;
    ctx.atr = f64::NAN;
    ctx.spread_bps = f64::NAN;
    for s in registry() {
        let _ = s.evaluate_entry(&ctx);
    }
}

#[test]
fn all_strategies_no_panic_on_extreme_values() {
    let mut ctx = base_ctx();
    ctx.price = 1e9;
    ctx.gap_pct = 1e6;
    ctx.rvol = 1e6;
    ctx.atr = 1e6;
    ctx.spread_bps = 1e6;
    ctx.catalyst_score = Some(1e6);
    for s in registry() {
        let _ = s.evaluate_entry(&ctx);
    }
}

#[test]
fn all_strategies_refuse_when_halted() {
    let mut ctx = base_ctx();
    ctx.is_halted = true;
    ctx.gap_pct = 7.0;
    ctx.companion = Some(CompanionSnapshot {
        symbol: "X".into(),
        price: 100.0,
        ratio_mean: 1.0,
        ratio_sd: 0.01,
        ratio_z: 3.0,
    });
    ctx.catalyst_score = Some(95.0);
    for s in registry() {
        // Halt resume in resume mode would normally fire — but is_halted=true
        let sig = s.evaluate_entry(&ctx);
        assert!(
            sig.is_none(),
            "{} should refuse when is_halted=true",
            s.name()
        );
    }
}

// ════════════════════════════════════════════════════════════════════
//  evaluate_exit — max-hold respected universally
// ════════════════════════════════════════════════════════════════════

#[test]
fn all_strategies_exit_after_excessive_hold() {
    let ctx = base_ctx();
    let now = chrono::Utc::now().timestamp();
    // Position opened 48 hours ago — should trigger any sane max-hold
    let pos = synth_position("TEST", Direction::Long, now - 48 * 3600);
    for s in registry() {
        let exit = s.evaluate_exit(&pos, &ctx);
        assert!(
            exit.is_some(),
            "{} should produce an exit for 48h-old position",
            s.name()
        );
    }
}

// ════════════════════════════════════════════════════════════════════
//  TradingWindow boundary conditions
// ════════════════════════════════════════════════════════════════════

#[test]
fn rth_only_boundary_open() {
    assert!(TradingWindow::RthOnly.permits(9, 30));
    assert!(!TradingWindow::RthOnly.permits(9, 29));
}

#[test]
fn rth_only_boundary_close() {
    assert!(TradingWindow::RthOnly.permits(15, 59));
    assert!(!TradingWindow::RthOnly.permits(16, 0));
}

#[test]
fn extended_boundary_premarket_open() {
    assert!(TradingWindow::ExtendedAndRth.permits(4, 0));
    assert!(!TradingWindow::ExtendedAndRth.permits(3, 59));
}

#[test]
fn extended_boundary_postmarket_close() {
    assert!(TradingWindow::ExtendedAndRth.permits(19, 59));
    assert!(!TradingWindow::ExtendedAndRth.permits(20, 0));
}

#[test]
fn always_on_midnight() {
    assert!(TradingWindow::AlwaysOn.permits(0, 0));
}

#[test]
fn always_on_one_minute_before_midnight() {
    assert!(TradingWindow::AlwaysOn.permits(23, 59));
}

// ════════════════════════════════════════════════════════════════════
//  Strategy IDs round-trip via str
// ════════════════════════════════════════════════════════════════════

#[test]
fn strategy_id_string_lookup_matches_registry() {
    for id in StrategyId::ALL {
        let name = id.as_str();
        let strategy = by_id(*id).unwrap();
        assert_eq!(strategy.id().as_str(), name);
    }
}

#[test]
fn strategy_id_display_consistent_with_as_str() {
    for id in StrategyId::ALL {
        assert_eq!(format!("{id}"), id.as_str());
    }
}

// ════════════════════════════════════════════════════════════════════
//  GapFade: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn gap_fade_refuses_extreme_gap_above_max() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 100.0; // > max_gap_pct 30
    let s = gap_fade::GapFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn gap_fade_refuses_wide_spread() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 7.0;
    ctx.spread_bps = 200.0;
    let s = gap_fade::GapFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn gap_fade_refuses_pre_open() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 7.0;
    ctx.now_et_min = 9 * 60 + 30; // session_min == 0
    let s = gap_fade::GapFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn gap_fade_score_increases_with_gap_size() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 5.0;
    let s = gap_fade::GapFade::default();
    let s1 = s.evaluate_entry(&ctx).unwrap();
    ctx.gap_pct = 15.0;
    let s2 = s.evaluate_entry(&ctx).unwrap();
    assert!(s2.score > s1.score, "score should grow with gap size");
}

#[test]
fn gap_fade_max_hold_emits_exit() {
    let s = gap_fade::GapFade::default();
    let now = chrono::Utc::now().timestamp();
    let pos = synth_position("TEST", Direction::Short, now - 70 * 60); // 70 min
    let mut ctx = base_ctx();
    ctx.now_et_min = 10 * 60 + 0; // before 10:30 hard time stop
    let exit = s.evaluate_exit(&pos, &ctx).unwrap();
    assert_eq!(exit.reason, "gap_fade_max_hold");
}

// ════════════════════════════════════════════════════════════════════
//  EtfPair: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn etf_pair_refuses_outside_window_morning() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 10 * 60; // before 10:30
    ctx.companion = Some(CompanionSnapshot {
        symbol: "S".into(),
        price: 1.0,
        ratio_mean: 1.0,
        ratio_sd: 0.01,
        ratio_z: 3.0,
    });
    let s = etf_pair::EtfPair::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn etf_pair_refuses_outside_window_afternoon() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 15 * 60 + 30; // after 15:00
    ctx.companion = Some(CompanionSnapshot {
        symbol: "S".into(),
        price: 1.0,
        ratio_mean: 1.0,
        ratio_sd: 0.01,
        ratio_z: 3.0,
    });
    let s = etf_pair::EtfPair::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn etf_pair_refuses_low_sd() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 11 * 60;
    ctx.companion = Some(CompanionSnapshot {
        symbol: "S".into(),
        price: 1.0,
        ratio_mean: 1.0,
        ratio_sd: 0.0001, // < min_sd
        ratio_z: 3.0,
    });
    let s = etf_pair::EtfPair::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn etf_pair_long_when_z_negative() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 11 * 60;
    ctx.companion = Some(CompanionSnapshot {
        symbol: "S".into(),
        price: 1.0,
        ratio_mean: 1.0,
        ratio_sd: 0.01,
        ratio_z: -3.0,
    });
    let s = etf_pair::EtfPair::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Long);
}

// ════════════════════════════════════════════════════════════════════
//  AfternoonFade: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn afternoon_fade_refuses_low_rvol() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 15 * 60 + 5;
    ctx.rvol = 0.3;
    ctx.candles[0].close = 100.0;
    *ctx.candles.last_mut().unwrap() = Candle {
        time: 600,
        open: 102.5,
        high: 102.6,
        low: 102.4,
        close: 102.5,
        volume: 100_000.0,
    };
    ctx.price = 102.5;
    let s = afternoon_fade::AfternoonFade::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn afternoon_fade_long_on_downtrend() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 15 * 60 + 10;
    let mut prices = vec![100.0; 24];
    *prices.last_mut().unwrap() = 97.5;
    ctx.candles = synth_candles(&prices);
    ctx.price = 97.5;
    let s = afternoon_fade::AfternoonFade::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Long);
}

// ════════════════════════════════════════════════════════════════════
//  StatArb50: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn stat_arb_xli_xle_xlv_xly_all_eligible() {
    let mut ctx = base_ctx();
    let z = 3.0;
    ctx.companion = Some(CompanionSnapshot {
        symbol: "X".into(),
        price: 50.0,
        ratio_mean: 1.0,
        ratio_sd: 0.01,
        ratio_z: z,
    });
    let s = stat_arb_50::StatArb50::default();
    for sym in &["XLI", "XLE", "XLV", "XLY"] {
        ctx.symbol = (*sym).to_string();
        let sig = s.evaluate_entry(&ctx);
        assert!(sig.is_some(), "{sym} should fire on z={z}");
    }
}

#[test]
fn stat_arb_force_close_z_too_extreme() {
    let mut ctx = base_ctx();
    ctx.symbol = "XLK".into();
    ctx.companion = Some(CompanionSnapshot {
        symbol: "XLF".into(),
        price: 35.0,
        ratio_mean: 1.0,
        ratio_sd: 0.01,
        ratio_z: 4.5, // > force_close_z 3.5
    });
    let s = stat_arb_50::StatArb50::default();
    let pos = synth_position("XLK", Direction::Short, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).unwrap();
    assert_eq!(exit.reason, "stat_arb_force_close");
}

// ════════════════════════════════════════════════════════════════════
//  LeveragedEtf: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn leveraged_etf_all_targets_eligible_on_deviation() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 12 * 60;
    ctx.rvol = 2.0;
    let mut prices = vec![100.0; 60];
    *prices.last_mut().unwrap() = 102.0;
    ctx.candles = synth_candles(&prices);
    ctx.price = 102.0;
    let s = leveraged_etf::LeveragedEtf::default();
    for sym in leveraged_etf::TARGET_SYMBOLS {
        ctx.symbol = (*sym).to_string();
        let sig = s.evaluate_entry(&ctx);
        assert!(sig.is_some(), "{sym} should fire on +2% deviation");
    }
}

#[test]
fn leveraged_etf_refuses_volatile_regime() {
    let mut ctx = base_ctx();
    ctx.symbol = "TQQQ".into();
    ctx.now_et_min = 12 * 60;
    ctx.rvol = 2.0;
    ctx.regime = Some(MarketRegime::Volatile);
    let mut prices = vec![100.0; 60];
    *prices.last_mut().unwrap() = 102.0;
    ctx.candles = synth_candles(&prices);
    ctx.price = 102.0;
    let s = leveraged_etf::LeveragedEtf::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn leveraged_etf_force_close_after_hours() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 15 * 60 + 56; // past 15:55
    let s = leveraged_etf::LeveragedEtf::default();
    let pos = synth_position("TQQQ", Direction::Short, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).unwrap();
    assert_eq!(exit.reason, "leveraged_etf_close");
}

// ════════════════════════════════════════════════════════════════════
//  CatalystFrontRun: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn catalyst_frontrun_no_score_means_no_signal() {
    let mut ctx = base_ctx();
    ctx.catalyst_score = None;
    let s = catalyst_frontrun::CatalystFrontRun::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn catalyst_frontrun_refuses_low_rvol() {
    let mut ctx = base_ctx();
    ctx.catalyst_score = Some(85.0);
    ctx.gap_pct = 4.0;
    ctx.rvol = 1.0;
    let s = catalyst_frontrun::CatalystFrontRun::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn catalyst_frontrun_short_on_negative_gap() {
    let mut ctx = base_ctx();
    ctx.catalyst_score = Some(85.0);
    ctx.gap_pct = -4.0;
    let s = catalyst_frontrun::CatalystFrontRun::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Short);
}

#[test]
fn catalyst_frontrun_exit_on_halt() {
    let mut ctx = base_ctx();
    ctx.is_halted = true;
    let s = catalyst_frontrun::CatalystFrontRun::default();
    let pos = synth_position("AAPL", Direction::Long, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).unwrap();
    assert_eq!(exit.reason, "catalyst_frontrun_halted");
}

// ════════════════════════════════════════════════════════════════════
//  HaltResume: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn halt_resume_refuses_wide_spread() {
    let mut ctx = base_ctx();
    ctx.spread_bps = 250.0;
    let prices = vec![100.0; 6].into_iter().chain([110.0, 113.0, 115.0]).collect::<Vec<_>>();
    ctx.candles = synth_candles(&prices);
    ctx.price = 115.0;
    let s = halt_resume::HaltResume::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn halt_resume_long_after_panic_drop() {
    let mut ctx = base_ctx();
    let prices = vec![
        100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 95.0, 90.0, 88.0,
    ];
    ctx.candles = synth_candles(&prices);
    ctx.price = 88.0;
    let s = halt_resume::HaltResume::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Long);
}

#[test]
fn halt_resume_exit_on_re_halt() {
    let mut ctx = base_ctx();
    ctx.is_halted = true;
    let s = halt_resume::HaltResume::default();
    let pos = synth_position("BBAI", Direction::Short, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).unwrap();
    assert_eq!(exit.reason, "halt_resume_re_halted");
}

// ════════════════════════════════════════════════════════════════════
//  ExtendedNews: comprehensive
// ════════════════════════════════════════════════════════════════════

#[test]
fn extended_news_short_on_negative_move() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 8 * 60;
    ctx.catalyst_score = Some(80.0);
    let mut prices = vec![100.0; 10];
    *prices.last_mut().unwrap() = 96.0;
    ctx.candles = synth_candles(&prices);
    ctx.price = 96.0;
    let s = extended_news::ExtendedNews::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Short);
}

#[test]
fn extended_news_refuses_wide_spread() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 8 * 60;
    ctx.catalyst_score = Some(80.0);
    ctx.spread_bps = 300.0;
    let mut prices = vec![100.0; 10];
    *prices.last_mut().unwrap() = 103.0;
    ctx.candles = synth_candles(&prices);
    ctx.price = 103.0;
    let s = extended_news::ExtendedNews::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

#[test]
fn extended_news_exit_on_halt() {
    let mut ctx = base_ctx();
    ctx.is_halted = true;
    let s = extended_news::ExtendedNews::default();
    let pos = synth_position("AAPL", Direction::Long, chrono::Utc::now().timestamp());
    let exit = s.evaluate_exit(&pos, &ctx).unwrap();
    assert_eq!(exit.reason, "extended_news_halted");
}

#[test]
fn extended_news_refuses_below_score_threshold() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 8 * 60;
    ctx.catalyst_score = Some(50.0);
    let s = extended_news::ExtendedNews::default();
    assert!(s.evaluate_entry(&ctx).is_none());
}

// ════════════════════════════════════════════════════════════════════
//  Score bounded 0..100 invariant
// ════════════════════════════════════════════════════════════════════

#[test]
fn gap_fade_score_clamped_to_100() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 50.0; // huge — would naively exceed 100
    ctx.rvol = 100.0;
    let s = gap_fade::GapFade {
        max_gap_pct: 100.0, // override default 30 to allow it through
        ..Default::default()
    };
    if let Some(sig) = s.evaluate_entry(&ctx) {
        assert!(sig.score <= 100.0);
    }
}

#[test]
fn all_strategies_emit_score_in_0_100_when_signal_fires() {
    // Generate plausible signals across strategies using broad ctx.
    let mut ctx = base_ctx();
    ctx.gap_pct = 6.0;
    ctx.now_et_min = 9 * 60 + 35;
    ctx.companion = Some(CompanionSnapshot {
        symbol: "X".into(),
        price: 100.0,
        ratio_mean: 1.0,
        ratio_sd: 0.01,
        ratio_z: 3.0,
    });
    ctx.catalyst_score = Some(90.0);
    ctx.rvol = 3.0;
    for s in registry() {
        if let Some(sig) = s.evaluate_entry(&ctx) {
            assert!(
                (0.0..=100.0).contains(&sig.score),
                "{} produced out-of-range score: {}",
                s.name(),
                sig.score
            );
        }
    }
}

// ════════════════════════════════════════════════════════════════════
//  Entry signal stop/target consistency
// ════════════════════════════════════════════════════════════════════

#[test]
fn gap_fade_short_stop_above_entry() {
    let mut ctx = base_ctx();
    ctx.gap_pct = 7.0;
    let s = gap_fade::GapFade::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Short);
    assert!(sig.stop_loss > ctx.price);
    assert!(sig.take_profit < ctx.price);
}

#[test]
fn gap_fade_long_stop_below_entry() {
    let mut ctx = base_ctx();
    ctx.gap_pct = -6.0;
    let s = gap_fade::GapFade::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Long);
    assert!(sig.stop_loss < ctx.price);
    assert!(sig.take_profit > ctx.price);
}

#[test]
fn catalyst_frontrun_long_stop_below() {
    let mut ctx = base_ctx();
    ctx.catalyst_score = Some(90.0);
    ctx.gap_pct = 3.0;
    let s = catalyst_frontrun::CatalystFrontRun::default();
    let sig = s.evaluate_entry(&ctx).unwrap();
    assert_eq!(sig.side, Direction::Long);
    assert!(sig.stop_loss < ctx.price);
    assert!(sig.take_profit > ctx.price);
}

// ════════════════════════════════════════════════════════════════════
//  Default param sanity
// ════════════════════════════════════════════════════════════════════

#[test]
fn default_gap_fade_params_in_sensible_ranges() {
    let s = gap_fade::GapFade::default();
    assert!(s.min_gap_pct >= 1.0 && s.min_gap_pct <= 20.0);
    assert!(s.atr_stop_mult > 0.0);
    assert!(s.max_hold_minutes > 0);
}

#[test]
fn default_etf_pair_params_in_sensible_ranges() {
    let s = etf_pair::EtfPair::default();
    assert!(s.min_z > 1.0 && s.min_z < 5.0);
    assert!(s.force_close_z > s.min_z);
    assert!(s.entry_window_start_min < s.entry_window_end_min);
}

#[test]
fn default_catalyst_frontrun_max_hold_short() {
    // Strategy thesis decays fast → max hold should be ≤ 15 min
    let s = catalyst_frontrun::CatalystFrontRun::default();
    assert!(s.max_hold_minutes <= 15);
}
