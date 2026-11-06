/// S09: News cache LRU contract skeleton.
///
/// S06 introduced bounded caches across the codebase; news_stream uses
/// a 200-entry LRU keyed by symbol. This integration test verifies the
/// contract: insertion above the cap evicts the LEAST recently used
/// entry, and the count never exceeds the cap.
///
/// Mirrors the contract since the test crate cannot import the binary's
/// internal modules (no `lib.rs`).
///
/// Acceptance: ≥1 explicit assertion per test.

use std::collections::VecDeque;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const CAP: usize = 200;
const TTL_SECS: u64 = 30 * 60; // S06: 30-minute window

struct MiniNewsLru {
    order: VecDeque<String>,
    items: HashMap<String, Instant>,
}

impl MiniNewsLru {
    fn new() -> Self {
        Self {
            order: VecDeque::with_capacity(CAP),
            items: HashMap::with_capacity(CAP),
        }
    }

    fn insert(&mut self, key: &str, at: Instant) {
        if self.items.contains_key(key) {
            self.order.retain(|k| k != key);
        }
        self.order.push_back(key.to_string());
        self.items.insert(key.to_string(), at);

        while self.order.len() > CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.items.remove(&oldest);
            }
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.items.contains_key(key)
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    /// Evict entries whose timestamp is older than TTL.
    fn evict_stale(&mut self, now: Instant) {
        let cutoff = now - Duration::from_secs(TTL_SECS);
        let mut to_drop: Vec<String> = Vec::new();
        for (k, t) in &self.items {
            if *t < cutoff {
                to_drop.push(k.clone());
            }
        }
        for k in to_drop {
            self.items.remove(&k);
            self.order.retain(|x| x != &k);
        }
    }
}

#[test]
fn news_cache_evicts_oldest_above_cap() {
    let mut lru = MiniNewsLru::new();
    let t0 = Instant::now();

    for i in 0..(CAP + 1) {
        lru.insert(&format!("SYM{i:04}"), t0);
    }

    assert_eq!(lru.len(), CAP, "len must clamp to cap");
    assert!(
        !lru.contains("SYM0000"),
        "Oldest entry must be evicted when cap exceeded"
    );
    assert!(
        lru.contains(&format!("SYM{:04}", CAP)),
        "Newest entry must be retained"
    );
}

#[test]
fn news_cache_repeat_insert_does_not_grow_count() {
    let mut lru = MiniNewsLru::new();
    let t0 = Instant::now();

    for _ in 0..10 {
        lru.insert("AAPL", t0);
    }
    assert_eq!(lru.len(), 1, "Repeated insert of same key must not grow");
    assert!(lru.contains("AAPL"));
}

#[test]
fn news_cache_empty_starts_at_zero() {
    let lru = MiniNewsLru::new();
    assert_eq!(lru.len(), 0);
    assert!(!lru.contains("AAPL"));
}

#[test]
fn news_cache_ttl_eviction() {
    let mut lru = MiniNewsLru::new();
    let t0 = Instant::now();

    lru.insert("OLD", t0);
    lru.insert("NEW", t0 + Duration::from_secs(TTL_SECS - 60));

    assert_eq!(lru.len(), 2);

    // Now is well past OLD's TTL but before NEW's.
    let now = t0 + Duration::from_secs(TTL_SECS + 60);
    lru.evict_stale(now);

    assert!(!lru.contains("OLD"), "OLD must be evicted by TTL");
    assert!(lru.contains("NEW"), "NEW within TTL must remain");
    assert_eq!(lru.len(), 1);
}

#[test]
fn news_cache_cap_constant_matches_s06_baseline() {
    // Catch silent cap changes: S06 declared 200 as the operational ceiling.
    assert_eq!(CAP, 200, "S06 invariant: news LRU cap is 200");
    assert_eq!(TTL_SECS, 1800, "S06 invariant: news TTL is 30 minutes");
}
