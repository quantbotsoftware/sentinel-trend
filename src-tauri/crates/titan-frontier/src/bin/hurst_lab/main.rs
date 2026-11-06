//! Hurst gate lab — the single pre-registered run of
//! `docs/meridian/validation/2026-09-26_hurst_gate_preregistration.md` on
//! the real MERIDIAN engine (`titan_meridian::sim::run_with`), outside the
//! live strategy hash.
//!
//! hurst_lab --spy PATH --qqq PATH --out DIR
//!
//! Scenarios (all: N 14, k 1/1.25/1.5, σ_A 1 %, L 2, σ_B 0.6 %, B offset 11,
//! overlay ON, $100k fixed, $0.01/share; gate window 390 intraday returns):
//!   base  A (engine sleeve) + B          — must reproduce 17 963 fills / $43 058
//!   a     A alone                        — must reproduce 12 953 fills
//!   h1    A gated H>0.55 + B             — PRIMARY vs base
//!   h1a   A gated H>0.55 alone           — attribution vs a
//!   hn    A gated H∉[0.45,0.55] + B      — literal proposal (diagnostic)
//!   h50   A gated H>0.50 alone           — weakest threshold (diagnostic)
//! Writes `DIR/<scenario>.csv` (date, pnl, A, B) and prints a Markdown table.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use titan_frontier::hurst_gate::{GateRule, GateStats, HurstGated};
use titan_meridian::data::load_jsonl_days;
use titan_meridian::risk::RiskParams;
use titan_meridian::sim::{run_with, SimConfig, SimResult, SymbolData};
use titan_meridian::sleeve::{Sleeve, SleeveId};
use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

const CAPITAL: f64 = 100_000.0;
const MULTS: [f64; 3] = [1.0, 1.25, 1.5];
const POST_PUB: &str = "2024-06-01";
const WINDOW: usize = 390;

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    gate: Option<GateRule>,
    b: bool,
}

const SCENARIOS: [Scenario; 6] = [
    Scenario { name: "base", gate: None, b: true },
    Scenario { name: "a", gate: None, b: false },
    Scenario { name: "h1", gate: Some(GateRule::TrendAbove(0.55)), b: true },
    Scenario { name: "h1a", gate: Some(GateRule::TrendAbove(0.55)), b: false },
    Scenario { name: "hn", gate: Some(GateRule::OutsideBand(0.45, 0.55)), b: true },
    Scenario { name: "h50", gate: Some(GateRule::TrendAbove(0.50)), b: false },
];

fn a_params() -> IntradayParams {
    IntradayParams { lookback_days: 14, band_mult: 1.0, sigma_target_daily: 0.01, max_leverage: 2.0, allow_short: true }
}

fn b_params() -> OvernightParams {
    OvernightParams { sigma_target_night: 0.006, decide_bars_before_close: 11, ..Default::default() }
}

fn sleeves_for(sc: Scenario, stats: &Arc<GateStats>) -> Vec<Box<dyn Sleeve>> {
    let mut v: Vec<Box<dyn Sleeve>> = Vec::new();
    for m in IntradayMomentum::ensemble(a_params(), &MULTS) {
        match sc.gate {
            None => v.push(Box::new(m)),
            Some(rule) => v.push(Box::new(HurstGated::new(m, rule, WINDOW, stats.clone()))),
        }
    }
    if sc.b {
        v.push(Box::new(OvernightCarry::new(b_params())));
    }
    v
}

fn config() -> SimConfig {
    SimConfig {
        initial_equity: CAPITAL,
        slippage_per_share: 0.01,
        envelope_lookback: 14,
        envelope_mult: 1.0,
        warmup_days: 28,
        fixed_sizing_equity: Some(CAPITAL),
        risk: Some(RiskParams::default()),
        ..Default::default()
    }
}

/// Daily net $ P/L (fixed sizing ⇒ equity differences are additive).
fn daily_pnl(r: &SimResult) -> Vec<f64> {
    let mut prev = CAPITAL;
    r.equity
        .iter()
        .map(|e| {
            let d = e - prev;
            prev = *e;
            d
        })
        .collect()
}

fn sharpe(x: &[f64]) -> f64 {
    let n = x.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let m = x.iter().sum::<f64>() / n;
    let sd = (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
    if sd > 0.0 { m / sd * 252f64.sqrt() } else { 0.0 }
}

fn mdd(x: &[f64]) -> f64 {
    let (mut cum, mut peak, mut worst) = (0.0f64, 0.0f64, 0.0f64);
    for v in x {
        cum += v;
        peak = peak.max(cum);
        worst = worst.min(cum - peak);
    }
    worst
}

/// Max |exposure| per symbol and max gross (fraction of capital), from the fills.
fn exposure(r: &SimResult) -> (f64, f64) {
    let mut pos: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    let (mut max_sym, mut max_gross) = (0.0f64, 0.0f64);
    let mut i = 0;
    while i < r.trades.len() {
        let key = (r.trades[i].date, r.trades[i].bar_idx);
        while i < r.trades.len() && (r.trades[i].date, r.trades[i].bar_idx) == key {
            let t = &r.trades[i];
            let e = pos.entry(t.symbol.as_str()).or_insert((0.0, 0.0));
            e.0 += t.delta_shares;
            e.1 = t.fill_price;
            max_sym = max_sym.max((e.0 * e.1).abs() / CAPITAL);
            i += 1;
        }
        max_gross = max_gross.max(pos.values().map(|(q, p)| (q * p).abs()).sum::<f64>() / CAPITAL);
    }
    (max_sym, max_gross)
}

struct Row {
    name: &'static str,
    dates: Vec<String>,
    pnl: Vec<f64>,
    a: Vec<f64>,
    b: Vec<f64>,
    cost: f64,
    fills: usize,
    halts: u32,
    max_sym: f64,
    max_gross: f64,
    blocked: f64,
    decisions: u64,
    h_pct: (f64, f64, f64),
}

fn summarize(sc: Scenario, r: &SimResult, stats: &GateStats) -> Row {
    let pick = |id: SleeveId| r.sleeve_pnl.get(&id).cloned().unwrap_or_else(|| vec![0.0; r.dates.len()]);
    let (max_sym, max_gross) = exposure(r);
    Row {
        name: sc.name,
        dates: r.dates.iter().map(|d| d.to_string()).collect(),
        pnl: daily_pnl(r),
        a: pick(SleeveId::A),
        b: pick(SleeveId::B),
        cost: r.total_cost,
        fills: r.trades.len(),
        halts: r.risk_halts,
        max_sym,
        max_gross,
        blocked: stats.blocked_fraction(),
        decisions: stats.decisions.load(Ordering::Relaxed),
        h_pct: stats.h_percentiles(),
    }
}

fn split_post(row: &Row, v: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let (mut pre, mut post) = (Vec::new(), Vec::new());
    for (d, x) in row.dates.iter().zip(v) {
        if d.as_str() >= POST_PUB { post.push(*x) } else { pre.push(*x) }
    }
    (pre, post)
}

fn line(r: &Row) -> String {
    let years = r.dates.len() as f64 / 252.0;
    let (pre, post) = split_post(r, &r.pnl);
    let worst = r.pnl.iter().copied().fold(f64::MAX, f64::min);
    let (p10, p50, p90) = r.h_pct;
    format!(
        "| {} | {:+.0} | {:.3} | {:.2} | {:.2} | {:.0} | {:.0} | {:.0} | {} | {} | {:.2} | {:.2} | {:.1} % ({}) | {:.2}/{:.2}/{:.2} |",
        r.name,
        r.pnl.iter().sum::<f64>() / years,
        sharpe(&r.pnl),
        sharpe(&pre),
        sharpe(&post),
        mdd(&r.pnl),
        worst,
        r.cost,
        r.fills,
        r.halts,
        r.max_sym,
        r.max_gross,
        r.blocked * 100.0,
        r.decisions,
        p10,
        p50,
        p90
    )
}

fn write_csv(dir: &std::path::Path, row: &Row) -> Result<(), String> {
    let mut s = String::from("date,pnl,A,B\n");
    for i in 0..row.dates.len() {
        s.push_str(&format!("{},{:.2},{:.2},{:.2}\n", row.dates[i], row.pnl[i], row.a[i], row.b[i]));
    }
    std::fs::write(dir.join(format!("{}.csv", row.name)), s).map_err(|e| e.to_string())
}

fn arg(name: &str) -> Option<String> {
    let v: Vec<String> = std::env::args().collect();
    v.iter().position(|a| a == name).and_then(|i| v.get(i + 1).cloned())
}

fn main() -> Result<(), String> {
    let spy = arg("--spy").ok_or("need --spy")?;
    let qqq = arg("--qqq").ok_or("need --qqq")?;
    let out = PathBuf::from(arg("--out").ok_or("need --out")?);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    for (sym, p) in [("SPY", spy), ("QQQ", qqq)] {
        let (days, st) = load_jsonl_days(&PathBuf::from(&p), 300).map_err(|e| format!("{sym}: {e}"))?;
        eprintln!("{sym}: {} sessions kept, {} dropped", st.sessions_kept, st.sessions_dropped_incomplete);
        data.push(SymbolData { symbol: sym.to_string(), days });
    }
    let data = &data;
    let cfg = config();
    let rows: Vec<Row> = std::thread::scope(|s| {
        let hs: Vec<_> = SCENARIOS
            .iter()
            .map(|&sc| {
                let cfg = &cfg;
                s.spawn(move || {
                    let stats = Arc::new(GateStats::default());
                    let r = run_with(cfg, data, &|_| sleeves_for(sc, &stats));
                    eprintln!("{}: done ({} fills)", sc.name, r.trades.len());
                    summarize(sc, &r, &stats)
                })
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    if rows.len() != SCENARIOS.len() {
        return Err("a scenario thread panicked".into());
    }
    for r in &rows {
        write_csv(&out, r)?;
    }
    println!("| scenario | $/yr net | Sharpe | pre-pub | post-pub | MDD $ | worst day $ | costs $ | fills | halts | max/sym | max gross | blocked (decisions) | H p10/p50/p90 |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        println!("{}", line(r));
    }
    Ok(())
}
