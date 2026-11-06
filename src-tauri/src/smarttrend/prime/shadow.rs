//! PRIME F5a — Shadow trading (counterfactual veto engine).
//!
//! Every PRIME-vetoed entry candidate becomes a *shadow trade*: a
//! paper-only record of what the signal would have done, tracked
//! against subsequent prices until it hits its own stop, its target,
//! or expires on its time stop. The aggregate answers the question the
//! decision journal alone cannot: **how much money is each veto stage
//! saving — or costing?** A veto stage whose shadows keep hitting
//! their targets is destroying edge; one whose shadows keep stopping
//! out is doing its job. The nightly calibration (F5b) and the
//! operator both read this table.
//!
//! Bounded (S06): at most [`MAX_OPEN`] open shadows and
//! [`MAX_RESOLVED`] resolved ones are retained, oldest evicted first.
//! Everything here is pure in-memory state — no I/O, no locks of its
//! own (the owner wraps it in a mutex), fully testable (R4).

use std::collections::VecDeque;

use serde::Serialize;

/// Max simultaneously tracked open shadows.
pub const MAX_OPEN: usize = 256;
/// Max retained resolved shadows (ring).
pub const MAX_RESOLVED: usize = 2048;

#[derive(Debug, Clone, Serialize)]
pub struct ShadowTrade {
    pub symbol: String,
    /// Playbook that authored the vetoed signal.
    pub source: String,
    pub veto_stage: String,
    pub veto_reason: String,
    pub side_long: bool,
    pub entry_price: f64,
    pub stop: f64,
    pub target: f64,
    pub opened_ts: i64,
    pub max_hold_minutes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum ShadowResolution {
    Target,
    Stop,
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedShadow {
    pub trade: ShadowTrade,
    pub resolution: ShadowResolution,
    pub exit_price: f64,
    /// Signed R-multiple outcome (+1 = one full risk unit gained).
    pub pnl_r: f64,
    pub closed_ts: i64,
}

/// One row of the aggregate: "vetoes at stage X from playbook Y would
/// have produced N trades totalling Z R-multiples".
#[derive(Debug, Clone, Serialize)]
pub struct ShadowSummaryRow {
    pub veto_stage: String,
    pub source: String,
    pub resolved: u32,
    pub wins: u32,
    pub total_r: f64,
}

#[derive(Debug, Default)]
pub struct ShadowBook {
    open: Vec<ShadowTrade>,
    resolved: VecDeque<ResolvedShadow>,
}

impl ShadowBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a vetoed candidate. Degenerate inputs (no stop distance,
    /// non-finite prices) are silently dropped — a shadow without a
    /// risk unit cannot produce an R-multiple.
    pub fn open(&mut self, trade: ShadowTrade) {
        if !trade.entry_price.is_finite()
            || trade.entry_price <= 0.0
            || !trade.stop.is_finite()
            || (trade.entry_price - trade.stop).abs() <= trade.entry_price * 1e-4
        {
            return;
        }
        // One open shadow per (symbol, source) — a strategy re-vetoed
        // on every bar must not flood the book with near-duplicates.
        if self
            .open
            .iter()
            .any(|t| t.symbol == trade.symbol && t.source == trade.source)
        {
            return;
        }
        if self.open.len() >= MAX_OPEN {
            self.open.remove(0);
        }
        self.open.push(trade);
    }

    /// Feed a price observation for `symbol`; resolves any open shadow
    /// whose stop/target/time-stop triggers. Returns how many resolved.
    pub fn on_price(&mut self, symbol: &str, price: f64, ts: i64) -> usize {
        if !price.is_finite() || price <= 0.0 {
            return 0;
        }
        let mut resolved_count = 0usize;
        let mut i = 0;
        while i < self.open.len() {
            let t = &self.open[i];
            if t.symbol != symbol {
                i += 1;
                continue;
            }
            let expired = t.max_hold_minutes > 0
                && ts.saturating_sub(t.opened_ts) >= i64::from(t.max_hold_minutes) * 60;
            let (hit_target, hit_stop) = if t.side_long {
                (t.target > 0.0 && price >= t.target, price <= t.stop)
            } else {
                (t.target > 0.0 && price <= t.target, price >= t.stop)
            };
            let resolution = if hit_stop {
                Some(ShadowResolution::Stop)
            } else if hit_target {
                Some(ShadowResolution::Target)
            } else if expired {
                Some(ShadowResolution::Expired)
            } else {
                None
            };
            match resolution {
                Some(res) => {
                    let t = self.open.remove(i);
                    let risk = (t.entry_price - t.stop).abs();
                    let signed_move = if t.side_long {
                        price - t.entry_price
                    } else {
                        t.entry_price - price
                    };
                    let pnl_r = if risk > 1e-9 { signed_move / risk } else { 0.0 };
                    if self.resolved.len() >= MAX_RESOLVED {
                        self.resolved.pop_front();
                    }
                    self.resolved.push_back(ResolvedShadow {
                        trade: t,
                        resolution: res,
                        exit_price: price,
                        pnl_r,
                        closed_ts: ts,
                    });
                    resolved_count += 1;
                }
                None => i += 1,
            }
        }
        resolved_count
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    pub fn resolved_count(&self) -> usize {
        self.resolved.len()
    }

    /// Aggregate resolved shadows by `(veto_stage, source)`, sorted by
    /// |total_r| descending — the most consequential vetoes first.
    pub fn summary(&self) -> Vec<ShadowSummaryRow> {
        use std::collections::HashMap;
        let mut agg: HashMap<(String, String), ShadowSummaryRow> = HashMap::new();
        for r in &self.resolved {
            let key = (r.trade.veto_stage.clone(), r.trade.source.clone());
            let row = agg.entry(key).or_insert_with(|| ShadowSummaryRow {
                veto_stage: r.trade.veto_stage.clone(),
                source: r.trade.source.clone(),
                resolved: 0,
                wins: 0,
                total_r: 0.0,
            });
            row.resolved += 1;
            if r.pnl_r > 0.0 {
                row.wins += 1;
            }
            row.total_r += r.pnl_r;
        }
        let mut rows: Vec<ShadowSummaryRow> = agg.into_values().collect();
        rows.sort_by(|a, b| {
            b.total_r
                .abs()
                .partial_cmp(&a.total_r.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows
    }

    /// Most recent resolved shadows, newest first (UI feed).
    pub fn recent_resolved(&self, limit: usize) -> Vec<ResolvedShadow> {
        self.resolved.iter().rev().take(limit).cloned().collect()
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn long_shadow(symbol: &str, source: &str) -> ShadowTrade {
        ShadowTrade {
            symbol: symbol.into(),
            source: source.into(),
            veto_stage: "prime".into(),
            veto_reason: "below_threshold".into(),
            side_long: true,
            entry_price: 100.0,
            stop: 98.0,
            target: 104.0,
            opened_ts: 1_700_000_000,
            max_hold_minutes: 60,
        }
    }

    #[test]
    fn target_hit_resolves_positive_r() {
        let mut book = ShadowBook::new();
        book.open(long_shadow("AAPL", "gap_fade"));
        assert_eq!(book.on_price("AAPL", 104.5, 1_700_000_100), 1);
        let recent = book.recent_resolved(1);
        assert_eq!(recent[0].resolution, ShadowResolution::Target);
        // (104.5 - 100) / 2 = +2.25R
        assert!((recent[0].pnl_r - 2.25).abs() < 1e-9);
    }

    #[test]
    fn stop_hit_resolves_negative_r() {
        let mut book = ShadowBook::new();
        book.open(long_shadow("AAPL", "gap_fade"));
        assert_eq!(book.on_price("AAPL", 97.8, 1_700_000_100), 1);
        let recent = book.recent_resolved(1);
        assert_eq!(recent[0].resolution, ShadowResolution::Stop);
        assert!(recent[0].pnl_r < -1.0 + 1e-9);
    }

    #[test]
    fn expiry_resolves_at_market() {
        let mut book = ShadowBook::new();
        book.open(long_shadow("AAPL", "gap_fade"));
        // 61 minutes later, price drifted +1.0 (no stop/target hit).
        assert_eq!(book.on_price("AAPL", 101.0, 1_700_000_000 + 61 * 60), 1);
        let recent = book.recent_resolved(1);
        assert_eq!(recent[0].resolution, ShadowResolution::Expired);
        assert!((recent[0].pnl_r - 0.5).abs() < 1e-9);
    }

    #[test]
    fn short_side_mirrors() {
        let mut book = ShadowBook::new();
        let mut t = long_shadow("TSLA", "catalyst_frontrun");
        t.side_long = false;
        t.stop = 102.0;
        t.target = 96.0;
        book.open(t);
        assert_eq!(book.on_price("TSLA", 95.9, 1_700_000_100), 1);
        let recent = book.recent_resolved(1);
        assert_eq!(recent[0].resolution, ShadowResolution::Target);
        assert!(recent[0].pnl_r > 2.0);
    }

    #[test]
    fn duplicate_symbol_source_not_double_tracked() {
        let mut book = ShadowBook::new();
        book.open(long_shadow("AAPL", "gap_fade"));
        book.open(long_shadow("AAPL", "gap_fade"));
        assert_eq!(book.open_count(), 1);
        // Different source is a distinct shadow.
        book.open(long_shadow("AAPL", "fsm"));
        assert_eq!(book.open_count(), 2);
    }

    #[test]
    fn degenerate_stop_dropped() {
        let mut book = ShadowBook::new();
        let mut t = long_shadow("AAPL", "gap_fade");
        t.stop = t.entry_price;
        book.open(t);
        assert_eq!(book.open_count(), 0);
    }

    #[test]
    fn caps_enforced() {
        let mut book = ShadowBook::new();
        for i in 0..(MAX_OPEN + 10) {
            book.open(long_shadow(&format!("S{i}"), "gap_fade"));
        }
        assert_eq!(book.open_count(), MAX_OPEN);
    }

    #[test]
    fn summary_groups_by_stage_and_source() {
        let mut book = ShadowBook::new();
        book.open(long_shadow("A", "gap_fade"));
        book.open(long_shadow("B", "halt_resume"));
        book.on_price("A", 104.5, 1_700_000_100); // +2.25R
        book.on_price("B", 97.0, 1_700_000_100); // stop
        let rows = book.summary();
        assert_eq!(rows.len(), 2);
        let gf = rows.iter().find(|r| r.source == "gap_fade").unwrap();
        assert_eq!(gf.resolved, 1);
        assert_eq!(gf.wins, 1);
        assert!(gf.total_r > 2.0);
    }
}
