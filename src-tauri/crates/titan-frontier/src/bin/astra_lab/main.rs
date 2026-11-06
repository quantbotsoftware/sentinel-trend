//! D1/D2/D5 scenarios of the Astra diagnostics batch
//! (`docs/meridian/validation/2026-09-27_diagnostice_astra_preregistration.md`)
//! on the real MERIDIAN engine, outside the live strategy hash.
//!
//! astra_lab --spy PATH --qqq PATH --out DIR
//!
//!   base      A + B (engine)                       — must reproduce 17 963 / $43 058
//!   b_full    B alone, FULL overlay (loss states)  — D1 counterfactual
//!   cap05     A + B, per-symbol overnight cap 0.5  — D2 (portfolio ≤ 1.0×)
//!   cap_port  A + B, portfolio overnight cap 1.0×  — D2 (decision-order wrapper)
//!   a_delay   A delayed one bar + B                — D5
//! Writes DIR/<scenario>.csv (date,pnl,A,B) and DIR/base_trades.csv.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::NaiveDate;
use titan_frontier::diag::{DelayOneBar, NightBook, PortfolioNightCap};
use titan_meridian::data::load_jsonl_days;
use titan_meridian::risk::RiskParams;
use titan_meridian::sim::{run_with, SimConfig, SimResult, SymbolData};
use titan_meridian::sleeve::{Sleeve, SleeveId};
use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

const CAPITAL: f64 = 100_000.0;
const MULTS: [f64; 3] = [1.0, 1.25, 1.5];
const POST_PUB: &str = "2024-06-01";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Base,
    BFull,
    Cap05,
    CapPort,
    ADelay,
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    kind: Kind,
}

const SCENARIOS: [Scenario; 5] = [
    Scenario { name: "base", kind: Kind::Base },
    Scenario { name: "b_full", kind: Kind::BFull },
    Scenario { name: "cap05", kind: Kind::Cap05 },
    Scenario { name: "cap_port", kind: Kind::CapPort },
    Scenario { name: "a_delay", kind: Kind::ADelay },
];

fn a_params() -> IntradayParams {
    IntradayParams { lookback_days: 14, band_mult: 1.0, sigma_target_daily: 0.01, max_leverage: 2.0, allow_short: true }
}

fn b_params() -> OvernightParams {
    OvernightParams { sigma_target_night: 0.006, decide_bars_before_close: 11, ..Default::default() }
}

fn sleeves_for(sc: Scenario, book: &Arc<NightBook>) -> Vec<Box<dyn Sleeve>> {
    let mut v: Vec<Box<dyn Sleeve>> = Vec::new();
    if sc.kind != Kind::BFull {
        for m in IntradayMomentum::ensemble(a_params(), &MULTS) {
            match sc.kind {
                Kind::ADelay => v.push(Box::new(DelayOneBar::new(m))),
                _ => v.push(Box::new(m)),
            }
        }
    }
    let b = OvernightCarry::new(b_params());
    match sc.kind {
        Kind::CapPort => v.push(Box::new(PortfolioNightCap::new(b, book.clone(), 1.0))),
        _ => v.push(Box::new(b)),
    }
    v
}

fn config(sc: Scenario) -> SimConfig {
    let risk = match sc.kind {
        Kind::Cap05 => RiskParams { gross_overnight_max: 0.5, ..RiskParams::default() },
        _ => RiskParams::default(),
    };
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

/// Overnight gross per session (fraction of capital) from the fills: the
/// position after the day's last fill is what is carried.
fn night_gross(r: &SimResult) -> BTreeMap<NaiveDate, f64> {
    let mut pos: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    let mut out: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    let mut cur: Option<NaiveDate> = None;
    let flush = |pos: &BTreeMap<String, (f64, f64)>, d: NaiveDate, out: &mut BTreeMap<NaiveDate, f64>| {
        out.insert(d, pos.values().map(|(q, p)| (q * p).abs()).sum::<f64>() / CAPITAL);
    };
    for t in &r.trades {
        if let Some(d) = cur {
            if d != t.date {
                flush(&pos, d, &mut out);
            }
        }
        cur = Some(t.date);
        let e = pos.entry(t.symbol.clone()).or_insert((0.0, 0.0));
        e.0 += t.delta_shares;
        e.1 = t.fill_price;
    }
    if let Some(d) = cur {
        flush(&pos, d, &mut out);
    }
    out
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
    nights_ge_19: usize,
    nights_ge_10: usize,
    clipped: usize,
}

fn summarize(sc: Scenario, r: &SimResult, book: &NightBook) -> Row {
    let pick = |id: SleeveId| r.sleeve_pnl.get(&id).cloned().unwrap_or_else(|| vec![0.0; r.dates.len()]);
    let (max_sym, max_gross) = exposure(r);
    let ng = night_gross(r);
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
        nights_ge_19: ng.values().filter(|g| **g >= 1.9).count(),
        nights_ge_10: ng.values().filter(|g| **g >= 1.0).count(),
        clipped: book.clipped.lock().map(|c| c.len()).unwrap_or(0),
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
        "| {} | {:+.0} | {:.3} | {:.2} | {:.2} | {:.0} | {:.0} | {:.0} | {} | {} | {:.2} | {:.2} | {} | {} | {} |",
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
        r.nights_ge_10,
        r.nights_ge_19,
        r.clipped
    )
}

fn write_csv(dir: &std::path::Path, row: &Row) -> Result<(), String> {
    let mut s = String::from("date,pnl,A,B\n");
    for i in 0..row.dates.len() {
        s.push_str(&format!("{},{:.2},{:.2},{:.2}\n", row.dates[i], row.pnl[i], row.a[i], row.b[i]));
    }
    std::fs::write(dir.join(format!("{}.csv", row.name)), s).map_err(|e| e.to_string())
}

fn write_trades(dir: &std::path::Path, name: &str, r: &SimResult) -> Result<(), String> {
    let mut s = String::from("date,symbol,bar_idx,delta_shares,fill_price,cost,sleeves\n");
    for t in &r.trades {
        let sl: Vec<&str> = t.sleeves.iter().map(|s| s.as_str()).collect();
        s.push_str(&format!("{},{},{},{:.0},{:.4},{:.2},{}\n", t.date, t.symbol, t.bar_idx, t.delta_shares, t.fill_price, t.cost, sl.join("+")));
    }
    std::fs::write(dir.join(format!("{name}_trades.csv")), s).map_err(|e| e.to_string())
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
    let out_ref = &out;
    let rows: Vec<Row> = std::thread::scope(|s| {
        let hs: Vec<_> = SCENARIOS
            .iter()
            .map(|&sc| {
                s.spawn(move || {
                    let book = Arc::new(NightBook::default());
                    let cfg = config(sc);
                    let r = run_with(&cfg, data, &|_| sleeves_for(sc, &book));
                    eprintln!("{}: done ({} fills)", sc.name, r.trades.len());
                    if matches!(sc.kind, Kind::Base | Kind::CapPort) {
                        if let Err(e) = write_trades(out_ref, sc.name, &r) {
                            eprintln!("{}: trades csv: {e}", sc.name);
                        }
                    }
                    summarize(sc, &r, &book)
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
    println!("| scenario | $/yr net | Sharpe | pre-pub | post-pub | MDD $ | worst day $ | costs $ | fills | halts | max/sym | max gross | nights ≥1.0× | nights ≥1.9× | clipped |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        println!("{}", line(r));
    }
    Ok(())
}
