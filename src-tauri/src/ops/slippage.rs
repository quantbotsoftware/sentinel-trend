//! P5 — Slippage Ledger.
//!
//! Every fill is scored against the price the system *decided* at
//! (the live price captured when the order was authored). The signed
//! difference in basis points is the realized execution cost — the
//! number the backtester assumes (5 bps flat) but live trading never
//! measured. The ledger answers two questions:
//!
//! 1. Operator: "which symbols are eating my edge at the fill?"
//!    (`summary()` per-symbol table in the PRIME tab).
//! 2. Engine: "what execution cost should the conviction fusion
//!    assume for this symbol?" (`recent_bps()` feeds the spread
//!    penalty as a realized cost prior — a symbol that keeps filling
//!    20 bps adverse needs proportionally more signal to clear the
//!    threshold).
//!
//! Placed in `ops/` (not `execution/`) so `smarttrend/` may read the
//! cost prior without violating R1. In-memory, bounded (S06 —
//! [`MAX_FILLS`] ring), process-lifetime only: slippage regimes shift
//! with liquidity, so persistence would mostly preserve stale costs.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

/// Ring cap (S06). ~15 trades/day × 2 fills → months of headroom.
pub const MAX_FILLS: usize = 1024;
/// Only fills younger than this feed `recent_bps` (7 days).
const RECENT_WINDOW_SECS: i64 = 7 * 86_400;
/// Cost prior is clamped here — one catastrophic fill must not
/// permanently veto a symbol.
const MAX_PRIOR_BPS: f64 = 40.0;

#[derive(Debug, Clone, Serialize)]
pub struct FillSlip {
    pub symbol: String,
    /// true = buy fill (entry long / cover short).
    pub is_buy: bool,
    pub expected: f64,
    pub filled: f64,
    /// Signed slippage in basis points; POSITIVE = adverse (paid more
    /// on a buy / received less on a sell).
    pub bps: f64,
    pub ts: i64,
}

/// Per-symbol aggregate for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct SlipSummaryRow {
    pub symbol: String,
    pub fills: u32,
    pub avg_bps: f64,
    pub worst_bps: f64,
}

#[derive(Debug, Default)]
struct Ledger {
    fills: VecDeque<FillSlip>,
}

static GLOBAL: OnceLock<Mutex<Ledger>> = OnceLock::new();

fn ledger() -> &'static Mutex<Ledger> {
    GLOBAL.get_or_init(|| Mutex::new(Ledger::default()))
}

/// Signed adverse bps of one fill. Pure.
pub fn slip_bps(is_buy: bool, expected: f64, filled: f64) -> Option<f64> {
    if !(expected.is_finite() && filled.is_finite()) || expected <= 0.0 || filled <= 0.0 {
        return None;
    }
    let raw = (filled - expected) / expected * 10_000.0;
    Some(if is_buy { raw } else { -raw })
}

/// Record one fill. Degenerate inputs are dropped silently — the
/// ledger is diagnostics, it must never disturb the fill path.
pub fn record(symbol: &str, is_buy: bool, expected: f64, filled: f64, ts: i64) {
    let Some(bps) = slip_bps(is_buy, expected, filled) else {
        return;
    };
    let mut g = ledger().lock().unwrap_or_else(|p| p.into_inner());
    if g.fills.len() >= MAX_FILLS {
        g.fills.pop_front();
    }
    g.fills.push_back(FillSlip {
        symbol: symbol.to_uppercase(),
        is_buy,
        expected,
        filled,
        bps,
        ts,
    });
}

/// P5 cost prior: mean ADVERSE bps for `symbol` over the trailing
/// window (only positive-mean results count — favorable slippage is
/// luck, not a discount to bank on). Falls back to 0.0 when the
/// symbol has no recent fills. Clamped to [`MAX_PRIOR_BPS`].
pub fn recent_bps(symbol: &str, now_ts: i64) -> f64 {
    let sym = symbol.to_uppercase();
    let g = ledger().lock().unwrap_or_else(|p| p.into_inner());
    let cutoff = now_ts - RECENT_WINDOW_SECS;
    let rows: Vec<f64> = g
        .fills
        .iter()
        .filter(|f| f.symbol == sym && f.ts >= cutoff)
        .map(|f| f.bps)
        .collect();
    if rows.is_empty() {
        return 0.0;
    }
    let mean = rows.iter().sum::<f64>() / rows.len() as f64;
    mean.clamp(0.0, MAX_PRIOR_BPS)
}

/// Per-symbol aggregate, worst average first. UI feed.
pub fn summary() -> Vec<SlipSummaryRow> {
    use std::collections::HashMap;
    let g = ledger().lock().unwrap_or_else(|p| p.into_inner());
    let mut agg: HashMap<String, (u32, f64, f64)> = HashMap::new();
    for f in &g.fills {
        let e = agg.entry(f.symbol.clone()).or_insert((0, 0.0, f64::MIN));
        e.0 += 1;
        e.1 += f.bps;
        e.2 = e.2.max(f.bps);
    }
    let mut rows: Vec<SlipSummaryRow> = agg
        .into_iter()
        .map(|(symbol, (fills, sum, worst))| SlipSummaryRow {
            symbol,
            fills,
            avg_bps: sum / fills as f64,
            worst_bps: worst,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.avg_bps
            .partial_cmp(&a.avg_bps)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

/// Most recent fills, newest first. UI feed.
pub fn recent(limit: usize) -> Vec<FillSlip> {
    let g = ledger().lock().unwrap_or_else(|p| p.into_inner());
    g.fills.iter().rev().take(limit).cloned().collect()
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buy_above_expected_is_adverse() {
        // Bought at 10.05 when the decision was made at 10.00 → +50 bps.
        let bps = slip_bps(true, 10.0, 10.05).unwrap();
        assert!((bps - 50.0).abs() < 1e-9);
        // Sell at 10.05 when expected 10.00 → favorable (−50).
        let bps = slip_bps(false, 10.0, 10.05).unwrap();
        assert!((bps + 50.0).abs() < 1e-9);
    }

    #[test]
    fn degenerate_inputs_dropped() {
        assert!(slip_bps(true, 0.0, 10.0).is_none());
        assert!(slip_bps(true, 10.0, f64::NAN).is_none());
        assert!(slip_bps(true, -1.0, 10.0).is_none());
    }

    // The global-ledger tests share one process-wide singleton, so
    // they use disjoint symbols to stay independent under the parallel
    // test runner.

    #[test]
    fn record_and_prior_roundtrip() {
        let now = 2_000_000_000;
        record("TESTA", true, 10.0, 10.02, now); // +20 bps
        record("TESTA", true, 10.0, 10.04, now); // +40 bps
        let prior = recent_bps("TESTA", now);
        assert!((prior - 30.0).abs() < 1e-6, "prior {prior}");
        // Unknown symbol → 0.
        assert_eq!(recent_bps("TESTB_UNSEEN", now), 0.0);
    }

    #[test]
    fn favorable_mean_clamps_to_zero() {
        let now = 2_000_000_000;
        record("TESTC", false, 10.0, 10.05, now); // −50 bps (favorable)
        assert_eq!(recent_bps("TESTC", now), 0.0);
    }

    #[test]
    fn old_fills_age_out_of_prior() {
        let now = 2_000_000_000;
        record("TESTD", true, 10.0, 10.10, now - 8 * 86_400); // stale
        assert_eq!(recent_bps("TESTD", now), 0.0);
    }

    #[test]
    fn summary_orders_worst_first() {
        let now = 2_000_000_000;
        record("TESTE", true, 10.0, 10.001, now); // +1 bps
        record("TESTF", true, 10.0, 10.03, now); // +30 bps
        let rows = summary();
        let pos_e = rows.iter().position(|r| r.symbol == "TESTE").unwrap();
        let pos_f = rows.iter().position(|r| r.symbol == "TESTF").unwrap();
        assert!(pos_f < pos_e);
    }
}
