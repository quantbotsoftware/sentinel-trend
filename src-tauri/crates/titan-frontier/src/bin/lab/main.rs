//! Chat 8 lab — the pre-registered E1 / E2 runs on the real MERIDIAN engine
//! (`titan_meridian::sim::run_with`), outside the live strategy hash.
//!
//! lab --spy PATH --qqq PATH --out DIR
//!
//! Scenarios (all: N 14, k 1/1.25/1.5, σ_A 1 %, L 2, σ_B 0.6 %, B offset 11,
//! overlay ON, $100k fixed, $0.01/share):
//!   base  A (engine sleeve) + B                      — must reproduce e3b4f80
//!   e1    A on 9 members (offsets 0/10/20 × k) + B
//!   d0/d10/d20  A alone on one grid offset (diagnostic)
//!   a     A alone, engine sleeve (E2 part 1; must equal d0)
//!   b     B alone, caps only, no loss states (E2 part 2)
//! Writes `DIR/<scenario>.csv` (date, pnl, A, B) and prints a Markdown table.

use std::collections::BTreeMap;
use std::path::PathBuf;

use titan_frontier::grid::GridMomentum;
use titan_meridian::data::load_jsonl_days;
use titan_meridian::risk::RiskParams;
use titan_meridian::sim::{run_with, SimConfig, SimResult, SymbolData};
use titan_meridian::sleeve::{Sleeve, SleeveId};
use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

const CAPITAL: f64 = 100_000.0;
const MULTS: [f64; 3] = [1.0, 1.25, 1.5];
const POST_PUB: &str = "2024-06-01";

#[derive(Clone, Copy)]
enum AKind {
    None,
    Engine,
    Grid(&'static [usize]),
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    a: AKind,
    b: bool,
    loss_states: bool,
}

const SCENARIOS: [Scenario; 7] = [
    Scenario { name: "base", a: AKind::Engine, b: true, loss_states: true },
    Scenario { name: "e1", a: AKind::Grid(&[0, 10, 20]), b: true, loss_states: true },
    Scenario { name: "d0", a: AKind::Grid(&[0]), b: false, loss_states: true },
    Scenario { name: "d10", a: AKind::Grid(&[10]), b: false, loss_states: true },
    Scenario { name: "d20", a: AKind::Grid(&[20]), b: false, loss_states: true },
    Scenario { name: "a", a: AKind::Engine, b: false, loss_states: true },
    Scenario { name: "b", a: AKind::None, b: true, loss_states: false },
];

fn a_params() -> IntradayParams {
    IntradayParams { lookback_days: 14, band_mult: 1.0, sigma_target_daily: 0.01, max_leverage: 2.0, allow_short: true }
}

fn b_params() -> OvernightParams {
    OvernightParams { sigma_target_night: 0.006, decide_bars_before_close: 11, ..Default::default() }
}

fn sleeves_for(sc: Scenario) -> Vec<Box<dyn Sleeve>> {
    let mut v: Vec<Box<dyn Sleeve>> = Vec::new();
    match sc.a {
        AKind::None => {}
        AKind::Engine => v.extend(IntradayMomentum::ensemble(a_params(), &MULTS).into_iter().map(|m| Box::new(m) as Box<dyn Sleeve>)),
        AKind::Grid(offs) => v.extend(GridMomentum::ensemble(a_params(), &MULTS, offs).into_iter().map(|m| Box::new(m) as Box<dyn Sleeve>)),
    }
    if sc.b {
        v.push(Box::new(OvernightCarry::new(b_params())));
    }
    v
}

fn config(sc: Scenario) -> SimConfig {
    let risk = if sc.loss_states { RiskParams::default() } else { RiskParams { daily_loss_pct: 1.0, weekly_loss_pct: 1.0, monthly_loss_pct: 1.0, ..RiskParams::default() } };
    SimConfig {
        initial_equity: CAPITAL,
        slippage_per_share: 0.01,
        envelope_lookback: 14,
        envelope_mult: 1.0,
        warmup_days: 28,
        fixed_sizing_equity: Some(CAPITAL),
        risk: Some(risk),
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
}

fn summarize(sc: Scenario, r: &SimResult) -> Row {
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
    format!(
        "| {} | {:+.0} | {:.3} | {:.2} | {:.2} | {:.0} | {:.0} | {:.0} | {} | {} | {:.2} | {:.2} |",
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
        r.max_gross
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
    let rows: Vec<Row> = std::thread::scope(|s| {
        let hs: Vec<_> = SCENARIOS
            .iter()
            .map(|&sc| {
                s.spawn(move || {
                    let r = run_with(&config(sc), data, &|_| sleeves_for(sc));
                    eprintln!("{}: done ({} fills)", sc.name, r.trades.len());
                    summarize(sc, &r)
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
    println!("| scenario | $/yr net | Sharpe | pre-pub | post-pub | MDD $ | worst day $ | costs $ | fills | halts | max/sym | max gross |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        println!("{}", line(r));
    }
    // E2: A alone (own loss budget) + B alone (caps only), $ curves added on common dates.
    let (Some(a), Some(b)) = (rows.iter().find(|r| r.name == "a"), rows.iter().find(|r| r.name == "b")) else {
        return Err("missing a/b".into());
    };
    let bmap: BTreeMap<&str, f64> = b.dates.iter().map(|d| d.as_str()).zip(b.pnl.iter().copied()).collect();
    let amap: BTreeMap<&str, f64> = a.dates.iter().map(|d| d.as_str()).zip(a.pnl.iter().copied()).collect();
    let mut all: Vec<&str> = amap.keys().chain(bmap.keys()).copied().collect();
    all.sort();
    all.dedup();
    let e2 = Row {
        name: "e2",
        dates: all.iter().map(|d| d.to_string()).collect(),
        pnl: all.iter().map(|d| amap.get(d).copied().unwrap_or(0.0) + bmap.get(d).copied().unwrap_or(0.0)).collect(),
        a: all.iter().map(|d| amap.get(d).copied().unwrap_or(0.0)).collect(),
        b: all.iter().map(|d| bmap.get(d).copied().unwrap_or(0.0)).collect(),
        cost: a.cost + b.cost,
        fills: a.fills + b.fills,
        halts: a.halts,
        max_sym: a.max_sym.max(b.max_sym),
        max_gross: a.max_gross + b.max_gross,
    };
    println!("{}", line(&e2));
    write_csv(&out, &e2)?;
    Ok(())
}
