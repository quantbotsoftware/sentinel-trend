//! D Evolution Protocol — strategy framework (Sesiunea 1, Phase 1.2).
//!
//! Eight modular strategies sit behind a single [`Strategy`] trait so the
//! bot loop, the genetic optimizer, and the tournament tracker can all
//! treat them interchangeably. The trait is intentionally minimal — five
//! methods, no shared state — to keep the plug-in surface small and the
//! tests deterministic.
//!
//! ## Wiring
//!
//! The bot's loop iterates over a slice of `Box<dyn Strategy>`. For each
//! symbol in the active universe it:
//!   1. Builds a [`MarketContext`] (price + candles + indicators + regime).
//!   2. Calls [`Strategy::allowed_window`] to short-circuit closed sessions.
//!   3. Calls [`Strategy::evaluate_entry`] when no position is open for the
//!      strategy on that symbol.
//!   4. For open positions, calls [`Strategy::evaluate_exit`] every tick.
//!   5. [`Strategy::position_size`] is consulted at entry time only;
//!      result is in shares (rounded down by the caller).
//!
//! ## Why one shared `MarketContext` instead of per-strategy plumbing?
//!
//! The cost of building the context once and passing a borrow to all eight
//! strategies is dominated by the candle clone (~200 floats); whereas the
//! cost of each strategy reaching back into `Arc<MarketDataState>` would
//! be eight lock acquisitions per symbol per tick. Centralising the lock
//! tour in the loop is cheaper and removes an entire class of race bugs
//! (a strategy seeing partially-updated state mid-tick).
//!
//! ## Failure semantics
//!
//! Strategies are *advisory*. They return `Option<…>` rather than
//! `Result<…>` because:
//!   - "no signal right now" is the steady state (90 %+ of ticks)
//!   - hard errors (panic) terminate the bot via the abort policy
//!   - soft errors (stale data, NaN inputs) become `None` + tracing line
//!
//! The bot loop wraps each call in a span so `tracing` shows which
//! strategy emitted which signal without each strategy having to log
//! its own identity.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use titan_types::Candle;

use crate::state::{Direction, Position};

pub mod apex;
pub mod apex_calibration;
mod apex_eval;
pub mod gap_fade;
pub mod etf_pair;
pub mod afternoon_fade;
pub mod stat_arb_50;
pub mod leveraged_etf;
pub mod catalyst_frontrun;
pub mod halt_resume;
pub mod extended_news;
pub mod opening_range_breakout;
pub mod frontier;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod edge_tests;

// ════════════════════════════════════════════════════════════════════
//  StrategyId — stable identifier (used by tournament + GA + journal)
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyId {
    /// APEX — the house strategy (momentum ignition continuation).
    /// When `evolution_protocol.apex_only` is set (default), this is
    /// the ONLY strategy allowed to dispatch live orders.
    Apex,
    GapFade,
    EtfPair,
    AfternoonFade,
    #[serde(alias = "stat_arb_50")]
    StatArb50,
    LeveragedEtf,
    #[serde(alias = "catalyst_frontrun")]
    CatalystFrontRun,
    HaltResume,
    ExtendedNews,
    OpeningRangeBreakout,
    /// F3 — Frontier Absolute (tape / true_resume / open_sweep).
    Frontier,
}

impl StrategyId {
    pub const ALL: &'static [StrategyId] = &[
        StrategyId::Apex,
        StrategyId::GapFade,
        StrategyId::EtfPair,
        StrategyId::AfternoonFade,
        StrategyId::StatArb50,
        StrategyId::LeveragedEtf,
        StrategyId::CatalystFrontRun,
        StrategyId::HaltResume,
        StrategyId::ExtendedNews,
        StrategyId::OpeningRangeBreakout,
        StrategyId::Frontier,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StrategyId::Apex => "apex",
            StrategyId::GapFade => "gap_fade",
            StrategyId::EtfPair => "etf_pair",
            StrategyId::AfternoonFade => "afternoon_fade",
            StrategyId::StatArb50 => "stat_arb_50",
            StrategyId::LeveragedEtf => "leveraged_etf",
            StrategyId::CatalystFrontRun => "catalyst_frontrun",
            StrategyId::HaltResume => "halt_resume",
            StrategyId::ExtendedNews => "extended_news",
            StrategyId::OpeningRangeBreakout => "opening_range_breakout",
            StrategyId::Frontier => "frontier",
        }
    }
}

impl std::fmt::Display for StrategyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ════════════════════════════════════════════════════════════════════
//  TradingWindow — RTH / extended / always-on
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradingWindow {
    /// 09:30 ET → 16:00 ET (regular trading hours).
    /// Default for momentum/equity strategies — small caps become
    /// illiquid wide-spread snipe-traps outside RTH.
    RthOnly,
    /// 04:00 ET → 20:00 ET (premarket + RTH + post-market).
    /// Used by news-driven plays where catalyst flow continues.
    ExtendedAndRth,
    /// 00:00 → 24:00. Useful for futures-correlated stat arb when
    /// crypto-style 24/7 trading is supported (NOT today; reserved).
    AlwaysOn,
}

impl TradingWindow {
    /// Returns `true` if the strategy is allowed to evaluate entries
    /// at the supplied ET time.
    pub fn permits(self, hour: u32, minute: u32) -> bool {
        let mins = hour * 60 + minute;
        match self {
            TradingWindow::RthOnly => (9 * 60 + 30..16 * 60).contains(&mins),
            TradingWindow::ExtendedAndRth => (4 * 60..20 * 60).contains(&mins),
            TradingWindow::AlwaysOn => true,
        }
    }
}

// ════════════════════════════════════════════════════════════════════
//  MarketContext — all the ambient state a strategy needs to decide
// ════════════════════════════════════════════════════════════════════

/// Snapshot of a single symbol's market state at evaluation time.
///
/// Built once per symbol per bot tick by the loop runner. Strategies
/// borrow it and never mutate it. Optional fields are `None` when the
/// upstream source hasn't populated them yet (e.g. catalyst score on a
/// fresh symbol pre-scan).
#[derive(Debug, Clone)]
pub struct MarketContext {
    pub symbol: String,
    pub timeframe: &'static str,           // "5m" / "1m"
    /// Most recent close price.
    pub price: f64,
    pub prev_close: f64,
    /// Open of the first RTH bar of the day (for gap calculations).
    pub session_open: f64,
    /// Last N candles (newest at the end). Length depends on caller —
    /// strategies treat <20 bars as "warmup" and refuse to signal.
    pub candles: Vec<Candle>,
    /// 14-period Average True Range.
    pub atr: f64,
    /// Relative volume vs the trailing 20-day average for the same
    /// time-of-day bucket.
    pub rvol: f64,
    /// % gap from the previous regular-session close to today's open.
    pub gap_pct: f64,
    /// Bid-ask spread in basis points (0.01% = 1 bp).
    pub spread_bps: f64,
    /// 0–100 catalyst score from `market::catalyst::CatalystFlags`.
    pub catalyst_score: Option<f64>,
    /// Current minute-of-day in ET (0..1440).
    pub now_et_min: u32,
    /// Current hour in ET (0..23).
    pub now_et_hour: u32,
    /// Current minute-within-hour in ET (0..59).
    pub now_et_minute: u32,
    /// Active market regime from `regime_detector`. None until the
    /// detector has computed at least one regime (first 5 min after start).
    pub regime: Option<MarketRegime>,
    /// Has Alpaca's status feed marked the symbol as halted?
    pub is_halted: bool,
    /// Optional companion symbols (for pair / stat-arb strategies).
    pub companion: Option<CompanionSnapshot>,
    /// Public float (shares outstanding × free-float pct). Optional —
    /// FMP cache may not have data for every symbol. Strategies that
    /// don't need it ignore the field.
    pub float_shares: Option<f64>,
    /// PRIME F0.6 — aggregate polarity of recent news headlines for the
    /// symbol, in [-1, +1]. Computed by `watchlist_refresh` (keyword
    /// scoring in `market::catalyst`) and carried through
    /// `MarketDataState.watchlist_metrics` so this module keeps zero
    /// deps on `market::` (R1). `None` = no fresh news / no metrics row.
    pub news_sentiment: Option<f64>,
    /// Global per-strategy parameter overrides.
    pub overrides: std::collections::HashMap<StrategyId, std::collections::HashMap<String, f64>>,
}

impl MarketContext {
    pub fn get_param(&self, strategy: StrategyId, name: &str, default: f64) -> f64 {
        self.overrides
            .get(&strategy)
            .and_then(|m| m.get(name).copied())
            .unwrap_or(default)
    }
}


/// Optional second leg for pair-trade strategies (ETF Pair, Stat Arb 50).
#[derive(Debug, Clone)]
pub struct CompanionSnapshot {
    pub symbol: String,
    pub price: f64,
    /// Trailing N-bar ratio mean (price_a / price_b).
    pub ratio_mean: f64,
    /// Trailing N-bar ratio standard deviation.
    pub ratio_sd: f64,
    /// Current Z-score of (price_a / price_b) vs the rolling window.
    pub ratio_z: f64,
}

// ════════════════════════════════════════════════════════════════════
//  Regime — coarse classification used by GA + per-strategy gates
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MarketRegime {
    #[default]
    Calm,
    Trending,
    Volatile,
    Panic,
}

// ════════════════════════════════════════════════════════════════════
//  Signals
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct EntrySignal {
    pub strategy: StrategyId,
    pub side: Direction,
    /// Target price for a limit order. `None` = market order.
    pub limit_price: Option<f64>,
    /// Hard stop-loss price.
    pub stop_loss: f64,
    /// Bracket take-profit price.
    pub take_profit: f64,
    /// Time-stop in minutes (force-exit if still open after this).
    pub max_hold_minutes: u32,
    /// Composite quality score 0–100. Used by the bot to break ties
    /// when multiple strategies signal on the same symbol.
    pub score: f64,
    /// Human-readable reason, persisted in the journal entry.
    pub reason: &'static str,
}

#[derive(Debug, Clone)]
pub struct ExitSignal {
    pub strategy: StrategyId,
    /// Target price for a limit close. `None` = market.
    pub limit_price: Option<f64>,
    pub reason: &'static str,
}

// ════════════════════════════════════════════════════════════════════
//  Strategy trait
// ════════════════════════════════════════════════════════════════════

pub trait Strategy: Send + Sync {
    fn id(&self) -> StrategyId;

    fn name(&self) -> &'static str {
        self.id().as_str()
    }

    fn allowed_window(&self) -> TradingWindow;

    /// Returns `Some(EntrySignal)` if the strategy wants to open a
    /// position for `ctx.symbol`. Strategies MUST refuse early on
    /// missing/insufficient data — no panics, no NaNs.
    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal>;

    /// Returns `Some(ExitSignal)` if the open `pos` should be closed.
    /// Strategies SHOULD honour the `pos.opened_at` timestamp to
    /// implement their max-hold time stop.
    fn evaluate_exit(&self, pos: &Position, ctx: &MarketContext) -> Option<ExitSignal>;

    /// Position size in dollars *to deploy on this single trade*. The
    /// caller divides by `ctx.price` to get share count and applies
    /// `floor()`. Always ≥ 0; return 0 to refuse the trade size-side
    /// (same effect as a `None` from `evaluate_entry`).
    fn position_size(&self, ctx: &MarketContext, sub_wallet: f64) -> f64;

    /// PRIME F0.4 — the widest bid-ask spread (bps) this strategy is
    /// willing to enter through. The bot-layer OFI spread gate uses
    /// this instead of a global hardcoded 15 bps, so wide-spread
    /// specialists (HaltResume 100, ExtendedNews 250) are no longer
    /// blocked by a limit tuned for liquid RTH momentum names.
    fn max_spread_bps(&self) -> f64 {
        15.0
    }
}

// ════════════════════════════════════════════════════════════════════
//  Registry — single global construction site
// ════════════════════════════════════════════════════════════════════

static REGISTRY: OnceLock<Vec<Box<dyn Strategy>>> = OnceLock::new();

/// Returns the canonical strategy registry. Construction is `OnceLock`,
/// so callers can borrow without re-instantiating per tick.
pub fn registry() -> &'static [Box<dyn Strategy>] {
    REGISTRY
        .get_or_init(|| {
            vec![
                Box::new(apex::Apex::default()) as Box<dyn Strategy>,
                Box::new(gap_fade::GapFade::default()),
                Box::new(etf_pair::EtfPair::default()),
                Box::new(afternoon_fade::AfternoonFade::default()),
                Box::new(stat_arb_50::StatArb50::default()),
                Box::new(leveraged_etf::LeveragedEtf::default()),
                Box::new(catalyst_frontrun::CatalystFrontRun::default()),
                Box::new(halt_resume::HaltResume::default()),
                Box::new(extended_news::ExtendedNews::default()),
                Box::new(opening_range_breakout::OpeningRangeBreakout::default()),
                Box::new(frontier::Frontier),
            ]
        })
        .as_slice()
}

/// Returns the strategy with `id` from the canonical registry, or
/// `None` if not present (impossible today; future-proof).
pub fn by_id(id: StrategyId) -> Option<&'static dyn Strategy> {
    registry().iter().find(|s| s.id() == id).map(|b| &**b)
}

/// PRIME F0.7 — the fixed symbol universe the ETF / pair / leveraged
/// strategies operate on, plus SPY for the regime detector. The
/// streamer unions this into its watched set so these strategies
/// actually receive bars. Empirical motivation: `etf_pair`,
/// `stat_arb_50` and `leveraged_etf` had ZERO trades in the entire
/// journal because the scanner universe (small-cap gappers) never
/// intersects these tickers — they were strategies without data.
pub const PINNED_UNIVERSE: &[&str] = &[
    // Regime probe + broad index pairs
    "SPY", "SH", "QQQ", "PSQ", "IWM", "RWM",
    // Leveraged pairs (EtfPair map + LeveragedEtf targets)
    "SPXL", "SPXS", "TQQQ", "SQQQ", "SOXL", "SOXS",
    "UPRO", "SPXU", "TNA", "TZA",
    "FAS", "FAZ", "TMF", "TMV", "NUGT", "DUST", "GUSH", "DRIP",
    // Sector ETFs (StatArb50 primaries + cointegration companions)
    "XLK", "XLF", "XLE", "XLY", "XLP", "XLV", "XLU", "XLI", "XLB",
];

// ════════════════════════════════════════════════════════════════════
//  Helpers shared across strategies (no state, pure functions)
// ════════════════════════════════════════════════════════════════════

/// Quality gate: rejects ctx with NaN/inf inputs or insufficient
/// history. Each strategy delegates here at the top of
/// `evaluate_entry`.
#[inline]
pub(crate) fn ctx_is_sane(ctx: &MarketContext, min_candles: usize) -> bool {
    if !ctx.price.is_finite()
        || ctx.price <= 0.0
        || !ctx.atr.is_finite()
        || ctx.atr < 0.0
    {
        return false;
    }
    ctx.candles.len() >= min_candles
}

/// Rolling-window standard deviation of the last `n` closes. Returns
/// 0.0 if `n < 2` or the candle slice is empty.
pub(crate) fn rolling_sd(candles: &[Candle], n: usize) -> f64 {
    if candles.len() < n.max(2) {
        return 0.0;
    }
    let slice = &candles[candles.len() - n..];
    let mean: f64 = slice.iter().map(|c| c.close).sum::<f64>() / slice.len() as f64;
    let var: f64 = slice
        .iter()
        .map(|c| (c.close - mean).powi(2))
        .sum::<f64>()
        / slice.len() as f64;
    var.sqrt()
}

/// Rolling-window mean of the last `n` closes.
pub(crate) fn rolling_mean(candles: &[Candle], n: usize) -> f64 {
    if candles.is_empty() {
        return 0.0;
    }
    let take = n.min(candles.len());
    let slice = &candles[candles.len() - take..];
    slice.iter().map(|c| c.close).sum::<f64>() / take as f64
}

/// 14-period True Range average — replicated here (vs the existing
/// `titan_indicators::atr`) because the strategies module deliberately
/// has zero deps on the live indicator cache. Pure data → pure number.
pub(crate) fn pure_atr(candles: &[Candle], period: usize) -> f64 {
    if candles.len() < 2 {
        return 0.0;
    }
    let mut trs: Vec<f64> = Vec::with_capacity(candles.len() - 1);
    for i in 1..candles.len() {
        let h = candles[i].high;
        let l = candles[i].low;
        let pc = candles[i - 1].close;
        let tr = (h - l).max((h - pc).abs()).max((l - pc).abs());
        trs.push(tr);
    }
    let n = period.min(trs.len()).max(1);
    let slice = &trs[trs.len() - n..];
    slice.iter().sum::<f64>() / n as f64
}

/// Computes simple position sizing: dollars-at-risk / stop-distance
/// gives shares, multiplied by current price gives dollars deployed.
/// Returns the dollar amount; caller computes shares.
pub(crate) fn risk_based_size(
    risk_dollars: f64,
    entry: f64,
    stop: f64,
    sub_wallet_cap_pct: f64,
    sub_wallet: f64,
) -> f64 {
    if !entry.is_finite() || entry <= 0.0 {
        return 0.0;
    }
    let distance = (entry - stop).abs().max(0.001);
    let shares = (risk_dollars / distance).max(1.0).floor();
    let dollars = shares * entry;
    let cap = sub_wallet * sub_wallet_cap_pct.clamp(0.0, 1.0);
    dollars.min(cap).max(0.0)
}
