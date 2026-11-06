//! Runda 5 (Astra) — Q2 / Q3 on the real R6 engine, outside the strategy hash.
//! Pre-registration: `docs/meridian/validation/2026-09-27_runda5_q2_q3_preregistration.md`.
//!
//! r5_lab --spy PATH --qqq PATH --out DIR
//!
//!   base      A + B, R6 engine (portfolio cap 1.0×)     — must reproduce 17 498 / $39 916
//!   q2_cond   A + B, Astra's conditional cap G_t ∈ {1.0, 1.9} on SPY v20 ≤ v252 (engine cap 1.9)
//!   q2_cond2  same at $0.02/share (informative)
//!   q3_scale  A × 0.15, B × 0.2833 after internal caps, R6 engine unchanged
//! Writes DIR/<scenario>.csv (date,pnl,A,B) and prints the summary table.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Datelike, NaiveDate};
use titan_frontier::r5::{cond_caps, CondNightCap, NightLedger, ScaleExposure};
use titan_meridian::data::load_jsonl_days;
use titan_meridian::risk::RiskParams;
use titan_meridian::sim::{run_with, SimConfig, SimResult, SymbolData};
use titan_meridian::sleeve::{Sleeve, SleeveId};
use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

const CAPITAL: f64 = 100_000.0;
const MULTS: [f64; 3] = [1.0, 1.25, 1.5];
const POST_PUB: &str = "2024-06-01";
const X_A: f64 = 0.15;
const Y_B: f64 = 0.2833;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Base,
    Q2Cond,
    Q2Cond2c,
    Q3Scale,
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    kind: Kind,
}

const SCENARIOS: [Scenario; 4] = [
    Scenario { name: "base", kind: Kind::Base },
    Scenario { name: "q2_cond", kind: Kind::Q2Cond },
    Scenario { name: "q2_cond2", kind: Kind::Q2Cond2c },
    Scenario { name: "q3_scale", kind: Kind::Q3Scale },
];

fn a_params() -> IntradayParams {
    IntradayParams { lookback_days: 14, band_mult: 1.0, sigma_target_daily: 0.01, max_leverage: 2.0, allow_short: true }
}

fn b_params() -> OvernightParams {
    OvernightParams { sigma_target_night: 0.006, decide_bars_before_close: 11, ..Default::default() }
}

fn sleeves_for(sc: Scenario, ledger: &Arc<NightLedger>, caps: &Arc<BTreeMap<NaiveDate, f64>>) -> Vec<Box<dyn Sleeve>> {
    let mut v: Vec<Box<dyn Sleeve>> = Vec::new();
    for m in IntradayMomentum::ensemble(a_params(), &MULTS) {
        match sc.kind {
            Kind::Q3Scale => v.push(Box::new(ScaleExposure::new(m, X_A))),
            _ => v.push(Box::new(m)),
        }
    }
    let b = OvernightCarry::new(b_params());
    match sc.kind {
        Kind::Q2Cond | Kind::Q2Cond2c => v.push(Box::new(CondNightCap::new(b, ledger.clone(), caps.clone()))),
        Kind::Q3Scale => v.push(Box::new(ScaleExposure::new(b, Y_B))),
        Kind::Base => v.push(Box::new(b)),
    }
    v
}

fn config(sc: Scenario) -> SimConfig {
    let risk = match sc.kind {
        // the wrapper imposes G_t ∈ {1.0, 1.9}; the engine's own portfolio rail is lifted to 1.9
        Kind::Q2Cond | Kind::Q2Cond2c => RiskParams { gross_overnight_portfolio_max: 1.9, ..RiskParams::default() },
        _ => RiskParams::default(),
    };
    SimConfig {
        initial_equity: CAPITAL,
        slippage_per_share: if sc.kind == Kind::Q2Cond2c { 0.02 } else { 0.01 },
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

fn ann_vol_pct(x: &[f64]) -> f64 {
    let n = x.len() as f64;
    let m = x.iter().sum::<f64>() / n;
    (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt() * 252f64.sqrt() / CAPITAL * 100.0
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

/// Months whose running P/L from the month's start dips below −15 % of $98k (Astra Q3 count).
fn months_below_15pct(dates: &[NaiveDate], pnl: &[f64]) -> (usize, usize, f64) {
    let mut worst_min = 1.0f64;
    let (mut months, mut bad) = (0usize, 0usize);
    let mut cur: Option<(i32, u32)> = None;
    let mut e = 98_000.0;
    let mut start = e;
    let mut month_min = 1.0;
    for (d, p) in dates.iter().zip(pnl) {
        let key = (d.year(), d.month());
        if cur != Some(key) {
            if cur.is_some() {
                months += 1;
                if month_min < 0.85 {
                    bad += 1;
                }
                worst_min = worst_min.min(month_min);
            }
            cur = Some(key);
            start = e;
            month_min = 1.0;
        }
        e += p;
        month_min = month_min.min(e / start);
    }
    months += 1;
    if month_min < 0.85 {
        bad += 1;
    }
    worst_min = worst_min.min(month_min);
    (bad, months, worst_min)
}

fn night_gross(r: &SimResult) -> Vec<f64> {
    let mut pos: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    let mut out = Vec::new();
    let mut cur: Option<NaiveDate> = None;
    for t in &r.trades {
        if let Some(d) = cur {
            if d != t.date {
                out.push(pos.values().map(|(q, p)| (q * p).abs()).sum::<f64>() / CAPITAL);
            }
        }
        cur = Some(t.date);
        let e = pos.entry(t.symbol.clone()).or_insert((0.0, 0.0));
        e.0 += t.delta_shares;
        e.1 = t.fill_price;
    }
    if cur.is_some() {
        out.push(pos.values().map(|(q, p)| (q * p).abs()).sum::<f64>() / CAPITAL);
    }
    out
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
    nights_ge_10: usize,
    nights_ge_19: usize,
    clipped: usize,
}

fn summarize(sc: Scenario, r: &SimResult, ledger: &NightLedger) -> Row {
    let pick = |id: SleeveId| r.sleeve_pnl.get(&id).cloned().unwrap_or_else(|| vec![0.0; r.dates.len()]);
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
        nights_ge_10: ng.iter().filter(|g| **g >= 1.0 + 1e-9).count(),
        nights_ge_19: ng.iter().filter(|g| **g >= 1.9).count(),
        clipped: ledger.clipped.lock().map(|c| *c).unwrap_or(0),
    }
}

fn line(r: &Row) -> String {
    let years = r.dates.len() as f64 / 252.0;
    let cut = NaiveDate::parse_from_str(POST_PUB, "%Y-%m-%d").unwrap_or(NaiveDate::MIN);
    let post: Vec<f64> = r.dates.iter().zip(&r.pnl).filter(|(d, _)| **d >= cut).map(|(_, p)| *p).collect();
    let worst = r.pnl.iter().copied().fold(f64::MAX, f64::min);
    let (bad, months, wmin) = months_below_15pct(&r.dates, &r.pnl);
    format!(
        "| {} | {:+.0} | {:.3} | {:.2} | {:.0} | {:.0} | {:.0} | {} | {} | {:.2} % | {}/{} (min {:.3}) | {} | {} | {} |",
        r.name,
        r.pnl.iter().sum::<f64>() / years,
        sharpe(&r.pnl),
        sharpe(&post),
        mdd(&r.pnl),
        worst,
        r.cost,
        r.fills,
        r.halts,
        ann_vol_pct(&r.pnl),
        bad,
        months,
        wmin,
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
    let caps = Arc::new(cond_caps(&data[0].days));
    let stressed = caps.values().filter(|c| **c > 1.0).count();
    eprintln!("Q2 caps from SPY: {} sessions, {} at 1.9× ({:.1} %)", caps.len(), stressed, 100.0 * stressed as f64 / caps.len() as f64);
    let data = &data;
    let out_ref = &out;
    let caps_ref = &caps;
    let rows: Vec<Row> = std::thread::scope(|s| {
        let hs: Vec<_> = SCENARIOS
            .iter()
            .map(|&sc| {
                s.spawn(move || {
                    let ledger = Arc::new(NightLedger::default());
                    let cfg = config(sc);
                    let r = run_with(&cfg, data, &|_| sleeves_for(sc, &ledger, caps_ref));
                    eprintln!("{}: done ({} fills)", sc.name, r.trades.len());
                    let row = summarize(sc, &r, &ledger);
                    if let Err(e) = write_csv(out_ref, &row) {
                        eprintln!("{}: csv: {e}", sc.name);
                    }
                    row
                })
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    if rows.len() != SCENARIOS.len() {
        return Err("a scenario thread panicked".into());
    }
    println!("| scenario | $/yr net | Sharpe | post-pub | MDD $ | worst day $ | costs $ | fills | halts | ann vol | months < 0.85 | nights > 1.0× | nights ≥ 1.9× | clipped |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        println!("{}", line(r));
    }
    Ok(())
}
