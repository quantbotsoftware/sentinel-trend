//! Walk-forward validation for the nightly evolver (2026-07-04).
//!
//! The pre-existing GA cycle scored genomes on the last 30 days and
//! "validated" them on a 90-day window that CONTAINS those 30 days —
//! in-sample validation, the canonical overfitting trap (parameters
//! that memorised the recent tape get adopted, then bleed live).
//!
//! This module provides the honest split:
//!
//! ```text
//!   |───────────── train (fitness + 90d veto) ─────────────|── OOS ──|
//!   now-90d                                            now-14d     now
//! ```
//!
//! The optimizer NEVER sees the OOS window. A genome is adopted only
//! if it also survives on those unseen days. The OOS metrics of the
//! adopted (or retained) parameters are persisted to
//! `evolver/oos_baseline.json` — the "expected performance" that the
//! live edge-health monitor (`commands::edge_health`) compares real
//! trades against.

use std::collections::HashMap;
use std::path::Path;
use rayon::prelude::*;

use serde::{Deserialize, Serialize};
use titan_types::Candle;

use crate::backtest::engine::{simulate, SimParams};
use crate::backtest::fills::FillModel;
use crate::smarttrend::strategies::{by_id, StrategyId};

/// Days held back from training and used exclusively for validation.
pub const OOS_DAYS: i64 = 14;

/// Minimum OOS trades for a verdict — below this the window says
/// nothing (a 14-day window with 1 lucky trade is noise, not proof).
pub const OOS_MIN_TRADES: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OosBaseline {
    /// ET date of the evaluation run (YYYY-MM-DD).
    pub date: String,
    pub window_days: i64,
    pub trades: usize,
    pub win_rate_pct: f64,
    pub profit_factor: f64,
    pub net: f64,
    /// Mean net P/L per trade.
    pub expectancy: f64,
}

/// Split a fetched universe at `now − OOS_DAYS`: everything strictly
/// before the cutoff is train, everything at/after is out-of-sample.
pub fn split_train_oos(
    candles: &HashMap<String, Vec<Candle>>,
    now_secs: i64,
) -> (HashMap<String, Vec<Candle>>, HashMap<String, Vec<Candle>>) {
    let cutoff = now_secs - OOS_DAYS * 24 * 3600;
    let mut train: HashMap<String, Vec<Candle>> = HashMap::new();
    let mut oos: HashMap<String, Vec<Candle>> = HashMap::new();
    for (sym, bars) in candles {
        let split_idx = bars.iter().position(|c| c.time >= cutoff).unwrap_or(bars.len());
        if split_idx > 0 {
            train.insert(sym.clone(), bars[..split_idx].to_vec());
        }
        if split_idx < bars.len() {
            oos.insert(sym.clone(), bars[split_idx..].to_vec());
        }
    }
    (train, oos)
}

/// Replay `params` for `strategy_id` over the supplied (OOS) universe
/// and aggregate the headline numbers. Returns `None` when the
/// strategy id is unknown or the universe is empty.
pub fn evaluate_oos(
    strategy_id: StrategyId,
    params: &HashMap<String, f64>,
    oos: &HashMap<String, Vec<Candle>>,
) -> Option<OosBaseline> {
    if oos.is_empty() {
        return None;
    }
    let strategy = by_id(strategy_id)?;
    let trades: Vec<_> = oos
        .par_iter()
        .flat_map(|(symbol, bars)| {
            let sim_params = SimParams {
                symbol: symbol.clone(),
                timeframe: "5m",
                initial_equity: 10_000.0,
                fill_model: FillModel {
                    slippage_bps: 5.0,
                    commission: 0.0,
                    per_share_fee: 0.0,
                },
                warmup_bars: 30,
                spread_bps: 5.0,
                overrides: params.clone(),
            };
            simulate(strategy, bars, &sim_params).trades
        })
        .collect();
    let n = trades.len();
    let wins = trades.iter().filter(|t| t.net_pnl > 0.0).count();
    let gross_profit: f64 = trades.iter().filter(|t| t.net_pnl > 0.0).map(|t| t.net_pnl).sum();
    let gross_loss: f64 = trades.iter().filter(|t| t.net_pnl < 0.0).map(|t| t.net_pnl).sum();
    let net: f64 = trades.iter().map(|t| t.net_pnl).sum();
    let pf = if gross_loss.abs() > 0.0 {
        gross_profit / gross_loss.abs()
    } else if gross_profit > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };
    Some(OosBaseline {
        date: chrono::Utc::now()
            .with_timezone(&chrono_tz::US::Eastern)
            .format("%Y-%m-%d")
            .to_string(),
        window_days: OOS_DAYS,
        trades: n,
        win_rate_pct: if n > 0 { wins as f64 / n as f64 * 100.0 } else { 0.0 },
        profit_factor: pf,
        net,
        expectancy: if n > 0 { net / n as f64 } else { 0.0 },
    })
}

/// Adoption gate: the genome must still make money on days the
/// optimizer never saw. Insufficient sample fails closed — an
/// unproven parameter set never replaces a proven one.
pub fn oos_passes(b: &OosBaseline) -> bool {
    b.trades >= OOS_MIN_TRADES && b.profit_factor >= 1.0 && b.net > 0.0
}

// ─── Baseline persistence (evolver/oos_baseline.json) ────────────────

pub fn load_baselines(base_dir: &Path) -> HashMap<String, OosBaseline> {
    let path = base_dir.join("oos_baseline.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

pub fn save_baseline(
    base_dir: &Path,
    strategy_id: StrategyId,
    baseline: &OosBaseline,
) -> std::io::Result<()> {
    std::fs::create_dir_all(base_dir)?;
    let mut all = load_baselines(base_dir);
    all.insert(strategy_id.as_str().to_string(), baseline.clone());
    let body = serde_json::to_string_pretty(&all).map_err(std::io::Error::other)?;
    std::fs::write(base_dir.join("oos_baseline.json"), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(time: i64) -> Candle {
        Candle { time, open: 10.0, high: 10.5, low: 9.5, close: 10.2, volume: 1000.0 }
    }

    #[test]
    fn split_respects_oos_cutoff() {
        let now = 1_000_000_000i64;
        let cutoff = now - OOS_DAYS * 24 * 3600;
        let mut map = HashMap::new();
        map.insert(
            "AAA".to_string(),
            vec![bar(cutoff - 100), bar(cutoff - 50), bar(cutoff + 50)],
        );
        let (train, oos) = split_train_oos(&map, now);
        assert_eq!(train.get("AAA").map(|v| v.len()), Some(2));
        assert_eq!(oos.get("AAA").map(|v| v.len()), Some(1));
        assert!(train["AAA"].iter().all(|c| c.time < cutoff));
        assert!(oos["AAA"].iter().all(|c| c.time >= cutoff));
    }

    #[test]
    fn oos_gate_fails_closed_on_thin_sample() {
        let thin = OosBaseline {
            date: "2026-07-04".into(),
            window_days: OOS_DAYS,
            trades: OOS_MIN_TRADES - 1,
            win_rate_pct: 100.0,
            profit_factor: 9.0,
            net: 500.0,
            expectancy: 250.0,
        };
        assert!(!oos_passes(&thin));
        let losing = OosBaseline { trades: 10, profit_factor: 0.9, net: -20.0, ..thin.clone() };
        assert!(!oos_passes(&losing));
        let good = OosBaseline { trades: 10, profit_factor: 1.3, net: 80.0, ..thin };
        assert!(oos_passes(&good));
    }

    #[test]
    fn baseline_roundtrip_on_disk() {
        let dir = std::env::temp_dir().join(format!("st_wf_test_{}", std::process::id()));
        let b = OosBaseline {
            date: "2026-07-04".into(),
            window_days: OOS_DAYS,
            trades: 12,
            win_rate_pct: 41.7,
            profit_factor: 1.31,
            net: 96.4,
            expectancy: 8.03,
        };
        save_baseline(&dir, StrategyId::Apex, &b).unwrap();
        let loaded = load_baselines(&dir);
        assert_eq!(loaded.get("apex").map(|x| x.trades), Some(12));
        std::fs::remove_dir_all(&dir).ok();
    }
}
