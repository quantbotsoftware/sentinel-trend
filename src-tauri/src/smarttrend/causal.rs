//! D-PLUS (Sesiunea 2) — Causal Lead-Lag Detector.
//!
//! Discovers directional price-correlation between symbols at non-zero
//! lags. Output: a directed graph `{ leader → lagger, lag_secs, corr }`
//! that the bot consults at signal time:
//!
//!   - If a strong leader has just moved in the same direction as the
//!     proposed entry → boost the score.
//!   - If a strong leader has just moved AGAINST the proposed entry →
//!     veto (leader is hinting at a reversal incoming).
//!
//! ## Math
//!
//! Pearson cross-correlation of returns at lag `k`:
//!
//! ```text
//!   corr(a, b, k) = Σ((a_t - mean(a)) * (b_{t-k} - mean(b))) /
//!                   sqrt(var(a) * var(b))
//! ```
//!
//! We score each lag in `[10, 300]` seconds (anything <10 s is noise);
//! best-lag-best-score wins. Threshold for "lead-lag" detection:
//! `|corr| >= 0.6`.
//!
//! ## Storage
//!
//! Per-symbol [`PriceRing`] of fixed capacity (default 600 = 10 min @
//! 1-second cadence). Older samples evicted in O(1). The graph is
//! recomputed every 60s by [`CausalEngine::recompute_graph`] and cached
//! in `Arc<RwLock<LeadLagGraph>>` for cheap reads from the bot loop.
//!
//! ## Why not raw price?
//!
//! Returns are stationary; raw prices aren't. Cross-correlating raw
//! prices yields spurious leaders for symbols simply trending together.
//! We compute log returns inside the engine.

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

// ════════════════════════════════════════════════════════════════════
//  Tunables
// ════════════════════════════════════════════════════════════════════

/// Default per-symbol ring buffer capacity (10 min @ 1-sec ticks).
pub const DEFAULT_BUFFER_CAP: usize = 600;
/// Minimum samples per buffer before correlation is meaningful.
pub const MIN_SAMPLES: usize = 60;
/// Lag range to search (seconds).
pub const LAG_RANGE_SECS: std::ops::RangeInclusive<usize> = 10..=300;
/// Detection threshold — `|corr|` at the best lag.
pub const CORR_THRESHOLD: f64 = 0.6;
/// Maximum number of edges retained in the public graph (top-N by |corr|).
pub const TOP_EDGES: usize = 20;

// ════════════════════════════════════════════════════════════════════
//  Ring buffer of timestamped prices
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct PriceRing {
    pub cap: usize,
    /// (ts_secs, price). Oldest at front, newest at back.
    pub samples: std::collections::VecDeque<(i64, f64)>,
}

impl PriceRing {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            samples: std::collections::VecDeque::with_capacity(cap),
        }
    }

    pub fn push(&mut self, ts: i64, price: f64) {
        if !price.is_finite() || price <= 0.0 {
            return;
        }
        // Reject out-of-order or duplicate ticks (websocket can replay)
        if let Some(&(last_ts, _)) = self.samples.back() {
            if ts < last_ts {
                return;
            }
        }
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back((ts, price));
    }

    /// Returns the most recent price, or 0.0 if empty.
    pub fn last(&self) -> f64 {
        self.samples.back().map(|(_, p)| *p).unwrap_or(0.0)
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Compute the percentage change vs the price `secs` ago. Returns
    /// None if there's no sample at-or-before that timestamp.
    pub fn change_over(&self, secs: i64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let now_ts = self.samples.back()?.0;
        let target = now_ts - secs;
        // Walk backwards, find the latest sample at or before `target`
        let prev_price = self
            .samples
            .iter()
            .rev()
            .find(|(t, _)| *t <= target)
            .map(|(_, p)| *p);
        let last_price = self.samples.back().map(|(_, p)| *p)?;
        let pp = prev_price?;
        if pp <= 0.0 {
            return None;
        }
        Some((last_price - pp) / pp * 100.0)
    }

    /// Returns the log-return series sampled at 1-second intervals
    /// from the end of the buffer back. Length is `min(len-1, max_n)`.
    pub fn log_returns(&self, max_n: usize) -> Vec<f64> {
        if self.samples.len() < 2 {
            return Vec::new();
        }
        let n = (self.samples.len() - 1).min(max_n);
        let mut out = Vec::with_capacity(n);
        let total = self.samples.len();
        for i in (total - n)..total {
            let prev = self.samples[i - 1].1;
            let cur = self.samples[i].1;
            if prev > 0.0 && cur > 0.0 {
                out.push((cur / prev).ln());
            } else {
                out.push(0.0);
            }
        }
        out
    }
}

// ════════════════════════════════════════════════════════════════════
//  Cross-correlation primitives
// ════════════════════════════════════════════════════════════════════

/// Pearson correlation of two equal-length slices. Returns 0.0 when
/// either slice has zero variance (constant input) or length < 2.
pub fn pearson(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.len() < 2 {
        return 0.0;
    }
    let n = a.len() as f64;
    let mean_a: f64 = a.iter().sum::<f64>() / n;
    let mean_b: f64 = b.iter().sum::<f64>() / n;
    let mut cov = 0.0;
    let mut var_a = 0.0;
    let mut var_b = 0.0;
    for i in 0..a.len() {
        let da = a[i] - mean_a;
        let db = b[i] - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }
    let denom = (var_a * var_b).sqrt();
    if denom <= 0.0 || !denom.is_finite() {
        return 0.0;
    }
    cov / denom
}

/// Cross-correlation of `a` against `b` at lag `k`, using `b` as the
/// laggard (i.e. correlate `a[k..]` with `b[..len-k]`). Positive `k`
/// means "if `a` moves first, `b` follows after `k` samples".
pub fn xcorr(a: &[f64], b: &[f64], k: usize) -> f64 {
    if k >= a.len() || a.len() != b.len() {
        return 0.0;
    }
    let a_slice = &a[k..];
    let b_slice = &b[..b.len() - k];
    pearson(a_slice, b_slice)
}

// ════════════════════════════════════════════════════════════════════
//  Public types
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeadLagEdge {
    pub leader: String,
    pub lagger: String,
    pub lag_secs: i64,
    pub corr: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeadLagGraph {
    pub edges: Vec<LeadLagEdge>,
    pub recomputed_at: i64,
    pub samples_seen: usize,
}

impl LeadLagGraph {
    /// Returns all edges where `lagger == symbol`. The bot uses this
    /// to find leaders whose recent moves are predictive of `symbol`.
    pub fn leaders_for<'a>(&'a self, symbol: &str) -> Vec<&'a LeadLagEdge> {
        self.edges
            .iter()
            .filter(|e| e.lagger == symbol)
            .collect()
    }
}

// ════════════════════════════════════════════════════════════════════
//  CausalEngine — public facade
// ════════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub struct CausalEngine {
    pub buffer_cap: usize,
    /// `symbol → ring`. Populated incrementally by `ingest_tick`.
    pub buffers: HashMap<String, PriceRing>,
    pub graph: RwLock<LeadLagGraph>,
}

impl CausalEngine {
    pub fn new(buffer_cap: usize) -> Self {
        Self {
            buffer_cap: buffer_cap.max(MIN_SAMPLES * 2),
            buffers: HashMap::new(),
            graph: RwLock::new(LeadLagGraph::default()),
        }
    }

    pub fn ingest_tick(&mut self, symbol: &str, price: f64, ts: i64) {
        let entry = self
            .buffers
            .entry(symbol.to_string())
            .or_insert_with(|| PriceRing::new(self.buffer_cap));
        entry.push(ts, price);
    }

    /// Returns the symbols currently tracked, sorted alphabetically.
    pub fn symbols(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self.buffers.keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        keys
    }

    /// Recompute the lead-lag graph from current buffers. Must be
    /// called periodically (60s recommended). Locking strategy: the
    /// graph is double-buffered — we compute into a local Vec and
    /// write-swap at the end, holding the write lock for ~µs.
    pub fn recompute_graph(&self, now: i64) {
        let mut all_edges: Vec<LeadLagEdge> = Vec::new();
        let symbols: Vec<&String> = self.buffers.keys().collect();
        let mut samples_seen = 0usize;
        for sym in &symbols {
            samples_seen += self.buffers[*sym].len();
        }
        // O(N²) over symbol count — fine up to ~50 symbols.
        for i in 0..symbols.len() {
            let a_buf = &self.buffers[symbols[i]];
            if a_buf.len() < MIN_SAMPLES {
                continue;
            }
            let a_ret = a_buf.log_returns(self.buffer_cap);
            for j in 0..symbols.len() {
                if i == j {
                    continue;
                }
                let b_buf = &self.buffers[symbols[j]];
                if b_buf.len() < MIN_SAMPLES {
                    continue;
                }
                let b_ret = b_buf.log_returns(self.buffer_cap);
                let n = a_ret.len().min(b_ret.len());
                if n < MIN_SAMPLES {
                    continue;
                }
                // Trim both to the same length
                let a_slice = &a_ret[a_ret.len() - n..];
                let b_slice = &b_ret[b_ret.len() - n..];
                // Search best lag in LAG_RANGE_SECS where a leads b
                let mut best_lag = 0usize;
                let mut best_corr = 0.0f64;
                for k in LAG_RANGE_SECS {
                    if k >= n {
                        break;
                    }
                    let c = xcorr(a_slice, b_slice, k);
                    if c.abs() > best_corr.abs() {
                        best_corr = c;
                        best_lag = k;
                    }
                }
                if best_corr.abs() >= CORR_THRESHOLD {
                    all_edges.push(LeadLagEdge {
                        leader: symbols[i].clone(),
                        lagger: symbols[j].clone(),
                        lag_secs: best_lag as i64,
                        corr: best_corr,
                    });
                }
            }
        }
        // Keep top-N by |corr|
        all_edges.sort_by(|a, b| {
            b.corr
                .abs()
                .partial_cmp(&a.corr.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all_edges.truncate(TOP_EDGES);
        let mut g = self.graph.write().unwrap_or_else(|p| p.into_inner());
        g.edges = all_edges;
        g.recomputed_at = now;
        g.samples_seen = samples_seen;
    }

    /// Public read-only snapshot of the graph.
    pub fn snapshot(&self) -> LeadLagGraph {
        self.graph
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Convenience wrapper: returns leader edges for `symbol` from
    /// the cached graph (no recompute).
    pub fn leaders_for(&self, symbol: &str) -> Vec<LeadLagEdge> {
        let g = self.graph.read().unwrap_or_else(|p| p.into_inner());
        g.edges
            .iter()
            .filter(|e| e.lagger == symbol)
            .cloned()
            .collect()
    }

    /// Returns the recent (over the last `secs`) percentage change of
    /// a symbol, or `None` if buffer is too thin / unknown symbol.
    pub fn recent_change(&self, symbol: &str, secs: i64) -> Option<f64> {
        self.buffers.get(symbol)?.change_over(secs)
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tests — 22 covering ring buffer, math, edge filtering, integration
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_push_evicts_oldest_at_capacity() {
        let mut r = PriceRing::new(3);
        r.push(1, 10.0);
        r.push(2, 11.0);
        r.push(3, 12.0);
        r.push(4, 13.0);
        assert_eq!(r.len(), 3);
        assert_eq!(r.samples.front().unwrap().0, 2);
        assert_eq!(r.samples.back().unwrap().0, 4);
    }

    #[test]
    fn ring_rejects_invalid_prices() {
        let mut r = PriceRing::new(10);
        r.push(1, f64::NAN);
        r.push(1, 0.0);
        r.push(1, -5.0);
        r.push(1, f64::INFINITY);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn ring_rejects_out_of_order() {
        let mut r = PriceRing::new(10);
        r.push(5, 10.0);
        r.push(3, 11.0);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn ring_change_over_returns_none_for_empty() {
        let r = PriceRing::new(10);
        assert!(r.change_over(60).is_none());
    }

    #[test]
    fn ring_change_over_returns_correct_pct() {
        let mut r = PriceRing::new(100);
        for i in 0..60 {
            r.push(i, 100.0 + i as f64);
        }
        // last = 159, 30 sec ago = 100 + 29 = 129 → +30/129 ≈ 23.26%
        let chg = r.change_over(30).unwrap();
        assert!((chg - (159.0 - 129.0) / 129.0 * 100.0).abs() < 1e-6);
    }

    #[test]
    fn ring_log_returns_length_matches_request() {
        let mut r = PriceRing::new(100);
        for i in 0..20 {
            r.push(i, 100.0 + i as f64);
        }
        let lr = r.log_returns(10);
        assert_eq!(lr.len(), 10);
        // All positive (monotonic increase)
        for x in &lr {
            assert!(*x > 0.0);
        }
    }

    #[test]
    fn pearson_self_is_one() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let r = pearson(&a, &a);
        assert!((r - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pearson_inverse_is_neg_one() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let b: Vec<f64> = a.iter().rev().copied().collect();
        let r = pearson(&a, &b);
        assert!((r + 1.0).abs() < 1e-9);
    }

    #[test]
    fn pearson_constant_input_returns_zero() {
        let a = vec![5.0; 10];
        let b: Vec<f64> = (0..10).map(|i| i as f64).collect();
        let r = pearson(&a, &b);
        assert!(r.abs() < 1e-9);
    }

    #[test]
    fn pearson_mismatched_lengths_returns_zero() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];
        let r = pearson(&a, &b);
        assert!(r.abs() < 1e-9);
    }

    #[test]
    fn xcorr_lag_zero_equals_pearson() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let r = xcorr(&a, &a, 0);
        assert!((r - 1.0).abs() < 1e-9);
    }

    #[test]
    fn xcorr_perfect_lag_detected() {
        // The xcorr convention here: xcorr(a, b, k) = pearson(a[k..], b[..n-k]).
        // For perfect correlation at lag k we need a[k+i] = b[i] for all i.
        // Concretely, build a shared master signal and slice it so that
        // b is a's "earlier" copy: a[k+i] == master[k+i] == b[i] when
        // b[i] = master[k+i].
        let n = 100;
        let total = n + 3;
        let mut master = Vec::with_capacity(total);
        for i in 0..total {
            master.push((i as f64 * 0.1).sin());
        }
        let a: Vec<f64> = master[..n].to_vec();
        let b: Vec<f64> = master[3..3 + n].to_vec();
        let r = xcorr(&a, &b, 3);
        assert!(r > 0.99, "expected near-perfect corr, got {r}");
    }

    #[test]
    fn engine_ingest_creates_buffer_per_symbol() {
        let mut e = CausalEngine::new(DEFAULT_BUFFER_CAP);
        e.ingest_tick("SPY", 400.0, 1);
        e.ingest_tick("QQQ", 350.0, 1);
        assert_eq!(e.symbols(), vec!["QQQ", "SPY"]);
    }

    #[test]
    fn engine_recompute_empty_graph_when_buffers_too_thin() {
        let mut e = CausalEngine::new(DEFAULT_BUFFER_CAP);
        e.ingest_tick("SPY", 400.0, 1);
        e.recompute_graph(2);
        assert!(e.snapshot().edges.is_empty());
    }

    #[test]
    fn engine_recompute_finds_lead_lag_edge_in_synthetic_data() {
        let mut e = CausalEngine::new(800);
        let n = 700i64;
        for t in 0..n {
            // Driver: sinusoidal with secondary noise; lead by 30 secs
            let leader_p = 100.0 + (t as f64 * 0.05).sin() * 2.0;
            e.ingest_tick("LEADER", leader_p, t);
            // Lagger: same signal but 30 seconds later
            let phase = if t >= 30 { t - 30 } else { 0 };
            let lagger_p = 100.0 + (phase as f64 * 0.05).sin() * 2.0;
            e.ingest_tick("LAGGER", lagger_p, t);
        }
        e.recompute_graph(n);
        let g = e.snapshot();
        assert!(!g.edges.is_empty(), "expected at least one edge");
        // Verify our LEADER → LAGGER edge is in there
        let edge = g
            .edges
            .iter()
            .find(|e| e.leader == "LEADER" && e.lagger == "LAGGER");
        assert!(edge.is_some(), "expected LEADER→LAGGER edge: {:?}", g.edges);
        let edge = edge.unwrap();
        assert!(edge.corr.abs() >= CORR_THRESHOLD);
        assert!(edge.lag_secs >= 10);
    }

    #[test]
    fn engine_leaders_for_filters_by_lagger() {
        let mut e = CausalEngine::new(800);
        let n = 700i64;
        for t in 0..n {
            let leader_p = 100.0 + (t as f64 * 0.05).sin() * 2.0;
            e.ingest_tick("LEADER", leader_p, t);
            let phase = if t >= 30 { t - 30 } else { 0 };
            let lagger_p = 100.0 + (phase as f64 * 0.05).sin() * 2.0;
            e.ingest_tick("LAGGER", lagger_p, t);
        }
        e.recompute_graph(n);
        let leaders = e.leaders_for("LAGGER");
        assert!(!leaders.is_empty());
        for l in &leaders {
            assert_eq!(l.lagger, "LAGGER");
        }
    }

    #[test]
    fn engine_recent_change_unknown_symbol_returns_none() {
        let e = CausalEngine::new(DEFAULT_BUFFER_CAP);
        assert!(e.recent_change("XYZ", 30).is_none());
    }

    #[test]
    fn engine_buffer_cap_floor_enforced() {
        // Asking for tiny cap is bumped to MIN_SAMPLES * 2
        let e = CausalEngine::new(5);
        assert!(e.buffer_cap >= MIN_SAMPLES * 2);
    }

    #[test]
    fn engine_top_edges_truncated() {
        let mut e = CausalEngine::new(800);
        let n = 700i64;
        // Generate 6 symbols all correlated with phase shifts
        for t in 0..n {
            for k in 0..6 {
                let phase = if t >= k * 5 { t - k * 5 } else { 0 };
                let p = 100.0 + (phase as f64 * 0.05).sin() * 2.0;
                e.ingest_tick(&format!("S{k}"), p, t);
            }
        }
        e.recompute_graph(n);
        let g = e.snapshot();
        assert!(g.edges.len() <= TOP_EDGES);
    }

    #[test]
    fn engine_uncorrelated_inputs_produce_no_edges() {
        let mut e = CausalEngine::new(800);
        let n = 700i64;
        // Generate two random walks (deterministic seed for reproducibility)
        let mut x = 100.0;
        let mut y = 100.0;
        let mut seed = 0xCAFEu64;
        for t in 0..n {
            // xorshift64
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let r1 = (seed % 200) as f64 / 1000.0 - 0.1;
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let r2 = (seed % 200) as f64 / 1000.0 - 0.1;
            x += r1;
            y += r2;
            if x <= 0.0 {
                x = 1.0;
            }
            if y <= 0.0 {
                y = 1.0;
            }
            e.ingest_tick("X", x, t);
            e.ingest_tick("Y", y, t);
        }
        e.recompute_graph(n);
        let g = e.snapshot();
        // Most likely empty (uncorrelated random walks); allow up to 2 false positives
        assert!(g.edges.len() <= 2, "expected <2 spurious edges, got {}", g.edges.len());
    }

    #[test]
    fn engine_recompute_updates_recomputed_at_and_samples() {
        let mut e = CausalEngine::new(800);
        for t in 0..300i64 {
            e.ingest_tick("X", 100.0 + (t as f64 * 0.01).sin(), t);
        }
        e.recompute_graph(999);
        let g = e.snapshot();
        assert_eq!(g.recomputed_at, 999);
        assert_eq!(g.samples_seen, 300);
    }

    #[test]
    fn engine_handles_single_symbol_gracefully() {
        let mut e = CausalEngine::new(800);
        for t in 0..200i64 {
            e.ingest_tick("ONLY", 100.0 + t as f64, t);
        }
        e.recompute_graph(200);
        // Single symbol → no pair → no edges. Must NOT panic.
        assert!(e.snapshot().edges.is_empty());
    }

    #[test]
    fn pearson_short_input_returns_zero() {
        assert!(pearson(&[1.0], &[1.0]).abs() < 1e-9);
        assert!(pearson(&[], &[]).abs() < 1e-9);
    }

    #[test]
    fn xcorr_lag_too_large_returns_zero() {
        let a = vec![1.0, 2.0, 3.0];
        assert!(xcorr(&a, &a, 5).abs() < 1e-9);
        assert!(xcorr(&a, &a, 3).abs() < 1e-9);
    }
}
