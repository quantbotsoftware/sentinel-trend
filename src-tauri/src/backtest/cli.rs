//! Headless backtest CLI — runs the Strategy Lab engine from the
//! terminal (no Tauri window, no live-trading loops). All subcommands
//! read Alpaca credentials from the persisted config, share the gzip
//! candle cache with the UI, and print plain-text tables to stdout.
//!
//! Subcommands:
//! ```text
//! sentinel-trend backtest  [SYMBOLS_CSV] [TIMEFRAME] [DAYS]   # all strategies, one TF
//! sentinel-trend orb-tf    [SYMBOLS_CSV] [DAYS]               # ORB across every TF
//! sentinel-trend orb-sweep [SYMBOLS_CSV] [TIMEFRAME] [DAYS]   # ORB parameter grid
//! ```

use chrono::{Duration, Utc};
use titan_types::Candle;

use crate::ops;
use crate::paths;
use crate::smarttrend::strategies::afternoon_fade::AfternoonFade;
use crate::smarttrend::strategies::gap_fade::GapFade;
use crate::smarttrend::strategies::opening_range_breakout::OpeningRangeBreakout;
use crate::smarttrend::strategies::{Strategy, StrategyId};

use super::engine::{simulate, SimParams, SimTrade};
use super::fills::FillModel;
use super::runner::{fetch_universe, run, static_timeframe_pub, BacktestRequest};

/// Every timeframe the engine + Alpaca support, in ascending order.
const ALL_TF: [&str; 8] = ["1m", "5m", "10m", "15m", "30m", "1h", "4h", "1d"];

/// Default universe for ORB / sweep work: the small-cap names actually
/// traded live (where these strategies have signals — mega-caps don't
/// trigger them).
const DEFAULT_UNIVERSE: [&str; 16] = [
    "STTK", "ARMG", "ELVN", "MRAM", "DXC", "NVTS", "AEVA", "NRIX", "BFLY", "EOSE", "ABAT",
    "PTON", "LAC", "BBCP", "TNGX", "RGNX",
];

/// Dispatch a CLI subcommand. Returns `true` when a subcommand was
/// recognised and handled (caller should then exit), `false` otherwise.
pub fn dispatch(args: &[String]) -> bool {
    match args.get(1).map(String::as_str) {
        Some("backtest") => {
            run_all_strategies(args);
            true
        }
        Some("orb-tf") => {
            run_orb_all_tf(args);
            true
        }
        Some("orb-sweep") => {
            run_orb_sweep(args);
            true
        }
        Some("af-sweep") => {
            run_af_sweep(args);
            true
        }
        Some("gf-sweep") => {
            run_gf_sweep(args);
            true
        }
        Some("apex-sweep") => {
            super::apex_sweep::run_apex_sweep(args);
            true
        }
        Some("apex-tf") => {
            super::apex_sweep::run_apex_tf(args);
            true
        }
        _ => false,
    }
}

// ─── Shared helpers ─────────────────────────────────────────────────

pub(super) struct Creds {
    pub(super) key: String,
    pub(super) secret: String,
}

pub(super) fn load_creds() -> Creds {
    let cfg = ops::persistence::load_config();
    let key = cfg.market.alpaca_key_id.clone();
    let secret = cfg.market.alpaca_secret.clone();
    if key.is_empty() || secret.is_empty() {
        eprintln!("backtest: Alpaca API keys not configured — set them in the UI first.");
        std::process::exit(1);
    }
    Creds { key, secret }
}

pub(super) fn parse_symbols(arg: Option<&String>) -> Vec<String> {
    let parsed: Vec<String> = arg
        .map(|csv| {
            csv.split(',')
                .map(|s| s.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // Empty / whitespace-only CSV (e.g. `""` placeholder to reach a
    // later positional arg) falls back to the default universe.
    if parsed.is_empty() {
        DEFAULT_UNIVERSE.iter().map(|s| s.to_string()).collect()
    } else {
        parsed
    }
}

pub(super) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

pub(super) fn cache_dir() -> std::path::PathBuf {
    paths::data_dir().join("backtest_cache")
}

fn sim_params(symbol: &str, tf_static: &'static str) -> SimParams {
    SimParams {
        symbol: symbol.to_string(),
        timeframe: tf_static,
        initial_equity: 10_000.0,
        fill_model: FillModel {
            slippage_bps: 5.0,
            commission: 0.0,
            per_share_fee: 0.0,
        },
        warmup_bars: 30,
        spread_bps: 5.0,
        overrides: std::collections::HashMap::new(),
    }
}

/// Aggregate a flat trade ledger across symbols into headline stats:
/// `(trades, win_rate_pct, profit_factor, net_pnl)`.
pub(super) fn aggregate(trades: &[SimTrade]) -> (usize, f64, f64, f64) {
    let n = trades.len();
    if n == 0 {
        return (0, 0.0, 0.0, 0.0);
    }
    let wins = trades.iter().filter(|t| t.net_pnl > 0.0).count();
    let gross_profit: f64 = trades.iter().filter(|t| t.net_pnl > 0.0).map(|t| t.net_pnl).sum();
    let gross_loss: f64 = trades.iter().filter(|t| t.net_pnl < 0.0).map(|t| t.net_pnl).sum();
    let net: f64 = trades.iter().map(|t| t.net_pnl).sum();
    let win_rate = wins as f64 / n as f64 * 100.0;
    let pf = if gross_loss.abs() > 0.0 {
        gross_profit / gross_loss.abs()
    } else if gross_profit > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };
    (n, win_rate, pf, net)
}

pub(super) fn fmt_pf(pf: f64) -> String {
    if pf.is_finite() {
        format!("{pf:.2}")
    } else {
        "inf".to_string()
    }
}

/// Run a single strategy configuration over a pre-fetched universe and
/// return the merged trade ledger.
pub(super) fn simulate_strat(
    strat: &dyn Strategy,
    universe: &std::collections::HashMap<String, Vec<Candle>>,
    tf_static: &'static str,
) -> Vec<SimTrade> {
    let mut trades = Vec::new();
    for (sym, bars) in universe {
        let params = sim_params(sym, tf_static);
        let report = simulate(strat, bars, &params);
        trades.extend(report.trades);
    }
    trades
}

/// Fetch a universe once for the given timeframe, exiting on empty.
pub(super) fn fetch_or_exit(
    rt: &tokio::runtime::Runtime,
    creds: &Creds,
    symbols: &[String],
    timeframe: &str,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
) -> std::collections::HashMap<String, Vec<Candle>> {
    let client = reqwest::Client::new();
    let (universe, _failed) = rt.block_on(fetch_universe(
        symbols,
        timeframe,
        start,
        end,
        &client,
        &creds.key,
        &creds.secret,
        Some(cache_dir().as_path()),
    ));
    if universe.is_empty() {
        eprintln!("sweep: no candle data fetched");
        std::process::exit(1);
    }
    universe
}

// ─── `backtest` — every strategy, one timeframe ─────────────────────

fn run_all_strategies(args: &[String]) {
    let creds = load_creds();
    let symbols = parse_symbols(args.get(2));
    let timeframe = args.get(3).cloned().unwrap_or_else(|| "5m".to_string());
    let days: i64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let end = Utc::now();
    let start = end - Duration::days(days);

    let request = BacktestRequest {
        symbols: symbols.clone(),
        strategies: StrategyId::ALL.to_vec(),
        timeframe: timeframe.clone(),
        start,
        end,
        initial_equity: 10_000.0,
        slippage_bps: 5.0,
        commission: 0.0,
        spread_bps: 5.0,
        use_cache: true,
    };
    eprintln!(
        "backtest: {} symbols × {} strategies | tf={timeframe} | {days}d [{} → {}]",
        symbols.len(),
        StrategyId::ALL.len(),
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d"),
    );

    let client = reqwest::Client::new();
    let report = match runtime().block_on(run(
        &request,
        &client,
        &creds.key,
        &creds.secret,
        Some(cache_dir()),
    )) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("backtest failed: {e}");
            std::process::exit(1);
        }
    };

    let mut rows = report.per_strategy;
    rows.sort_by(|a, b| {
        b.metrics
            .net_pnl
            .partial_cmp(&a.metrics.net_pnl)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "\n{:<22} {:>6} {:>7} {:>7} {:>10} {:>8} {:>5}",
        "STRATEGY", "TRADES", "WIN%", "PF", "NET$", "MAXDD%", "SYMS"
    );
    println!("{}", "─".repeat(72));
    for r in &rows {
        let m = &r.metrics;
        println!(
            "{:<22} {:>6} {:>6.1}% {:>7} {:>10.2} {:>7.2} {:>5}",
            r.strategy.as_str(),
            m.trades,
            m.win_rate,
            fmt_pf(m.profit_factor),
            m.net_pnl,
            m.max_drawdown_pct,
            r.symbols_traded,
        );
    }
    println!(
        "\nfetched {} symbols ({} failed) | simulate {} ms",
        report.symbols_fetched,
        report.symbols_failed.len(),
        report.simulate_ms,
    );
}

// ─── `orb-tf` — ORB across every timeframe ──────────────────────────

fn run_orb_all_tf(args: &[String]) {
    let creds = load_creds();
    let symbols = parse_symbols(args.get(2));
    let days: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    let end = Utc::now();
    let start = end - Duration::days(days);
    let client = reqwest::Client::new();
    let rt = runtime();

    eprintln!(
        "orb-tf: {} symbols × {} timeframes | {days}d [{} → {}]",
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
        let orb = OpeningRangeBreakout::default();
        let trades = simulate_strat(&orb, &universe, tf_static);
        let (n, win, pf, net) = aggregate(&trades);
        println!(
            "{tf:<6} {n:>6} {win:>6.1}% {:>7} {net:>10.2} {:>5}",
            fmt_pf(pf),
            universe.len(),
        );
    }
}

// ─── `orb-sweep` — ORB parameter grid on one timeframe ──────────────

fn run_orb_sweep(args: &[String]) {
    let creds = load_creds();
    let symbols = parse_symbols(args.get(2));
    let timeframe = args.get(3).cloned().unwrap_or_else(|| "5m".to_string());
    let days: i64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let end = Utc::now();
    let start = end - Duration::days(days);
    let client = reqwest::Client::new();
    let rt = runtime();

    let (universe, _failed) = rt.block_on(fetch_universe(
        &symbols,
        &timeframe,
        start,
        end,
        &client,
        &creds.key,
        &creds.secret,
        Some(cache_dir().as_path()),
    ));
    if universe.is_empty() {
        eprintln!("orb-sweep: no candle data fetched");
        std::process::exit(1);
    }
    let tf_static = static_timeframe_pub(&timeframe);

    // Parameter grid — the three knobs with the biggest impact on the
    // risk geometry. Defaults sit inside the grid so the baseline is
    // comparable.
    let target_rs = [1.5_f64, 2.0, 2.5, 3.0];
    let stop_atrs = [0.75_f64, 1.0, 1.5];
    let min_rvols = [1.2_f64, 1.5, 2.0];

    eprintln!(
        "orb-sweep: {} symbols | tf={timeframe} | {days}d | {} combos",
        universe.len(),
        target_rs.len() * stop_atrs.len() * min_rvols.len(),
    );

    let mut results: Vec<(f64, f64, f64, usize, f64, f64, f64)> = Vec::new();
    for &tr in &target_rs {
        for &sa in &stop_atrs {
            for &rv in &min_rvols {
                let orb = OpeningRangeBreakout {
                    target_r_multiple: tr,
                    stop_atr_mult: sa,
                    min_rvol: rv,
                    ..OpeningRangeBreakout::default()
                };
                let trades = simulate_strat(&orb, &universe, tf_static);
                let (n, win, pf, net) = aggregate(&trades);
                results.push((tr, sa, rv, n, win, pf, net));
            }
        }
    }
    // Best net P/L first.
    results.sort_by(|a, b| b.6.partial_cmp(&a.6).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "\n{:>6} {:>6} {:>6} {:>6} {:>7} {:>7} {:>10}",
        "R:R", "STOP", "RVOL", "TRADE", "WIN%", "PF", "NET$"
    );
    println!("{}", "─".repeat(56));
    for (tr, sa, rv, n, win, pf, net) in &results {
        println!(
            "{tr:>6.1} {sa:>6.2} {rv:>6.1} {n:>6} {win:>6.1}% {:>7} {net:>10.2}",
            fmt_pf(*pf),
        );
    }
    if let Some(best) = results.first() {
        println!(
            "\nbest by NET$: R:R={:.1} stop={:.2}×ATR rvol≥{:.1} → {} trades, {:.1}% win, PF {}, ${:.2}",
            best.0,
            best.1,
            best.2,
            best.3,
            best.4,
            fmt_pf(best.5),
            best.6,
        );
    }
}

// ─── `af-sweep` — AfternoonFade parameter grid ──────────────────────

fn run_af_sweep(args: &[String]) {
    let creds = load_creds();
    let symbols = parse_symbols(args.get(2));
    let timeframe = args.get(3).cloned().unwrap_or_else(|| "5m".to_string());
    let days: i64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let end = Utc::now();
    let start = end - Duration::days(days);
    let rt = runtime();
    let universe = fetch_or_exit(&rt, &creds, &symbols, &timeframe, start, end);
    let tf_static = static_timeframe_pub(&timeframe);

    // Knobs: how extreme a move to fade, stop width (small-caps need
    // room), and a volume gate to avoid fading dead tape.
    let anchors = [1.2_f64, 2.0, 3.0, 4.0];
    let stops = [0.005_f64, 0.01, 0.015, 0.02];
    let rvols = [0.8_f64, 1.5, 2.0];

    eprintln!(
        "af-sweep: {} symbols | tf={timeframe} | {days}d | {} combos",
        universe.len(),
        anchors.len() * stops.len() * rvols.len(),
    );

    let mut results: Vec<(f64, f64, f64, usize, f64, f64, f64)> = Vec::new();
    for &mv in &anchors {
        for &sp in &stops {
            for &rv in &rvols {
                let af = AfternoonFade {
                    min_anchor_move_pct: mv,
                    stop_pct: sp,
                    min_rvol: rv,
                    ..AfternoonFade::default()
                };
                let trades = simulate_strat(&af, &universe, tf_static);
                let (n, win, pf, net) = aggregate(&trades);
                results.push((mv, sp, rv, n, win, pf, net));
            }
        }
    }
    results.sort_by(|a, b| b.6.partial_cmp(&a.6).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "\n{:>6} {:>7} {:>6} {:>6} {:>7} {:>7} {:>10}",
        "MOVE%", "STOP%", "RVOL", "TRADE", "WIN%", "PF", "NET$"
    );
    println!("{}", "─".repeat(58));
    for (mv, sp, rv, n, win, pf, net) in &results {
        println!(
            "{mv:>6.1} {:>7.2} {rv:>6.1} {n:>6} {win:>6.1}% {:>7} {net:>10.2}",
            sp * 100.0,
            fmt_pf(*pf),
        );
    }
    if let Some(best) = results.first() {
        println!(
            "\nbest by NET$: move≥{:.1}% stop={:.2}% rvol≥{:.1} → {} trades, {:.1}% win, PF {}, ${:.2}",
            best.0,
            best.1 * 100.0,
            best.2,
            best.3,
            best.4,
            fmt_pf(best.5),
            best.6,
        );
    }
}

// ─── `gf-sweep` — GapFade parameter grid ────────────────────────────

fn run_gf_sweep(args: &[String]) {
    let creds = load_creds();
    let symbols = parse_symbols(args.get(2));
    let timeframe = args.get(3).cloned().unwrap_or_else(|| "5m".to_string());
    let days: i64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let end = Utc::now();
    let start = end - Duration::days(days);
    let rt = runtime();
    let universe = fetch_or_exit(&rt, &creds, &symbols, &timeframe, start, end);
    let tf_static = static_timeframe_pub(&timeframe);

    // Knobs: stop width, reward multiple, and the volume conviction gate.
    let stop_mults = [1.0_f64, 1.5, 2.0];
    let target_mults = [1.5_f64, 2.0, 3.0];
    let rvols = [1.5_f64, 2.0, 3.0];

    eprintln!(
        "gf-sweep: {} symbols | tf={timeframe} | {days}d | {} combos",
        universe.len(),
        stop_mults.len() * target_mults.len() * rvols.len(),
    );

    let mut results: Vec<(f64, f64, f64, usize, f64, f64, f64)> = Vec::new();
    for &sm in &stop_mults {
        for &tm in &target_mults {
            for &rv in &rvols {
                let gf = GapFade {
                    atr_stop_mult: sm,
                    atr_target_mult: tm,
                    min_rvol: rv,
                    ..GapFade::default()
                };
                let trades = simulate_strat(&gf, &universe, tf_static);
                let (n, win, pf, net) = aggregate(&trades);
                results.push((sm, tm, rv, n, win, pf, net));
            }
        }
    }
    results.sort_by(|a, b| b.6.partial_cmp(&a.6).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "\n{:>6} {:>7} {:>6} {:>6} {:>7} {:>7} {:>10}",
        "STOP×", "TGT×", "RVOL", "TRADE", "WIN%", "PF", "NET$"
    );
    println!("{}", "─".repeat(58));
    for (sm, tm, rv, n, win, pf, net) in &results {
        println!(
            "{sm:>6.2} {tm:>7.2} {rv:>6.1} {n:>6} {win:>6.1}% {:>7} {net:>10.2}",
            fmt_pf(*pf),
        );
    }
    if let Some(best) = results.first() {
        println!(
            "\nbest by NET$: stop={:.2}×ATR target={:.2}×ATR rvol≥{:.1} → {} trades, {:.1}% win, PF {}, ${:.2}",
            best.0,
            best.1,
            best.2,
            best.3,
            best.4,
            fmt_pf(best.5),
            best.6,
        );
    }
}
