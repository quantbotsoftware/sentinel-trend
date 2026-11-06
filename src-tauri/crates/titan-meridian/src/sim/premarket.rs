//! Event-driven simulation of Sleeve C-PM over the premarket archive.
//!
//! For every (date, symbol) with a headline: skip if any headline of the
//! day is an excluded class; otherwise take the first headline whose class
//! passes the filter and run `PmTracker` over the symbol's extended bars.
//! Per day, at most `max_positions_per_day` trades (earliest entries win).
//! Equity is held fixed (event-study convention): daily return = Σ P/L / equity.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::catalyst::CatalystClass;
use crate::data::pm_archive::{load_day, NewsEvent};
use crate::sim::engine::SimResult;
use crate::sleeve::SleeveId;
use crate::sleeves::{ExitReason, Fill, PmParams, PmTracker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassFilter {
    /// Classes C-PM would trade live.
    Tradeable,
    /// Control arm: analyst notes only.
    Analyst,
    /// Control arm: every non-excluded headline.
    All,
    Only(CatalystClass),
}

impl ClassFilter {
    pub fn accepts(&self, c: CatalystClass) -> bool {
        match self {
            ClassFilter::Tradeable => c.is_tradeable_long(),
            ClassFilter::Analyst => c == CatalystClass::Analyst,
            ClassFilter::All => !c.is_excluded(),
            ClassFilter::Only(k) => c == *k,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmSimConfig {
    pub params: PmParams,
    pub equity: f64,
    pub max_positions_per_day: usize,
    pub filter: ClassFilter,
}

impl Default for PmSimConfig {
    fn default() -> Self {
        Self { params: PmParams::default(), equity: 100_000.0, max_positions_per_day: 3, filter: ClassFilter::Tradeable }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmTrade {
    pub date: NaiveDate,
    pub symbol: String,
    pub class: CatalystClass,
    pub headline: String,
    pub event_idx: usize,
    pub entry_idx: usize,
    pub entry_px: f64,
    pub shares: f64,
    pub exits: Vec<Fill>,
    pub pnl: f64,
    /// P/L in units of initial risk (entry − stop) × shares.
    pub r_multiple: f64,
    pub cost: f64,
    pub last_exit: Option<ExitReason>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PmResult {
    pub trades: Vec<PmTrade>,
    pub dates: Vec<NaiveDate>,
    pub daily_pnl: Vec<f64>,
    pub skipped: BTreeMap<String, usize>,
    pub days_with_archive: usize,
    pub days_missing_archive: usize,
    pub candidates: usize,
}

fn cost_of(fills: &[Fill], p: &PmParams) -> f64 {
    fills.iter().map(|f| f.shares.abs() * (p.slip_per_share + p.slip_frac * f.price)).sum()
}

/// Simulate one (symbol, day). `None` when no trade happened.
pub fn simulate_symbol_day(cfg: &PmSimConfig, ev: &NewsEvent, bars: &[crate::sleeves::ExtBar], prev_close: f64) -> Result<PmTrade, &'static str> {
    if prev_close <= 0.0 || prev_close.is_nan() {
        return Err("no previous close");
    }
    let mut tr = PmTracker::new(cfg.params, ev.idx, prev_close, cfg.equity);
    let mut fills: Vec<Fill> = Vec::new();
    let mut last_px = prev_close;
    for b in bars {
        fills.extend(tr.on_bar(b));
        last_px = b.c;
        if tr.is_done() {
            break;
        }
    }
    if let Some(f) = tr.force_close(bars.last().map(|b| b.idx).unwrap_or(ev.idx), last_px) {
        fills.push(f);
    }
    let Some(entry) = fills.first().filter(|f| f.shares > 0.0).copied() else {
        return Err(tr.skip_reason.unwrap_or("no bars after headline"));
    };
    let exits: Vec<Fill> = fills.iter().skip(1).copied().collect();
    let proceeds: f64 = exits.iter().map(|f| -f.shares * f.price).sum();
    let pnl = proceeds - entry.shares * entry.price;
    let risk = (cfg.equity * cfg.params.risk_frac).max(1e-9);
    Ok(PmTrade {
        date: ev.date,
        symbol: ev.symbol.clone(),
        class: ev.class,
        headline: ev.headline.clone(),
        event_idx: ev.idx,
        entry_idx: entry.idx,
        entry_px: entry.price,
        shares: entry.shares,
        pnl,
        r_multiple: pnl / risk,
        cost: cost_of(&fills, &cfg.params),
        last_exit: exits.last().and_then(|f| f.reason),
        exits,
    })
}

/// Pick, per symbol-day, the headline to act on (or a skip reason).
pub fn select_event<'a>(cfg: &PmSimConfig, evs: &[&'a NewsEvent]) -> Result<&'a NewsEvent, &'static str> {
    if evs.iter().any(|e| e.class.is_excluded()) {
        return Err("excluded class present");
    }
    evs.iter()
        .filter(|e| cfg.filter.accepts(e.class) && e.idx <= cfg.params.entry_last_idx)
        .min_by_key(|e| e.idx)
        .copied()
        .ok_or("no accepted headline")
}

pub fn run_pm(cfg: &PmSimConfig, events: &[NewsEvent], archive_dir: &Path) -> PmResult {
    run_pm_multi(std::slice::from_ref(cfg), events, archive_dir).pop().unwrap_or_default()
}

/// Run several configurations over the archive, loading each day once
/// (the JSON archive dominates run time). Result `i` belongs to `cfgs[i]`.
pub fn run_pm_multi(cfgs: &[PmSimConfig], events: &[NewsEvent], archive_dir: &Path) -> Vec<PmResult> {
    let mut results: Vec<PmResult> = cfgs.iter().map(|_| PmResult::default()).collect();
    let mut by_date: BTreeMap<NaiveDate, Vec<&NewsEvent>> = BTreeMap::new();
    for e in events {
        by_date.entry(e.date).or_default().push(e);
    }
    for (date, evs) in by_date {
        let arch = match load_day(archive_dir, date) {
            Ok(Some(a)) => a,
            _ => {
                for r in results.iter_mut() {
                    r.days_missing_archive += 1;
                }
                continue;
            }
        };
        let mut by_sym: BTreeMap<&str, Vec<&NewsEvent>> = BTreeMap::new();
        for e in evs {
            by_sym.entry(e.symbol.as_str()).or_default().push(e);
        }
        for (cfg, res) in cfgs.iter().zip(results.iter_mut()) {
            res.days_with_archive += 1;
            let mut day_trades: Vec<PmTrade> = Vec::new();
            for (sym, list) in &by_sym {
                let ev = match select_event(cfg, list) {
                    Ok(e) => e,
                    Err(r) => {
                        *res.skipped.entry(r.to_string()).or_default() += 1;
                        continue;
                    }
                };
                res.candidates += 1;
                let bars = arch.bars.get(*sym).map(|v| v.as_slice()).unwrap_or(&[]);
                let prev = arch.prev_close.get(*sym).copied().unwrap_or(0.0);
                match simulate_symbol_day(cfg, ev, bars, prev) {
                    Ok(t) => day_trades.push(t),
                    Err(r) => *res.skipped.entry(r.to_string()).or_default() += 1,
                }
            }
            day_trades.sort_by_key(|t| (t.entry_idx, t.symbol.clone()));
            day_trades.truncate(cfg.max_positions_per_day);
            let pnl: f64 = day_trades.iter().map(|t| t.pnl).sum();
            res.dates.push(date);
            res.daily_pnl.push(pnl);
            res.trades.extend(day_trades);
        }
    }
    results
}

impl PmResult {
    /// Adapt to the generic report (fixed-equity daily returns, sleeve C).
    pub fn to_sim_result(&self, equity: f64) -> SimResult {
        let mut eq = equity;
        let mut out = SimResult { dates: self.dates.clone(), ..Default::default() };
        for p in &self.daily_pnl {
            out.daily_returns.push(p / equity);
            eq += p;
            out.equity.push(eq);
        }
        out.sleeve_pnl.insert(SleeveId::C, self.daily_pnl.clone());
        out.total_cost = self.trades.iter().map(|t| t.cost).sum();
        out
    }

    /// Per-class table: n, hit rate, avg R, profit factor, total P/L, cost.
    pub fn class_table(&self) -> Vec<(CatalystClass, usize, f64, f64, f64, f64, f64)> {
        let mut m: BTreeMap<CatalystClass, Vec<&PmTrade>> = BTreeMap::new();
        for t in &self.trades {
            m.entry(t.class).or_default().push(t);
        }
        m.into_iter()
            .map(|(c, v)| {
                let n = v.len();
                let hit = v.iter().filter(|t| t.pnl > 0.0).count() as f64 / n as f64;
                let avg_r = v.iter().map(|t| t.r_multiple).sum::<f64>() / n as f64;
                let g: f64 = v.iter().filter(|t| t.pnl > 0.0).map(|t| t.pnl).sum();
                let l: f64 = v.iter().filter(|t| t.pnl < 0.0).map(|t| -t.pnl).sum();
                let pf = if l > 0.0 { g / l } else if g > 0.0 { f64::INFINITY } else { 0.0 };
                (c, n, hit, avg_r, pf, v.iter().map(|t| t.pnl).sum(), v.iter().map(|t| t.cost).sum())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sleeves::ExtBar;

    fn ev(sym: &str, idx: usize, class: CatalystClass) -> NewsEvent {
        NewsEvent { date: NaiveDate::from_ymd_opt(2026, 9, 3).unwrap(), idx, symbol: sym.into(), class, headline: String::new() }
    }

    #[test]
    fn select_event_rules() {
        let cfg = PmSimConfig::default();
        let a = ev("X", 100, CatalystClass::Analyst);
        let b = ev("X", 110, CatalystClass::EarningsBeat);
        let o = ev("X", 120, CatalystClass::Offering);
        assert_eq!(select_event(&cfg, &[&a, &b]).unwrap().idx, 110);
        assert_eq!(select_event(&cfg, &[&a]).unwrap_err(), "no accepted headline");
        assert_eq!(select_event(&cfg, &[&b, &o]).unwrap_err(), "excluded class present");
        let late = ev("X", 340, CatalystClass::EarningsBeat);
        assert_eq!(select_event(&cfg, &[&late]).unwrap_err(), "no accepted headline");
        let analyst = PmSimConfig { filter: ClassFilter::Analyst, ..Default::default() };
        assert_eq!(select_event(&analyst, &[&a, &b]).unwrap().idx, 100);
    }

    #[test]
    fn simulate_symbol_day_pnl_and_r() {
        let cfg = PmSimConfig::default();
        let e = ev("DLTH", 105, CatalystClass::EarningsBeat);
        let bars = vec![
            ExtBar { idx: 109, o: 3.2, h: 3.5, l: 3.18, c: 3.45, v: 200_000.0, n: 300.0 },
            ExtBar { idx: 330, o: 3.9, h: 3.95, l: 3.85, c: 3.9, v: 100_000.0, n: 300.0 },
            ExtBar { idx: 331, o: 3.9, h: 3.9, l: 3.5, c: 3.5, v: 100_000.0, n: 300.0 }, // below VWAP → trail exit
        ];
        let t = simulate_symbol_day(&cfg, &e, &bars, 3.0).unwrap();
        assert_eq!(t.entry_idx, 109);
        assert_eq!(t.exits.len(), 2);
        assert_eq!(t.last_exit, Some(ExitReason::VwapTrail));
        assert!(t.pnl > 0.0 && t.r_multiple > 0.0 && t.cost > 0.0);
        let net: f64 = t.shares + t.exits.iter().map(|f| f.shares).sum::<f64>();
        assert_eq!(net, 0.0);
        assert_eq!(simulate_symbol_day(&cfg, &e, &bars, 0.0).unwrap_err(), "no previous close");
        assert_eq!(simulate_symbol_day(&cfg, &e, &[], 3.0).unwrap_err(), "no bars after headline");
    }

    #[test]
    fn to_sim_result_and_class_table() {
        let mut r = PmResult { dates: vec![NaiveDate::from_ymd_opt(2026, 9, 3).unwrap()], daily_pnl: vec![250.0], ..Default::default() };
        r.trades.push(PmTrade {
            date: r.dates[0],
            symbol: "X".into(),
            class: CatalystClass::EarningsBeat,
            headline: String::new(),
            event_idx: 1,
            entry_idx: 2,
            entry_px: 1.0,
            shares: 1.0,
            exits: vec![],
            pnl: 250.0,
            r_multiple: 0.5,
            cost: 5.0,
            last_exit: None,
        });
        let s = r.to_sim_result(100_000.0);
        assert!((s.daily_returns[0] - 0.0025).abs() < 1e-12);
        assert_eq!(s.equity[0], 100_250.0);
        let t = r.class_table();
        assert_eq!(t[0].0, CatalystClass::EarningsBeat);
        assert_eq!(t[0].1, 1);
        assert_eq!(t[0].2, 1.0);
    }
}
