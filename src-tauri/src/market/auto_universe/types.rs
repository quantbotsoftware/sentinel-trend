use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};


// ─── Configuration ──────────────────────────────────────────────────

/// Tunables for the AutoUniverse manager. Persisted in
/// `RuntimeConfig` (Tier-A); a clone is passed to the manager on
/// each call so the manager itself stays free of config-tier locks.
///
/// All thresholds are conservative defaults — `enabled = false`
/// preserves the legacy "trade-from-favorites" behaviour for users
/// who want to opt out.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoUniverseConfig {
    /// Master switch. When `false`, `active_set()` falls back to
    /// `cfg.ui.favorites` semantics (handled at the call site by
    /// the caller checking this flag).
    pub enabled: bool,
    /// Hard cap on `pinned.len() + auto_pool.len()`. Includes the
    /// active symbol if it's also pinned/auto. Alpaca SIP free tier
    /// has a 30-symbol streaming cap; default 20 keeps headroom for
    /// the active symbol + any in-flight position monitoring.
    pub max_total_slots: usize,
    /// Cap on `pinned.len()` independent of `max_total_slots`.
    /// Prevents user from pinning so much that auto can't operate.
    pub max_pinned_slots: usize,
    /// Minimum catalyst score required for auto-promotion from a
    /// scan result. Default `50.0` — half the maximum, enough to
    /// require *some* edge (news pill, big gap, RVOL spike, etc.).
    pub min_promote_score: f64,
    /// Minimum RVOL required for auto-promotion. Default `1.5×`.
    /// Below this the symbol is statistically dormant — promoting
    /// it just spends the slot on noise.
    pub min_promote_rvol: f64,
    /// Consecutive losing trades on the same symbol that trigger a
    /// blacklist entry. Default `2` — first loss can be variance,
    /// the second indicates the strategy mismatch is structural for
    /// that ticker on this day. `0` disables auto-blacklist.
    pub loss_strikes_to_blacklist: u32,
    /// Single-trade loss percentage of allocated capital that
    /// triggers an immediate blacklist (skips the strikes counter).
    /// Default `2.0` — a single 2% loss ($6 on a $300 position) is
    /// already dispositive of bad timing or wrong instrument.
    /// `0.0` disables this fast path.
    pub big_loss_pct_threshold: f64,
    /// How long a blacklist entry stays active. Default 4h — long
    /// enough to skip the rest of the morning chop window or post-
    /// catalyst burnout, short enough that an end-of-day comeback
    /// rally on the same name isn't permanently barred.
    /// `0` means "until daily reset" (effectively rest-of-session).
    pub blacklist_ttl_hours: u32,
    /// Auto-pool entries whose latest catalyst score has dropped
    /// below this for `score_decay_scans` consecutive scans are
    /// demoted (removed from auto-pool, not blacklisted). Default
    /// `30.0`.
    pub score_decay_threshold: f64,
    /// Number of consecutive scans an auto-pool entry must score
    /// below `score_decay_threshold` before demotion. Default `2`.
    pub score_decay_scans: u32,
    /// Maximum minutes an auto entry can hold a slot without
    /// reappearing in a scan (no fresh data, no movement, no news).
    /// Default `60`. Active positions are exempt.
    pub stale_catalyst_minutes: u32,
    /// When `true` (default), a winning trade resets the symbol's
    /// `loss_strikes` counter. When `false`, losses persist
    /// cumulatively until daily reset (more punitive).
    pub preserve_winners: bool,
    /// S19 "Fresh-born" — hours that a symbol stays in
    /// `recently_traded` after EOD flush. Scanner refuses to re-
    /// promote a name until this TTL elapses. Default 18h: covers
    /// 16:00 ET close → 10:00 ET next morning (RTH + premarket of
    /// next session). Set to `0` to disable the exclusion.
    pub recently_traded_ttl_hours: u32,
}

impl Default for AutoUniverseConfig {
    fn default() -> Self {
        Self {
            enabled: false, // S18 opt-in; existing users see no behaviour change
            max_total_slots: 20,
            max_pinned_slots: 8,
            min_promote_score: 50.0,
            min_promote_rvol: 1.5,
            loss_strikes_to_blacklist: 2,
            big_loss_pct_threshold: 2.0,
            blacklist_ttl_hours: 4,
            score_decay_threshold: 30.0,
            score_decay_scans: 2,
            stale_catalyst_minutes: 60,
            preserve_winners: true,
            recently_traded_ttl_hours: 18,
        }
    }
}

// ─── Data structures ────────────────────────────────────────────────

/// Per-symbol metadata for auto-promoted candidates. Tracks the
/// info needed to (a) report intelligently to the UI and (b) decide
/// when to demote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoEntry {
    pub symbol: String,
    pub added_at: DateTime<Utc>,
    /// Catalyst score at the moment of promotion. Used for slot-
    /// eviction comparison ("only displace someone weaker than the
    /// new candidate").
    pub score_at_add: f64,
    /// Most recently observed score (last scan). Compared against
    /// `score_decay_threshold` to detect quality-decay demotions.
    pub last_score: f64,
    /// Last time this symbol was seen in a scan result. Symbols
    /// missing from N consecutive scans become stale candidates.
    pub last_seen_in_scan: DateTime<Utc>,
    /// Consecutive scans where `last_score < score_decay_threshold`.
    /// Reset to 0 on a healthy scan.
    pub low_score_streak: u32,
    /// Cumulative realised P/L on this symbol since promotion (for
    /// UI display only — actual blacklist decisions go through
    /// `record_trade_outcome`).
    pub realized_pnl: f64,
    /// Win/loss totals since promotion (UI metric).
    pub wins: u32,
    pub losses: u32,
}

impl AutoEntry {
    pub(crate) fn new(symbol: String, score: f64, now: DateTime<Utc>) -> Self {
        Self {
            symbol,
            added_at: now,
            score_at_add: score,
            last_score: score,
            last_seen_in_scan: now,
            low_score_streak: 0,
            realized_pnl: 0.0,
            wins: 0,
            losses: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BanReason {
    /// Hit the consecutive-loss strike count.
    ConsecutiveLosses,
    /// One trade lost more than `big_loss_pct_threshold` of allocated capital.
    BigLoss,
    /// Catalyst score decayed below threshold for too many scans.
    ScoreDecay,
    /// Symbol stayed in auto-pool without reappearing in a scan for `stale_catalyst_minutes`.
    Stale,
    /// Operator explicitly requested a ban via UI.
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlacklistEntry {
    pub symbol: String,
    pub banned_at: DateTime<Utc>,
    /// Hours of cool-down. `0` means "until daily reset" — the
    /// risk loop's daily-reset boundary clears the entire blacklist
    /// regardless of TTL.
    pub ttl_hours: u32,
    pub reason: BanReason,
    /// Loss size that triggered the ban (informational; UI only).
    pub last_loss_amount: f64,
    /// Strike count at time of ban.
    pub loss_strikes_at_ban: u32,
}

impl BlacklistEntry {
    /// `true` if this entry is past its TTL (lazy expiration).
    /// `ttl_hours == 0` is treated as never-expires-by-time
    /// (only daily-reset clears it).
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        if self.ttl_hours == 0 {
            return false;
        }
        let ttl_secs = (self.ttl_hours as i64) * 3600;
        (now - self.banned_at).num_seconds() >= ttl_secs
    }

    /// Remaining seconds until TTL expiry (UI countdown).
    /// Returns `None` for `ttl_hours == 0` (until-reset).
    /// Tested via `blacklist_ttl_zero_means_until_reset`; reserved
    /// for the Tauri command layer / future server-side rendering
    /// when the JS countdown is replaced by a backend push.
    #[allow(dead_code)]
    pub fn remaining_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        if self.ttl_hours == 0 {
            return None;
        }
        let ttl_secs = (self.ttl_hours as i64) * 3600;
        Some((ttl_secs - (now - self.banned_at).num_seconds()).max(0))
    }
}

/// Outcome of `process_scan_results` for telemetry / UI toast.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PromotionReport {
    pub promoted: Vec<String>,
    pub demoted: Vec<(String, String)>, // (symbol, reason)
    pub skipped_blacklisted: Vec<String>,
    pub skipped_low_score: Vec<String>,
    /// S19 — symbols the scanner *would* have promoted but the
    /// "fresh-born" exclusion blocked because they were on
    /// yesterday's watchlist.
    #[serde(default)]
    pub skipped_recently_traded: Vec<String>,
}

/// Outcome of `eod_flush` for telemetry / UI toast.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EodFlushReport {
    pub auto_cleared: usize,
    pub pinned_cleared: usize,
    pub blacklist_cleared: usize,
    pub recently_traded_size: usize,
}

/// Outcome of `record_trade_outcome` for telemetry / UI toast.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TradeOutcomeReport {
    pub symbol: String,
    pub pnl: f64,
    pub new_loss_strikes: u32,
    pub now_blacklisted: bool,
    pub blacklist_reason: Option<BanReason>,
}
