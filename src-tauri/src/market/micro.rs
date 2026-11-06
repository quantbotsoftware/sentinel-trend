//! F1 / F1b — per-symbol microstructure book + tape burst discovery.
//!
//! Tier-B7 (`MarketDataState.micro`). Cap 50 watched entries (S06).
//! Hot path: streamer `on_quote` / `on_trade`. Consumers clone via `get()`.
//! Never hold B7 across Tier-A locks.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use serde::Serialize;

use crate::market::market_state::compute_microprice;

pub const MICRO_CAP: usize = 50;
pub const BURST_CAP: usize = 2_048;
const WINDOW_15S: f64 = 15.0;
const WINDOW_60S: f64 = 60.0;
const WINDOW_5M: f64 = 300.0;

/// Snapshot returned to strategy / gate consumers (cloned, no lock held).
#[derive(Debug, Clone, Serialize)]
pub struct MicroSnapshot {
    pub bid: f64,
    pub ask: f64,
    pub bid_size: f64,
    pub ask_size: f64,
    pub spread_bps: f64,
    pub imbalance: f64,
    pub microprice: f64,
    pub quote_age_ms: u128,
    pub prints_per_sec_15s: f64,
    pub prints_per_sec_60s: f64,
    pub signed_notional_15s: f64,
    pub signed_notional_60s: f64,
    pub signed_notional_5m: f64,
    pub large_print_pct_15s: f64,
    pub last_trade_price: f64,
    pub update_count: u64,
    pub fresh: bool,
}

#[derive(Clone, Copy)]
struct TradeTick {
    t: Instant,
    signed_notional: f64,
    size: f64,
}

struct MicroEntry {
    bid: f64,
    ask: f64,
    bid_size: f64,
    ask_size: f64,
    last_quote: Instant,
    last_trade_price: f64,
    trades: VecDeque<TradeTick>,
    update_count: u64,
    touched_at: Instant,
    bid_deplete: u32,
    ask_deplete: u32,
    prev_bid_size: f64,
    prev_ask_size: f64,
}

impl MicroEntry {
    fn fresh(now: Instant) -> Self {
        Self {
            bid: 0.0,
            ask: 0.0,
            bid_size: 0.0,
            ask_size: 0.0,
            last_quote: now,
            last_trade_price: 0.0,
            trades: VecDeque::with_capacity(64),
            update_count: 0,
            touched_at: now,
            bid_deplete: 0,
            ask_deplete: 0,
            prev_bid_size: 0.0,
            prev_ask_size: 0.0,
        }
    }

    fn prune(&mut self, now: Instant) {
        let cutoff = now
            .checked_duration_since(self.last_quote)
            .map(|_| WINDOW_5M)
            .unwrap_or(WINDOW_5M);
        let _ = cutoff;
        while let Some(front) = self.trades.front() {
            if now.duration_since(front.t).as_secs_f64() > WINDOW_5M {
                self.trades.pop_front();
            } else {
                break;
            }
        }
    }

    fn window_stats(&self, now: Instant, window_secs: f64) -> (f64, f64, f64) {
        let mut n = 0.0;
        let mut signed = 0.0;
        let mut large = 0.0;
        let mut total_sz = 0.0;
        for t in &self.trades {
            if now.duration_since(t.t).as_secs_f64() <= window_secs {
                n += 1.0;
                signed += t.signed_notional;
                total_sz += t.size;
                if t.size >= 1_000.0 {
                    large += 1.0;
                }
            }
        }
        let pps = if window_secs > 0.0 { n / window_secs } else { 0.0 };
        let large_pct = if n > 0.0 { large / n } else { 0.0 };
        let _ = total_sz;
        (pps, signed, large_pct)
    }

    fn snapshot(&self, now: Instant) -> MicroSnapshot {
        let total = self.bid_size + self.ask_size;
        let imbalance = if total > 0.0 {
            (self.bid_size - self.ask_size) / total
        } else {
            0.0
        };
        let spread_bps = if self.bid > 0.0 {
            ((self.ask - self.bid) / self.bid) * 10_000.0
        } else {
            0.0
        };
        let (pps15, signed15, large15) = self.window_stats(now, WINDOW_15S);
        let (pps60, signed60, _) = self.window_stats(now, WINDOW_60S);
        let (_, signed5m, _) = self.window_stats(now, WINDOW_5M);
        let quote_age_ms = now.duration_since(self.last_quote).as_millis();
        MicroSnapshot {
            bid: self.bid,
            ask: self.ask,
            bid_size: self.bid_size,
            ask_size: self.ask_size,
            spread_bps,
            imbalance,
            microprice: compute_microprice(self.bid, self.ask, self.bid_size, self.ask_size),
            quote_age_ms,
            prints_per_sec_15s: pps15,
            prints_per_sec_60s: pps60,
            signed_notional_15s: signed15,
            signed_notional_60s: signed60,
            signed_notional_5m: signed5m,
            large_print_pct_15s: large15,
            last_trade_price: self.last_trade_price,
            update_count: self.update_count,
            fresh: quote_age_ms <= 2_000,
        }
    }
}

/// Quote-aligned trade side: buy if `>= ask`, sell if `<= bid`, else tick.
pub fn classify_quote_aligned(
    price: f64,
    bid: f64,
    ask: f64,
    last_trade: f64,
) -> bool {
    if ask > 0.0 && price >= ask {
        return true;
    }
    if bid > 0.0 && price <= bid {
        return false;
    }
    // Mid / unknown → tick fallback
    if last_trade > 0.0 {
        return price >= last_trade;
    }
    true
}

/// Per-watched-symbol micro book (Tier B7).
pub struct MicroBook {
    map: HashMap<String, MicroEntry>,
}

impl Default for MicroBook {
    fn default() -> Self {
        Self::new()
    }
}

impl MicroBook {
    pub fn new() -> Self {
        Self {
            map: HashMap::with_capacity(MICRO_CAP),
        }
    }

    pub fn on_quote(
        &mut self,
        symbol: &str,
        bid: f64,
        ask: f64,
        bid_size: f64,
        ask_size: f64,
        now: Instant,
    ) {
        if bid <= 0.0 || ask <= 0.0 {
            return;
        }
        let e = self.entry_mut(symbol, now);
        if e.prev_bid_size > 0.0 && bid_size + 1e-9 < e.prev_bid_size {
            e.bid_deplete = e.bid_deplete.saturating_add(1);
        }
        if e.prev_ask_size > 0.0 && ask_size + 1e-9 < e.prev_ask_size {
            e.ask_deplete = e.ask_deplete.saturating_add(1);
        }
        e.prev_bid_size = bid_size;
        e.prev_ask_size = ask_size;
        e.bid = bid;
        e.ask = ask;
        e.bid_size = bid_size;
        e.ask_size = ask_size;
        e.last_quote = now;
        e.touched_at = now;
        e.update_count = e.update_count.saturating_add(1);
    }

    pub fn on_trade(
        &mut self,
        symbol: &str,
        price: f64,
        size: f64,
        now: Instant,
    ) {
        if price <= 0.0 || size <= 0.0 {
            return;
        }
        let (bid, ask, last) = {
            let e = self.map.get(symbol);
            match e {
                Some(e) => (e.bid, e.ask, e.last_trade_price),
                None => (0.0, 0.0, 0.0),
            }
        };
        let is_buy = classify_quote_aligned(price, bid, ask, last);
        let signed = if is_buy { price * size } else { -(price * size) };
        let e = self.entry_mut(symbol, now);
        e.last_trade_price = price;
        e.trades.push_back(TradeTick {
            t: now,
            signed_notional: signed,
            size,
        });
        e.prune(now);
        e.touched_at = now;
        e.update_count = e.update_count.saturating_add(1);
    }

    pub fn get(&self, symbol: &str) -> Option<MicroSnapshot> {
        self.map.get(symbol).map(|e| e.snapshot(Instant::now()))
    }

    pub fn imbalance(&self, symbol: &str) -> Option<f64> {
        self.get(symbol).map(|s| s.imbalance)
    }

    pub fn reset_daily(&mut self) {
        self.map.clear();
    }

    pub fn retain_watched(&mut self, watched: &std::collections::HashSet<String>) {
        self.map.retain(|k, _| watched.contains(k));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn entry_mut(&mut self, symbol: &str, now: Instant) -> &mut MicroEntry {
        if !self.map.contains_key(symbol) {
            self.evict_lru_if_full();
            self.map
                .insert(symbol.to_string(), MicroEntry::fresh(now));
        }
        self.map.get_mut(symbol).expect("just inserted")
    }

    fn evict_lru_if_full(&mut self) {
        if self.map.len() < MICRO_CAP {
            return;
        }
        if let Some(victim) = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.touched_at)
            .map(|(k, _)| k.clone())
        {
            self.map.remove(&victim);
        }
    }
}

// ─── F1b: tape-wide burst discovery (level-1 counters) ───────────────

#[derive(Debug, Clone)]
pub struct BurstPromotion {
    pub symbol: String,
    pub notional_per_sec: f64,
    pub prints_per_sec: f64,
    pub move_pct_60s: f64,
}

struct BurstEntry {
    window_start: Instant,
    prints: u32,
    notional: f64,
    first_price: f64,
    last_price: f64,
    touched_at: Instant,
}

/// Bounded tape-wide burst detector. Feeds promotions into watched set.
pub struct BurstScanner {
    map: HashMap<String, BurstEntry>,
    /// Notional $/sec threshold for promotion (calibrated in F4).
    pub notional_per_sec_threshold: f64,
    /// Absolute |move| % over ~60s window required with notional spike.
    pub move_pct_threshold: f64,
    promotions_today: u32,
}

impl Default for BurstScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl BurstScanner {
    pub fn new() -> Self {
        Self {
            map: HashMap::with_capacity(512),
            notional_per_sec_threshold: 50_000.0,
            move_pct_threshold: 1.5,
            promotions_today: 0,
        }
    }

    /// Observe a trade on ANY symbol (pre-watched filter). Returns
    /// `Some(BurstPromotion)` when thresholds are crossed.
    pub fn on_trade(
        &mut self,
        symbol: &str,
        price: f64,
        size: f64,
        now: Instant,
    ) -> Option<BurstPromotion> {
        if price <= 0.0 || size <= 0.0 || symbol.is_empty() {
            return None;
        }
        if self.map.len() >= BURST_CAP && !self.map.contains_key(symbol) {
            self.evict_lru();
        }
        let e = self.map.entry(symbol.to_string()).or_insert_with(|| BurstEntry {
            window_start: now,
            prints: 0,
            notional: 0.0,
            first_price: price,
            last_price: price,
            touched_at: now,
        });
        // Roll window every ~60s
        if now.duration_since(e.window_start).as_secs_f64() > WINDOW_60S {
            e.window_start = now;
            e.prints = 0;
            e.notional = 0.0;
            e.first_price = price;
        }
        e.prints = e.prints.saturating_add(1);
        e.notional += price * size;
        e.last_price = price;
        e.touched_at = now;

        let elapsed = now.duration_since(e.window_start).as_secs_f64().max(1.0);
        let nps = e.notional / elapsed;
        let pps = e.prints as f64 / elapsed;
        let move_pct = if e.first_price > 0.0 {
            ((e.last_price - e.first_price) / e.first_price).abs() * 100.0
        } else {
            0.0
        };
        if nps >= self.notional_per_sec_threshold && move_pct >= self.move_pct_threshold {
            // Reset so we don't spam the same symbol every trade
            e.window_start = now;
            e.prints = 0;
            e.notional = 0.0;
            e.first_price = price;
            self.promotions_today = self.promotions_today.saturating_add(1);
            return Some(BurstPromotion {
                symbol: symbol.to_string(),
                notional_per_sec: nps,
                prints_per_sec: pps,
                move_pct_60s: move_pct,
            });
        }
        None
    }

    pub fn promotions_today(&self) -> u32 {
        self.promotions_today
    }

    pub fn reset_daily(&mut self) {
        self.map.clear();
        self.promotions_today = 0;
    }

    fn evict_lru(&mut self) {
        if let Some(victim) = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.touched_at)
            .map(|(k, _)| k.clone())
        {
            self.map.remove(&victim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn no_cross_symbol_contamination() {
        let mut book = MicroBook::new();
        let t0 = Instant::now();
        book.on_quote("AAPL", 100.0, 100.1, 500.0, 100.0, t0);
        book.on_quote("XYZ", 5.0, 5.05, 10.0, 900.0, t0);
        let a = book.get("AAPL").unwrap();
        let x = book.get("XYZ").unwrap();
        assert!(a.imbalance > 0.0, "AAPL bid-heavy");
        assert!(x.imbalance < 0.0, "XYZ ask-heavy");
        assert!((a.imbalance - x.imbalance).abs() > 0.5);
    }

    #[test]
    fn quote_aligned_signing() {
        assert!(classify_quote_aligned(10.05, 10.0, 10.05, 10.0)); // at ask → buy
        assert!(!classify_quote_aligned(10.0, 10.0, 10.05, 10.02)); // at bid → sell
        assert!(classify_quote_aligned(10.03, 10.0, 10.05, 10.02)); // mid uptick → buy
    }

    #[test]
    fn lru_cap_enforced() {
        let mut book = MicroBook::new();
        let t0 = Instant::now();
        for i in 0..(MICRO_CAP + 5) {
            let sym = format!("S{i}");
            book.on_quote(&sym, 1.0, 1.01, 1.0, 1.0, t0 + Duration::from_millis(i as u64));
        }
        assert_eq!(book.len(), MICRO_CAP);
    }

    #[test]
    fn burst_promotes_on_notional_and_move() {
        let mut burst = BurstScanner::new();
        burst.notional_per_sec_threshold = 1_000.0;
        burst.move_pct_threshold = 1.0;
        let t0 = Instant::now();
        let mut promo = None;
        // 10 trades of $500 notional each at rising prices over ~2s
        for i in 0..20 {
            let price = 10.0 + (i as f64) * 0.1; // +20% move
            promo = burst.on_trade("RUNR", price, 100.0, t0 + Duration::from_millis(i * 100));
            if promo.is_some() {
                break;
            }
        }
        let p = promo.expect("must promote");
        assert_eq!(p.symbol, "RUNR");
        assert!(p.move_pct_60s >= 1.0);
    }
}
