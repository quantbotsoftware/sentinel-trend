//! Catalyst / premarket research commands (C-PM, C-ORB, news classification).
//! The line is closed (`docs/meridian/validation/2026-09-18_corb2_result.md`);
//! the commands stay so the pre-registered runs remain reproducible.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::NaiveDate;
use titan_meridian::catalyst::CatalystClass;
use titan_meridian::data::{load_news, NewsEvent};
use titan_meridian::sim::{markdown_report, run_pm, ClassFilter, PmSimConfig};
use titan_meridian::sleeves::PmParams;
use titan_meridian::stats::{deflated_sharpe, pbo, performance, sharpe_moments};

use super::{num, oos_date, Args};

pub fn load_all_news(args: &Args) -> Result<Vec<NewsEvent>, String> {
    let dir = args.kv.get("news-dir").ok_or("need --news-dir DIR (news_pm_*.jsonl[.gz])")?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("news_pm_") && n.contains(".jsonl")).unwrap_or(false))
        .collect();
    paths.sort();
    let mut all = Vec::new();
    for p in &paths {
        all.extend(load_news(p).map_err(|e| format!("{}: {e}", p.display()))?);
    }
    all.sort_by(|a, b| (a.date, a.idx).cmp(&(b.date, b.idx)));
    eprintln!("news: {} files, {} symbol-headlines", paths.len(), all.len());
    Ok(all)
}

/// Class distribution of the archive — sanity check of the classifier on real data.
pub fn cmd_classify(args: &Args) -> Result<(), String> {
    let all = load_all_news(args)?;
    let mut counts: BTreeMap<CatalystClass, usize> = BTreeMap::new();
    let mut sample: BTreeMap<CatalystClass, Vec<String>> = BTreeMap::new();
    for e in &all {
        *counts.entry(e.class).or_default() += 1;
        let v = sample.entry(e.class).or_default();
        if v.len() < 3 && e.idx % 7 == 3 {
            v.push(e.headline.chars().take(90).collect());
        }
    }
    println!("| Class | Count | Share | Samples |\n|---|---|---|---|");
    for (c, n) in &counts {
        println!("| {} | {} | {:.1} % | {} |", c.as_str(), n, *n as f64 / all.len() as f64 * 100.0, sample.get(c).map(|v| v.join(" · ")).unwrap_or_default());
    }
    Ok(())
}

pub fn cmd_backtest_pm(args: &Args) -> Result<(), String> {
    let all = load_all_news(args)?;
    let dir = PathBuf::from(args.kv.get("bars-dir").ok_or("need --bars-dir DIR")?);
    let filter = match args.kv.get("classes").map(|s| s.as_str()).unwrap_or("tradeable") {
        "tradeable" => ClassFilter::Tradeable,
        "analyst" => ClassFilter::Analyst,
        "all" => ClassFilter::All,
        "ma" => ClassFilter::Only(CatalystClass::MergerAcquisition),
        "fda" => ClassFilter::Only(CatalystClass::FdaPositive),
        "earnings" => ClassFilter::Only(CatalystClass::EarningsBeat),
        other => return Err(format!("unknown --classes {other}")),
    };
    let params = PmParams {
        min_move: num(args, "min-move", 0.05),
        min_move_post: num(args, "min-move-post", 0.02),
        min_stop_frac: num(args, "min-stop", 0.03),
        min_dollar_volume: num(args, "min-dollar", 500_000.0),
        min_trades: num(args, "min-trades", 200.0),
        window_min: num(args, "window", 15.0) as usize,
        risk_frac: num(args, "risk", 0.005),
        open_exit_frac: num(args, "open-exit", 0.5),
        ..Default::default()
    };
    let cfg = PmSimConfig { params, equity: num(args, "equity", 100_000.0), max_positions_per_day: num(args, "max-pos", 3.0) as usize, filter };
    let r = run_pm(&cfg, &all, &dir);
    let sim = r.to_sim_result(cfg.equity);
    let title = format!(
        "MERIDIAN C-PM backtest — classes {:?} · move ≥ {:.0} % · $vol ≥ {:.0}k · window {} min · risk {:.2} %",
        filter,
        params.min_move * 100.0,
        params.min_dollar_volume / 1000.0,
        params.window_min,
        params.risk_frac * 100.0
    );
    println!("{}", markdown_report(&title, &sim, oos_date(args)));
    println!(
        "Archive days {} (missing {}) · candidates {} · trades {} · total P/L ${:.0} · costs ${:.0}\n",
        r.days_with_archive,
        r.days_missing_archive,
        r.candidates,
        r.trades.len(),
        r.trades.iter().map(|t| t.pnl).sum::<f64>(),
        sim.total_cost
    );
    println!("| Class | Trades | Hit | Avg R | PF | P/L $ | Cost $ |\n|---|---|---|---|---|---|---|");
    for (c, n, hit, avg_r, pf, pnl, cost) in r.class_table() {
        println!("| {} | {} | {:.0} % | {:+.2} | {:.2} | {:+.0} | {:.0} |", c.as_str(), n, hit * 100.0, avg_r, pf, pnl, cost);
    }
    println!("\nSkips: {}", r.skipped.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "));
    if let Some(p) = args.kv.get("trades-csv") {
        let mut s = String::from("date,symbol,class,event_idx,entry_idx,entry_px,shares,pnl,r,cost,last_exit,headline\n");
        for t in &r.trades {
            s.push_str(&format!(
                "{},{},{},{},{},{:.4},{},{:.2},{:.3},{:.2},{:?},\"{}\"\n",
                t.date, t.symbol, t.class.as_str(), t.event_idx, t.entry_idx, t.entry_px, t.shares, t.pnl, t.r_multiple, t.cost, t.last_exit, t.headline.replace('"', "'")
            ));
        }
        std::fs::write(p, s).map_err(|e| e.to_string())?;
        eprintln!("wrote {p}");
    }
    Ok(())
}

/// Pre-registered C-PM parameter family (36 configs) → per-config IS/OOS
/// table, PBO (CSCV) and DSR of the IS-best. The grid is fixed in code on
/// purpose: adding configs after seeing results is what PBO punishes.
/// C-ORB: treatment + both control arms + the 8-config pre-registered
/// family with DSR/PBO, in one run (docs/meridian/validation/2026-09-16_corb_preregistration.md).
pub fn cmd_orb(args: &Args) -> Result<(), String> {
    use titan_meridian::sim::{run_orb_multi, OrbConfig, OrbParams, StopRule};
    let all = load_all_news(args)?;
    let dir = PathBuf::from(args.kv.get("bars-dir").ok_or("need --bars-dir DIR")?);
    let oos = oos_date(args).unwrap_or(NaiveDate::from_ymd_opt(2023, 1, 1).ok_or("date")?);
    let mut names = Vec::new();
    let mut cfgs = Vec::new();
    for (label, filter) in [("treatment", ClassFilter::Tradeable), ("control all", ClassFilter::All), ("control analyst", ClassFilter::Analyst)] {
        names.push(format!("{label} · default (OR5, pm-high, OR-low)"));
        cfgs.push(OrbConfig { filter, ..Default::default() });
    }
    let family_start = cfgs.len();
    // C-ORB-2 (2026-09-18_corb2_preregistration.md): stop fixed at OR low, 4 configs.
    let corb2 = args.kv.get("family").map(|f| f == "corb2").unwrap_or(false);
    let stops: &[StopRule] = if corb2 { &[StopRule::OrLow] } else { &[StopRule::OrLow, StopRule::OrMid] };
    for or_min in [5usize, 15] {
        for confirm in [true, false] {
            for stop in stops {
                names.push(format!("OR{or_min} confirm-pm={confirm} stop={stop:?}"));
                cfgs.push(OrbConfig { params: OrbParams { or_min, confirm_pm_high: confirm, stop: *stop, ..Default::default() }, ..Default::default() });
            }
        }
    }
    let results = run_orb_multi(&cfgs, &all, &dir);
    let equity = cfgs[0].equity;
    println!("## MERIDIAN {} — pre-registered run · OOS from {oos}\n", if corb2 { "C-ORB-2 (hold to close)" } else { "C-ORB" });
    println!("Archive days {} (missing {}) · candidates (treatment) {}\n", results[0].days_with_archive, results[0].days_missing_archive, results[0].candidates);
    println!("| Config | Trades | Hit | PF | Avg R | Sharpe IS | Sharpe OOS | Trades OOS | PF OOS | P/L IS $ | P/L OOS $ |\n|---|---|---|---|---|---|---|---|---|---|---|");
    let mats: Vec<Vec<f64>> = results.iter().map(|r| r.daily_pnl.iter().map(|p| p / equity).collect()).collect();
    let mut best = family_start;
    let mut best_is = f64::MIN;
    for (i, r) in results.iter().enumerate() {
        let split = r.dates.iter().position(|d| *d >= oos).unwrap_or(r.dates.len());
        let (is_r, oos_r) = mats[i].split_at(split);
        let (s_is, s_oos) = (performance(is_r).sharpe, performance(oos_r).sharpe);
        let pf = |ts: &[titan_meridian::sim::OrbTrade]| {
            let g: f64 = ts.iter().filter(|t| t.pnl > 0.0).map(|t| t.pnl).sum();
            let l: f64 = ts.iter().filter(|t| t.pnl < 0.0).map(|t| -t.pnl).sum();
            if l > 0.0 { g / l } else { 0.0 }
        };
        let oos_trades: Vec<_> = r.trades.iter().filter(|t| t.date >= oos).cloned().collect();
        let hit = if r.trades.is_empty() { 0.0 } else { r.trades.iter().filter(|t| t.pnl > 0.0).count() as f64 / r.trades.len() as f64 };
        let avg_r = if r.trades.is_empty() { 0.0 } else { r.trades.iter().map(|t| t.r_multiple).sum::<f64>() / r.trades.len() as f64 };
        if i >= family_start && s_is > best_is {
            best_is = s_is;
            best = i;
        }
        println!(
            "| {} | {} | {:.0} % | {:.2} | {:+.2} | {:.2} | {:.2} | {} | {:.2} | {:+.0} | {:+.0} |",
            names[i], r.trades.len(), hit * 100.0, pf(&r.trades), avg_r, s_is, s_oos, oos_trades.len(), pf(&oos_trades),
            r.daily_pnl[..split].iter().sum::<f64>(), r.daily_pnl[split..].iter().sum::<f64>()
        );
    }
    let fam = &mats[family_start..];
    println!("\nIS-best (family): **{}**", names[best]);
    let sharpes: Vec<f64> = fam.iter().map(|m| sharpe_moments(m).sharpe).collect();
    let mu = sharpes.iter().sum::<f64>() / sharpes.len() as f64;
    let var = sharpes.iter().map(|s| (s - mu).powi(2)).sum::<f64>() / (sharpes.len() as f64 - 1.0).max(1.0);
    let dsr = deflated_sharpe(&sharpe_moments(&mats[best]), fam.len(), var);
    println!("**DSR** of IS-best (trials={}): **{dsr:.3}** → {}", fam.len(), if dsr >= 0.95 { "PASS" } else { "FAIL" });
    match pbo(fam, 16) {
        Some(p) => println!("**PBO (CSCV, S=16)**: **{:.1} %** → {}", p.pbo * 100.0, if p.pbo <= 0.10 { "PASS" } else { "FAIL" }),
        None => println!("**PBO**: n/a"),
    }
    let by_exit = results[0].trades.iter().filter(|t| t.stopped).count();
    let last_exit = results[0].trades.iter().map(|t| t.exit_idx).max().unwrap_or(0);
    println!("\nTreatment exits: {} stopped, {} held to the archive end (last exit idx {last_exit})", by_exit, results[0].trades.len() - by_exit);
    Ok(())
}

pub fn cmd_sweep_pm(args: &Args) -> Result<(), String> {
    let all = load_all_news(args)?;
    let dir = PathBuf::from(args.kv.get("bars-dir").ok_or("need --bars-dir DIR")?);
    let oos = oos_date(args).unwrap_or(NaiveDate::from_ymd_opt(2023, 1, 1).ok_or("date")?);
    let mut names = Vec::new();
    let mut cfgs = Vec::new();
    for mv in [0.03, 0.05, 0.08] {
        for stop in [0.03, 0.05, 0.08] {
            for window in [15usize, 30] {
                for open_exit in [0.5, 1.0] {
                    names.push(format!("move {:.0}% stop {:.0}% win {} open {:.0}%", mv * 100.0, stop * 100.0, window, open_exit * 100.0));
                    cfgs.push(PmSimConfig {
                        params: PmParams { min_move: mv, min_stop_frac: stop, window_min: window, open_exit_frac: open_exit, ..Default::default() },
                        ..Default::default()
                    });
                }
            }
        }
    }
    let results = titan_meridian::sim::run_pm_multi(&cfgs, &all, &dir);
    let equity = cfgs[0].equity;
    let mats: Vec<Vec<f64>> = results.iter().map(|r| r.daily_pnl.iter().map(|p| p / equity).collect()).collect();
    println!("## MERIDIAN C-PM sweep — pre-registered family ({} configs) · OOS from {oos}\n", cfgs.len());
    println!("| Config | Trades | PF | Avg R | Sharpe IS | Sharpe OOS | P/L IS $ | P/L OOS $ |\n|---|---|---|---|---|---|---|---|");
    let mut best = 0usize;
    let mut best_is = f64::MIN;
    for (i, r) in results.iter().enumerate() {
        let split = r.dates.iter().position(|d| *d >= oos).unwrap_or(r.dates.len());
        let (is_r, oos_r) = mats[i].split_at(split);
        let s_is = performance(is_r).sharpe;
        let s_oos = performance(oos_r).sharpe;
        let g: f64 = r.trades.iter().filter(|t| t.pnl > 0.0).map(|t| t.pnl).sum();
        let l: f64 = r.trades.iter().filter(|t| t.pnl < 0.0).map(|t| -t.pnl).sum();
        let avg_r = if r.trades.is_empty() { 0.0 } else { r.trades.iter().map(|t| t.r_multiple).sum::<f64>() / r.trades.len() as f64 };
        if s_is > best_is {
            best_is = s_is;
            best = i;
        }
        println!(
            "| {} | {} | {:.2} | {:+.2} | {:.2} | {:.2} | {:+.0} | {:+.0} |",
            names[i],
            r.trades.len(),
            if l > 0.0 { g / l } else { 0.0 },
            avg_r,
            s_is,
            s_oos,
            r.daily_pnl[..split].iter().sum::<f64>(),
            r.daily_pnl[split..].iter().sum::<f64>()
        );
    }
    println!("\nIS-best: **{}**", names[best]);
    let sharpes: Vec<f64> = mats.iter().map(|m| sharpe_moments(m).sharpe).collect();
    let mu = sharpes.iter().sum::<f64>() / sharpes.len() as f64;
    let var = sharpes.iter().map(|s| (s - mu).powi(2)).sum::<f64>() / (sharpes.len() as f64 - 1.0).max(1.0);
    let dsr = deflated_sharpe(&sharpe_moments(&mats[best]), cfgs.len(), var);
    println!("**DSR** of IS-best over the full period (trials={}): **{dsr:.3}** → {}", cfgs.len(), if dsr >= 0.95 { "PASS" } else { "FAIL" });
    match pbo(&mats, 16) {
        Some(p) => println!(
            "**PBO (CSCV, S=16)**: **{:.1} %** → {} · OOS Sharpe of IS-best {:.2} vs family mean {:.2}",
            p.pbo * 100.0,
            if p.pbo <= 0.10 { "PASS" } else { "FAIL" },
            p.oos_sharpe_of_selected,
            p.oos_sharpe_mean
        ),
        None => println!("PBO: not enough data"),
    }
    Ok(())
}
