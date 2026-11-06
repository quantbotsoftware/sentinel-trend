//! Human-readable reports from a `SimResult`.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use super::engine::SimResult;
use crate::sleeve::SleeveId;
use crate::stats::{performance, Performance};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YearRow {
    pub year: i32,
    pub days: usize,
    pub ret: f64,
    pub sharpe: f64,
    pub mdd: f64,
}

pub fn yearly_table(dates: &[NaiveDate], returns: &[f64]) -> Vec<YearRow> {
    let mut rows: Vec<YearRow> = Vec::new();
    let mut i = 0;
    while i < dates.len() {
        let y = dates[i].year();
        let mut j = i;
        while j < dates.len() && dates[j].year() == y {
            j += 1;
        }
        let p = performance(&returns[i..j]);
        rows.push(YearRow { year: y, days: j - i, ret: p.total_return, sharpe: p.sharpe, mdd: p.max_drawdown });
        i = j;
    }
    rows
}

fn fmt_perf(label: &str, p: &Performance) -> String {
    format!(
        "| {label} | {} | {:+.1} % | {:.1} % | {:.2} | {:.2} | {:.1} % | {:.2} | {:.0} % |\n",
        p.n,
        p.cagr * 100.0,
        p.ann_vol * 100.0,
        p.sharpe,
        p.sortino,
        p.max_drawdown * 100.0,
        p.profit_factor,
        p.hit_rate * 100.0
    )
}

/// Split-aware report: overall, in-sample (< `oos_from`), out-of-sample.
pub fn markdown_report(title: &str, r: &SimResult, oos_from: Option<NaiveDate>) -> String {
    let mut s = format!("## {title}\n\n");
    if r.dates.is_empty() {
        s.push_str("_no trading days_\n");
        return s;
    }
    s.push_str(&format!(
        "Period {} → {} · {} days · {} fills · costs ${:.0} · risk halts {}\n\n",
        r.dates[0],
        r.dates[r.dates.len() - 1],
        r.dates.len(),
        r.trades.len(),
        r.total_cost,
        r.risk_halts
    ));
    s.push_str("| Segment | Days | CAGR | Vol | Sharpe | Sortino | MDD | PF(daily) | Hit |\n|---|---|---|---|---|---|---|---|---|\n");
    s.push_str(&fmt_perf("ALL", &performance(&r.daily_returns)));
    if let Some(split) = oos_from {
        let k = r.dates.iter().position(|d| *d >= split).unwrap_or(r.dates.len());
        if k > 1 {
            s.push_str(&fmt_perf("IS", &performance(&r.daily_returns[..k])));
        }
        if r.dates.len() - k > 1 {
            s.push_str(&fmt_perf("OOS", &performance(&r.daily_returns[k..])));
        }
    }
    s.push_str("\n| Year | Days | Return | Sharpe | MDD |\n|---|---|---|---|---|\n");
    for y in yearly_table(&r.dates, &r.daily_returns) {
        s.push_str(&format!(
            "| {} | {} | {:+.1} % | {:.2} | {:.1} % |\n",
            y.year,
            y.days,
            y.ret * 100.0,
            y.sharpe,
            y.mdd * 100.0
        ));
    }
    if !r.sleeve_pnl.is_empty() {
        s.push_str("\n| Sleeve | Gross P/L $ | Days active | Sharpe (gross, on start equity) |\n|---|---|---|---|\n");
        for (id, v) in &r.sleeve_pnl {
            let eq0 = r.equity.first().copied().unwrap_or(1.0).max(1.0);
            let rets: Vec<f64> = v.iter().map(|p| p / eq0).collect();
            let p = performance(&rets);
            s.push_str(&format!(
                "| {} | {:+.0} | {} | {:.2} |\n",
                sleeve_name(*id),
                v.iter().sum::<f64>(),
                v.iter().filter(|x| **x != 0.0).count(),
                p.sharpe
            ));
        }
    }
    s
}

fn sleeve_name(id: SleeveId) -> &'static str {
    match id {
        SleeveId::A => "A intraday momentum",
        SleeveId::B => "B overnight carry",
        SleeveId::C => "C stocks-in-play ORB",
        SleeveId::D => "D variance premium",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yearly_split_and_report_smoke() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        let dates = vec![d(2024, 12, 30), d(2024, 12, 31), d(2025, 1, 2), d(2025, 1, 3)];
        let rets = vec![0.01, -0.01, 0.02, 0.0];
        let rows = yearly_table(&dates, &rets);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].year, 2024);
        assert_eq!(rows[1].days, 2);
        let r = SimResult { dates, daily_returns: rets, equity: vec![1.0; 4], ..Default::default() };
        let md = markdown_report("t", &r, Some(d(2025, 1, 1)));
        assert!(md.contains("| IS |") && md.contains("| OOS |") && md.contains("| 2025 |"));
        assert!(markdown_report("e", &SimResult::default(), None).contains("no trading"));
    }
}
