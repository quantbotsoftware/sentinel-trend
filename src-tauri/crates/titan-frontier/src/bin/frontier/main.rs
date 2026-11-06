//! MERIDIAN frontier CLI.
//!
//! frontier backtest --dir DIR [--symbols IWM,…] [--calendar SPY] [--lookbacks 252,126]
//!                   [--start-lookback 252] [--ab-csv PATH] [--csv-prefix PATH]
//!                   [--from YYYY-MM-DD] [--to YYYY-MM-DD] [--stooq] [--label TEXT]
//! frontier bocpd --ref sleeves_ref.csv --live sleeves_live.csv --since YYYY-MM-DD [--k 10] [--hazard 120]
//!
//! `backtest` prints the Markdown report of the pre-registered F1 run; with
//! `--ab-csv` it evaluates the A+B+T gate. `bocpd` feeds `spc::Bocpd` with
//! per-sleeve engine P/L (the `--sleeve-csv` files of `meridian backtest`)
//! and prints the change-point probabilities the daily report shows (F4).

mod report;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use titan_frontier::curve::stdev;
use titan_frontier::daily::{load_alpaca_jsonl, load_stooq_csv};
use titan_frontier::gate::{combine, cost_share, evaluate, var_across_trials, Thresholds};
use titan_frontier::{run, Curve, Panel, Series, SignalParams, SimParams, TResult};
use titan_meridian::spc::Bocpd;
use titan_meridian::stats::sharpe_moments;

pub struct Args {
    pub cmd: String,
    pub kv: HashMap<String, String>,
    pub flags: Vec<String>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_default();
    let (mut kv, mut flags) = (HashMap::new(), Vec::new());
    let rest: Vec<String> = it.collect();
    let mut i = 0;
    while i < rest.len() {
        if let Some(k) = rest[i].strip_prefix("--") {
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

fn date(a: &Args, k: &str) -> Option<NaiveDate> {
    a.kv.get(k).and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

const UNIVERSE: &str = "IWM,EFA,EEM,TLT,IEF,LQD,GLD,SLV,VNQ,DBC";

/// `DIR/SYM_*.jsonl` (Alpaca) or `DIR/sym*.csv` (Stooq).
fn find_file(dir: &Path, sym: &str, stooq: bool) -> Result<PathBuf, String> {
    let ext = if stooq { "csv" } else { "jsonl" };
    let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_uppercase();
            name.starts_with(&format!("{}_", sym.to_uppercase())) && name.ends_with(&format!(".{}", ext.to_uppercase()))
        })
        .collect();
    hits.sort();
    hits.pop().ok_or_else(|| format!("no {ext} file for {sym} in {}", dir.display()))
}

fn load_series(dir: &Path, sym: &str, stooq: bool) -> Result<Series, String> {
    let p = find_file(dir, sym, stooq)?;
    let s = if stooq { load_stooq_csv(&p, sym) } else { load_alpaca_jsonl(&p, sym) }.map_err(|e| format!("{sym}: {e}"))?;
    eprintln!("{sym}: {} closes {} → {} ({})", s.rows.len(), s.rows.first().map(|r| r.0.to_string()).unwrap_or_default(), s.rows.last().map(|r| r.0.to_string()).unwrap_or_default(), p.display());
    Ok(s)
}

fn cmd_backtest(args: &Args) -> Result<(), String> {
    let dir = PathBuf::from(args.kv.get("dir").ok_or("need --dir")?);
    let stooq = args.flags.iter().any(|f| f == "stooq");
    let symbols: Vec<String> = args.kv.get("symbols").map(|s| s.as_str()).unwrap_or(UNIVERSE).split(',').map(|s| s.trim().to_uppercase()).collect();
    let cal_sym = args.kv.get("calendar").cloned().unwrap_or_else(|| "SPY".into());
    let capital = args.kv.get("capital").and_then(|v| v.parse().ok()).unwrap_or(100_000.0);
    let series: Vec<Series> = symbols.iter().map(|s| load_series(&dir, s, stooq)).collect::<Result<_, _>>()?;
    let spy = load_series(&dir, &cal_sym, stooq).ok();
    // calendar: the reference symbol's dates, else the union of all dates
    let mut calendar: Vec<NaiveDate> = match &spy {
        Some(s) => s.rows.iter().map(|r| r.0).collect(),
        None => {
            let mut v: Vec<NaiveDate> = series.iter().flat_map(|s| s.rows.iter().map(|r| r.0)).collect();
            v.sort_unstable();
            v.dedup();
            v
        }
    };
    if let Some(to) = date(args, "to") {
        calendar.retain(|d| *d <= to);
    }
    let panel = Panel::build(&calendar, &series);
    let lookbacks: Vec<usize> = args.kv.get("lookbacks").map(|s| s.as_str()).unwrap_or("252,126").split(',').filter_map(|x| x.trim().parse().ok()).collect();
    let start_lb: usize = args.kv.get("start-lookback").and_then(|v| v.parse().ok()).unwrap_or(252);
    let mut start = panel.first_full_history(start_lb).ok_or("not enough history for the common start")?;
    if let Some(from) = date(args, "from") {
        start = start.max(panel.dates.iter().position(|d| *d >= from).unwrap_or(start));
    }
    let results: Vec<(usize, TResult)> = lookbacks
        .iter()
        .map(|&lb| {
            let p = SimParams { signal: SignalParams { lookback: lb, ..SignalParams::default() }, capital, ..SimParams::default() };
            (lb, run(&panel, &p, start))
        })
        .collect();
    let spy_pnl = |r: &TResult| -> Option<Vec<f64>> {
        let s = spy.as_ref()?;
        let m: HashMap<NaiveDate, f64> = s.rows.iter().copied().collect();
        let mut prev: Option<f64> = None;
        let mut out = Vec::with_capacity(r.dates.len());
        // buy-and-hold `capital` in SPY, marked close to close
        for d in &r.dates {
            let px = *m.get(d)?;
            out.push(prev.map(|p| capital / p * (px - p)).unwrap_or(0.0));
            prev = Some(px);
        }
        Some(out)
    };
    let variants: Vec<report::Variant> = results.iter().map(|(lb, r)| report::Variant { label: format!("T L={lb}"), res: r, spy_pnl: spy_pnl(r) }).collect();
    let label = args.kv.get("label").cloned().unwrap_or_else(|| "Sleeve T — F1".into());
    println!("## {label}\n");
    println!("Sursă: {} · univers {} · calendar {} · start comun {} (warm-up {start_lb}) · bază ${capital:.0} · vol țintă 8 %/an, plafon 25 %, brut ≤ 1,0×, bandă 10 %, vineri MOC, $0,01/acț. + SEC $27,80/M + TAF\n", if stooq { "Stooq (robustețe, NU intră în poartă)" } else { "Alpaca 1Day adjustment=all feed=sip" }, symbols.join("/"), cal_sym, panel.dates.get(start).map(|d| d.to_string()).unwrap_or_default());
    println!("{}", report::t_header());
    for v in &variants {
        println!("{}", report::t_summary(v, capital));
    }
    if let Some(prefix) = args.kv.get("csv-prefix") {
        for (lb, r) in &results {
            let mut s = String::from("date,pnl,gross_pnl,costs,gross_exposure,n_active\n");
            for i in 0..r.dates.len() {
                s.push_str(&format!("{},{:.2},{:.2},{:.2},{:.4},{}\n", r.dates[i], r.pnl[i], r.gross_pnl[i], r.costs[i], r.gross_exposure[i], r.n_active[i]));
            }
            let p = format!("{prefix}_L{lb}.csv");
            std::fs::write(&p, s).map_err(|e| e.to_string())?;
            eprintln!("wrote {p}");
        }
    }
    let ab = match args.kv.get("ab-csv") {
        Some(p) => Some(Curve::load_meridian_csv(&PathBuf::from(p)).map_err(|e| format!("ab-csv: {e}"))?),
        None => None,
    };
    println!("\n### P/L pe ani ($, net)\n\n{}", report::yearly_table(&variants, ab.as_ref()));
    let Some(ab) = ab else { return Ok(()) };
    // combination on the common window; cross-trial variance from every trial run here
    let mut combos = Vec::new();
    for (lb, r) in &results {
        let (dates, a, t) = Curve::align(&ab, &Curve::new(r.dates.clone(), r.pnl.clone()));
        let c = combine(&a, &t, capital);
        let costs: f64 = r.costs.iter().zip(&r.dates).filter(|(_, d)| dates.contains(d)).map(|(c, _)| c).sum();
        let gross: f64 = r.gross_pnl.iter().zip(&r.dates).filter(|(_, d)| dates.contains(d)).map(|(g, _)| g).sum();
        combos.push((*lb, c, cost_share(costs, gross), t));
    }
    let mut per_obs: Vec<f64> = combos.iter().map(|(_, c, _, _)| sharpe_moments(&c.comb_pnl.iter().map(|p| p / capital).collect::<Vec<_>>()).sharpe).collect();
    if let Some((_, c, _, _)) = combos.first() {
        let n = c.n;
        let (_, a, _) = Curve::align(&ab, &Curve::new(results[0].1.dates.clone(), results[0].1.pnl.clone()));
        per_obs.push(sharpe_moments(&a[..n].iter().map(|p| p / capital).collect::<Vec<_>>()).sharpe);
    }
    let thr = Thresholds::default();
    let var = var_across_trials(&per_obs, thr.var_floor);
    println!("Varianța Sharpe-urilor per observație între încercările {{AB, AB+T…}}: {:.2e} (sd {:.2e}); podea X0 1,38e-5 ⇒ folosită {:.2e}; N = {} încercări\n", stdev(&per_obs).powi(2), stdev(&per_obs), var, thr.trials);
    for (i, (lb, c, cs, t)) in combos.iter().enumerate() {
        let tr: Vec<f64> = t.iter().map(|p| p / capital).collect();
        let g = evaluate(c, &tr, *cs, var, &thr, capital);
        println!("{}", report::gate_table(&format!("L={lb}"), c, &g, i == 0, capital));
    }
    Ok(())
}

/// Per-sleeve `(id, [(date, $ P/L)])`.
type SleeveSeries = Vec<(String, Vec<(NaiveDate, f64)>)>;

/// `date,id1,id2,…` → per-sleeve `(dates, values)`.
fn load_sleeve_csv(path: &str) -> Result<SleeveSeries, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut lines = text.lines();
    let head: Vec<&str> = lines.next().ok_or("empty csv")?.split(',').skip(1).collect();
    let mut out: SleeveSeries = head.iter().map(|h| (h.to_string(), Vec::new())).collect();
    for l in lines {
        let cols: Vec<&str> = l.split(',').collect();
        let Ok(d) = NaiveDate::parse_from_str(cols[0], "%Y-%m-%d") else { continue };
        for (i, v) in cols.iter().skip(1).enumerate() {
            if let (Some(slot), Ok(x)) = (out.get_mut(i), v.trim().parse::<f64>()) {
                slot.1.push((d, x));
            }
        }
    }
    Ok(out)
}

/// F4: BOCPD per sleeve, warmed on the archive backtest, continued on the live
/// engine replay since `--since`. Prints one line per sleeve.
fn cmd_bocpd(args: &Args) -> Result<(), String> {
    let reference = load_sleeve_csv(args.kv.get("ref").ok_or("need --ref")?)?;
    let live = load_sleeve_csv(args.kv.get("live").ok_or("need --live")?)?;
    let since = date(args, "since").ok_or("need --since YYYY-MM-DD")?;
    let k: usize = args.kv.get("k").and_then(|v| v.parse().ok()).unwrap_or(10);
    let hazard: f64 = args.kv.get("hazard").and_then(|v| v.parse().ok()).unwrap_or(120.0);
    for (id, hist) in &reference {
        let xs: Vec<f64> = hist.iter().map(|r| r.1).collect();
        if xs.len() < 2 {
            continue;
        }
        let mu = xs.iter().sum::<f64>() / xs.len() as f64;
        let sd = stdev(&xs).max(1e-9);
        let mut b = Bocpd::new(1.0 / hazard, sd * sd, mu, sd * sd, 400);
        for x in &xs {
            b.push(*x);
        }
        let last_ref = hist.last().map(|r| r.0).unwrap_or(since);
        let live_rows: Vec<&(NaiveDate, f64)> = live.iter().find(|(lid, _)| lid == id).map(|(_, v)| v.iter().filter(|r| r.0 >= since && r.0 > last_ref).collect()).unwrap_or_default();
        let mut recent = Vec::new();
        for r in &live_rows {
            b.push(r.1);
            recent.push(r.1);
        }
        let n_live = recent.len();
        let run = b.most_likely_run_length();
        // mean of the current (most likely) regime, over the live days it covers
        let seg: Vec<f64> = recent.iter().rev().take(run.max(1)).copied().collect();
        let seg_mean = if seg.is_empty() { f64::NAN } else { seg.iter().sum::<f64>() / seg.len() as f64 };
        let p_recent = b.prob_recent_change(k);
        let p_paper = b.prob_recent_change(n_live + 1);
        let state = if n_live < k {
            "warm-up"
        } else if p_paper >= 0.5 && run <= n_live && seg_mean < mu {
            "RUPTURĂ?"
        } else if p_paper >= 0.5 && run <= n_live {
            "schimbare (în sus)"
        } else {
            "ok"
        };
        println!(
            "{id}: P(ruptură de la startul paper)={p_paper:.2} · P(ultimele {k} zile)={p_recent:.2} · regim curent {run} zile ({}) · {n_live} zile live · medie regim ${seg_mean:.0}/zi vs μ ${mu:.0} → {state}",
            if run > n_live { "dinainte de paper" } else { "început în paper" }
        );
    }
    Ok(())
}

fn main() {
    let args = parse_args();
    let res = match args.cmd.as_str() {
        "backtest" => cmd_backtest(&args),
        "bocpd" => cmd_bocpd(&args),
        _ => Err("usage: frontier backtest --dir DIR [--ab-csv PATH] [--lookbacks 252,126] [--stooq] | frontier bocpd --ref CSV --live CSV --since YYYY-MM-DD".into()),
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
