//! Tests for the APEX strategy — sibling file chained from apex.rs
//! via `#[path]` to keep the module under the R2 line cap.

use super::*;
use crate::smarttrend::strategies::apex_calibration::merge_apex_overrides;
use crate::smarttrend::strategies::MarketRegime;
use titan_types::Candle;

fn strict_apex() -> Apex {
    Apex {
        min_price: 2.0,
        min_rvol: 2.5,
        evidence_rvol: 3.0,
        min_catalyst_score: 40.0,
        max_spread_bps: 40.0,
        ignition_range_atr: 1.8,
        ignition_close_pos: 0.70,
        ignition_vol_mult: 2.0,
        max_extension_atr: 3.0,
        stop_atr_min: 0.75,
        stop_atr_max: 2.0,
        target_r_multiple: 3.0,
        max_hold_minutes: 240,
        entry_start_et_min: 9 * 60 + 35,
        entry_end_et_min: 11 * 60 + 30,
        risk_dollars: 40.0,
        sub_wallet_cap_pct: 0.25,
    }
}

fn flat_candles(n: usize, price: f64) -> Vec<Candle> {
    (0..n)
        .map(|i| Candle {
            time: i as i64 * 300,
            open: price,
            high: price * 1.002,
            low: price * 0.998,
            close: price,
            volume: 100_000.0,
        })
        .collect()
}

/// Context primed to fire a long APEX ignition.
fn ignition_ctx() -> MarketContext {
    let mut candles = flat_candles(30, 10.0);
    // Ignition bar: wide range, closes at the high, 3× volume.
    candles.push(Candle {
        time: 30 * 300,
        open: 10.0,
        high: 10.60,
        low: 9.98,
        close: 10.58,
        volume: 320_000.0,
    });
    MarketContext {
        symbol: "TEST".into(),
        timeframe: "5m",
        price: 10.58,
        prev_close: 9.80,
        session_open: 10.0,
        candles,
        atr: 0.30,
        rvol: 4.5,
        gap_pct: 2.0,
        catalyst_score: Some(60.0),
        spread_bps: 12.0,
        now_et_min: 10 * 60,
        now_et_hour: 10,
        now_et_minute: 0,
        regime: Some(MarketRegime::Trending),
        is_halted: false,
        companion: None,
        float_shares: None,
        news_sentiment: None,
        overrides: std::collections::HashMap::new(),
    }
}

#[test]
fn fires_long_on_ignition_bar() {
    let sig = strict_apex()
        .evaluate_entry(&ignition_ctx())
        .expect("should fire");
    assert_eq!(sig.side, Direction::Long);
    assert!(sig.stop_loss < 10.58);
    assert!(sig.take_profit > 10.58);
    // 3R asymmetry: reward ≥ 2.9× risk (float tolerance).
    let risk = 10.58 - sig.stop_loss;
    let reward = sig.take_profit - 10.58;
    assert!(reward / risk > 2.9, "reward/risk = {}", reward / risk);
    assert_eq!(sig.max_hold_minutes, 240);
}

#[test]
fn refuses_outside_morning_window() {
    let mut ctx = ignition_ctx();
    ctx.now_et_min = 8 * 60; // 08:00 ET — premarket
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
    ctx.now_et_min = 17 * 60; // 17:00 ET — closed
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
}

#[test]
fn fires_on_adaptive_spread_under_5_dollars() {
    let mut ctx = ignition_ctx();
    ctx.price = 2.50; // price < 5.0
    ctx.session_open = 2.40;
    ctx.prev_close = 2.38;
    for c in ctx.candles.iter_mut() {
        c.open = 2.40;
        c.high = 2.41;
        c.low = 2.39;
        c.close = 2.40;
    }
    if let Some(last) = ctx.candles.last_mut() {
        last.open = 2.40;
        last.high = 2.52;
        last.low = 2.39;
        last.close = 2.50;
        last.volume = 320_000.0;
    }
    ctx.atr = 0.05;
    ctx.rvol = 6.0; // rvol >= 5.0
    ctx.spread_bps = 45.0; // spread > 40 bps, but < 60 bps (allowed!)
    assert!(strict_apex().evaluate_entry(&ctx).is_some());

    ctx.spread_bps = 65.0; // spread > 60 bps (vetoed!)
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
}

#[test]
fn regime_aware_rvol_thresholds() {
    let mut ctx = ignition_ctx();
    // Default min_rvol is 2.5.
    // In Trending regime, min_rvol is relaxed to 1.8.
    ctx.regime = Some(MarketRegime::Trending);
    ctx.rvol = 1.9;
    assert!(strict_apex().evaluate_entry(&ctx).is_some());

    // In Calm regime, min_rvol is 2.2 -> 1.9 should be rejected.
    ctx.regime = Some(MarketRegime::Calm);
    assert!(strict_apex().evaluate_entry(&ctx).is_none());

    // In Volatile regime, min_rvol is 3.0 -> 2.6 should be rejected.
    ctx.regime = Some(MarketRegime::Volatile);
    ctx.rvol = 2.6;
    assert!(strict_apex().evaluate_entry(&ctx).is_none());

    ctx.rvol = 3.1; // above 3.0 -> accepted
    assert!(strict_apex().evaluate_entry(&ctx).is_some());
}

#[test]
fn refuses_without_event_evidence() {
    let mut ctx = ignition_ctx();
    ctx.catalyst_score = Some(10.0);
    ctx.rvol = 2.6; // above floor but below evidence threshold
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
}

#[test]
fn refuses_indecisive_bar() {
    let mut ctx = ignition_ctx();
    // Same range but closing mid-bar — no ignition.
    if let Some(last) = ctx.candles.last_mut() {
        last.close = 10.29;
    }
    ctx.price = 10.29;
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
}

#[test]
fn refuses_low_volume_bar() {
    let mut ctx = ignition_ctx();
    if let Some(last) = ctx.candles.last_mut() {
        last.volume = 120_000.0; // < 2× the 100k average
    }
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
}

#[test]
fn refuses_below_vwap() {
    let mut ctx = ignition_ctx();
    // Ignition bar below both VWAP and session open.
    if let Some(last) = ctx.candles.last_mut() {
        last.open = 9.40;
        last.high = 9.95;
        last.low = 9.35;
        last.close = 9.90;
    }
    ctx.price = 9.90;
    // close 9.90 < vwap(~10) and < session_open 10.0, closes near
    // high → would be a long, but alignment fails.
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
}

#[test]
fn short_requires_non_calm_regime() {
    let mut ctx = ignition_ctx();
    // Mirror to a breakdown bar.
    if let Some(last) = ctx.candles.last_mut() {
        last.open = 10.0;
        last.high = 10.02;
        last.low = 9.40;
        last.close = 9.42;
    }
    ctx.price = 9.42;
    ctx.session_open = 10.2;
    ctx.regime = Some(MarketRegime::Calm);
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
    ctx.regime = Some(MarketRegime::Volatile);
    let sig = strict_apex()
        .evaluate_entry(&ctx)
        .expect("short should fire");
    assert_eq!(sig.side, Direction::Short);
}

// ── F4-APEX: full condition-vector evaluation ────────────────────────

#[test]
fn full_vector_all_pass_on_ignition() {
    let (conds, res) = strict_apex().evaluate_entry_full(&ignition_ctx());
    assert!(res.is_ok(), "ignition ctx must fire: {res:?}");
    assert_eq!(conds.len(), 11, "one hit per gate");
    assert!(conds.iter().all(|c| c.passed), "all gates pass");
    // The rvol condition is regime-named (Trending in ignition_ctx).
    assert!(conds.iter().any(|c| c.name == "rvol_trending"));
}

#[test]
fn full_vector_sole_blocker_low_volume() {
    let mut ctx = ignition_ctx();
    if let Some(last) = ctx.candles.last_mut() {
        last.volume = 120_000.0; // < 2× the 100k average
    }
    let (conds, res) = strict_apex().evaluate_entry_full(&ctx);
    assert_eq!(res.unwrap_err(), Apex::R_LOW_VOLUME);
    let failed: Vec<_> = conds.iter().filter(|c| !c.passed).collect();
    assert_eq!(failed.len(), 1, "sole blocker");
    assert_eq!(failed[0].name, "ignition_vol");
    assert!((failed[0].value - 1.2).abs() < 1e-9, "measured ratio 1.2×");
    assert!((failed[0].threshold - 2.0).abs() < 1e-9);
}

#[test]
fn full_vector_first_fail_priority_matches_legacy_order() {
    // Both spread AND volume fail → the reported reason is the earlier
    // gate (spread), exactly like the legacy early-return chain.
    let mut ctx = ignition_ctx();
    ctx.spread_bps = 500.0;
    if let Some(last) = ctx.candles.last_mut() {
        last.volume = 100_000.0;
    }
    let (conds, res) = strict_apex().evaluate_entry_full(&ctx);
    assert_eq!(res.unwrap_err(), "spread too wide");
    assert!(conds.iter().filter(|c| !c.passed).count() >= 2);
}

#[test]
fn merge_apex_overrides_strips_prefix_and_filters() {
    let mut dst = std::collections::HashMap::new();
    let mut cfg = std::collections::HashMap::new();
    cfg.insert("apex_rvol_trending".to_string(), 1.4);
    cfg.insert("apex_bogus_key".to_string(), 9.9); // not calibratable
    cfg.insert("tape_min_pps_15s".to_string(), 2.0); // frontier key
    merge_apex_overrides(&mut dst, &cfg);
    let apex = dst.get(&StrategyId::Apex).expect("apex bucket");
    assert_eq!(apex.get("rvol_trending"), Some(&1.4));
    assert!(!apex.contains_key("bogus_key"));
    assert_eq!(apex.len(), 1, "frontier keys never leak into apex");
}

#[test]
fn calibration_override_relaxes_regime_rvol() {
    let mut ctx = ignition_ctx();
    ctx.regime = Some(MarketRegime::Trending);
    ctx.rvol = 1.5; // below the default 1.8 Trending floor
    assert!(strict_apex().evaluate_entry(&ctx).is_none());
    let mut cfg = std::collections::HashMap::new();
    cfg.insert("apex_rvol_trending".to_string(), 1.4);
    merge_apex_overrides(&mut ctx.overrides, &cfg);
    assert!(
        strict_apex().evaluate_entry(&ctx).is_some(),
        "override lowers the Trending RVOL floor"
    );
}

#[test]
fn no_first_red_bar_exit() {
    // A modest dip must NOT trigger an exit — that reactive pattern
    // realized −$100.37 in the audit. Only VWAP−ATR invalidation may.
    let apex = strict_apex();
    let mut ctx = ignition_ctx();
    ctx.price = 10.40; // small pullback, well above vwap − atr
    let pos = Position {
        symbol: "TEST".into(),
        qty: rust_decimal_macros::dec!(10),
        avg_entry: rust_decimal_macros::dec!(10.58),
        current_price: 10.40,
        unrealized_pnl: -1.8,
        side: Direction::Long,
        opened_at: Some(chrono::Utc::now().timestamp() - 600),
        source: "bot".into(),
        first_seen: 0,
        version: 0,
        trade_id: "t".into(),
        is_swing_intent: false,
        entry_timeframe: "5m".into(),
        stop_loss: 10.10,
        take_profit: 12.0,
    };
    assert!(apex.evaluate_exit(&pos, &ctx).is_none());
    // Full invalidation: price an ATR below VWAP → exit fires.
    ctx.price = 9.50;
    assert!(apex.evaluate_exit(&pos, &ctx).is_some());
}
