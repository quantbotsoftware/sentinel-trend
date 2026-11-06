//! VIX term-structure allocation lab — the single pre-registered run of
//! `docs/meridian/validation/2026-09-26_vix_regime_preregistration.md` on
//! the real MERIDIAN engine (`titan_meridian::sim::run_with`), outside the
//! live strategy hash.
//!
//! vix_lab --spy PATH --qqq PATH --vix CSV --vix3m CSV --out DIR
//!
//! Scenarios (all: N 14, k 1/1.25/1.5, σ_A 1 %, L 2, σ_B 0.6 %, B offset 11,
//! overlay ON, $100k fixed, $0.01/share):
//!   base  A + B                                  — must reproduce 17 963 fills / $43 058
//!   v1    plan: contango ⇒ A ×0.5; backwardation ⇒ B ×0.5   — PRIMARY vs base
//!   v1r   mirror (diagnostic)
//! Also prints the per-regime attribution of `base` (A and B separately on
//! contango vs. backwardation days). Writes `DIR/<scenario>.csv`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::NaiveDate;
use titan_frontier::regime_scale::{Regime, RegimeMap, RegimeScaled, ScaleRule};
use titan_meridian::data::load_jsonl_days;
use titan_meridian::risk::RiskParams;
use titan_meridian::sim::{run_with, SimConfig, SimResult, SymbolData};
use titan_meridian::sleeve::{Sleeve, SleeveId};
use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

const CAPITAL: f64 = 100_000.0;
const MULTS: [f64; 3] = [1.0, 1.25, 1.5];
const POST_PUB: &str = "2024-06-01";

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    rule: Option<ScaleRule>,
}

const SCENARIOS: [Scenario; 3] = [
    Scenario { name: "base", rule: None },
    Scenario { name: "v1", rule: Some(ScaleRule::Plan) },
    Scenario { name: "v1r", rule: Some(ScaleRule::Reverse) },
];

fn a_params() -> IntradayParams {
    IntradayParams { lookback_days: 14, band_mult: 1.0, sigma_target_daily: 0.01, max_leverage: 2.0, allow_short: true }
}

fn b_params() -> OvernightParams {
    OvernightParams { sigma_target_night: 0.006, decide_bars_before_close: 11, ..Default::default() }
}

fn sleeves_for(sc: Scenario, map: &Arc<RegimeMap>) -> Vec<Box<dyn Sleeve>> {
    let mut v: Vec<Box<dyn Sleeve>> = Vec::new();
    for m in IntradayMomentum::ensemble(a_params(), &MULTS) {
        match sc.rule {
            None => v.push(Box::new(m)),
            Some(rule) => v.push(Box::new(RegimeScaled::new(m, map.clone(), rule))),
        }
    }
    let b = OvernightCarry::new(b_params());
    match sc.rule {
        None => v.push(Box::new(b)),
        Some(rule) => v.push(Box::new(RegimeScaled::new(b, map.clone(), rule))),
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
    dates: Vec<NaiveDate>,
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
        dates: r.dates.clone(),
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
    let cut = NaiveDate::parse_from_str(POST_PUB, "%Y-%m-%d").unwrap_or(NaiveDate::MIN);
    let (mut pre, mut post) = (Vec::new(), Vec::new());
    for (d, x) in row.dates.iter().zip(v) {
        if *d >= cut { post.push(*x) } else { pre.push(*x) }
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

/// Per-regime attribution of one row: (regime, sleeve) → (days, $/yr, Sharpe).
fn attribution(row: &Row, map: &RegimeMap) {
    println!("\n### `{}` — atribuire pe regim (eticheta zilei precedente)\n", row.name);
    println!("| regim | zile | A $/an | A Sharpe | B $/an | B Sharpe | total $/an | total Sharpe |");
    println!("|---|---|---|---|---|---|---|---|");
    for (label, want) in [("contango", Regime::Contango), ("backwardation", Regime::Backwardation)] {
        let idx: Vec<usize> = (0..row.dates.len()).filter(|&i| map.for_session(row.dates[i]) == Some(want)).collect();
        let pick = |v: &[f64]| idx.iter().map(|&i| v[i]).collect::<Vec<f64>>();
        let (a, b, t) = (pick(&row.a), pick(&row.b), pick(&row.pnl));
        // $/yr scaled to the regime's share of the calendar: sum / (days/252).
        let yrs = (idx.len() as f64 / 252.0).max(1e-9);
        println!(
            "| {label} | {} ({:.1} %) | {:+.0} | {:.2} | {:+.0} | {:.2} | {:+.0} | {:.2} |",
            idx.len(),
            100.0 * idx.len() as f64 / row.dates.len().max(1) as f64,
            a.iter().sum::<f64>() / yrs,
            sharpe(&a),
            b.iter().sum::<f64>() / yrs,
            sharpe(&b),
            t.iter().sum::<f64>() / yrs,
            sharpe(&t)
        );
    }
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
    let vix = std::fs::read_to_string(arg("--vix").ok_or("need --vix")?).map_err(|e| format!("vix: {e}"))?;
    let vix3m = std::fs::read_to_string(arg("--vix3m").ok_or("need --vix3m")?).map_err(|e| format!("vix3m: {e}"))?;
    let out = PathBuf::from(arg("--out").ok_or("need --out")?);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;

    let map = Arc::new(RegimeMap::from_cboe_csv(&vix, &vix3m));
    eprintln!("regime map: {} CBOE days, {:.1} % backwardation", map.len(), 100.0 * map.backwardation_share());

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
                let (cfg, map) = (&cfg, map.clone());
                s.spawn(move || {
                    let r = run_with(cfg, data, &|_| sleeves_for(sc, &map));
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
    if let Some(base) = rows.iter().find(|r| r.name == "base") {
        attribution(base, &map);
    }
    Ok(())
}
