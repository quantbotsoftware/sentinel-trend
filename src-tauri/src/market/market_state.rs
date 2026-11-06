use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::market::candle_buffer::CandleRingBuffer;
use crate::market::indicator_cache::IndicatorCache;

// ═══════════════════════════════════════════════════════════════════
//  OfiState — NBBO-derived Order Flow Imbalance (lock-free via ArcSwap)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default, Serialize)]
pub struct OfiState {
    pub current_ofi: f64,
    pub best_bid: f64,
    pub best_ask: f64,
    pub spread_bps: f64,
    pub bid_depth_total: f64,
    pub ask_depth_total: f64,
    pub imbalance_ratio: f64,
    pub microprice: f64,
    pub update_count: u64,
    #[serde(skip)]
    #[allow(dead_code)] // Read by streamer for timing checks
    pub last_update: Option<Instant>,
}

impl OfiState {
    #[allow(dead_code)] // Used by bot safety checks (Session 9)
    pub fn is_stale(&self, max_age_ms: u128) -> bool {
        match self.last_update {
            None => true,
            Some(t) => t.elapsed().as_millis() > max_age_ms,
        }
    }
}

/// Depth-weighted microprice. Falls back to simple mid if sizes are zero.
pub fn compute_microprice(bid: f64, ask: f64, bid_size: f64, ask_size: f64) -> f64 {
    let total_size = bid_size + ask_size;
    if bid <= 0.0 || ask <= 0.0 {
        return 0.0;
    }
    if total_size <= 0.0 {
        return (bid + ask) * 0.5;
    }
    (bid * ask_size + ask * bid_size) / total_size
}

// ═══════════════════════════════════════════════════════════════════
//  HaltState (S13) — primary halt signal sourced from Alpaca SIP
//  `T="s"` status messages. Heuristic-only halts (long quiet trade
//  age in RTH) are computed at the consume site and DO NOT mutate
//  this state — keeps the StatusSub signal clean for observability.
//
//  WatchlistMetrics (S14) — pre-computed per-symbol watchlist row
//  refreshed by `market::watchlist_refresh::run_watchlist_refresh_loop`
//  every 30s. Read by `commands::discovery::get_watchlist` to avoid
//  the watchlist going blind between scanner runs (every 5/30 min).
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HaltSource {
    /// Halt asserted by Alpaca SIP `T="s"` status message.
    StatusSub,
    /// Reserved: heuristic-derived halt (RTH + age > 120s). Never written
    /// to MarketDataState.halts; kept here for future telemetry.
    #[allow(dead_code)]
    HeuristicAge,
}

/// F2 — first N prints captured after SIP resume (for TRUE_RESUME).
#[derive(Debug, Clone, Serialize)]
pub struct ResumePrint {
    pub ts: DateTime<Utc>,
    pub price: f64,
    pub size: f64,
    /// `true` = buy-classified (quote-aligned when available).
    pub is_buy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HaltState {
    pub halted_since: DateTime<Utc>,
    pub reason: Option<String>,
    pub source: HaltSource,
    /// SIP status code that opened the halt (`H`/`2`/`P`).
    #[serde(default)]
    pub halt_code: Option<String>,
    /// Last trade price observed before the halt (best-effort).
    #[serde(default)]
    pub pre_halt_price: Option<f64>,
    /// When SIP resume codes (`Q`/`T`/`R`/`3`) arrived. `None` while still halted.
    #[serde(default)]
    pub resumed_at: Option<DateTime<Utc>>,
    /// First prints after resume (capped).
    #[serde(default)]
    pub resume_prints: Vec<ResumePrint>,
}

/// F2 — event pushed on SIP resume for immediate Frontier evaluation.
#[derive(Debug, Clone)]
pub struct ResumeEvent {
    pub symbol: String,
    pub resumed_at: DateTime<Utc>,
    pub halt_code: Option<String>,
    pub pre_halt_price: Option<f64>,
    pub halted_since: DateTime<Utc>,
}

// ─── WatchlistMetrics (S14) ─────────────────────────────────────────

/// Pre-computed per-symbol metrics for the watchlist UI.
///
/// Owned by `MarketDataState.watchlist_metrics` (Tier B6). Refreshed
/// every 30s by `market::watchlist_refresh::run_watchlist_refresh_loop`.
/// `commands::discovery::get_watchlist` reads this map first (fresh,
/// ≤30s) and only falls back to the older `scan_results`-based path for
/// symbols not yet covered (cold-start window).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchlistMetrics {
    pub symbol: String,
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
    pub spread_bps: f64,
    pub change_percent: f64,
    pub volume: f64,
    pub rvol: f64,
    pub gap_percent: f64,
    pub last_trade_age_seconds: u32,
    pub is_stale: bool,
    pub catalyst_flags: crate::market::catalyst::CatalystFlags,
    pub catalyst_pills: Vec<String>,
    /// PRIME F0.6 — aggregate headline polarity in [-1, +1]; `None`
    /// when no fresh news matched any sentiment keyword.
    #[serde(default)]
    pub news_sentiment: Option<f64>,
    pub updated_at: DateTime<Utc>,
}

// ═══════════════════════════════════════════════════════════════════
//  MarketDataState — central market data store  (Tier B)
//
//  S05/S13/S14 lock ordering (acquire ascending — matches streamer hot path):
//    1. last_prices
//    2. cvd
//    3. candle_buffers
//    4. indicator_cache
//    5.halts             (S13: populated by streamer status messages)
//    6. watchlist_metrics (S14: written by watchlist_refresh loop @30s)
//    7. micro             (F1: MicroBook per-symbol)
//    8. burst             (F1b: BurstScanner tape-wide)
//
//  Lock-free / atomic (no ordering constraint):
//    - ofi_state   (ArcSwap)
//    - bars_dropped (AtomicU64)
//    - stream_stale (AtomicBool)
//    - stream_reconnected_at (AtomicU64)
//
//  Boundary rule: never hold a Tier-A (TitanState) lock while
//  acquiring a Tier-B lock, or vice-versa. See state.rs for the
//  full two-tier hierarchy.
// ═══════════════════════════════════════════════════════════════════

pub struct MarketDataState {
    // --- Tier B mutexes (acquire in numbered order) ---
    pub last_prices: Mutex<HashMap<String, f64>>,       // B1
    pub cvd: Mutex<HashMap<String, f64>>,                // B2
    pub candle_buffers: Mutex<HashMap<String, CandleRingBuffer>>, // B3
    #[allow(dead_code)] // Used by sentinel loop (Session 6)
    pub indicator_cache: Mutex<IndicatorCache>,           // B4
    /// S13: trading-halt state per symbol. Written by
    /// `apply_status_message` from the streamer (Alpaca SIP `T="s"`).
    /// Read by scanner/watchlist via `halts_snapshot` (clone-and-drop).
    pub halts: Mutex<HashMap<String, HaltState>>,         // B5
    /// S14: pre-computed watchlist metrics. Written by the 30s
    /// `watchlist_refresh` background task. Read by
    /// `commands::discovery::get_watchlist` via `watchlist_metrics_snapshot`
    /// (clone-and-drop). Acquire AFTER `halts` (B5) when both are needed.
    pub watchlist_metrics: Mutex<HashMap<String, WatchlistMetrics>>, // B6
    /// F1 — per-watched-symbol NBBO + signed-flow rings (15s/60s/5m).
    pub micro: Mutex<crate::market::micro::MicroBook>, // B7
    /// F1b — tape-wide burst counters (all SIP trades, capped).
    pub burst: Mutex<crate::market::micro::BurstScanner>, // B8
    /// MERIDIAN C-PM — premarket 1-min bar builder over ALL SIP trades
    /// (04:00–10:30 ET). Written by the streamer, drained by
    /// `premarket_live`. Acquired alone; never together with B1–B8.
    pub premarket_tape: Mutex<crate::market::premarket_tape::PremarketTape>, // B9
    // --- Lock-free / atomic ---
    pub ofi_state: arc_swap::ArcSwap<OfiState>,
    pub stream_stale: AtomicBool,
    pub stream_reconnected_at: AtomicU64,
    pub bars_dropped: AtomicU64,
    pub bot_actions_dropped: AtomicU64,
    /// F2 — resume event fan-out (streamer → sentinel / frontier loop).
    /// Utility mutex — not part of Tier-B ordering; only swap/send.
    pub resume_tx: Mutex<Option<tokio::sync::mpsc::Sender<ResumeEvent>>>,
}

impl MarketDataState {
    pub fn new() -> Self {
        Self {
            last_prices: Mutex::new(HashMap::new()),
            cvd: Mutex::new(HashMap::new()),
            candle_buffers: Mutex::new(HashMap::new()),
            indicator_cache: Mutex::new(IndicatorCache::new()),
            halts: Mutex::new(HashMap::new()),
            watchlist_metrics: Mutex::new(HashMap::new()),
            micro: Mutex::new(crate::market::micro::MicroBook::new()),
            burst: Mutex::new(crate::market::micro::BurstScanner::new()),
            premarket_tape: Mutex::new(crate::market::premarket_tape::PremarketTape::new()),
            ofi_state: arc_swap::ArcSwap::from_pointee(OfiState::default()),
            stream_stale: AtomicBool::new(false),
            stream_reconnected_at: AtomicU64::new(0),
            bars_dropped: AtomicU64::new(0),
            bot_actions_dropped: AtomicU64::new(0),
            resume_tx: Mutex::new(None),
        }
    }

    /// S13: process an Alpaca SIP `T="s"` status message and update
    /// `halts` accordingly. Per official Alpaca real-time stock docs:
    ///   - field `sc` = status code (machine-readable);
    ///   - field `sm` = human status text;
    ///   - field `rm` = human reason text.
    ///
    /// Halt codes: `H` (UTP halt), `2` (CTA halt), `P` (UTP volatility
    /// trading pause). All three mean "do not trade".
    /// Resume codes: `Q` (UTP quote resumption), `T` (UTP trade
    /// resumption), `R` (generic resume), `3` (CTA resume).
    /// Anything else is logged at trace level and ignored.
    ///
    /// Acquires only B5 (`halts`); never overlaps with other tiers.
    pub fn apply_status_message(&self, item: &serde_json::Value) {
        let raw_sym = item.get("S").and_then(|v| v.as_str()).unwrap_or("");
        if raw_sym.is_empty() {
            return;
        }
        let sym = raw_sym.to_uppercase();
        let sc = item.get("sc").and_then(|v| v.as_str()).unwrap_or("");
        let sm = item.get("sm").and_then(|v| v.as_str()).unwrap_or("");
        let rm = item.get("rm").and_then(|v| v.as_str());

        match sc {
            "H" | "2" | "P" => {
                let reason = match (sm.is_empty(), rm) {
                    (false, Some(r)) if !r.is_empty() => Some(format!("{sm} ({r})")),
                    (false, _) => Some(sm.to_string()),
                    (true, Some(r)) if !r.is_empty() => Some(r.to_string()),
                    _ => None,
                };
                // Best-effort pre-halt price from last_prices (B1 alone).
                let pre_halt_price = self
                    .last_prices
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&sym)
                    .copied()
                    .filter(|p| *p > 0.0);
                let mut halts = self.halts.lock().unwrap_or_else(|p| p.into_inner());
                halts.insert(
                    sym,
                    HaltState {
                        halted_since: Utc::now(),
                        reason,
                        source: HaltSource::StatusSub,
                        halt_code: Some(sc.to_string()),
                        pre_halt_price,
                        resumed_at: None,
                        resume_prints: Vec::new(),
                    },
                );
            }
            "Q" | "T" | "R" | "3" => {
                // F2: keep the row, stamp resumed_at (do NOT delete).
                let resumed_at = Utc::now();
                let event = {
                    let mut halts = self.halts.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(h) = halts.get_mut(&sym) {
                        if h.resumed_at.is_none() {
                            h.resumed_at = Some(resumed_at);
                            Some(ResumeEvent {
                                symbol: sym.clone(),
                                resumed_at,
                                halt_code: h.halt_code.clone(),
                                pre_halt_price: h.pre_halt_price,
                                halted_since: h.halted_since,
                            })
                        } else {
                            None
                        }
                    } else {
                        // Resume without prior halt row — synthesize minimal state.
                        let ev = ResumeEvent {
                            symbol: sym.clone(),
                            resumed_at,
                            halt_code: None,
                            pre_halt_price: None,
                            halted_since: resumed_at,
                        };
                        halts.insert(
                            sym.clone(),
                            HaltState {
                                halted_since: resumed_at,
                                reason: Some("resume_without_halt".into()),
                                source: HaltSource::StatusSub,
                                halt_code: None,
                                pre_halt_price: None,
                                resumed_at: Some(resumed_at),
                                resume_prints: Vec::new(),
                            },
                        );
                        Some(ev)
                    }
                };
                if let Some(ev) = event {
                    if let Ok(guard) = self.resume_tx.lock() {
                        if let Some(tx) = guard.as_ref() {
                            let _ = tx.try_send(ev);
                        }
                    }
                }
            }
            other => {
                tracing::trace!(
                    component = "streamer",
                    symbol = %sym,
                    sc = %other,
                    "status code not recognised as halt or resume"
                );
            }
        }
    }

    /// F2 — append a post-resume print (cap 32). Call after Tier-B B5 only.
    pub fn record_resume_print(&self, symbol: &str, price: f64, size: f64, is_buy: bool) {
        if price <= 0.0 || size <= 0.0 {
            return;
        }
        let mut halts = self.halts.lock().unwrap_or_else(|p| p.into_inner());
        let Some(h) = halts.get_mut(symbol) else { return };
        if h.resumed_at.is_none() {
            return;
        }
        if h.resume_prints.len() >= 32 {
            return;
        }
        h.resume_prints.push(ResumePrint {
            ts: Utc::now(),
            price,
            size,
            is_buy,
        });
    }

    /// Still-halted = present in map AND resumed_at is None.
    pub fn is_symbol_halted(&self, symbol: &str) -> bool {
        self.halts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(symbol)
            .map(|h| h.resumed_at.is_none())
            .unwrap_or(false)
    }

    /// S13: clone-and-release of the halts map for downstream consumers
    /// (scanner / watchlist) that need to read without holding B5.
    pub fn halts_snapshot(&self) -> HashMap<String, HaltState> {
        self.halts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// S14: clone-and-release of the watchlist metrics map. Mirrors
    /// `halts_snapshot` — the lock is held only for the clone, never
    /// across `await` or downstream computation. `commands::discovery`
    /// uses this in the primary path of `get_watchlist`.
    pub fn watchlist_metrics_snapshot(&self) -> HashMap<String, WatchlistMetrics> {
        self.watchlist_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// S14: replace the entire watchlist metrics map with a freshly
    /// computed batch. Called from `watchlist_refresh::run_watchlist_refresh_loop`
    /// after the per-symbol catalyst computation is done; the lock is
    /// held only for the swap. Pre-allocates capacity for callers.
    pub fn replace_watchlist_metrics(
        &self,
        fresh: HashMap<String, WatchlistMetrics>,
    ) {
        let mut g = self
            .watchlist_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *g = fresh;
    }

    pub fn mark_stream_stale(&self) {
        self.stream_stale.store(true, Ordering::SeqCst);
    }

    pub fn mark_stream_data_received(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let recon_at = self.stream_reconnected_at.load(Ordering::Relaxed);
        if recon_at > 0
            && now.saturating_sub(recon_at) >= 30
            && self.stream_stale.load(Ordering::Relaxed)
        {
            self.stream_stale.store(false, Ordering::SeqCst);
            tracing::info!(component = "streamer", "Stream data fresh — stale flag cleared");
        }
    }

    pub fn mark_stream_reconnected(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.stream_reconnected_at.store(now, Ordering::SeqCst);
    }

    #[allow(dead_code)] // Used by sentinel/bot (Session 6+)
    pub fn is_stream_stale(&self) -> bool {
        self.stream_stale.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn stream_stale_flag_on_disconnect() {
        let ms = MarketDataState::new();
        assert!(!ms.is_stream_stale());

        ms.mark_stream_stale();
        assert!(ms.is_stream_stale());

        ms.mark_stream_reconnected();
        ms.mark_stream_data_received();
        assert!(ms.is_stream_stale(), "still stale within 30s warmup");

        ms.stream_reconnected_at.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(31),
            Ordering::SeqCst,
        );
        ms.mark_stream_data_received();
        assert!(!ms.is_stream_stale(), "fresh after 30s warmup");
    }

    #[test]
    fn last_prices_and_ofi_swap() {
        let ms = MarketDataState::new();
        ms.last_prices.lock().unwrap().insert("AAPL".into(), 185.50);
        assert_eq!(*ms.last_prices.lock().unwrap().get("AAPL").unwrap(), 185.50);

        let ofi = ms.ofi_state.load();
        let _ = Arc::clone(&ofi);

        let new_ofi = OfiState { current_ofi: 42.0, ..Default::default() };
        ms.ofi_state.store(Arc::new(new_ofi));
        assert!((ms.ofi_state.load().current_ofi - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cvd_accumulation() {
        let ms = MarketDataState::new();
        {
            let mut cvd = ms.cvd.lock().unwrap();
            *cvd.entry("AAPL".into()).or_insert(0.0) += 100.0;
            *cvd.entry("AAPL".into()).or_insert(0.0) += 50.0;
            *cvd.entry("AAPL".into()).or_insert(0.0) -= 30.0;
        }
        let cvd = ms.cvd.lock().unwrap();
        let val = *cvd.get("AAPL").unwrap();
        assert!((val - 120.0).abs() < 1e-10);
    }

    #[test]
    fn microprice_symmetric() {
        let mp = compute_microprice(100.0, 101.0, 100.0, 100.0);
        assert!((mp - 100.5).abs() < 1e-10);
    }

    #[test]
    fn microprice_asymmetric() {
        let mp = compute_microprice(100.0, 101.0, 200.0, 100.0);
        assert!((mp - 100.6667).abs() < 0.001);
        assert!(mp > 100.5);
    }

    #[test]
    fn microprice_zero_sizes() {
        let mp = compute_microprice(100.0, 102.0, 0.0, 0.0);
        assert!((mp - 101.0).abs() < 1e-10);
    }

    #[test]
    fn microprice_zero_prices() {
        assert!(compute_microprice(0.0, 0.0, 100.0, 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ofi_state_stale_detection() {
        let s = OfiState::default();
        assert!(s.is_stale(1000));

        let s2 = OfiState {
            last_update: Some(Instant::now()),
            ..Default::default()
        };
        assert!(!s2.is_stale(5000));
    }

    // ─── S13 halt tests ────────────────────────────────────────────

    #[test]
    fn halt_status_sub_marks_symbol_halted() {
        let ms = MarketDataState::new();
        let msg = serde_json::json!({
            "T": "s",
            "S": "AAPL",
            "sc": "H",
            "sm": "Trading Halt",
            "rm": "Halt News Pending",
            "t": "2026-04-29T14:30:00Z",
            "z": "C",
        });
        ms.apply_status_message(&msg);

        let halts = ms.halts_snapshot();
        let h = halts.get("AAPL").expect("AAPL must be in halts");
        assert_eq!(h.source, HaltSource::StatusSub);
        assert!(h.reason.as_deref().unwrap_or("").contains("Trading Halt"));
        assert!(h.reason.as_deref().unwrap_or("").contains("Halt News Pending"));
    }

    #[test]
    fn halt_resume_stamps_resumed_at() {
        let ms = MarketDataState::new();

        // First halt the symbol
        ms.apply_status_message(&serde_json::json!({
            "T": "s", "S": "AAPL", "sc": "H", "sm": "Trading Halt",
            "rm": "", "t": "2026-04-29T14:30:00Z", "z": "C",
        }));
        assert!(ms.is_symbol_halted("AAPL"));

        // F2: resume codes stamp resumed_at; row is retained for first prints.
        for resume_code in &["Q", "T", "R", "3"] {
            ms.apply_status_message(&serde_json::json!({
                "T": "s", "S": "AAPL", "sc": "H", "sm": "halt",
                "t": "2026-04-29T14:30:00Z", "z": "C",
            }));
            assert!(ms.is_symbol_halted("AAPL"), "pre-resume halt missing for code {resume_code}");

            ms.apply_status_message(&serde_json::json!({
                "T": "s", "S": "AAPL", "sc": resume_code, "sm": "Resume",
                "t": "2026-04-29T14:35:00Z", "z": "C",
            }));
            let h = ms.halts_snapshot().get("AAPL").cloned();
            assert!(h.is_some(), "resume code {resume_code} must retain halt row");
            assert!(h.unwrap().resumed_at.is_some(), "resume code {resume_code} must set resumed_at");
            assert!(!ms.is_symbol_halted("AAPL"), "must not be actively halted after resume");
        }
    }

    #[test]
    fn resume_captures_first_prints_and_emits_event() {
        let ms = MarketDataState::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        *ms.resume_tx.lock().unwrap() = Some(tx);

        ms.last_prices
            .lock()
            .unwrap()
            .insert("ZZZ".into(), 10.0);
        ms.apply_status_message(&serde_json::json!({
            "T": "s", "S": "ZZZ", "sc": "H", "sm": "halt",
            "t": "2026-04-29T14:30:00Z", "z": "C",
        }));
        assert_eq!(
            ms.halts_snapshot().get("ZZZ").and_then(|h| h.pre_halt_price),
            Some(10.0)
        );

        ms.apply_status_message(&serde_json::json!({
            "T": "s", "S": "ZZZ", "sc": "Q", "sm": "Resume",
            "t": "2026-04-29T14:35:00Z", "z": "C",
        }));
        let ev = rx.try_recv().expect("ResumeEvent must be sent");
        assert_eq!(ev.symbol, "ZZZ");
        assert_eq!(ev.pre_halt_price, Some(10.0));

        ms.record_resume_print("ZZZ", 10.5, 200.0, true);
        ms.record_resume_print("ZZZ", 10.4, 100.0, false);
        let h = ms.halts_snapshot().get("ZZZ").cloned().unwrap();
        assert_eq!(h.resume_prints.len(), 2);
        assert!(h.resume_prints[0].is_buy);
        assert!((h.resume_prints[0].price - 10.5).abs() < 1e-9);
    }

    #[test]
    fn halt_pause_volatility_treated_as_halt() {
        let ms = MarketDataState::new();
        ms.apply_status_message(&serde_json::json!({
            "T": "s",
            "S": "TSLA",
            "sc": "P",
            "sm": "Volatility Trading Pause",
            "rm": "LULD Pause",
            "t": "2026-04-29T14:30:00Z",
            "z": "C",
        }));

        let halts = ms.halts_snapshot();
        let h = halts.get("TSLA").expect("TSLA must be in halts (LULD pause)");
        assert_eq!(h.source, HaltSource::StatusSub);
    }

    #[test]
    fn halt_unknown_status_code_ignored() {
        let ms = MarketDataState::new();
        ms.apply_status_message(&serde_json::json!({
            "T": "s", "S": "AAPL", "sc": "Z9", "sm": "Unknown",
            "t": "2026-04-29T14:30:00Z", "z": "C",
        }));
        assert!(ms.halts_snapshot().is_empty());
    }

    #[test]
    fn halt_lowercase_symbol_normalised_uppercase() {
        let ms = MarketDataState::new();
        ms.apply_status_message(&serde_json::json!({
            "T": "s", "S": "aapl", "sc": "H", "sm": "Halt",
            "t": "2026-04-29T14:30:00Z", "z": "C",
        }));
        let halts = ms.halts_snapshot();
        assert!(halts.contains_key("AAPL"));
        assert!(!halts.contains_key("aapl"));
    }

    // ─── S14 watchlist_metrics tests ────────────────────────────────

    fn sample_metrics(symbol: &str) -> WatchlistMetrics {
        WatchlistMetrics {
            symbol: symbol.to_string(),
            price: 150.0,
            bid: 149.95,
            ask: 150.05,
            spread_bps: 6.67,
            change_percent: 2.5,
            volume: 1_500_000.0,
            rvol: 3.2,
            gap_percent: 1.4,
            last_trade_age_seconds: 4,
            is_stale: false,
            catalyst_flags: crate::market::catalyst::CatalystFlags::default(),
            catalyst_pills: Vec::new(),
            news_sentiment: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn watchlist_metrics_field_default_empty() {
        let ms = MarketDataState::new();
        assert!(ms.watchlist_metrics_snapshot().is_empty());
    }

    #[test]
    fn watchlist_metrics_replace_then_snapshot() {
        let ms = MarketDataState::new();
        let mut fresh = HashMap::new();
        fresh.insert("AAPL".to_string(), sample_metrics("AAPL"));
        fresh.insert("MSFT".to_string(), sample_metrics("MSFT"));
        ms.replace_watchlist_metrics(fresh);

        let snap = ms.watchlist_metrics_snapshot();
        assert_eq!(snap.len(), 2);
        let aapl = snap.get("AAPL").expect("AAPL present");
        assert_eq!(aapl.symbol, "AAPL");
        assert!((aapl.spread_bps - 6.67).abs() < 1e-6);
        assert!(!aapl.is_stale);

        // Replace empties out previous content (no merging).
        ms.replace_watchlist_metrics(HashMap::new());
        assert!(ms.watchlist_metrics_snapshot().is_empty());
    }

    #[test]
    fn watchlist_metrics_serde_roundtrip() {
        let m = sample_metrics("TSLA");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"symbol\":\"TSLA\""));
        assert!(json.contains("\"spread_bps\":6.67"));
        assert!(json.contains("\"catalyst_flags\""));
        assert!(json.contains("\"catalyst_pills\""));
        assert!(json.contains("\"updated_at\""));
        let loaded: WatchlistMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, m);
    }
}
