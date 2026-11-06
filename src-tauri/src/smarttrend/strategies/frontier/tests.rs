//! Setup-level tests for the Frontier module (sibling file — R2 cap).
use super::*;
use crate::market::micro::MicroSnapshot;

fn micro_ok() -> MicroSnapshot {
    MicroSnapshot {
        bid: 10.0,
        ask: 10.02,
        bid_size: 500.0,
        ask_size: 200.0,
        spread_bps: 20.0,
        imbalance: 0.4,
        microprice: 10.014,
        quote_age_ms: 50,
        prints_per_sec_15s: 5.0,
        prints_per_sec_60s: 3.0,
        signed_notional_15s: 80_000.0,
        signed_notional_60s: 200_000.0,
        signed_notional_5m: 500_000.0,
        large_print_pct_15s: 0.2,
        last_trade_price: 10.01,
        update_count: 100,
        fresh: true,
    }
}

fn base_ctx() -> FrontierContext {
    let mut candles = Vec::new();
    for i in 0..30 {
        candles.push(Candle {
            time: 1_700_000_000 + i * 60,
            open: 10.0,
            high: 10.2,
            low: 9.9,
            close: 10.0 + (i as f64) * 0.01,
            volume: 10_000.0,
        });
    }
    FrontierContext {
        symbol: "TEST".into(),
        price: 10.3,
        atr: 0.15,
        candles_1m: candles,
        micro: micro_ok(),
        now_et_min: 10 * 60,
        now_utc: Utc::now(),
        micro_age_secs: 300,
        resume: None,
        prev_close: 9.5,
        session_open: 9.8,
        baseline_pps_5m: 1.0,
        entered_today: std::collections::HashSet::new(),
        catalyst_news: false,
        catalyst_gap_pct: 0.0,
    }
}

#[test]
fn tape_ignition_fires_on_burst() {
    let ctx = base_ctx();
    let th = FrontierThresholds::default();
    let cand = eval_tape_ignition_full(&ctx, &th)
        .candidate
        .expect("tape should fire");
    assert_eq!(cand.setup, FrontierSetup::TapeIgnition);
    assert_eq!(cand.signal.side, Direction::Long);
    assert!(cand.conviction > 0.0);
}

#[test]
fn verbose_eval_reports_failed_conditions() {
    let mut ctx = base_ctx();
    ctx.micro.spread_bps = 90.0; // above tape_max_spread_bps
    let th = FrontierThresholds::default();
    let evals = evaluate_frontier_verbose(&ctx, &th);
    let tape = evals
        .iter()
        .find(|e| e.setup == FrontierSetup::TapeIgnition)
        .expect("tape eval present");
    assert!(tape.candidate.is_none());
    assert!(tape
        .conditions
        .iter()
        .any(|c| c.name == "spread_bps" && !c.passed));
}

#[test]
fn missing_micro_fresh_fail_closed() {
    let mut ctx = base_ctx();
    ctx.micro.fresh = false;
    assert!(evaluate_frontier(&ctx, &FrontierThresholds::default()).is_none());
}

#[test]
fn true_resume_fade_uses_print_pressure() {
    let mut ctx = base_ctx();
    ctx.resume = Some(ResumeSnapshot {
        resumed_at: Utc::now(),
        halt_code: Some("H".into()),
        pre_halt_price: Some(9.0),
        halted_since: Utc::now() - chrono::Duration::minutes(5),
        first_prints: vec![
            (10.5, 1_000.0, true),
            (10.6, 800.0, true),
            (10.55, 500.0, true),
        ],
    });
    let cand = eval_true_resume_full(&ctx, &FrontierThresholds::default())
        .candidate
        .expect("resume");
    assert_eq!(cand.setup, FrontierSetup::TrueResume);
    // Fade the up-prints → Short
    assert_eq!(cand.signal.side, Direction::Short);
}

#[test]
fn open_sweep_respects_window_and_once_per_day() {
    let mut ctx = base_ctx();
    ctx.now_et_min = 10 * 60;
    ctx.session_open = 10.0;
    ctx.prev_close = 9.0; // ~11% gap
    let th = FrontierThresholds::default();
    assert!(eval_open_sweep_full(&ctx, &th).candidate.is_some());
    ctx.entered_today.insert(FrontierSetup::OpenSweep.as_str());
    assert!(eval_open_sweep_full(&ctx, &th).candidate.is_none());
    ctx.entered_today.clear();
    ctx.now_et_min = 14 * 60; // outside window
    assert!(eval_open_sweep_full(&ctx, &th).candidate.is_none());
}

fn premarket_ctx() -> FrontierContext {
    let mut ctx = base_ctx();
    ctx.now_et_min = 8 * 60; // 08:00 ET
    ctx.catalyst_gap_pct = 12.0;
    ctx.micro.spread_bps = 60.0; // premarket-wide but under pm cap
    ctx
}

#[test]
fn premarket_ignition_fires_on_gap_catalyst() {
    let th = FrontierThresholds::default();
    let cand = eval_premarket_ignition_full(&premarket_ctx(), &th)
        .candidate
        .expect("premarket should fire");
    assert_eq!(cand.setup, FrontierSetup::PremarketIgnition);
    assert_eq!(cand.signal.side, Direction::Long);
    // Extended session: entry MUST be limit-capped at the touch.
    assert_eq!(cand.signal.limit_price, Some(10.02));
}

#[test]
fn premarket_ignition_news_bypasses_gap() {
    let th = FrontierThresholds::default();
    let mut ctx = premarket_ctx();
    ctx.catalyst_gap_pct = 1.0; // below pm_min_gap_pct
    assert!(eval_premarket_ignition_full(&ctx, &th).candidate.is_none());
    ctx.catalyst_news = true;
    assert!(eval_premarket_ignition_full(&ctx, &th).candidate.is_some());
}

#[test]
fn premarket_ignition_respects_window_and_once_per_day() {
    let th = FrontierThresholds::default();
    let mut ctx = premarket_ctx();
    ctx.now_et_min = 10 * 60; // RTH — outside premarket window
    assert!(eval_premarket_ignition_full(&ctx, &th).candidate.is_none());
    ctx.now_et_min = 8 * 60;
    ctx.entered_today
        .insert(FrontierSetup::PremarketIgnition.as_str());
    assert!(eval_premarket_ignition_full(&ctx, &th).candidate.is_none());
}

