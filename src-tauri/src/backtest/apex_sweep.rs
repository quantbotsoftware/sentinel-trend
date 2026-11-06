//! `apex-sweep` — APEX parameter grid on historical bars (2026-07-04).
//!
//! First empirical calibration of the house strategy: the default
//! 60-day backtest landed at PF 1.00 (156 trades, 32% WR) — breakeven.
//! This sweep walks the risk-geometry and ignition-anatomy grid to find
//! the parameter plateau (not a single lucky point) before any live
//! capital sees the strategy.
//!
//! ```text
//! sentinel-trend apex-sweep [SYMBOLS_CSV] [TIMEFRAME] [DAYS]
//! ```

use chrono::{Duration, Utc};

use crate::smarttrend::strategies::apex::Apex;

use super::cli::{
    aggregate, cache_dir, fetch_or_exit, fmt_pf, load_creds, parse_symbols, runtime,
    simulate_strat,
};
use super::runner::{fetch_universe, static_timeframe_pub};

/// One sweep row: the four grid knobs plus the aggregate outcome.
struct SweepRow {
    target_r: f64,
    range_atr: f64,
    vol_mult: f64,
    evidence_rvol: f64,
    trades: usize,
    win_pct: f64,
    pf: f64,
    net: f64,
}

pub fn run_apex_sweep(args: &[String]) {
    let creds = load_creds();
    let symbols = parse_symbols(args.get(2));
    let timeframe = args.get(3).cloned().unwrap_or_else(|| "5m".to_string());
    let days: i64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let end = Utc::now();
    let start = end - Duration::days(days);
    let rt = runtime();
    let universe = fetch_or_exit(&rt, &creds, &symbols, &timeframe, start, end);
    let tf_static = static_timeframe_pub(&timeframe);

    // Grid: reward multiple × ignition bar anatomy × evidence gate.
    // target_r drives the win-rate/expectancy trade-off (32% WR needs
    // <2:1 to profit); range/vol multipliers control selectivity.
    let target_rs = [1.5_f64, 2.0, 2.5, 3.0];
    let range_atrs = [1.0_f64, 1.2, 1.5, 1.8];
    let vol_mults = [1.5_f64, 2.0, 2.5, 3.0];
    let evidence_rvols = [3.0_f64, 4.0, 5.0];

    let combos = target_rs.len() * range_atrs.len() * vol_mults.len() * evidence_rvols.len();
    eprintln!(
        "apex-sweep: {} symbols | tf={timeframe} | {days}d | {combos} combos",
        universe.len(),
    );

    let mut results: Vec<SweepRow> = Vec::new();
    for &tr in &target_rs {
        for &ra in &range_atrs {
            for &vm in &vol_mults {
                for &ev in &evidence_rvols {
                    let apex = Apex {
                        target_r_multiple: tr,
                        ignition_range_atr: ra,
                        ignition_vol_mult: vm,
                        evidence_rvol: ev,
                        ..Apex::default()
                    };
                    let trades = simulate_strat(&apex, &universe, tf_static);
                    let (n, win, pf, net) = aggregate(&trades);
                    results.push(SweepRow {
                        target_r: tr,
                        range_atr: ra,
                        vol_mult: vm,
                        evidence_rvol: ev,
                        trades: n,
                        win_pct: win,
                        pf,
                        net,
                    });
                }
            }
        }
    }
    // Best net P/L first.
    results.sort_by(|a, b| b.net.partial_cmp(&a.net).unwrap_or(std::cmp::Ordering::Equal));

    let print_row = |r: &SweepRow| {
        println!(
            "{:>5.1} {:>6.1} {:>5.1} {:>5.1} {:>6} {:>6.1}% {:>7} {:>10.2}",
            r.target_r,
            r.range_atr,
            r.vol_mult,
            r.evidence_rvol,
            r.trades,
            r.win_pct,
            fmt_pf(r.pf),
            r.net,
        );
    };
    println!(
        "\n{:>5} {:>6} {:>5} {:>5} {:>6} {:>7} {:>7} {:>10}",
        "R:R", "RANGE", "VOLx", "EVID", "TRADE", "WIN%", "PF", "NET$"
    );
    println!("{}", "─".repeat(60));
    for r in results.iter().take(20) {
        print_row(r);
    }
    println!("{}", "─".repeat(60));
    println!("(worst 5 for contrast)");
    for r in results.iter().rev().take(5) {
        print_row(r);
    }
    if let Some(best) = results.first() {
        println!(
            "\nbest by NET$: R:R={:.1} range≥{:.1}×ATR vol≥{:.1}× evid_rvol≥{:.1} → {} trades, {:.1}% win, PF {}, ${:.2}",
            best.target_r,
            best.range_atr,
            best.vol_mult,
            best.evidence_rvol,
            best.trades,
            best.win_pct,
            fmt_pf(best.pf),
            best.net,
        );
    }
}

/// `apex-tf` — the calibrated APEX defaults replayed on EVERY supported
/// timeframe, answering "which TF should a manual trader follow?" with
/// measured numbers instead of opinion.
///
/// ```text
/// sentinel-trend apex-tf [SYMBOLS_CSV] [DAYS]
/// ```
pub fn run_apex_tf(args: &[String]) {
    const ALL_TF: [&str; 8] = ["1m", "5m", "10m", "15m", "30m", "1h", "4h", "1d"];

    let creds = load_creds();
    let symbols = parse_symbols(args.get(2));
    let days: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    let end = Utc::now();
    let start = end - Duration::days(days);
    let client = reqwest::Client::new();
    let rt = runtime();

    eprintln!(
        "apex-tf: {} symbols × {} timeframes | {days}d [{} → {}]",
        symbols.len(),
        ALL_TF.len(),
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d"),
    );
    println!(
        "\n{:<6} {:>6} {:>7} {:>7} {:>10} {:>5}",
        "TF", "TRADES", "WIN%", "PF", "NET$", "SYMS"
    );
    println!("{}", "─".repeat(46));

    for tf in ALL_TF {
        let (universe, _failed) = rt.block_on(fetch_universe(
            &symbols,
            tf,
            start,
            end,
            &client,
            &creds.key,
            &creds.secret,
            Some(cache_dir().as_path()),
        ));
        if universe.is_empty() {
            println!("{tf:<6} {:>6} {:>7} {:>7} {:>10} {:>5}", 0, "—", "—", "—", 0);
            continue;
        }
        let tf_static = static_timeframe_pub(tf);
        let apex = Apex::default();
        let trades = simulate_strat(&apex, &universe, tf_static);
        let (n, win, pf, net) = aggregate(&trades);
        println!(
            "{tf:<6} {n:>6} {win:>6.1}% {:>7} {net:>10.2} {:>5}",
            fmt_pf(pf),
            universe.len(),
        );
    }
}
