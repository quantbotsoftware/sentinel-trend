use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::market::scanner::ScanResult;
use super::types::{
    AutoUniverseConfig, AutoEntry, BlacklistEntry, BanReason, PromotionReport,
    EodFlushReport, TradeOutcomeReport,
};
use super::AutoUniverse;

impl AutoUniverse {
    /// S19 "Fresh-born" EOD flush. Wipes `auto_pool`, `pinned`, and
    /// `blacklist`, and snapshots every name that was on the
    /// watchlist into `recently_traded` so tomorrow's scanner skips
    /// them until the TTL elapses (default 18h covers the overnight
    /// + next-day premarket).
    ///
    /// `protected` is a courtesy set — symbols here are retained in
    /// `auto_pool` so the streamer keeps producing bars while the
    /// EOD liquidator drains the position. In normal operation this
    /// set is empty by the time `eod_flush` runs (the bot's
    /// `flat_by_close` exits all positions 10 minutes earlier), but
    /// if a manual fill is still settling we don't yank the data
    /// feed out from under it.
    ///
    /// Returns the count of items cleared (auto_pool + pinned + blacklist).
    pub fn eod_flush(
        &mut self,
        protected: &HashSet<String>,
        now: DateTime<Utc>,
    ) -> EodFlushReport {
        // 1) Snapshot the bot-curated universe into recently_traded so
        //    tomorrow's scanner excludes them. Only `auto_pool` goes
        //    into the deny-list — `pinned` is editorial (S22, see
        //    step 3) and must NOT block itself from re-engagement.
        for sym in self.auto_pool.keys() {
            self.recently_traded.insert(sym.clone(), now);
        }

        // 2) Wipe auto_pool, retaining only protected symbols.
        let pre_auto = self.auto_pool.len();
        self.auto_pool
            .retain(|sym, _| protected.contains(sym));
        let auto_cleared = pre_auto - self.auto_pool.len();

        // 3) S22 — Keep `pinned` across the EOD flush. Earlier
        //    versions (pre-2026-05-20) wiped pinned wholesale on the
        //    assumption that the next session's scanner would
        //    re-discover everything from scratch ("fresh-born").
        //    Operator directive (2026-05-20 16:00 ET, verbatim:
        //    "păstrăm totuși un asset permanent în watchlist ca să
        //    nu avem harta goală — QBTS"): pinned is the *editorial*
        //    tier — a user-curated conviction list. Wiping it leaves
        //    the watchlist empty between 16:00 ET and the first
        //    successful premarket promotion (~07:00 ET next day),
        //    a ~15h dead-air window that breaks the UI ("hartă
        //    goală") and forces the bot to rebuild candle buffers
        //    from zero each morning. Auto-curated symbols
        //    (`auto_pool`) still flush — fresh-born continues to
        //    apply to bot-statistical selections — but pinned
        //    persists. The `recently_traded` deny-list also skips
        //    pinned (see step 1) so the editorial choice is never
        //    self-blocked.
        let pinned_cleared = 0_usize;

        // 3b) Permanent chart anchors (QBTS) — re-assert after flush
        //     so a wiped/migrated universe never leaves the chart empty.
        let _ = self.ensure_permanent_anchors();

        // 4) Wipe blacklist — fresh-born means even yesterday's
        //    bad actors get a clean slate. The `recently_traded`
        //    list is what carries the "don't trade this again
        //    today" signal forward, with a tighter TTL than the
        //    old blacklist would have used.
        let blacklist_cleared = self.blacklist.len();
        self.blacklist.clear();

        EodFlushReport {
            auto_cleared,
            pinned_cleared,
            blacklist_cleared,
            recently_traded_size: self.recently_traded.len(),
        }
    }

    /// On first start after the S18 upgrade, copy `cfg.ui.favorites`
    /// into `pinned` so the existing user sees no behaviour change.
    /// Idempotent — only runs when both `pinned` and `auto_pool`
    /// are empty (i.e. fresh AutoUniverse).
    ///
    /// **Cap policy:** the `max_pinned_slots` cap is intentionally
    /// **not** enforced on migration. Existing users who already
    /// curated 10–20 favorites would otherwise lose half their
    /// watchlist on first launch with S18 — a regression. The cap
    /// is enforced only on new manual `pin()` calls afterwards. If
    /// the migrated count is over `max_total_slots`, the streamer
    /// will still subscribe (Tier-A limit is informational); the
    /// cap merely prevents *additional* pins until the operator
    /// trims manually.
    ///
    /// `_cfg` is reserved for future migration policies (e.g. an
    /// "import top-N by historical winrate" mode) and is currently
    /// unused — accepted as a parameter so the call-site signature
    /// is future-proof.
    pub fn migrate_from_favorites_if_empty(
        &mut self,
        favorites: &[String],
        _cfg: &AutoUniverseConfig,
    ) -> bool {
        if !self.pinned.is_empty() || !self.auto_pool.is_empty() {
            return false;
        }
        for sym in favorites.iter() {
            let key = sym.trim().to_uppercase();
            if !key.is_empty() && !self.is_pinned(&key) {
                self.pinned.push(key);
            }
        }
        true
    }

    /// Process scan results: promote top candidates into auto_pool,
    /// demote stale/decayed entries, refresh `last_seen_in_scan`.
    ///
    /// `protected_symbols` is the union of (a) symbols with open
    /// broker positions, (b) symbols whose FSM is `Active`/
    /// `Confirming`/`Exit`, plus the active UI symbol. These can
    /// never be auto-evicted.
    pub fn process_scan_results(
        &mut self,
        scan_results: &[ScanResult],
        protected_symbols: &HashSet<String>,
        cfg: &AutoUniverseConfig,
        now: DateTime<Utc>,
    ) -> PromotionReport {
        let mut report = PromotionReport::default();

        // 1) Lazy-cleanup expired blacklist + recently_traded entries.
        self.cleanup_expired_blacklist(now);
        self.cleanup_recently_traded(now, cfg.recently_traded_ttl_hours);

        // 1.5) S42 (operator directive 2026-05-29: "fresh today
        //      premarket, no tangency with the previous day").
        //
        //      The EOD flush (16:00 ET) is the PRIMARY mechanism that
        //      moves the prior session's `auto_pool` into
        //      `recently_traded`. But it only runs if the app is up at
        //      16:00 ET. If the operator closed the app before the
        //      close, a persisted `auto_pool` would otherwise survive
        //      into the new session and a name that moved yesterday AND
        //      again today could slip back in.
        //
        //      Catch-up guard: any non-protected auto entry whose
        //      last-seen ET date is BEFORE today's ET date is treated
        //      as prior-session carryover — removed from `auto_pool`
        //      and added to `recently_traded` (TTL block), exactly as
        //      the EOD flush would have. This runs BEFORE the refresh
        //      step (2) so a name merely re-appearing today cannot mask
        //      its prior-session origin. Protected (open-position)
        //      symbols are exempt so the data feed is never yanked.
        if cfg.recently_traded_ttl_hours > 0 {
            let today_et = now.with_timezone(&chrono_tz::US::Eastern).date_naive();
            let carryover: Vec<String> = self
                .auto_pool
                .iter()
                .filter(|(k, e)| {
                    !protected_symbols.contains(*k)
                        && e.last_seen_in_scan
                            .with_timezone(&chrono_tz::US::Eastern)
                            .date_naive()
                            < today_et
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in carryover {
                self.auto_pool.remove(&k);
                self.recently_traded.insert(k.clone(), now);
                report
                    .demoted
                    .push((k, "prior_session_carryover".to_string()));
            }
        }

        // 2) Refresh `last_seen_in_scan` and `last_score` for any
        //    auto entry that appears in the scan, and reset the
        //    low-score streak when the score recovers.
        for r in scan_results {
            let key = r.symbol.trim().to_uppercase();
            if let Some(e) = self.auto_pool.get_mut(&key) {
                e.last_seen_in_scan = now;
                e.last_score = r.score;
                if r.score >= cfg.score_decay_threshold {
                    e.low_score_streak = 0;
                } else {
                    e.low_score_streak = e.low_score_streak.saturating_add(1);
                }
            }
        }

        // 3) Demote auto-pool entries that decayed or went stale.
        //    Protected symbols are never auto-demoted.
        let demote_keys: Vec<(String, String)> = self
            .auto_pool
            .iter()
            .filter_map(|(k, e)| {
                if protected_symbols.contains(k) {
                    return None;
                }
                let stale_secs = (now - e.last_seen_in_scan).num_seconds();
                let stale_threshold_secs = (cfg.stale_catalyst_minutes as i64) * 60;
                if stale_secs > stale_threshold_secs {
                    return Some((k.clone(), "stale".to_string()));
                }
                if e.low_score_streak >= cfg.score_decay_scans {
                    return Some((k.clone(), "score_decay".to_string()));
                }
                None
            })
            .collect();
        for (k, reason) in demote_keys {
            self.auto_pool.remove(&k);
            report.demoted.push((k, reason));
        }

        // 4) Promote new candidates (in score-DESC order — scanner
        //    guarantees this, but we sort defensively to be robust
        //    to caller order changes).
        // SC11: never promote rows from a prior ET session.
        let today_et_str = now
            .with_timezone(&chrono_tz::US::Eastern)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let mut sorted: Vec<&ScanResult> = scan_results
            .iter()
            // Empty session_date_et = legacy/test fixtures. Live scans
            // always stamp today's ET date (SC11).
            .filter(|r| r.session_date_et.is_empty() || r.session_date_et == today_et_str)
            .collect();
        sorted.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        for r in sorted {
            let key = r.symbol.trim().to_uppercase();
            if key.is_empty() {
                continue;
            }
            // Skip if pinned (already in universe at higher tier).
            if self.is_pinned(&key) {
                continue;
            }
            // Skip if already in auto-pool (refreshed above).
            if self.auto_pool.contains_key(&key) {
                continue;
            }
            // Skip if blacklisted.
            if self.blacklist.contains_key(&key) {
                report.skipped_blacklisted.push(key);
                continue;
            }
            // S19 "Fresh-born" exclusion: don't re-promote a name
            // we already traded this session boundary.
            if cfg.recently_traded_ttl_hours > 0
                && self.recently_traded.contains_key(&key)
            {
                report.skipped_recently_traded.push(key);
                continue;
            }
            // Quality gates.
            if r.score < cfg.min_promote_score || r.rvol < cfg.min_promote_rvol {
                report.skipped_low_score.push(key);
                continue;
            }
            // Halt safety.
            if r.catalyst_flags.halted {
                report.skipped_low_score.push(key);
                continue;
            }
            // Slot capacity?
            let auto_slots_max = cfg
                .max_total_slots
                .saturating_sub(self.pinned.len());
            if self.auto_pool.len() >= auto_slots_max {
                // Find weakest unprotected entry to evict.
                let weakest: Option<(String, f64)> = self
                    .auto_pool
                    .iter()
                    .filter(|(k, _)| !protected_symbols.contains(*k))
                    .map(|(k, e)| (k.clone(), e.last_score))
                    .min_by(|a, b| {
                        a.1.partial_cmp(&b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                match weakest {
                    Some((wk, wscore)) if wscore < r.score => {
                        self.auto_pool.remove(&wk);
                        report
                            .demoted
                            .push((wk, format!("evicted_for_{:.0}", r.score)));
                    }
                    _ => {
                        // No room and no evictable entry weaker than this candidate.
                        continue;
                    }
                }
            }
            self.auto_pool
                .insert(key.clone(), AutoEntry::new(key.clone(), r.score, now));
            report.promoted.push(key);
        }

        report
    }

    /// Hook called on every realised close. `pnl` is the net P/L
    /// in USD; `allocated_usd` is the dollars committed to the
    /// position at entry (used for `big_loss_pct_threshold`). Set
    /// `allocated_usd <= 0` to disable the big-loss fast path
    /// for this call.
    ///
    /// `current_loss_streak` is the running consecutive-loss count
    /// from `trading.symbol_loss_streak` (after `update_loss_streak`
    /// has been applied for this trade). The function consults this
    /// rather than maintaining its own counter to remain consistent
    /// with the existing S17.1 entry blacklist.
    ///
    /// Pinned symbols are tracked but never auto-blacklisted.
    pub fn record_trade_outcome(
        &mut self,
        symbol: &str,
        pnl: f64,
        allocated_usd: f64,
        current_loss_streak: u32,
        cfg: &AutoUniverseConfig,
        now: DateTime<Utc>,
    ) -> TradeOutcomeReport {
        let key = symbol.trim().to_uppercase();
        let mut report = TradeOutcomeReport {
            symbol: key.clone(),
            pnl,
            new_loss_strikes: current_loss_streak,
            now_blacklisted: false,
            blacklist_reason: None,
        };
        if key.is_empty() {
            return report;
        }

        // Update auto-pool stats (for UI display only).
        if let Some(e) = self.auto_pool.get_mut(&key) {
            e.realized_pnl += pnl;
            if pnl > 0.0 {
                e.wins += 1;
            } else {
                e.losses += 1;
            }
        }

        // Pinned symbols: track outcome, never auto-blacklist.
        if self.is_pinned(&key) {
            return report;
        }

        // Big-loss fast path.
        if cfg.big_loss_pct_threshold > 0.0 && allocated_usd > 0.0 && pnl < 0.0 {
            let loss_pct = (-pnl / allocated_usd) * 100.0;
            if loss_pct >= cfg.big_loss_pct_threshold {
                self.auto_pool.remove(&key);
                self.blacklist.insert(
                    key.clone(),
                    BlacklistEntry {
                        symbol: key.clone(),
                        banned_at: now,
                        ttl_hours: cfg.blacklist_ttl_hours,
                        reason: BanReason::BigLoss,
                        last_loss_amount: pnl,
                        loss_strikes_at_ban: current_loss_streak,
                    },
                );
                report.now_blacklisted = true;
                report.blacklist_reason = Some(BanReason::BigLoss);
                return report;
            }
        }

        // Strikes path.
        if cfg.loss_strikes_to_blacklist > 0
            && current_loss_streak >= cfg.loss_strikes_to_blacklist
        {
            self.auto_pool.remove(&key);
            self.blacklist.insert(
                key.clone(),
                BlacklistEntry {
                    symbol: key.clone(),
                    banned_at: now,
                    ttl_hours: cfg.blacklist_ttl_hours,
                    reason: BanReason::ConsecutiveLosses,
                    last_loss_amount: pnl,
                    loss_strikes_at_ban: current_loss_streak,
                },
            );
            report.now_blacklisted = true;
            report.blacklist_reason = Some(BanReason::ConsecutiveLosses);
        }

        report
    }
}
