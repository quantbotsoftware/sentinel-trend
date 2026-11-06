//! MERIDIAN CLI — research / validation entry point.
//!
//! meridian backtest --spy PATH [--qqq PATH] [--sym TLT=PATH …] [--sleeve a|b|ab] [--lookback 14]
//!                   [--mult 1.0 | --mults 1.0,1.25,1.5] [--sigma-a 0.02] [--sigma-b 0.006]
//!                   [--slip 0.01] [--oos 2022-01-01] [--risk] [--csv OUT]
//!                   [--delay-bars 0] [--fixed-equity USD] [--b-decide-offset 0] [--no-halt-flatten]
//! meridian validate --spy PATH [--sleeve ab] [--trials N] [--oos DATE]
//!
//! `--mults` runs Sleeve A as an equal-budget ensemble over band multipliers
//! (plan §4.4 X0); `--mult` is the single-instance multiplier. `--sym` adds
//! any instrument (repeatable, or comma-separated `A=p1,B=p2`).
//!
//! Prints a Markdown report; `--csv` writes the daily equity curve.

mod pm;

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::NaiveDate;
use titan_meridian::data::load_jsonl_days;
use titan_meridian::risk::RiskParams;
use titan_meridian::sim::{markdown_report, run_with, SimConfig, SymbolData};
use titan_meridian::sleeve::Sleeve;
use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};
use titan_meridian::stats::{block_bootstrap, deflated_sharpe, pbo, performance, sharpe_moments};

pub struct Args {
    pub cmd: String,
    pub kv: HashMap<String, String>,
    pub flags: Vec<String>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_default();
    let mut kv = HashMap::new();
    let mut flags = Vec::new();
    let rest: Vec<String> = it.collect();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if let Some(k) = a.strip_prefix("--") {
            if i + 1 < rest.len() && !rest[i + 1].starts_with("--") {
                kv.insert(k.to_string(), rest[i + 1].clone());
                i += 2;
                continue;
            }
            flags.push(k.to_string());
        }
        i += 1;
    }
    Args { cmd, kv, flags }
}

pub fn num(a: &Args, k: &str, d: f64) -> f64 {
    a.kv.get(k).and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn load(args: &Args) -> Result<Vec<SymbolData>, String> {
    let mut out = Vec::new();
    let mut specs: Vec<(String, String)> = Vec::new();
    for (flag, sym) in [("spy", "SPY"), ("qqq", "QQQ")] {
        if let Some(p) = args.kv.get(flag) {
            specs.push((sym.to_string(), p.clone()));
        }
    }
    // --sym TLT=/path[,GLD=/path…]
    if let Some(list) = args.kv.get("sym") {
        for item in list.split(',') {
            let (sym, p) = item.split_once('=').ok_or_else(|| format!("--sym expects SYM=PATH, got {item}"))?;
            specs.push((sym.trim().to_uppercase(), p.trim().to_string()));
        }
    }
    for (sym, p) in specs {
        let (days, st) = load_jsonl_days(&PathBuf::from(&p), 300).map_err(|e| format!("{sym}: {e}"))?;
        eprintln!("{sym}: {} sessions kept, {} dropped, {} RTH bars", st.sessions_kept, st.sessions_dropped_incomplete, st.rth_bars);
        out.push(SymbolData { symbol: sym, days });
    }
    if out.is_empty() {
        return Err("need --spy PATH / --qqq PATH / --sym SYM=PATH".into());
    }
    Ok(out)
}

/// Sleeve set for one symbol. `--b-symbols SPY,QQQ` restricts Sleeve B to
/// the listed instruments (diversifiers run A only); default: every symbol.
type SleeveFactory = Box<dyn Fn(&str) -> Vec<Box<dyn Sleeve>>>;

/// Band multipliers for Sleeve A: `--mults a,b,c` (ensemble) or the single `mult`.
fn band_mults(args: &Args, mult: f64) -> Vec<f64> {
    match args.kv.get("mults") {
        Some(s) => s.split(',').filter_map(|x| x.trim().parse().ok()).collect(),
        None => vec![mult],
    }
}

fn build(args: &Args, lookback: usize, mult: f64, sigma_a: f64, sigma_b: f64) -> (SimConfig, SleeveFactory) {
    let which = args.kv.get("sleeve").map(|s| s.to_lowercase()).unwrap_or_else(|| "ab".into());
    let a = IntradayParams {
        lookback_days: lookback,
        band_mult: 1.0,
        sigma_target_daily: sigma_a,
        max_leverage: num(args, "max-lev", 4.0),
        allow_short: !args.flags.contains(&"long-only".to_string()),
    };
    let mults = band_mults(args, mult);
    let b = OvernightParams {
        sigma_target_night: sigma_b,
        ibs_cut: num(args, "ibs-cut", 0.7),
        decide_bars_before_close: num(args, "b-decide-offset", 0.0) as usize,
        ..Default::default()
    };
    let b_symbols: Option<Vec<String>> = args.kv.get("b-symbols").map(|s| s.split(',').map(|x| x.trim().to_uppercase()).collect());
    let factory: SleeveFactory = Box::new(move |symbol: &str| {
        let mut sleeves: Vec<Box<dyn Sleeve>> = Vec::new();
        if which.contains('a') {
            for m in IntradayMomentum::ensemble(a, &mults) {
                sleeves.push(Box::new(m));
            }
        }
        let b_here = b_symbols.as_ref().is_none_or(|l| l.iter().any(|s| s == symbol));
        if which.contains('b') && b_here {
            sleeves.push(Box::new(OvernightCarry::new(b)));
        }
        sleeves
    });
    let cfg = SimConfig {
        initial_equity: num(args, "equity", 100_000.0),
        slippage_per_share: num(args, "slip", 0.01),
        envelope_lookback: lookback,
        // the envelope is fitted at k = 1; each A member scales σ by its own multiplier
        envelope_mult: 1.0,
        warmup_days: lookback + 14,
        risk: args.flags.contains(&"risk".to_string()).then(RiskParams::default),
        fixed_sizing_equity: args.kv.get("fixed-equity").and_then(|v| v.parse().ok()),
        fill_delay_bars: num(args, "delay-bars", 0.0) as usize,
        halt_flatten_intraday: !args.flags.contains(&"no-halt-flatten".to_string()),
        netting: None,
        ..Default::default()
    };
    (cfg, factory)
}

pub fn oos_date(args: &Args) -> Option<NaiveDate> {
    args.kv.get("oos").and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

fn cmd_backtest(args: &Args) -> Result<(), String> {
    let data = load(args)?;
    let (cfg, factory) = build(args, num(args, "lookback", 14.0) as usize, num(args, "mult", 1.0), num(args, "sigma-a", 0.02), num(args, "sigma-b", 0.006));
    let r = run_with(&cfg, &data, &*factory);
    let title = format!(
        "MERIDIAN backtest — sleeves {} · N={} k={} σA={} σB={} slip=${}",
        args.kv.get("sleeve").cloned().unwrap_or_else(|| "ab".into()),
        cfg.envelope_lookback,
        band_mults(args, num(args, "mult", 1.0)).iter().map(|k| k.to_string()).collect::<Vec<_>>().join("/"),
        num(args, "sigma-a", 0.02),
        num(args, "sigma-b", 0.006),
        cfg.slippage_per_share
    );
    println!("{}", markdown_report(&title, &r, oos_date(args)));
    let dump = num(args, "dump-trades", 0.0) as usize;
    for t in r.trades.iter().take(dump) {
        eprintln!("{} {} idx={} Δ={:+.0} px={:.2} {:?}", t.date, t.symbol, t.bar_idx, t.delta_shares, t.fill_price, t.sleeves);
    }
    if let Some(p) = args.kv.get("csv") {
        let mut s = String::from("date,equity,ret\n");
        for i in 0..r.dates.len() {
            s.push_str(&format!("{},{:.2},{:.8}\n", r.dates[i], r.equity[i], r.daily_returns[i]));
        }
        std::fs::write(p, s).map_err(|e| e.to_string())?;
        eprintln!("wrote {p}");
    }
    // per-sleeve gross $ P/L per day (SPC / CUSUM input, plan §3.9)
    if let Some(p) = args.kv.get("sleeve-csv") {
        let ids: Vec<_> = r.sleeve_pnl.keys().copied().collect();
        let mut s = format!("date,{}\n", ids.iter().map(|i| i.as_str()).collect::<Vec<_>>().join(","));
        for (i, d) in r.dates.iter().enumerate() {
            let cols: Vec<String> = ids.iter().map(|id| format!("{:.2}", r.sleeve_pnl[id].get(i).copied().unwrap_or(0.0))).collect();
            s.push_str(&format!("{d},{}\n", cols.join(",")));
        }
        std::fs::write(p, s).map_err(|e| e.to_string())?;
        eprintln!("wrote {p}");
    }
    Ok(())
}

/// Grid over declared parameters → DSR of the best, PBO of the family,
/// bootstrap CI of the best. This is the pre-registered X0 gate.
fn cmd_validate(args: &Args) -> Result<(), String> {
    let data = load(args)?;
    let grid_n = [14usize, 30, 90];
    let grid_k = [1.0, 1.25, 1.5];
    let grid_b = [0.6, 0.7, 0.8];
    let mut series: Vec<(String, Vec<f64>, Vec<NaiveDate>)> = Vec::new();
    for &n in &grid_n {
        for &k in &grid_k {
            for &ib in &grid_b {
                let mut a2 = Args { cmd: args.cmd.clone(), kv: args.kv.clone(), flags: args.flags.clone() };
                a2.kv.insert("ibs-cut".into(), ib.to_string());
                let (mut cfg, factory) = build(&a2, n, k, num(args, "sigma-a", 0.02), num(args, "sigma-b", 0.006));
                cfg.warmup_days = 104; // common start for all configs (90 + 14)
                let r = run_with(&cfg, &data, &*factory);
                series.push((format!("N={n} k={k} ibs={ib}"), r.daily_returns, r.dates));
            }
        }
    }
    let len = series.iter().map(|s| s.1.len()).min().unwrap_or(0);
    let mats: Vec<Vec<f64>> = series.iter().map(|s| s.1[s.1.len() - len..].to_vec()).collect();
    let best = (0..mats.len())
        .max_by(|a, b| performance(&mats[*a]).sharpe.partial_cmp(&performance(&mats[*b]).sharpe).unwrap_or(std::cmp::Ordering::Equal))
        .ok_or("empty grid")?;
    let sharpes: Vec<f64> = mats.iter().map(|m| sharpe_moments(m).sharpe).collect();
    let mu = sharpes.iter().sum::<f64>() / sharpes.len() as f64;
    let var = sharpes.iter().map(|s| (s - mu).powi(2)).sum::<f64>() / (sharpes.len() as f64 - 1.0).max(1.0);
    let m = sharpe_moments(&mats[best]);
    let trials = num(args, "trials", mats.len() as f64) as usize;
    println!("## MERIDIAN validate — X0 anti-overfitting gate\n");
    println!("configs tested: {} · common days: {} · best: **{}**\n", mats.len(), len, series[best].0);
    println!("| Config | Sharpe | CAGR | MDD |\n|---|---|---|---|");
    for (i, s) in series.iter().enumerate() {
        let p = performance(&mats[i]);
        println!("| {}{} | {:.2} | {:+.1} % | {:.1} % |", s.0, if i == best { " ★" } else { "" }, p.sharpe, p.cagr * 100.0, p.max_drawdown * 100.0);
    }
    let dsr = deflated_sharpe(&m, trials, var);
    println!("\n**Deflated Sharpe Ratio** (trials={trials}, var across trials={var:.2e}): **{dsr:.3}** → {}", if dsr >= 0.95 { "PASS (≥ 0.95)" } else { "FAIL (< 0.95)" });
    match pbo(&mats, 16) {
        Some(p) => println!(
            "**PBO (CSCV, S=16, {} combos)**: **{:.1} %** → {} · OOS Sharpe of IS-best {:.2} vs family mean {:.2}",
            p.combinations,
            p.pbo * 100.0,
            if p.pbo <= 0.10 { "PASS (≤ 10 %)" } else { "FAIL (> 10 %)" },
            p.oos_sharpe_of_selected,
            p.oos_sharpe_mean
        ),
        None => println!("PBO: not enough data"),
    }
    if let Some(b) = block_bootstrap(&mats[best], 20, 5_000, 2026) {
        let point = performance(&mats[best]);
        println!(
            "**Block bootstrap** (20-day blocks, 5 000 replicas): Sharpe 5–95 % [{:.2}, {:.2}] · MDD 5 % worst {:.1} % (observed {:.1} %) → {} · P(loss) {:.1} %",
            b.sharpe_p05,
            b.sharpe_p95,
            b.mdd_p05 * 100.0,
            point.max_drawdown * 100.0,
            if b.mdd_p05 >= 2.0 * point.max_drawdown { "PASS (≤ 2× observed)" } else { "FAIL (> 2× observed)" },
            b.prob_loss * 100.0
        );
    }
    Ok(())
}

fn main() {
    let args = parse_args();
    let res = match args.cmd.as_str() {
        "backtest" => cmd_backtest(&args),
        "validate" => cmd_validate(&args),
        "backtest-pm" => pm::cmd_backtest_pm(&args),
        "sweep-pm" => pm::cmd_sweep_pm(&args),
        "orb" => pm::cmd_orb(&args),
        "classify-news" => pm::cmd_classify(&args),
        _ => Err("usage: meridian backtest|validate --spy PATH [--qqq PATH] [--sleeve a|b|ab] [--oos YYYY-MM-DD] … | backtest-pm --news-dir DIR --bars-dir DIR [--classes tradeable|analyst|all] | classify-news --news-dir DIR".into()),
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
