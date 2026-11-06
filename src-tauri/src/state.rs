use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Datelike, TimeZone, Utc, Weekday};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use titan_types::Candle;
use tokio_util::sync::CancellationToken;

use crate::config::RuntimeConfig;
use crate::market::streamer::StreamCmd;
use crate::smarttrend::account::{SmartTrendAccount, VirtualWallet};
use crate::smarttrend::engine::PillarDiagnostics;

// ═══════════════════════════════════════════════════════════════════════
//  BarClosedMsg — shared between market streamer and sentinel loop
//  Defined here (neutral territory) to avoid R1 coupling.
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct BarClosedMsg {
    pub symbol: String,
    pub timeframe: String,
    pub bar_time: i64,
    pub close_price: f64,
}

// ═══════════════════════════════════════════════════════════════════════
//  TitanState — 7 Mutexuri
//
//  S05 LOCK ORDER — two independent tiers, never overlapped.
//
//  Tier A  (TitanState — always acquire ascending):
//    1. config
//    2. market        (MarketState — display/UI snapshot, NOT MarketDataState)
//    3. smarttrend
//    4. trading
//    5. risk
//    6. journal
//    7. system
//
//  Tier B  (MarketDataState — inner mutexes, always acquire ascending):
//    1. last_prices
//    2. cvd
//    3. candle_buffers
//    4. indicator_cache
//    5. halts             (S13: status-sub trading halts)
//    6. watchlist_metrics (S14: 30s pre-computed watchlist rows)
//    7. micro             (F1: MicroBook per-symbol NBBO + flow rings)
//    8. burst             (F1b: tape-wide BurstScanner)
//    (ofi_state is lock-free via ArcSwap; bars_dropped/stream_stale are atomic)
//
//  Boundary rule: drop ALL Tier-A locks before acquiring any Tier-B lock
//  (and vice-versa). The two tiers are never held simultaneously.
//
//  Non-mutex fields (shutdown, session_id, http_client) are lock-free.
//
//  stream_tx, bar_tx: utility Mutexes — only accessed in setup() and
//  set_active_symbol. Never held concurrently with any numbered mutex.
//  NOT part of ordering.
//
//  Panic policy (release profile: panic = "abort"):
//    lock().unwrap() is used throughout. With panic=abort there is no
//    unwinding, so map_err/? cannot save the process. Poison implies a
//    background thread panicked — unrecoverable regardless.
//    Graceful shutdown saves state BEFORE cancel token fires.
// ═══════════════════════════════════════════════════════════════════════

pub struct TitanState {
    // --- Mutex group (acquire in numbered order) ---
    pub config: Mutex<RuntimeConfig>,        // 1
    pub market: Mutex<MarketState>,          // 2
    pub smarttrend: Mutex<SmartTrendState>,  // 3
    pub trading: Mutex<TradingState>,        // 4
    pub risk: Mutex<RiskState>,             // 5
    pub journal: Mutex<JournalState>,        // 6
    pub system: Mutex<SystemState>,          // 7

    // --- Lock-free / utility (not part of ordered group) ---
    pub shutdown: CancellationToken,
    pub session_id: String,
    pub stream_tx: Mutex<Option<tokio::sync::mpsc::Sender<StreamCmd>>>,
    pub bar_tx: Mutex<Option<tokio::sync::mpsc::Sender<BarClosedMsg>>>,
    pub http_client: reqwest::Client,
    /// D-PLUS (Sesiunea 2) facade — initialised once at boot inside
    /// `main.rs`. `None` for legacy users who keep
    /// `evolution_protocol.enabled = false`.
    pub dplus: std::sync::OnceLock<crate::smarttrend::dplus::DPlusHandles>,
    /// FRONTIER (Sesiunea 4) — Contextual bandit for final-stage
    /// size selection. Initialised at boot from disk + config; the
    /// `Mutex` is fine here because `select()` and `update()` are
    /// O(1) and called at most once per entry / per trade close.
    pub bandit: Mutex<crate::ml::bandit::ThompsonBandit>,
    /// FRONTIER — in-flight bandit context, keyed by symbol. Insert
    /// at entry time, consume at trade close. Restart-safe: stale
    /// rows are simply dropped on next boot (bandit misses one
    /// update — far better than corrupting the posterior with a
    /// stale context). Tier-B mutex (no other locks held while
    /// touching it).
    pub bandit_pending: Mutex<HashMap<String, crate::ml::bandit::BanditPendingContext>>,
    /// v2.5.0 NUCLEAR (N1) — public float cache (FMP-sourced shares-float
    /// with 7-day TTL). Lazy-initialised at startup; `OnceLock` so all
    /// hot-path readers get a zero-allocation `&Option<Mutex<FloatCache>>`
    /// and the cache itself is loaded once. Strategies use this to detect
    /// low-float runners (< 10M shares + RVOL > 5× = squeeze setup).
    pub float_cache: std::sync::OnceLock<Mutex<crate::market::float_cache::FloatCache>>,
}

impl TitanState {
    pub fn new(config: RuntimeConfig) -> Self {
        let bandit_enabled = config.evolution_protocol.bandit_enabled;
        let bandit_threshold = config.evolution_protocol.bandit_activation_threshold;
        let bandit = crate::ml::bandit::ThompsonBandit::load(
            &crate::ml::bandit::default_state_path(),
            bandit_enabled,
            bandit_threshold,
        );
        Self {
            config: Mutex::new(config),
            market: Mutex::new(MarketState::default()),
            smarttrend: Mutex::new(SmartTrendState::default()),
            trading: Mutex::new(TradingState::default()),
            risk: Mutex::new(RiskState::default()),
            journal: Mutex::new(JournalState::default()),
            system: Mutex::new(SystemState::new()),
            shutdown: CancellationToken::new(),
            session_id: uuid::Uuid::new_v4().to_string(),
            stream_tx: Mutex::new(None),
            bar_tx: Mutex::new(None),
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                // gzip cuts large Alpaca payloads (assets ~10MB → ~1MB)
                // enough to survive slow links within the 30s budget.
                .gzip(true)
                .build()
                .expect("Failed to build HTTP client"),
            dplus: std::sync::OnceLock::new(),
            bandit: Mutex::new(bandit),
            bandit_pending: Mutex::new(HashMap::new()),
            float_cache: std::sync::OnceLock::new(),
        }
    }

    /// Returns the D-PLUS handles if the protocol was enabled at boot,
    /// otherwise `None`. Hot-path safe (no allocation, no mutex).
    pub fn dplus(&self) -> Option<&crate::smarttrend::dplus::DPlusHandles> {
        self.dplus.get()
    }
}

// ─── Sub-state 2: Market ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mover {
    pub symbol: String,
    pub price: f64,
    pub change_pct: f64,
    pub volume: f64,
    pub last_update_ts: i64,
    /// SC11 — ET calendar date (`YYYY-MM-DD`) when this mover was published.
    #[serde(default)]
    pub session_date_et: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MarketState {
    pub active_symbol: String,
    pub candles: HashMap<String, Vec<Candle>>,
    pub last_quote_ts: HashMap<String, i64>,
    pub connected: bool,
    #[serde(default)]
    pub movers: Vec<Mover>,
    #[serde(default, skip)]
    pub earnings_today: HashSet<String>,
    #[serde(default, skip)]
    pub scan_results: Vec<crate::market::scanner::ScanResult>,
    #[serde(default)]
    pub scan_status: crate::market::auto_scan::ScanStatus,
    /// **S18** — Self-Curating Watchlist (AutoUniverse).
    ///
    /// Replaces the legacy `cfg.ui.favorites`-only universe with a
    /// pinned/auto-pool/blacklist trio managed by
    /// `crate::market::auto_universe`. See that module's header for
    /// the design and lifecycle.
    ///
    /// `#[serde(default)]` keeps pre-S18 `state_snapshot.json` files
    /// loading cleanly. `cfg.auto_universe.enabled` gates whether
    /// any of this is consulted at runtime — defaults to `false`,
    /// preserving legacy "trade-from-favorites" behaviour.
    #[serde(default)]
    pub auto_universe: crate::market::auto_universe::AutoUniverse,
    #[serde(default)]
    pub regime: crate::smarttrend::strategies::MarketRegime,
    /// F1b — symbols auto-promoted by tape burst / halt SIP. Unioned into
    /// the streamer watched set. Cap ~30; cleared on daily reset. Skipped
    /// from snapshot persistence (`skip`) — session-scoped discovery only.
    #[serde(default, skip)]
    pub burst_promoted: HashSet<String>,
}

// ─── Sub-state 3: SmartTrend ────────────────────────────────────────

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsmPhase {
    #[default]
    Idle,
    /// Legacy-only phase kept for snapshot/backward compatibility.
    /// Engine runtime normalizes this to Idle and does not use it operationally.
    Stalking,
    Confirming,
    Active,
    Pullback,
    Exit,
}


#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SymbolFsm {
    pub phase: FsmPhase,
    pub score: f64,
    pub confirm_count: u32,
    pub entry_price: Option<f64>,
    pub entry_time: Option<i64>,
    #[serde(default)]
    pub prev_close: f64,
    #[serde(default)]
    pub cooldown_bars: u32,
    #[serde(default)]
    pub peak_price: f64,
    #[serde(default)]
    pub hawkes_intensity: f64,
    #[serde(default)]
    pub last_event_time_ns: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerKind {
    #[deprecated(note = "Legacy — not emitted by engine")]
    Stalking,
    Entry,
    Exit,
    #[deprecated(note = "Legacy — not emitted by engine")]
    Trail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Long,
    Short,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartTrendMarker {
    pub kind: MarkerKind,
    pub direction: Direction,
    pub bar_time: i64,
    pub price: f64,
    pub score: f64,
    pub timeframe: String,
    pub symbol: String,
    /// S35.1 (2026-05-28) — `true` when the marker was emitted by the
    /// FSM but vetoed by post-eval gates (quality/trend/history) on a
    /// short timeframe (1m/5m). The marker is still persisted and
    /// rendered on the chart as a manual-trading reference, but the
    /// dispatcher MUST skip it when building `BotAction` so no real
    /// trade fires. Defaults to `false` for backwards compat with
    /// pre-S35 snapshots and legacy callers.
    #[serde(default)]
    pub display_only: bool,
    /// F3b — live-path provenance (`apex` / `frontier` / `fsm` / …).
    #[serde(default)]
    pub source: Option<String>,
    /// F3b — Frontier setup id when `source=frontier`.
    #[serde(default)]
    pub setup: Option<String>,
    /// F3b — PRIME / setup conviction in `[0,1]`.
    #[serde(default)]
    pub conviction: Option<f64>,
    /// F3b — `GO` | `VETO` | `HOLD` | `FSM` | `SHADOW`.
    #[serde(default)]
    pub verdict: Option<String>,
}

/// S35.4 (2026-05-28 15:25 ET) — FSM state keyed by `(symbol, timeframe)`
/// rather than `symbol` alone. Pre-fix: the FSM HashMap collapsed every
/// TF for a symbol into a single entry, so QBTS 1m / 5m / 10m / 15m / 30m
/// / 1h / 4h / 1D all clobbered each other's state at runtime. The
/// 1m bar at 13:45 ET would update the FSM to (say) Confirming, then
/// the 5m bar at 13:45 ET (firing within ~1 second) would read that as
/// "previous 5m state" and proceed from Confirming → Active even though
/// 5m had never seen a Confirming bar. This shuffled the FSM state
/// chaotically and explained why timeframes like 1h / 4h had NO recent
/// markers after April 2026: 1h was inheriting state mid-bar from 1m or
/// 5m and never settling into a real 1h transition.
///
/// All FSM read / write call sites must build their key via this helper.
pub fn fsm_key(symbol: &str, tf: &str) -> String {
    format!("{symbol}:{}", normalize_tf(tf))
}

/// Canonical short-form timeframe. All marker storage and queries go through this.
pub fn normalize_tf(tf: &str) -> &str {
    match tf {
        "1Min" => "1m",
        "5Min" => "5m",
        "10Min" => "10m",
        "15Min" => "15m",
        "30Min" => "30m",
        "1Hour" => "1h",
        "4Hour" => "4h",
        "1Day" | "1D" => "1d",
        other => other,
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SentinelStatus {
    pub symbols_tracked: u32,
    pub evals_total: u64,
    pub last_eval_ts: Option<i64>,
    pub running: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SmartTrendState {
    pub fsm: HashMap<String, SymbolFsm>,
    pub markers: Vec<SmartTrendMarker>,
    pub mode: TradingMode,
    pub sentinel_status: SentinelStatus,
    pub account: SmartTrendAccount,
    /// Cached diagnostics from the last sentinel evaluation (for ST Engine panel).
    #[serde(default)]
    pub last_diagnostics: Option<PillarDiagnostics>,
    #[serde(default)]
    pub user_wallet: VirtualWallet,
    #[serde(default)]
    pub bot_wallet: VirtualWallet,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradingMode {
    Manual,
    #[default]
    Auto,
}

// ─── Sub-state 4: Trading ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    #[serde(with = "crate::serde_decimal")]
    pub qty: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub avg_entry: Decimal,
    pub current_price: f64,
    pub unrealized_pnl: f64,
    pub side: Direction,
    pub opened_at: Option<i64>,
    /// "bot" | "user" | "" (legacy/recon)
    #[serde(default)]
    pub source: String,
    /// S03: monotonic wall-clock timestamp of when this position was first
    /// observed (by recon or fill). Always set at creation. Used as fallback
    /// anchor for max-hold when `opened_at` is None.
    #[serde(default)]
    pub first_seen: i64,
    /// S05: monotonic version counter. Incremented by trade_updates on every
    /// mutation of qty/avg_entry. position_recon uses this for CAS-style
    /// merge: if version changed during the recon HTTP round-trip, recon
    /// only refreshes current_price/unrealized_pnl (not qty/avg_entry).
    #[serde(default)]
    pub version: u64,
    /// S06: entry trade identifier (broker order_id at creation). Used by
    /// journal.record_trade_close for deterministic matching when closing.
    #[serde(default)]
    pub trade_id: String,
    /// S28 (2026-05-28): `true` when this entry was permitted only by the
    /// PDT swing-only override (`risk.allow_swing_when_pdt_blocked`).
    /// Downstream effects:
    ///   * bracket widened at entry time (TP × 1.20, SL × 0.85)
    ///   * `flat_by_close` skips this position (intended for overnight hold)
    ///
    /// Default `false` keeps the legacy behaviour intact for every other
    /// position.
    #[serde(default)]
    pub is_swing_intent: bool,
    /// S37 (2026-05-28 22:55 ET): the timeframe of the FSM that produced
    /// the Entry marker that opened this position. Used by
    /// `bot/exit.rs::handle_exit` to gate `SignalExit`: only an FSM
    /// emitting on this same TF is allowed to close the position via a
    /// signal-driven exit. Cross-TF FSM closes are now prevented (a 1m
    /// `SignalExit` will not close a 5m-opened position) while
    /// hard-coded exits (stop loss / take profit / max hold / flat by
    /// close) remain unaffected.
    ///
    /// Empty string means "unknown / legacy" — exits are NOT gated when
    /// the field is empty, preserving backward compatibility with
    /// positions reconstituted by `position_recon` from the broker
    /// where the entry FSM context was not captured.
    #[serde(default)]
    pub entry_timeframe: String,
    /// P0-B (audit 2026-05-29): bracket levels captured at entry so the
    /// close path can classify the real `exit_reason` (stop_loss vs
    /// take_profit vs time/signal) instead of the uniform "BrokerSync".
    /// `0.0` means "unknown" (legacy / recon-discovered position with no
    /// bracket trail) — classification falls back to a hold-time heuristic.
    #[serde(default)]
    pub stop_loss: f64,
    #[serde(default)]
    pub take_profit: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOrder {
    pub order_id: String,
    pub symbol: String,
    pub side: Direction,
    #[serde(with = "crate::serde_decimal")]
    pub qty: Decimal,
    pub submitted_at: i64,
    /// "bot" | "user"
    #[serde(default)]
    pub source: String,
    /// How much was withdrawn from the source wallet for this order.
    /// Used to refund on reject/cancel. 0.0 for close orders.
    #[serde(default, with = "crate::serde_decimal")]
    pub deducted_amount: Decimal,
    /// S03: deterministic CID for idempotent order submission and cancel-by-cid.
    #[serde(default)]
    pub client_order_id: String,
    /// S03: timestamp of last partial fill received for this order.
    /// Used by partial-fill timeout watchdog.
    #[serde(default)]
    pub last_fill_at: Option<i64>,
    /// S28: swing-intent flag propagated from entry-time PDT bypass to
    /// the resulting `Position` (when the fill arrives). `false` by
    /// default to keep legacy semantics.
    #[serde(default)]
    pub is_swing_intent: bool,
    /// S37: the timeframe of the FSM that authorised this order.
    /// Propagated to `Position.entry_timeframe` when the fill is observed
    /// by trade_updates. See doc-comment on `Position.entry_timeframe`.
    #[serde(default)]
    pub entry_timeframe: String,
    /// P0-B (audit 2026-05-29): bracket levels carried from the
    /// `OrderRequest` so they can be stamped onto the resulting
    /// `Position` at fill time. `0.0` = no bracket on this order.
    #[serde(default)]
    pub stop_loss: f64,
    #[serde(default)]
    pub take_profit: f64,
    /// PRIME F3 — fused conviction of the entry that authored this
    /// order. Carried from `OrderRequest` so the fill handler can stamp
    /// it into the journal entry (and from there the chart marker).
    /// `None` for legacy / manual / exit orders.
    #[serde(default)]
    pub conviction: Option<f64>,
    /// P5 — decision-time price for the slippage ledger. `0.0` =
    /// unknown (legacy rows / callers without a live quote).
    #[serde(default)]
    pub expected_price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderHistoryEntry {
    pub order_id: String,
    pub symbol: String,
    pub side: String,
    #[serde(with = "crate::serde_decimal")]
    pub qty: Decimal,
    pub order_type: String,
    #[serde(default, with = "crate::serde_decimal::option")]
    pub limit_price: Option<Decimal>,
    pub status: String,
    pub source: String,
    pub submitted_at: i64,
    #[serde(default, with = "crate::serde_decimal::option")]
    pub filled_price: Option<Decimal>,
    #[serde(default)]
    pub filled_at: Option<i64>,
    #[serde(default)]
    pub reject_reason: Option<String>,
    /// Strategy-side entry score at submission time. `Some` for
    /// Tournament strategy entries (whose `evaluate_entry` declares a
    /// score); `None` for legacy momentum / manual / exit orders.
    /// Recovered by `position_recon` so the journal row captures the
    /// real signal strength even when the WebSocket fill stream is
    /// silent on paper (the common path that historically lost it).
    #[serde(default)]
    pub entry_score: Option<f64>,
    /// Timeframe of the FSM/strategy that authored this order (e.g.
    /// "1m" / "5m" / "30m"). Mirrors `OrderRequest.entry_timeframe`;
    /// recovered by `position_recon` so the journal row records which
    /// TF the entry fired on even on the silent paper fill path. Empty
    /// for legacy / manual / exit orders.
    #[serde(default)]
    pub entry_timeframe: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TradingState {
    pub positions: Vec<Position>,
    pub pending_orders: Vec<PendingOrder>,
    pub daily_trades: u32,
    /// Cumulative fill tracking per order_id: (cum_qty, cum_avg_price).
    /// Used by trade_updates to compute incremental deltas and prevent double-counting.
    #[serde(skip)]
    pub fill_cumulative: HashMap<String, (Decimal, Decimal)>,
    #[serde(default)]
    pub order_history: Vec<OrderHistoryEntry>,
    /// **S15.14**: symbols whose AUTO entry has been *submitted to the broker
    /// but not yet acknowledged via `trade_updates`*. Acts as the duplicate-
    /// entry barrier. Populated under the same `trading` lock that runs the
    /// duplicate-symbol gate, so two concurrent sentinel signals for the
    /// same symbol within ~milliseconds cannot both pass.
    ///
    /// Value is the wall-clock UNIX timestamp (seconds) of insertion; the
    /// bot loop's housekeeping pass evicts entries older than
    /// `PENDING_ENTRY_TIMEOUT_SECS` (broker may have lost the request).
    #[serde(skip)]
    pub pending_entries: HashMap<String, i64>,
    /// **v2.5.0 NUCLEAR (HF3)** — direction lock for in-flight entries.
    ///
    /// `pending_entries` only tracks the *symbol* of an order in flight,
    /// so the Tournament dispatcher's C2 dedup cannot tell whether an
    /// opposite-direction signal would collide. Live evidence
    /// 2026-05-14: trade_updates websocket went silent for 17 h →
    /// `state.trading.positions` stayed empty → C2 dispatched LONG and
    /// SHORT on the same symbol within seconds → broker rejected with
    /// 403 "cannot open short while long open" while the wallet was
    /// already debited.
    ///
    /// Sibling of `pending_entries` (same lifecycle: insert on
    /// pending-claim, remove on fill / reject / cancel / timeout
    /// eviction). Keyed by uppercase symbol; value is the direction of
    /// the in-flight order so the dispatcher can drop opposite-side
    /// signals before they reach the broker.
    #[serde(skip)]
    pub pending_entry_dirs: HashMap<String, Direction>,
    /// **S16.6** — symmetric of `pending_entries` for the EXIT side.
    ///
    /// When the bot submits a closing order, it inserts the symbol here.
    /// On the next bot tick, if the FSM is still in `Exit` (because the
    /// market sell hasn't filled / removed the position from
    /// `trading.positions` yet), the duplicate-exit gate prevents a
    /// second SELL from being submitted. Without this gate, observed
    /// live: ~7 SELLs for QBTS within 5s, each one cancelling the
    /// previous via the `cancel_open_orders_for_symbol` cleanup.
    ///
    /// Cleared in `trade_updates` when the close branch runs and in
    /// `position_recon` when the broker confirms the symbol is gone, so
    /// either delivery path releases the gate.
    #[serde(skip)]
    pub pending_exits: HashMap<String, i64>,
    /// **S17.1** — per-symbol intraday performance tracker driving the
    /// loss-streak blacklist. Keyed by uppercase symbol; value is
    /// `(consecutive_losses, last_event_ts_unix)` where a *win* zeroes
    /// the counter and a *loss* increments it. The bot's entry path
    /// consults this map and refuses to enter a new position when the
    /// streak reaches `cfg.smarttrend.intraday_loss_blacklist_strikes`.
    ///
    /// Cleared every trading day at the daily-reset boundary (see
    /// `risk::daily_reset`), so a bad ticker on Monday gets a fresh
    /// chance on Tuesday.
    #[serde(default)]
    pub symbol_loss_streak: HashMap<String, (u32, i64)>,
    /// S44 — Trailing stop peak unrealized P&L percentage per symbol.
    #[serde(skip)]
    pub peak_pnl_pcts: HashMap<String, f64>,
}

/// S15.14: maximum age of a pending-entry record before the bot loop
/// considers it dead and evicts it (broker timeout / lost request).
pub const PENDING_ENTRY_TIMEOUT_SECS: i64 = 30;
/// S17.6: eviction window for `pending_exits`. Must be **strictly greater
/// than the bot tick cadence** (currently 30 s in
/// `loop_run::max_hold_interval`) or the gate will be evicted on the
/// same tick that re-evaluates the position, defeating the purpose of
/// the cooldown.
///
/// Live evidence (2026-05-06 03:02→03:52 UTC, symbol CRE): with the
/// previous 30 s value, an Alpaca-rejected exit (qty `held_for_orders`
/// by an earlier order) re-fired exactly every 30 s for 100
/// consecutive ticks — `retain(|_, ts| now - ts < 30)` evicts the
/// gate at t=30 s precisely when the next tick checks it. Bumping to
/// 60 s gives one full tick of headroom; if `trade_updates` /
/// `position_recon` truly fail to clear, the next retry fires at
/// t=60 s instead of t=30 s, halving the storm rate.
pub const PENDING_EXIT_TIMEOUT_SECS: i64 = 60;

// ─── Sub-state 5: Risk ──────────────────────────────────────────────

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BreakerLevel {
    #[default]
    Normal,
    Caution,
    Halt,
    Panic,
}

/// S40: serde default for `RiskState.shorting_enabled` — defaults to
/// `true` (allow shorts) so a missing field on an older snapshot does not
/// silently disable short trading on a margin account that supports it.
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingSettlement {
    pub amount: f64,
    pub settled_at: i64,
}

pub fn next_settlement_time(now_ts: i64) -> i64 {
    let dt = match chrono_tz::US::Eastern.timestamp_opt(now_ts, 0).single() {
        Some(t) => t,
        None => return now_ts + 86400, // fallback
    };
    let mut next_day = dt.date_naive() + chrono::Duration::days(1);
    while next_day.weekday() == Weekday::Sat || next_day.weekday() == Weekday::Sun {
        next_day += chrono::Duration::days(1);
    }
    let next_settlement = chrono_tz::US::Eastern
        .with_ymd_and_hms(next_day.year(), next_day.month(), next_day.day(), 9, 30, 0)
        .single();
    match next_settlement {
        Some(t) => t.timestamp(),
        None => now_ts + 86400,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskState {
    pub breaker: BreakerLevel,
    pub daily_pnl: f64,
    pub session_equity: f64,
    pub peak_equity: f64,
    pub current_drawdown_pct: f64,
    /// Last N equity readings for sparkline display (max 10, FIFO).
    #[serde(default)]
    pub equity_history: Vec<f64>,
    /// Initial equity at session/daily start (for daily PnL %).
    #[serde(default)]
    pub initial_equity: f64,
    #[serde(default)]
    pub cash: f64,
    #[serde(default)]
    pub buying_power: f64,
    /// S02: PDT day-trade count from Alpaca `/v2/account` (rolling 5-day window).
    #[serde(default)]
    pub daytrade_count: u32,
    /// S02: Alpaca PDT flag — when true, account is permanently flagged as
    /// pattern day trader and any new day trade is restricted under $25k.
    #[serde(default)]
    pub pattern_day_trader: bool,
    /// S40 (audit 2026-05-29): Alpaca account `multiplier` (1 = cash,
    /// 2/4 = margin). The PDT rule applies ONLY to margin accounts; on a
    /// cash account (`multiplier == 1`) the "3 day-trades / 5 days" limit
    /// does not exist — the real constraint is T+1 cash settlement.
    /// Default `0.0` = "not yet synced" → treated as margin (PDT applies)
    /// for safety, so we never disable PDT protection on a real margin
    /// account before a confirming `/v2/account` sync.
    #[serde(default)]
    pub account_multiplier: f64,
    /// S40: Alpaca `shorting_enabled`. Cash accounts cannot short, so any
    /// short order is rejected by the broker. When `false`, short signals
    /// are dropped before submission to avoid guaranteed rejects. Default
    /// `true` = allow until a sync proves otherwise (margin-safe).
    #[serde(default = "default_true")]
    pub shorting_enabled: bool,
    /// S02: Unix timestamp (seconds) of last successful equity_sync poll. Used
    /// by Gate 0 to reject entries when account snapshot is stale (>5 min).
    /// `0` means never synced.
    #[serde(default)]
    pub last_account_sync_at: i64,
    /// S19 — Alpaca's `last_equity` field (equity at the previous session
    /// close). The broker's Daily Change in the dashboard is computed as
    /// `equity - last_equity`. Mirrored here so the Journal tab matches
    /// the broker UI exactly, regardless of when the bot was restarted
    /// during the day. Set on every `equity_sync` tick. `0.0` until
    /// the first sync completes.
    #[serde(default)]
    pub alpaca_last_equity: f64,
    /// U2: T+1 cash settlement pending amounts.
    #[serde(default)]
    pub unsettled_funds: Vec<PendingSettlement>,
}

impl Default for RiskState {
    fn default() -> Self {
        Self {
            breaker: BreakerLevel::Normal,
            daily_pnl: 0.0,
            session_equity: 0.0,
            peak_equity: 0.0,
            current_drawdown_pct: 0.0,
            equity_history: Vec::new(),
            initial_equity: 0.0,
            cash: 0.0,
            buying_power: 0.0,
            daytrade_count: 0,
            pattern_day_trader: false,
            account_multiplier: 0.0,
            shorting_enabled: true,
            last_account_sync_at: 0,
            alpaca_last_equity: 0.0,
            unsettled_funds: Vec::new(),
        }
    }
}

impl RiskState {
    pub fn add_pending_settlement(&mut self, amount: f64, now_ts: i64) {
        if (self.account_multiplier - 1.0).abs() < 0.01 {
            let settled_at = next_settlement_time(now_ts);
            self.unsettled_funds.push(PendingSettlement {
                amount,
                settled_at,
            });
            tracing::info!(
                component = "risk",
                amount,
                settled_at,
                "Added T+1 pending settlement to lock-up queue"
            );
        }
    }

    pub fn get_unsettled_funds_total(&mut self, now_ts: i64) -> f64 {
        self.unsettled_funds.retain(|x| x.settled_at > now_ts);
        self.unsettled_funds.iter().map(|x| x.amount).sum()
    }
}

// ─── Sub-state 6: Journal ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub trade_id: String,
    pub symbol: String,
    pub side: Direction,
    pub entry_price: f64,
    pub exit_price: Option<f64>,
    pub qty: f64,
    pub pnl: Option<f64>,
    pub entry_time: i64,
    pub exit_time: Option<i64>,
    pub exit_reason: Option<String>,
    pub entry_score: f64,
    pub fsm_phase_at_entry: FsmPhase,
    /// v2.5.0 NUCLEAR (Faza 5) — strategy that originated this trade,
    /// parsed from the canonical `s2:<strategy>` order source. Empty
    /// for legacy FSM trades. Persisted in the journal so post-trade
    /// analytics can compute clean per-strategy P/L without scanning
    /// logs.
    #[serde(default)]
    pub strategy_id: Option<String>,
    /// Timeframe the entry fired on ("1m" / "5m" / "30m" / …). Recovered
    /// from the authoring order so post-trade analytics can break P/L
    /// down by TF. Empty for legacy rows written before this field and
    /// for broker-only positions with no authoring order.
    #[serde(default)]
    pub timeframe: String,
    /// PRIME F3 — fused conviction `[0, 1]` of the entry decision.
    /// `None` for legacy rows / manual trades / pre-PRIME entries.
    #[serde(default)]
    pub conviction: Option<f64>,
    /// P2 — market regime label at entry ("calm" / "trending" /
    /// "volatile" / "panic"). `None` for legacy rows. Feeds the
    /// context-conditional reliability calibration
    /// (`playbook|regime|session` keys).
    #[serde(default)]
    pub regime: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct JournalState {
    pub entries: Vec<JournalEntry>,
    pub dirty: bool,
}

// ─── Sub-state 7: System (Bonus 2 — HealthPulse) ───────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopHealth {
    pub running: bool,
    pub last_tick: DateTime<Utc>,
    pub tick_count: u64,
}

impl LoopHealth {
    pub fn new() -> Self {
        Self {
            running: false,
            last_tick: Utc::now(),
            tick_count: 0,
        }
    }

    #[allow(dead_code)] // Called by background loops (Sessions 3+)
    pub fn tick(&mut self) {
        self.running = true;
        self.last_tick = Utc::now();
        self.tick_count += 1;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemState {
    pub start_time: DateTime<Utc>,
    pub loops: HashMap<String, LoopHealth>,
    pub last_persist_ts: Option<DateTime<Utc>>,
}

impl SystemState {
    pub fn new() -> Self {
        Self {
            start_time: Utc::now(),
            loops: HashMap::new(),
            last_persist_ts: None,
        }
    }

    pub fn uptime_secs(&self) -> i64 {
        (Utc::now() - self.start_time).num_seconds()
    }

    pub fn register_loop(&mut self, name: &str) {
        self.loops
            .entry(name.to_string())
            .or_insert_with(LoopHealth::new);
    }
}

// ─── Snapshot for persistence ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub active_symbol: String,
    pub fsm_states: HashMap<String, SymbolFsm>,
    pub trading_mode: TradingMode,
    pub positions: Vec<Position>,
    pub risk: RiskState,
    pub journal_entries: Vec<JournalEntry>,
    pub session_id: String,
    pub saved_at: DateTime<Utc>,
    #[serde(default)]
    pub user_wallet: VirtualWallet,
    #[serde(default)]
    pub bot_wallet: VirtualWallet,
    #[serde(default)]
    pub order_history: Vec<OrderHistoryEntry>,
    /// **S18** — Self-Curating Watchlist persisted state. Survives
    /// restarts so the blacklist (per-symbol cool-down) and the
    /// auto-pool (promoted candidates with their score history) are
    /// not lost on every bot bounce. `#[serde(default)]` keeps
    /// pre-S18 snapshots loading cleanly.
    #[serde(default)]
    pub auto_universe: crate::market::auto_universe::AutoUniverse,
}

impl TitanState {
    /// Builds a snapshot from current state. Acquires locks in order.
    pub fn snapshot(&self) -> StateSnapshot {
        let market = self.market.lock().unwrap();
        let st = self.smarttrend.lock().unwrap();
        let trading = self.trading.lock().unwrap();
        let risk = self.risk.lock().unwrap().clone();
        let journal = self.journal.lock().unwrap();

        StateSnapshot {
            active_symbol: market.active_symbol.clone(),
            fsm_states: st.fsm.clone(),
            trading_mode: st.mode,
            positions: trading.positions.clone(),
            risk,
            journal_entries: journal.entries.clone(),
            session_id: self.session_id.clone(),
            saved_at: Utc::now(),
            user_wallet: st.user_wallet.clone(),
            bot_wallet: st.bot_wallet.clone(),
            order_history: trading.order_history.clone(),
            auto_universe: market.auto_universe.clone(),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn state_creation() {
        let state = TitanState::new(RuntimeConfig::default());
        assert!(!state.session_id.is_empty());
    }

    #[test]
    fn snapshot_captures_state() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut market = state.market.lock().unwrap();
            market.active_symbol = "NVDA".to_string();
        }
        let snap = state.snapshot();
        assert_eq!(snap.active_symbol, "NVDA");
    }

    #[test]
    fn system_state_uptime() {
        let sys = SystemState::new();
        assert!(sys.uptime_secs() >= 0);
    }

    #[test]
    fn loop_health_tick() {
        let mut health = LoopHealth::new();
        assert!(!health.running);
        health.tick();
        assert!(health.running);
        assert_eq!(health.tick_count, 1);
        health.tick();
        assert_eq!(health.tick_count, 2);
    }

    #[test]
    fn lock_ordering_no_deadlock() {
        let state = TitanState::new(RuntimeConfig::default());
        let _c = state.config.lock().unwrap();
        let _m = state.market.lock().unwrap();
        let _s = state.smarttrend.lock().unwrap();
        let _t = state.trading.lock().unwrap();
        let _r = state.risk.lock().unwrap();
        let _j = state.journal.lock().unwrap();
        let _y = state.system.lock().unwrap();
    }

    #[test]
    fn fsm_phase_default_is_idle() {
        assert_eq!(FsmPhase::default(), FsmPhase::Idle);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut st = state.smarttrend.lock().unwrap();
            st.user_wallet.topup(dec!(2500));
            st.bot_wallet.topup(dec!(1000));
        }
        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions.push(Position {
                symbol: "AAPL".into(),
                qty: dec!(10),
                avg_entry: dec!(180),
                current_price: 185.0,
                unrealized_pnl: 50.0,
                side: Direction::Long,
                opened_at: Some(1000),
                source: "bot".into(),
                first_seen: 1000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }
        let snap = state.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let loaded: StateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.session_id, snap.session_id);
        assert_eq!(loaded.user_wallet.balance, dec!(2500));
        assert_eq!(loaded.bot_wallet.balance, dec!(1000));
        assert_eq!(loaded.positions.len(), 1);
        assert_eq!(loaded.positions[0].source, "bot");
    }

    #[test]
    fn pending_order_deducted_amount_serde() {
        let po = PendingOrder {
            conviction: None,
            expected_price: 0.0,
            order_id: "abc".into(),
            symbol: "AAPL".into(),
            side: Direction::Long,
            qty: dec!(10),
            submitted_at: 1000,
            source: "bot".into(),
            deducted_amount: dec!(1800.50),
            client_order_id: String::new(),
            last_fill_at: None,
            is_swing_intent: false,
            entry_timeframe: String::new(),
            stop_loss: 0.0,
            take_profit: 0.0,
        };
        let json = serde_json::to_string(&po).unwrap();
        let loaded: PendingOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.deducted_amount, dec!(1800.50));

        // Backward compat: old JSON without deducted_amount defaults to 0
        let old_json = r#"{"order_id":"xyz","symbol":"NVDA","side":"Long","qty":5.0,"submitted_at":2000}"#;
        let old: PendingOrder = serde_json::from_str(old_json).unwrap();
        assert_eq!(old.deducted_amount, Decimal::ZERO);
        assert_eq!(old.source, "");
    }

    #[test]
    fn snapshot_backward_compat_no_source() {
        let json = r#"{
            "active_symbol": "AAPL",
            "fsm_states": {},
            "trading_mode": "Manual",
            "positions": [{"symbol":"NVDA","qty":5.0,"avg_entry":800.0,"current_price":810.0,"unrealized_pnl":50.0,"side":"Long","opened_at":null}],
            "risk": {"breaker":"Normal","daily_pnl":0,"session_equity":0,"peak_equity":0,"current_drawdown_pct":0},
            "journal_entries": [],
            "session_id": "test",
            "saved_at": "2025-01-01T00:00:00Z"
        }"#;
        let loaded: StateSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.positions[0].source, "");
        assert_eq!(loaded.user_wallet.balance, Decimal::ZERO);
        assert_eq!(loaded.bot_wallet.balance, Decimal::ZERO);
        assert!(loaded.order_history.is_empty());
    }

    #[test]
    fn snapshot_backward_compat_ignores_old_config_field() {
        let json = r#"{
            "config": {"smarttrend":{},"execution":{},"ui":{},"market":{}},
            "active_symbol": "AAPL",
            "fsm_states": {},
            "trading_mode": "Manual",
            "positions": [],
            "risk": {"breaker":"Normal","daily_pnl":0,"session_equity":0,"peak_equity":0,"current_drawdown_pct":0},
            "journal_entries": [],
            "session_id": "test",
            "saved_at": "2025-01-01T00:00:00Z"
        }"#;
        let loaded: StateSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.active_symbol, "AAPL");
    }

    #[test]
    fn order_history_entry_serde_roundtrip() {
        let entry = OrderHistoryEntry {
            order_id: "abc123".into(),
            symbol: "AAPL".into(),
            side: "buy".into(),
            qty: dec!(10),
            order_type: "limit".into(),
            limit_price: Some(dec!(185.50)),
            status: "filled".into(),
            source: "user".into(),
            submitted_at: 1700000000,
            filled_price: Some(dec!(185.25)),
            filled_at: Some(1700000060),
            reject_reason: None,
            entry_score: Some(72.5),
            entry_timeframe: "5m".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let loaded: OrderHistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.order_id, "abc123");
        assert_eq!(loaded.order_type, "limit");
        assert_eq!(loaded.limit_price.unwrap(), dec!(185.50));
        assert_eq!(loaded.filled_price.unwrap(), dec!(185.25));
        assert_eq!(loaded.filled_at, Some(1700000060));
        assert!(loaded.reject_reason.is_none());
        assert_eq!(loaded.entry_score, Some(72.5));
        assert_eq!(loaded.entry_timeframe, "5m");
    }

    #[test]
    fn order_history_entry_backward_compat() {
        let json = r#"{"order_id":"x","symbol":"NVDA","side":"sell","qty":5.0,"order_type":"market","limit_price":null,"status":"accepted","source":"bot","submitted_at":1000}"#;
        let loaded: OrderHistoryEntry = serde_json::from_str(json).unwrap();
        assert!(loaded.filled_price.is_none());
        assert!(loaded.filled_at.is_none());
        assert!(loaded.reject_reason.is_none());
        assert!(loaded.entry_score.is_none(),
                "legacy order_history rows must default entry_score to None");
        assert!(loaded.entry_timeframe.is_empty(),
                "legacy order_history rows must default entry_timeframe to empty");
    }

    #[test]
    fn trading_state_order_history_default() {
        let ts = TradingState::default();
        assert!(ts.order_history.is_empty());
    }

    #[test]
    fn s15_14_pending_entries_default_empty_and_skipped_in_serde() {
        let ts = TradingState::default();
        assert!(ts.pending_entries.is_empty(),
                "pending_entries must default to empty");

        // The field is `#[serde(skip)]` so it must NOT appear in
        // serialized state and must rehydrate as empty even if a
        // hostile snapshot tries to inject values.
        let json = serde_json::to_string(&ts).unwrap();
        assert!(!json.contains("pending_entries"),
                "pending_entries leaked into snapshot: {json}");
        let loaded: TradingState = serde_json::from_str(&json).unwrap();
        assert!(loaded.pending_entries.is_empty());
    }

    #[test]
    fn s15_14_pending_entries_eviction_threshold() {
        // The threshold gate the bot loop uses to evict stale claims;
        // pin the value so we don't accidentally regress to a number
        // that allows the symbol to stay locked for >>30 s after the
        // broker dropped the request.
        assert_eq!(PENDING_ENTRY_TIMEOUT_SECS, 30);
    }

    #[test]
    fn s16_6_pending_exits_default_empty_and_skipped_in_serde() {
        let ts = TradingState::default();
        assert!(ts.pending_exits.is_empty(),
                "pending_exits must default to empty");

        let json = serde_json::to_string(&ts).unwrap();
        assert!(!json.contains("pending_exits"),
                "pending_exits leaked into snapshot: {json}");
        let loaded: TradingState = serde_json::from_str(&json).unwrap();
        assert!(loaded.pending_exits.is_empty());
    }

    #[test]
    fn s17_6_pending_exits_eviction_threshold() {
        // S17.6 invariant: the cooldown MUST exceed the bot tick
        // cadence (`loop_run::max_hold_interval` = 30 s) or the
        // gate is evicted at the exact tick that re-evaluates the
        // position. See the live-evidence comment on
        // `PENDING_EXIT_TIMEOUT_SECS` for the CRE incident on
        // 2026-05-06.
        assert!(
            PENDING_EXIT_TIMEOUT_SECS > 30,
            "PENDING_EXIT_TIMEOUT_SECS ({PENDING_EXIT_TIMEOUT_SECS}) must exceed the 30 s tick cadence"
        );
        assert_eq!(PENDING_EXIT_TIMEOUT_SECS, 60);
    }

    #[test]
    fn position_default_version_zero() {
        let json = r#"{"symbol":"AAPL","qty":10.0,"avg_entry":180.0,"current_price":185.0,"unrealized_pnl":50.0,"side":"Long","opened_at":1000,"source":"bot","first_seen":1000}"#;
        let p: Position = serde_json::from_str(json).unwrap();
        assert_eq!(p.version, 0);
    }

    #[test]
    fn position_serde_backward_compat_no_version() {
        let json = r#"{"symbol":"NVDA","qty":5.0,"avg_entry":800.0,"current_price":810.0,"unrealized_pnl":50.0,"side":"Long"}"#;
        let p: Position = serde_json::from_str(json).unwrap();
        assert_eq!(p.version, 0);
        assert_eq!(p.first_seen, 0);
    }

    #[test]
    fn position_version_roundtrip() {
        let p = Position {
            symbol: "AAPL".into(),
            qty: dec!(10),
            avg_entry: dec!(180),
            current_price: 185.0,
            unrealized_pnl: 50.0,
            side: Direction::Long,
            opened_at: Some(1000),
            source: "bot".into(),
            first_seen: 1000,
            version: 42,
            trade_id: "test-order-1".into(),
            is_swing_intent: false,
            entry_timeframe: String::new(),
            stop_loss: 0.0,
            take_profit: 0.0,
        };
        let json = serde_json::to_string(&p).unwrap();
        let loaded: Position = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.version, 42);
        assert_eq!(loaded.trade_id, "test-order-1");
    }

    #[test]
    fn position_serde_default_trade_id() {
        let json = r#"{"symbol":"GOOG","qty":3.0,"avg_entry":100.0,"current_price":105.0,"unrealized_pnl":15.0,"side":"Long","opened_at":500,"source":"user","first_seen":500,"version":1}"#;
        let p: Position = serde_json::from_str(&json).unwrap();
        assert_eq!(p.trade_id, "", "Missing trade_id should default to empty string");
        assert_eq!(p.version, 1);
    }

    #[test]
    fn test_next_settlement_time() {
        use chrono::TimeZone;
        // Thursday 2026-07-09 12:00:00 ET
        let thur_et = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 7, 9, 12, 0, 0)
            .single()
            .unwrap();
        let thur_ts = thur_et.timestamp();
        let next_thur = next_settlement_time(thur_ts);
        let expected_fri = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 7, 10, 9, 30, 0)
            .single()
            .unwrap()
            .timestamp();
        assert_eq!(next_thur, expected_fri);

        // Friday 2026-07-10 15:30:00 ET
        let fri_et = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 7, 10, 15, 30, 0)
            .single()
            .unwrap();
        let fri_ts = fri_et.timestamp();
        let next_fri = next_settlement_time(fri_ts);
        let expected_mon = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 7, 13, 9, 30, 0)
            .single()
            .unwrap()
            .timestamp();
        assert_eq!(next_fri, expected_mon);

        // Saturday 2026-07-11 10:00:00 ET
        let sat_et = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 7, 11, 10, 0, 0)
            .single()
            .unwrap();
        let sat_ts = sat_et.timestamp();
        let next_sat = next_settlement_time(sat_ts);
        assert_eq!(next_sat, expected_mon);
    }
}
