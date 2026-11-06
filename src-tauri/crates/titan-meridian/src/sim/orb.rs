//! Sleeve C-ORB simulation — catalyst-selected premarket movers, entry
//! AFTER 09:30 on a break of max(premarket high, opening-range high),
//! stop at the opening-range low/mid, exit at stop or at the archive's
//! last bar (10:29). Rules: docs/meridian/validation/2026-09-16_corb_preregistration.md
//!
//! Pure; reuses the C-PM archive (`DayArchive`) and event selection.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::data::pm_archive::{load_day, NewsEvent};
use crate::session::EXT_RTH_OPEN_IDX;
use crate::sim::premarket::{select_event, ClassFilter, PmSimConfig};
use crate::sim::SimResult;
use crate::sleeve::SleeveId;
use crate::sleeves::ExtBar;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopRule {
    OrLow,
    OrMid,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OrbParams {
    pub or_min: usize,
    /// Require the breakout close above the premarket high as well.
    pub confirm_pm_high: bool,
    pub stop: StopRule,
    /// Last RTH bar index (0 = 09:30) at which an entry may trigger.
    pub entry_last_rth_idx: usize,
    pub min_pm_move: f64,
    pub min_pm_dollar_volume: f64,
    pub min_price: f64,
    pub max_stop_frac: f64,
    pub risk_frac: f64,
    pub max_notional_frac: f64,
    pub slip_frac: f64,
    pub slip_per_share: f64,
}

impl Default for OrbParams {
    fn default() -> Self {
        Self {
            or_min: 5,
            confirm_pm_high: true,
            stop: StopRule::OrLow,
            entry_last_rth_idx: 30,
            min_pm_move: 0.03,
            min_pm_dollar_volume: 500_000.0,
            min_price: 2.0,
            max_stop_frac: 0.08,
            risk_frac: 0.005,
            max_notional_frac: 0.10,
            slip_frac: 0.001,
            slip_per_share: 0.005,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrbConfig {
    pub params: OrbParams,
    pub equity: f64,
    pub max_positions_per_day: usize,
    pub filter: ClassFilter,
}

impl Default for OrbConfig {
    fn default() -> Self {
        Self { params: OrbParams::default(), equity: 100_000.0, max_positions_per_day: 5, filter: ClassFilter::Tradeable }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrbTrade {
    pub date: NaiveDate,
    pub symbol: String,
    pub entry_idx: usize,
    pub entry_px: f64,
    pub stop_px: f64,
    pub exit_idx: usize,
    pub exit_px: f64,
    pub stopped: bool,
    pub shares: f64,
    pub pnl: f64,
    pub r_multiple: f64,
    pub cost: f64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OrbResult {
    pub trades: Vec<OrbTrade>,
    pub dates: Vec<NaiveDate>,
    pub daily_pnl: Vec<f64>,
    pub skipped: BTreeMap<String, usize>,
    pub days_with_archive: usize,
    pub days_missing_archive: usize,
    pub candidates: usize,
}

/// Premarket summary at 09:29: (move vs prev close, dollar volume, high).
fn premarket_stats(bars: &[ExtBar], prev_close: f64) -> Option<(f64, f64, f64)> {
    let pm: Vec<&ExtBar> = bars.iter().filter(|b| b.idx < EXT_RTH_OPEN_IDX).collect();
    let last = pm.last()?;
    let dv: f64 = pm.iter().map(|b| b.c * b.v).sum();
    let hi = pm.iter().map(|b| b.h).fold(f64::MIN, f64::max);
    Some((last.c / prev_close - 1.0, dv, hi))
}

/// Simulate one (symbol, day).
pub fn simulate_symbol_day(cfg: &OrbConfig, bars: &[ExtBar], prev_close: f64) -> Result<OrbTrade, &'static str> {
    let p = cfg.params;
    if prev_close <= 0.0 || prev_close.is_nan() {
        return Err("no previous close");
    }
    let (mv, dv, pm_high) = premarket_stats(bars, prev_close).ok_or("no premarket bars")?;
    if mv < p.min_pm_move {
        return Err("premarket move below threshold");
    }
    if dv < p.min_pm_dollar_volume {
        return Err("premarket dollar volume below threshold");
    }
    let rth: Vec<&ExtBar> = bars.iter().filter(|b| b.idx >= EXT_RTH_OPEN_IDX).collect();
    if rth.first().map(|b| b.o < p.min_price).unwrap_or(true) {
        return Err("no RTH bars / below min price");
    }
    let or: Vec<&&ExtBar> = rth.iter().filter(|b| b.idx < EXT_RTH_OPEN_IDX + p.or_min).collect();
    if or.len() < p.or_min.min(3) {
        return Err("opening range incomplete");
    }
    let or_high = or.iter().map(|b| b.h).fold(f64::MIN, f64::max);
    let or_low = or.iter().map(|b| b.l).fold(f64::MAX, f64::min);
    let trigger = if p.confirm_pm_high { or_high.max(pm_high) } else { or_high };
    let stop_px = match p.stop {
        StopRule::OrLow => or_low,
        StopRule::OrMid => (or_high + or_low) / 2.0,
    };
    // entry: first bar after the OR (≤ entry_last) closing above the trigger
    let mut entry: Option<(usize, f64)> = None;
    for (i, b) in rth.iter().enumerate() {
        let rth_idx = b.idx - EXT_RTH_OPEN_IDX;
        if rth_idx < p.or_min {
            continue;
        }
        if rth_idx > p.entry_last_rth_idx {
            break;
        }
        if b.c > trigger {
            let Some(next) = rth.get(i + 1) else { return Err("breakout on last bar") };
            entry = Some((i + 1, next.o * (1.0 + p.slip_frac) + p.slip_per_share));
            break;
        }
    }
    let Some((ei, entry_px)) = entry else { return Err("no breakout") };
    if stop_px >= entry_px {
        return Err("stop above entry");
    }
    let stop_dist = entry_px - stop_px;
    if stop_dist / entry_px > p.max_stop_frac {
        return Err("stop too wide");
    }
    let risk = cfg.equity * p.risk_frac;
    let shares = (risk / stop_dist).min(cfg.equity * p.max_notional_frac / entry_px).floor();
    if shares < 1.0 {
        return Err("size < 1 share");
    }
    let mut exit_idx = rth[ei].idx;
    let mut exit_px = rth[ei].c;
    let mut stopped = false;
    for b in &rth[ei..] {
        if b.l <= stop_px {
            exit_idx = b.idx;
            exit_px = stop_px * (1.0 - p.slip_frac) - p.slip_per_share;
            stopped = true;
            break;
        }
        exit_idx = b.idx;
        exit_px = b.c * (1.0 - p.slip_frac) - p.slip_per_share;
    }
    let pnl = shares * (exit_px - entry_px);
    let cost = shares * 2.0 * (p.slip_per_share + p.slip_frac * entry_px);
    Ok(OrbTrade {
        date: NaiveDate::default(),
        symbol: String::new(),
        entry_idx: rth[ei].idx,
        entry_px,
        stop_px,
        exit_idx,
        exit_px,
        stopped,
        shares,
        pnl,
        r_multiple: pnl / risk.max(1e-9),
        cost,
    })
}

/// Run several configurations over the archive, loading each day once.
pub fn run_orb_multi(cfgs: &[OrbConfig], events: &[NewsEvent], archive_dir: &Path) -> Vec<OrbResult> {
    let mut results: Vec<OrbResult> = cfgs.iter().map(|_| OrbResult::default()).collect();
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
            // event selection shares the C-PM rules (first tradeable headline ≤ 09:25, no excluded class)
            let sel = PmSimConfig { filter: cfg.filter, ..Default::default() };
            let mut day_trades: Vec<OrbTrade> = Vec::new();
            for (sym, list) in &by_sym {
                if let Err(r) = select_event(&sel, list) {
                    *res.skipped.entry(r.to_string()).or_default() += 1;
                    continue;
                }
                res.candidates += 1;
                let bars = arch.bars.get(*sym).map(|v| v.as_slice()).unwrap_or(&[]);
                let prev = arch.prev_close.get(*sym).copied().unwrap_or(0.0);
                match simulate_symbol_day(cfg, bars, prev) {
                    Ok(mut t) => {
                        t.date = date;
                        t.symbol = sym.to_string();
                        day_trades.push(t);
                    }
                    Err(r) => *res.skipped.entry(r.to_string()).or_default() += 1,
                }
            }
            day_trades.sort_by_key(|t| (t.entry_idx, t.symbol.clone()));
            day_trades.truncate(cfg.max_positions_per_day);
            res.dates.push(date);
            res.daily_pnl.push(day_trades.iter().map(|t| t.pnl).sum());
            res.trades.extend(day_trades);
        }
    }
    results
}

impl OrbResult {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(idx: usize, o: f64, h: f64, l: f64, c: f64, v: f64) -> ExtBar {
        ExtBar { idx, o, h, l, c, v, n: 10.0 }
    }

    /// Premarket +5 % on $1M, OR 5 min 10.4–10.6, breakout at 09:37 above
    /// pm high 10.7, entry next open 10.80, runs to 11.50 at 10:29.
    fn day() -> Vec<ExtBar> {
        let mut v = Vec::new();
        for i in 300..EXT_RTH_OPEN_IDX {
            v.push(bar(i, 10.4, 10.7, 10.3, 10.5, 4_000.0)); // 30 bars × $42k ≈ $1.26M
        }
        for i in 0..5 {
            v.push(bar(EXT_RTH_OPEN_IDX + i, 10.5, 10.6, 10.4, 10.5, 50_000.0));
        }
        v.push(bar(EXT_RTH_OPEN_IDX + 5, 10.5, 10.65, 10.45, 10.62, 50_000.0)); // above OR high 10.6, not pm high 10.7
        v.push(bar(EXT_RTH_OPEN_IDX + 6, 10.6, 10.75, 10.55, 10.72, 50_000.0)); // > 10.7 ⇒ trigger
        v.push(bar(EXT_RTH_OPEN_IDX + 7, 10.8, 10.9, 10.75, 10.85, 50_000.0)); // entry at open 10.80
        for i in 8..60 {
            v.push(bar(EXT_RTH_OPEN_IDX + i, 11.0, 11.6, 10.9, 11.5, 20_000.0));
        }
        v
    }

    #[test]
    fn breakout_entry_stop_and_archive_exit() {
        let cfg = OrbConfig::default();
        let t = simulate_symbol_day(&cfg, &day(), 10.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(t.entry_idx, EXT_RTH_OPEN_IDX + 7);
        assert!((t.entry_px - (10.8 * 1.001 + 0.005)).abs() < 1e-9);
        assert_eq!(t.stop_px, 10.4);
        assert!(!t.stopped);
        assert_eq!(t.exit_idx, EXT_RTH_OPEN_IDX + 59);
        // risk $500 / stop dist ≈ 0.4158 ⇒ 1202 sh, capped at $10k/10.8158 ≈ 924 sh
        assert_eq!(t.shares, 924.0);
        assert!(t.pnl > 0.0 && t.r_multiple > 1.0);
    }

    #[test]
    fn without_pm_high_confirmation_entry_is_earlier() {
        let cfg = OrbConfig { params: OrbParams { confirm_pm_high: false, ..Default::default() }, ..Default::default() };
        let t = simulate_symbol_day(&cfg, &day(), 10.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(t.entry_idx, EXT_RTH_OPEN_IDX + 6, "OR high 10.6 broken at 09:35 ⇒ entry 09:36");
    }

    #[test]
    fn stop_hit_is_filled_at_stop_minus_slippage() {
        let mut d = day();
        d.truncate(5 + 30 + 3); // through the entry bar
        d.push(bar(EXT_RTH_OPEN_IDX + 8, 10.7, 10.7, 10.2, 10.3, 50_000.0)); // low 10.2 < stop 10.4
        let cfg = OrbConfig::default();
        let t = simulate_symbol_day(&cfg, &d, 10.0).unwrap_or_else(|e| panic!("{e}"));
        assert!(t.stopped);
        assert!((t.exit_px - (10.4 * 0.999 - 0.005)).abs() < 1e-9);
        assert!(t.r_multiple < 0.0 && t.r_multiple > -1.2, "≈ −1R: {}", t.r_multiple);
    }

    #[test]
    fn filters_reject_small_move_and_thin_premarket() {
        let cfg = OrbConfig::default();
        assert_eq!(simulate_symbol_day(&cfg, &day(), 10.4).unwrap_err(), "premarket move below threshold");
        let thin: Vec<ExtBar> = day().into_iter().map(|mut b| { if b.idx < EXT_RTH_OPEN_IDX { b.v = 10.0 } b }).collect();
        assert_eq!(simulate_symbol_day(&cfg, &thin, 10.0).unwrap_err(), "premarket dollar volume below threshold");
        let mut flat = day();
        for b in flat.iter_mut().filter(|b| b.idx >= EXT_RTH_OPEN_IDX + 5) {
            b.c = 10.5;
            b.h = 10.55;
        }
        assert_eq!(simulate_symbol_day(&cfg, &flat, 10.0).unwrap_err(), "no breakout");
    }
}
