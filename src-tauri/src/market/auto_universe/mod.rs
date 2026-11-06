//! S18 — Self-Curating Watchlist (AutoUniverse)
//!
//! ## Permanent chart anchors
//! `PERMANENT_CHART_SYMBOLS` (currently `QBTS`) are always kept in
//! `pinned` so the chart never boots empty after EOD flush / demote.
//! They cannot be unpinned or force-removed.
//!
//! ## Why
//! Up to S17 the bot's *trading universe* was hard-bound to
//! `cfg.ui.favorites` — a manually curated list of ≤20 symbols. The
//! `auto_scan` background loop produced fresh candidates every
//! ~30 minutes but those candidates **never reached the bot**: the
//! streamer subscribed only to `favorites`, the bar-close pipeline
//! only fired for `favorites`, and `record_trade_close` blacklisted
//! losers indefinitely (until the daily reset). The auto-scan was
//! cosmetic for the UI, operationally useless for trading.
//!
//! S18 introduces an **AutoUniverse** — a living set of symbols the
//! bot is allowed to trade, partitioned into three zones:
//!
//! ```text
//! ┌─ AutoUniverse (max_total_slots, default 20) ─────────────────────┐
//! │                                                                   │
//! │  📌 PINNED  — user-managed, sacred                                │
//! │     • added/removed only via UI (`pin_symbol`/`unpin_symbol`)     │
//! │     • never auto-evicted, never auto-blacklisted                  │
//! │     • cap: `max_pinned_slots` (default 8)                         │
//! │                                                                   │
//! │  🤖 AUTO    — bot-managed                                         │
//! │     • promoted from `scan_results` (top-N by score)               │
//! │     • evicted on score decay / loss strikes / open-position       │
//! │       absent / catalyst stale                                     │
//! │     • cap: `max_auto_slots` = max_total_slots − pinned.len()      │
//! │                                                                   │
//! │  🚫 BLACKLIST (TTL) — losers cooling off                          │
//! │     • populated when `record_trade_outcome` records the           │
//! │       Nth consecutive loss (`loss_strikes_to_blacklist`)          │
//! │     • prevents re-promotion until TTL expires                     │
//! │     • cleared at daily-reset (along with `symbol_loss_streak`)    │
//! └───────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Position protection (CRITICAL)
//! A symbol with an open broker position MUST stay in the universe so
//! the streamer keeps feeding bars and the bot can manage exits. The
//! caller is responsible for passing the current `protected_symbols`
//! set on every prune/evict call. AutoUniverse never evicts a
//! protected symbol from auto-pool; pinned is independently safe.
//!
//! ## Backward compatibility
//! `AutoUniverse::default()` is empty. The first call to
//! `migrate_from_favorites_if_empty` populates `pinned` from
//! `cfg.ui.favorites` (idempotent), so an existing user upgrading
//! sees their familiar watchlist on the first start with zero data
//! loss. Snapshot persistence uses `#[serde(default)]` so old
//! `state_snapshot.json` files load cleanly.
//!
//! ## Lock ordering
//! `AutoUniverse` lives inside `MarketState` (Tier-A slot 2). All
//! access happens behind `state.market.lock()`. No Tier-B locks are
//! held inside this module — the impl is pure on the data structures.

pub mod types;
pub mod actions;

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use types::{
    AutoUniverseConfig, AutoEntry, BanReason, BlacklistEntry,
    PromotionReport,
};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoUniverse {
    /// User-pinned symbols. Order preserved (insertion order).
    pub pinned: Vec<String>,
    /// Bot-managed symbols, keyed by uppercase symbol.
    pub auto_pool: HashMap<String, AutoEntry>,
    /// TTL-based ban list, keyed by uppercase symbol.
    pub blacklist: HashMap<String, BlacklistEntry>,
    /// S19 — "fresh-born" memory of the previous session's
    /// universe. Populated by `eod_flush()` at 16:00 ET with every
    /// symbol that was in `auto_pool` or `pinned` that day. The
    /// scanner refuses to re-promote any name still listed here
    /// for the duration of `recently_traded_ttl_hours` (default
    /// 18h ≈ overnight + premarket). Stale entries are evicted
    /// by `cleanup_recently_traded`.
    #[serde(default)]
    pub recently_traded: HashMap<String, DateTime<Utc>>,
}

impl AutoUniverse {
    // ── Membership queries ──────────────────────────────────────────

    /// Union of pinned + auto-pool. Used by streamer/watchlist as
    /// the "subscribe" set. Always uppercase.
    pub fn active_set(&self) -> HashSet<String> {
        let mut s: HashSet<String> = self
            .auto_pool
            .keys()
            .map(|k| k.to_uppercase())
            .collect();
        for p in &self.pinned {
            s.insert(p.to_uppercase());
        }
        s
    }

    pub fn is_pinned(&self, symbol: &str) -> bool {
        let key = symbol.trim().to_uppercase();
        self.pinned.iter().any(|s| s.eq_ignore_ascii_case(&key))
    }

    /// Symbols that must never leave the chart/watchlist (operator
    /// directive: always have something to plot). Currently `QBTS`.
    pub const PERMANENT_CHART_SYMBOLS: &'static [&'static str] = &["QBTS"];

    pub fn is_permanent_chart_symbol(symbol: &str) -> bool {
        let key = symbol.trim().to_uppercase();
        Self::PERMANENT_CHART_SYMBOLS
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&key))
    }

    /// Ensure every permanent chart anchor is present in `pinned`
    /// (front of list). Idempotent. Bypasses `max_pinned_slots` so a
    /// full pin list cannot push QBTS out.
    pub fn ensure_permanent_anchors(&mut self) -> bool {
        let mut changed = false;
        for &sym in Self::PERMANENT_CHART_SYMBOLS {
            let key = sym.to_uppercase();
            if self.is_pinned(&key) {
                continue;
            }
            self.auto_pool.remove(&key);
            self.pinned.insert(0, key);
            changed = true;
        }
        changed
    }

    /// Companion to `is_pinned`. Used by tests today; kept on the
    /// public API so future Tauri commands or scanner integrations
    /// can ask "is this symbol bot-managed?" without re-implementing
    /// the lookup.
    #[allow(dead_code)]
    pub fn is_auto(&self, symbol: &str) -> bool {
        self.auto_pool
            .contains_key(&symbol.trim().to_uppercase())
    }

    /// `true` if the symbol has an active blacklist entry. Also
    /// lazily removes expired entries to keep the map tidy.
    /// Hot-path consultation by the entry gate is currently routed
    /// via `is_blacklisted_check` against the stored map directly,
    /// but this `&mut` variant is preserved for test-time and
    /// future write-through callers.
    #[allow(dead_code)]
    pub fn is_blacklisted(&mut self, symbol: &str, now: DateTime<Utc>) -> bool {
        let key = symbol.trim().to_uppercase();
        let expired = self
            .blacklist
            .get(&key)
            .map(|e| e.is_expired(now))
            .unwrap_or(false);
        if expired {
            self.blacklist.remove(&key);
        }
        self.blacklist.contains_key(&key)
    }

    /// S19 — `true` if the symbol is on the "recently traded"
    /// list within the TTL window. Lazily removes stale entries.
    /// `ttl_hours == 0` disables the gate (always returns `false`).
    ///
    /// Currently exercised by unit tests; the hot path inside
    /// `process_scan_results` reads `recently_traded` directly to
    /// avoid the `&mut self` borrow needed for lazy cleanup.
    #[allow(dead_code)]
    pub fn is_recently_traded(
        &mut self,
        symbol: &str,
        now: DateTime<Utc>,
        ttl_hours: u32,
    ) -> bool {
        if ttl_hours == 0 {
            return false;
        }
        let key = symbol.trim().to_uppercase();
        let ttl_secs = (ttl_hours as i64) * 3600;
        let expired = self
            .recently_traded
            .get(&key)
            .map(|added| (now - *added).num_seconds() >= ttl_secs)
            .unwrap_or(false);
        if expired {
            self.recently_traded.remove(&key);
        }
        self.recently_traded.contains_key(&key)
    }

    /// S19 — drop entries past their TTL. Called from the EOD flush
    /// and from `process_scan_results` so the map can't drift forever.
    pub fn cleanup_recently_traded(&mut self, now: DateTime<Utc>, ttl_hours: u32) -> Vec<String> {
        if ttl_hours == 0 {
            let cleared: Vec<String> = self.recently_traded.keys().cloned().collect();
            self.recently_traded.clear();
            return cleared;
        }
        let ttl_secs = (ttl_hours as i64) * 3600;
        let mut expired = Vec::new();
        self.recently_traded.retain(|sym, added| {
            if (now - *added).num_seconds() >= ttl_secs {
                expired.push(sym.clone());
                false
            } else {
                true
            }
        });
        expired
    }

    /// Removes all expired blacklist entries. Cheap; safe to call
    /// from any tick.
    pub fn cleanup_expired_blacklist(&mut self, now: DateTime<Utc>) -> Vec<String> {
        let mut expired = Vec::new();
        self.blacklist.retain(|sym, entry| {
            if entry.is_expired(now) {
                expired.push(sym.clone());
                false
            } else {
                true
            }
        });
        expired
    }

    /// Total slot occupancy. Used by the snapshot command for UI
    /// header rendering; flagged here for the dead-code lint until
    /// that wiring lands.
    #[allow(dead_code)]
    pub fn total_slots_used(&self) -> usize {
        // De-dup: a symbol present in both pinned and auto counts once.
        let mut s = self.active_set();
        s.shrink_to_fit();
        s.len()
    }

    // ── Pin management (user-driven) ────────────────────────────────

    /// Add a symbol to the pinned list. Idempotent. Removes the
    /// symbol from auto-pool if present (pinning is a promotion).
    /// Returns `Ok(())` on success, `Err(reason)` if pinned cap hit.
    pub fn pin(&mut self, symbol: &str, cfg: &AutoUniverseConfig) -> Result<(), String> {
        let key = symbol.trim().to_uppercase();
        if key.is_empty() {
            return Err("empty symbol".into());
        }
        if self.is_pinned(&key) {
            return Ok(()); // idempotent
        }
        if self.pinned.len() >= cfg.max_pinned_slots {
            return Err(format!(
                "pinned cap reached ({}/{})",
                self.pinned.len(),
                cfg.max_pinned_slots
            ));
        }
        self.auto_pool.remove(&key);
        self.pinned.push(key);
        Ok(())
    }

    /// Remove a pinned symbol. Idempotent. Permanent chart anchors
    /// (`QBTS`) are refused — returns `Err`.
    pub fn unpin(&mut self, symbol: &str) -> Result<(), String> {
        let key = symbol.trim().to_uppercase();
        if Self::is_permanent_chart_symbol(&key) {
            return Err(format!(
                "{key} is a permanent chart symbol and cannot be unpinned"
            ));
        }
        self.pinned.retain(|s| !s.eq_ignore_ascii_case(&key));
        Ok(())
    }

    /// Force-remove a symbol from both pinned and auto-pool. Used
    /// by the UI "Force-remove" button. Does NOT add to blacklist
    /// — that's the operator's separate decision. Permanent chart
    /// anchors are refused.
    pub fn force_remove(&mut self, symbol: &str) -> Result<(), String> {
        let key = symbol.trim().to_uppercase();
        if Self::is_permanent_chart_symbol(&key) {
            return Err(format!(
                "{key} is a permanent chart symbol and cannot be removed"
            ));
        }
        self.pinned.retain(|s| !s.eq_ignore_ascii_case(&key));
        self.auto_pool.remove(&key);
        Ok(())
    }

    /// Manual blacklist (UI button). TTL defaults to config but the
    /// operator can override for "rest of day" via `ttl_hours = 0`.
    pub fn manual_blacklist(
        &mut self,
        symbol: &str,
        cfg: &AutoUniverseConfig,
        ttl_hours_override: Option<u32>,
        now: DateTime<Utc>,
    ) {
        let key = symbol.trim().to_uppercase();
        if key.is_empty() {
            return;
        }
        self.auto_pool.remove(&key);
        self.blacklist.insert(
            key.clone(),
            BlacklistEntry {
                symbol: key,
                banned_at: now,
                ttl_hours: ttl_hours_override.unwrap_or(cfg.blacklist_ttl_hours),
                reason: BanReason::Manual,
                last_loss_amount: 0.0,
                loss_strikes_at_ban: 0,
            },
        );
    }

    /// Clear a blacklist entry (UI override). Idempotent.
    pub fn clear_blacklist_entry(&mut self, symbol: &str) {
        let key = symbol.trim().to_uppercase();
        self.blacklist.remove(&key);
    }

    /// Clear the entire blacklist. Called by the daily-reset boundary.
    pub fn clear_all_blacklist(&mut self) {
        self.blacklist.clear();
    }

    /// Convert an auto entry to pinned (UI button). Returns `Err`
    /// if pinned cap is reached.
    pub fn promote_auto_to_pinned(
        &mut self,
        symbol: &str,
        cfg: &AutoUniverseConfig,
    ) -> Result<(), String> {
        let key = symbol.trim().to_uppercase();
        if !self.auto_pool.contains_key(&key) {
            return Err(format!("{key} not in auto-pool"));
        }
        self.pin(&key, cfg)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "tests_basic.rs"]
mod tests_basic;

#[cfg(test)]
#[path = "tests_scans.rs"]
mod tests_scans;
