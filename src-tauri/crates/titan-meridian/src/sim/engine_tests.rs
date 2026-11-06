//! Simulation engine tests (kept apart from `engine.rs` for the 500-line rule).

use super::*;
use crate::bars::Bar;
use crate::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

fn flat_day(n: u32, o: f64, c: f64) -> DaySeries {
    let date = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap() + chrono::Days::new(n as u64);
    let bars = (0..390)
        .map(|m| {
            let p = o + (c - o) * m as f64 / 389.0;
            Bar { o: p, h: p, l: p, c: p, v: 100.0 }
        })
        .collect();
    DaySeries { date, bars, real_bars: 390 }
}

/// Warm-up days: open 100, drift up 0.1–0.3 % ⇒ IBS = 1 (no carry).
fn warmup(n: u32) -> Vec<DaySeries> {
    (0..n).map(|i| flat_day(i, 100.0, 100.1 + (i % 3) as f64 * 0.1)).collect()
}

/// Live replay: the last day has only 60 bars so far. With
/// `live_session_bars = 390` the close-only logic (Sleeve B entry) must
/// NOT fire at bar 59; without it the truncated day is treated as a
/// complete (early-close) session and B enters at its "close".
#[test]
fn partial_last_day_does_not_trigger_close_logic_when_live() {
    let mut days = warmup(40);
    let mut partial = flat_day(40, 101.0, 100.0); // would be IBS 0 ⇒ full carry at the close
    partial.bars.truncate(60);
    partial.real_bars = 60;
    days.push(partial);
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    let mk = || -> Vec<Box<dyn Sleeve>> {
        vec![Box::new(OvernightCarry::new(OvernightParams { sigma_target_night: 1.0, ..Default::default() }))]
    };
    let live = SimConfig { warmup_days: 21, live_session_bars: Some(390), ..Default::default() };
    let r = run(&live, &data, &mk);
    assert!(r.trades.is_empty(), "no close entry on a day still in progress: {:?}", r.trades);
    let hist = SimConfig { warmup_days: 21, ..Default::default() };
    let r2 = run(&hist, &data, &mk);
    assert_eq!(r2.trades.len(), 1, "historical mode treats the 60-bar day as a complete session");
    assert_eq!(r2.trades[0].bar_idx, 59);
}

/// Close preview at "15:48": 379 bars, force_close ⇒ B decides now with
/// the same inputs the full-day replay (offset 11) will use at 15:59.
#[test]
fn close_preview_matches_full_day_decision() {
    let mut days = warmup(40);
    days.push(flat_day(40, 101.0, 100.0));
    let mk = || -> Vec<Box<dyn Sleeve>> {
        vec![Box::new(OvernightCarry::new(OvernightParams { sigma_target_night: 1.0, decide_bars_before_close: 11, ..Default::default() }))]
    };
    let full = run(&SimConfig { warmup_days: 21, ..Default::default() }, &[SymbolData { symbol: "SPY".into(), days: days.clone() }], &mk);
    let mut partial = days;
    if let Some(d) = partial.last_mut() {
        d.bars.truncate(379);
        d.real_bars = 379;
    }
    let cfg = SimConfig { warmup_days: 21, live_session_bars: Some(390), live_force_close: true, ..Default::default() };
    let prev = run(&cfg, &[SymbolData { symbol: "SPY".into(), days: partial }], &mk);
    assert_eq!(full.trades.len(), 1);
    assert_eq!(prev.trades.len(), 1, "{:?}", prev.trades);
    assert_eq!(prev.trades[0].delta_shares, full.trades[0].delta_shares);
    assert_eq!(prev.trades[0].bar_idx, 378);
}

#[test]
fn fixed_sizing_equity_ignores_compounding() {
    let mut days = warmup(40);
    days.push(flat_day(40, 101.0, 100.0));
    let mk = || -> Vec<Box<dyn Sleeve>> { vec![Box::new(OvernightCarry::new(OvernightParams { sigma_target_night: 1.0, ..Default::default() }))] };
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    // start with $200k simulated but size on $100k ⇒ 1000 shares, not 2000
    let cfg = SimConfig { warmup_days: 21, initial_equity: 200_000.0, fixed_sizing_equity: Some(100_000.0), ..Default::default() };
    let r = run(&cfg, &data, &mk);
    assert_eq!(r.trades[0].delta_shares, 1000.0);
    assert_eq!(r.sleeve_shares["SPY"][&SleeveId::B], 1000.0);
}

#[test]
fn overnight_only_run_matches_hand_computation() {
    let mut days = warmup(40);
    days.push(flat_day(40, 101.0, 100.0)); // closes at low ⇒ IBS 0 ⇒ full carry
    days.push(flat_day(41, 102.0, 102.5)); // opens +2 %, closes at high ⇒ no new carry
    let cfg = SimConfig { warmup_days: 21, ..Default::default() };
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    let mk = || -> Vec<Box<dyn Sleeve>> {
        vec![Box::new(OvernightCarry::new(OvernightParams {
            sigma_target_night: 1.0, // ⇒ capped at max_exposure 1.0 regardless of σ_on
            ..Default::default()
        }))]
    };
    let r = run(&cfg, &data, &mk);
    assert_eq!(r.trades.len(), 2, "{:?}", r.trades);
    let buy = &r.trades[0];
    assert_eq!((buy.delta_shares, buy.bar_idx), (1000.0, 389));
    assert!((buy.fill_price - 100.01).abs() < 1e-9);
    let sell = &r.trades[1];
    assert_eq!((sell.delta_shares, sell.bar_idx), (-1000.0, 0));
    assert!((sell.fill_price - 101.99).abs() < 1e-9);
    let last = *r.equity.last().unwrap();
    assert!((last - (100_000.0 + 1000.0 * (101.99 - 100.01))).abs() < 1e-6);
    assert!((r.total_cost - 20.0).abs() < 1e-9);
    let b = &r.sleeve_pnl[&SleeveId::B];
    assert!((b.iter().sum::<f64>() - 2000.0).abs() < 1e-6, "gross attribution = 1000 × (102 − 100)");
    assert!(r.sleeve_shares.is_empty(), "flat after the open sale: {:?}", r.sleeve_shares);
    // stop one day earlier: the carry is held by B
    let data2 = vec![SymbolData { symbol: "SPY".into(), days: data[0].days[..41].to_vec() }];
    let r2 = run(&cfg, &data2, &mk);
    assert_eq!(r2.sleeve_shares["SPY"][&SleeveId::B], 1000.0);
}

#[test]
fn netting_offsets_a_short_against_b_long_without_a_trade() {
    // Day 41: strongly down all day ⇒ A goes short at 10:00; it also
    // closes at its low ⇒ B wants long overnight. At the close A flattens
    // (buys back) while B buys: net single fill.
    let mut days = warmup(40);
    days.push(flat_day(40, 100.0, 96.0));
    days.push(flat_day(41, 96.0, 96.0));
    let cfg = SimConfig { warmup_days: 21, ..Default::default() };
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    let mk = || -> Vec<Box<dyn Sleeve>> {
        vec![
            Box::new(IntradayMomentum::new(IntradayParams { sigma_target_daily: 1.0, max_leverage: 1.0, ..Default::default() })),
            Box::new(OvernightCarry::new(OvernightParams { sigma_target_night: 1.0, ..Default::default() })),
        ]
    };
    let r = run(&cfg, &data, &mk);
    let day40: Vec<&TradeRecord> = r.trades.iter().filter(|t| t.date == days_date(40)).collect();
    assert_eq!(day40.len(), 2, "{day40:?}");
    assert!(day40[0].delta_shares < 0.0, "A short entry");
    let close_fill = day40[1];
    assert_eq!(close_fill.bar_idx, 389);
    assert_eq!(close_fill.sleeves, vec![SleeveId::A, SleeveId::B]);
    // A covers |short| and B buys floor(equity/96) ⇒ one net buy of both.
    let k = r.dates.iter().position(|d| *d == days_date(39)).unwrap();
    let equity_start = r.equity[k];
    let b_shares = (equity_start / 96.0).floor();
    let a_short = -day40[0].delta_shares;
    assert!(close_fill.delta_shares > 0.0);
    assert!((close_fill.delta_shares - (a_short + b_shares)).abs() < 1e-9, "{close_fill:?} vs {a_short}+{b_shares}");
}

#[test]
fn sleeve_state_is_per_symbol() {
    // Two symbols; only the second closes at its low on day 40. The
    // first must not inherit a "holding" flag and sell at the open.
    let mut d1 = warmup(42);
    let mut d2 = warmup(40);
    d2.push(flat_day(40, 101.0, 100.0));
    d2.push(flat_day(41, 102.0, 102.5));
    d1[40] = flat_day(40, 100.0, 100.3);
    d1[41] = flat_day(41, 100.0, 100.3);
    let cfg = SimConfig { warmup_days: 21, ..Default::default() };
    let data = vec![SymbolData { symbol: "SPY".into(), days: d1 }, SymbolData { symbol: "QQQ".into(), days: d2 }];
    let mk = || -> Vec<Box<dyn Sleeve>> {
        vec![Box::new(OvernightCarry::new(OvernightParams { sigma_target_night: 1.0, ..Default::default() }))]
    };
    let r = run(&cfg, &data, &mk);
    assert!(r.trades.iter().all(|t| t.symbol == "QQQ"), "{:?}", r.trades);
    assert_eq!(r.trades.len(), 2);
    assert_eq!(r.trades[1].bar_idx, 0, "QQQ carry sold at next open");
}

fn days_date(n: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 1, 5).unwrap() + chrono::Days::new(n as u64)
}

/// Piecewise-constant day: price jumps at the given bar indices.
fn path_day(n: u32, path: &[(usize, f64)]) -> DaySeries {
    let mut p = path[0].1;
    let bars = (0..390)
        .map(|m| {
            if let Some(&(_, np)) = path.iter().find(|(i, _)| *i == m) {
                p = np;
            }
            Bar { o: p, h: p, l: p, c: p, v: 100.0 }
        })
        .collect();
    DaySeries { date: days_date(n), bars, real_bars: 390 }
}

/// Risk halt while A is positioned: the halt fires on the very minute the
/// portfolio mark breaches the budget (every bar, like the live rail), A is
/// flattened at that bar's close, nothing new is opened, B takes no carry.
/// (Until 2026-09-18 the mark ran only at decision bars and a halted close
/// dropped A's own flatten, carrying the intraday position overnight.)
#[test]
fn halt_flattens_a_on_the_breach_bar_and_blocks_b_entry() {
    let mut days = warmup(40);
    // long at 10:00 (101 > UB), crash to 98.3 at bar 40 ⇒ −2.7 % ⇒ Halted at bar 40
    days.push(path_day(40, &[(0, 100.0), (10, 101.0), (40, 98.3)]));
    let cfg = SimConfig { warmup_days: 21, risk: Some(RiskParams::default()), ..Default::default() };
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    let mk = || -> Vec<Box<dyn Sleeve>> {
        vec![
            Box::new(IntradayMomentum::new(IntradayParams { sigma_target_daily: 1.0, max_leverage: 1.0, ..Default::default() })),
            Box::new(OvernightCarry::new(OvernightParams { sigma_target_night: 1.0, ..Default::default() })),
        ]
    };
    let r = run(&cfg, &data, &mk);
    assert_eq!(r.risk_halts, 1, "{:?}", r.trades);
    assert_eq!(r.trades.len(), 2, "entry + halt flatten, nothing else: {:?}", r.trades);
    let flat = &r.trades[1];
    assert_eq!((flat.bar_idx, flat.delta_shares), (40, -1000.0), "flattened on the breach bar, not at 10:30");
    assert!((flat.fill_price - 98.29).abs() < 1e-9);
    assert!(r.sleeve_shares.is_empty(), "nothing carried overnight on a halted day: {:?}", r.sleeve_shares);
}

/// Two symbols: the halt caused by SPY's crash at bar 40 must flatten QQQ's
/// long at bar 40 too, and QQQ must have traded normally before it (the old
/// symbol-major loop ran QQQ's whole day under a halt it had not yet earned).
#[test]
fn halt_is_synchronous_across_symbols() {
    let mut spy = warmup(40);
    spy.push(path_day(40, &[(0, 100.0), (10, 101.0), (40, 98.3)]));
    let mut qqq = warmup(40);
    qqq.push(path_day(40, &[(0, 100.0), (10, 101.0), (200, 103.0)])); // steady long, no crash
    let cfg = SimConfig { warmup_days: 21, risk: Some(RiskParams::default()), ..Default::default() };
    let data = vec![SymbolData { symbol: "SPY".into(), days: spy }, SymbolData { symbol: "QQQ".into(), days: qqq }];
    let mk = || -> Vec<Box<dyn Sleeve>> { vec![Box::new(IntradayMomentum::new(IntradayParams { sigma_target_daily: 1.0, max_leverage: 1.0, ..Default::default() }))] };
    let r = run(&cfg, &data, &mk);
    let q: Vec<&TradeRecord> = r.trades.iter().filter(|t| t.symbol == "QQQ").collect();
    assert_eq!(q.len(), 2, "QQQ enters at 10:00 and is flattened by the SPY-driven halt: {q:?}");
    assert_eq!((q[0].bar_idx, q[0].delta_shares), (29, 1000.0));
    assert_eq!((q[1].bar_idx, q[1].delta_shares), (40, -1000.0));
    // and the reverse: with the crash in QQQ instead, SPY is flattened at bar 40
    let mut spy2 = warmup(40);
    spy2.push(path_day(40, &[(0, 100.0), (10, 101.0), (200, 103.0)]));
    let mut qqq2 = warmup(40);
    qqq2.push(path_day(40, &[(0, 100.0), (10, 101.0), (40, 98.3)]));
    let r2 = run(&cfg, &[SymbolData { symbol: "SPY".into(), days: spy2 }, SymbolData { symbol: "QQQ".into(), days: qqq2 }], &mk);
    let sp: Vec<&TradeRecord> = r2.trades.iter().filter(|t| t.symbol == "SPY").collect();
    assert_eq!(sp.iter().map(|t| t.bar_idx).collect::<Vec<_>>(), vec![29, 40], "{sp:?}");
    assert_eq!(r2.risk_halts, 1);
}

/// Latency stress: with `fill_delay_bars = 1` the 10:00 decision fills at
/// the 10:01 bar's close (worse price on a trending day), while the closing
/// flatten stays on the closing bar (auction order, never delayed).
#[test]
fn fill_delay_moves_intraday_fills_but_not_the_close() {
    let mut days = warmup(40);
    days.push(flat_day(40, 100.0, 104.0)); // steady uptrend ⇒ long at 10:00
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    let mk = || -> Vec<Box<dyn Sleeve>> { vec![Box::new(IntradayMomentum::new(IntradayParams { sigma_target_daily: 1.0, max_leverage: 1.0, ..Default::default() }))] };
    let base = run(&SimConfig { warmup_days: 21, ..Default::default() }, &data, &mk);
    let late = run(&SimConfig { warmup_days: 21, fill_delay_bars: 1, ..Default::default() }, &data, &mk);
    let (b0, l0) = (&base.trades[0], &late.trades[0]);
    assert_eq!((b0.bar_idx, l0.bar_idx), (29, 30), "{:?} / {:?}", base.trades, late.trades);
    assert!(l0.fill_price > b0.fill_price, "one more bar of uptrend ⇒ dearer entry");
    assert_eq!(l0.delta_shares, b0.delta_shares, "sizing is decided at the decision bar");
    assert_eq!(late.trades.last().map(|t| t.bar_idx), Some(389));
    assert_eq!(late.trades.len(), base.trades.len());
    assert!(late.equity.last() < base.equity.last(), "latency costs money on a trend day");
}

/// The per-symbol risk cap (1.5×) binds on the sleeve's net, not per
/// ensemble member: three members at L/3 = 2/3 each must end at 1.5×, not
/// 2.0× (the pre-fix behaviour since the ensemble of 2026-09-18).
#[test]
fn per_symbol_cap_binds_on_the_ensemble_total() {
    let mut days = warmup(40);
    days.push(flat_day(40, 100.0, 104.0)); // steady uptrend ⇒ every member long at 10:00
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    let params = IntradayParams { sigma_target_daily: 1.0, max_leverage: 2.0, ..Default::default() };
    let mk = || -> Vec<Box<dyn Sleeve>> { IntradayMomentum::ensemble(params, &[1.0, 1.0, 1.0]).into_iter().map(|m| Box::new(m) as Box<dyn Sleeve>).collect() };
    let capped = run(&SimConfig { warmup_days: 21, risk: Some(RiskParams::default()), fixed_sizing_equity: Some(100_000.0), ..Default::default() }, &data, &mk);
    let entry: f64 = capped.trades.iter().filter(|t| t.bar_idx == 29).map(|t| t.delta_shares).sum();
    // members: 666 + 666 + floor(100 000 × (1.5 − 4/3) / 100) = 166 ⇒ 1 498 shares = 1.5× of $100k at $100
    assert!((entry - 1498.0).abs() < 1e-9, "sleeve total capped at 1.5×: {entry} ({:?})", capped.trades);
    let free = run(&SimConfig { warmup_days: 21, risk: None, fixed_sizing_equity: Some(100_000.0), ..Default::default() }, &data, &mk);
    let entry_free: f64 = free.trades.iter().filter(|t| t.bar_idx == 29).map(|t| t.delta_shares).sum();
    assert_eq!(entry_free, 1998.0, "without the overlay the ensemble takes the full L = 2.0×");
    // and the day still ends flat
    assert_eq!(capped.trades.last().map(|t| t.bar_idx), Some(389));
    assert!(capped.sleeve_shares.is_empty());
}

#[test]
fn deterministic() {
    let days: Vec<DaySeries> = (0..60).map(|i| flat_day(i, 100.0, 100.0 + ((i * 7) % 5) as f64 - 2.0)).collect();
    let data = vec![SymbolData { symbol: "SPY".into(), days }];
    let mk = || -> Vec<Box<dyn Sleeve>> {
        vec![Box::new(IntradayMomentum::new(IntradayParams::default())), Box::new(OvernightCarry::new(OvernightParams::default()))]
    };
    let cfg = SimConfig::default();
    let a = run(&cfg, &data, &mk);
    let b = run(&cfg, &data, &mk);
    assert_eq!(a.equity, b.equity);
    assert_eq!(a.trades, b.trades);
}
