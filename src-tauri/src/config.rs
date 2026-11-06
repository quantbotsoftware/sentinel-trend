use serde::{Deserialize, Serialize};

use crate::market::catalyst::CatalystWeights;

// ─── SmartTrend parameters ───────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartTrendConfig {
    pub exit_confirm_bars: u32,
    pub trend_min_slope_bps: f64,
    pub trend_min_efficiency: f64,
    pub pullback_max_atr: f64,
    pub pullback_reclaim_bars: u32,
    pub risk_per_trade: f64,
    pub max_drawdown_pct: f64,
    pub max_hold_minutes: u32,
    pub eval_outside_hours: bool,
    pub entry_min_momentum: f64,
    pub reentry_cooldown_bars: u32,
    /// S17.1 — last NY-time hour at which entries are still allowed.
    ///
    /// Live evidence (2026-05-05): per-hour profitability inverted at
    /// 11:00 ET — the 09:30–11:00 window produced +$36.53 net (32
    /// trades, 34% WR), but 11:00–13:00 produced -$75.10 across 30
    /// trades with 17% WR. Mid-day chop kills the momentum-pullback
    /// entry signal because there's no directional follow-through.
    ///
    /// Range: `[10, 16]`. Default `16` keeps the legacy "trade
    /// throughout RTH" behaviour so this is opt-in until validated
    /// across multiple sessions. Set to `11` (or whatever sample
    /// supports) to confine entries to the high-WR morning window;
    /// exits and safety logic continue normally until 16:00 ET.
    pub entry_window_end_et: u8,
    /// v2.4.3 — premarket / extended-hours entry switch for the
    /// legacy SentinelTrend momentum signal path (`strategy_id ==
    /// None`). The 8 Tournament strategies already declare their
    /// own `TradingWindow`; this flag opts the historical momentum
    /// path into the same `ExtendedAndRth` window (04:00–20:00 ET).
    ///
    /// Off by default — small-cap spreads explode 5–15× in premarket
    /// and stop-fills can land far from the bracket level. With
    /// virtual capital we accept that risk to harvest catalyst
    /// flow before the open (gappers, earnings overnight, halt
    /// resumes). The session-aware stale gate
    /// (`compute_is_stale` → 300 s outside RTH) and real-ATR SL
    /// already partially compensate.
    pub allow_premarket_entries: bool,
    /// v2.4.2 — opening-volatility skip: refuse entries during the
    /// first `entry_skip_minutes_after_open` minutes after 09:30 ET.
    ///
    /// Live evidence (2026-05-12 09:33–10:30 ET, ~22 entries triggered
    /// during this window, win rate ~25%, gross loss -$59 in 60 min):
    /// the opening 30 minutes are dominated by gap-fade, halt
    /// auctions, premarket-imbalance unwinds and stop hunts. The
    /// momentum-pullback strategy reads parabolic gap-ups as "trend
    /// continuation" and chases them at the exact moment institutional
    /// flow is starting to fade the move. Net effect: the bot pays
    /// the bid-ask, eats the reversal, hits the bracket SL inside
    /// 5 minutes.
    ///
    /// Range: `[0, 120]`. `0` (default) preserves historical
    /// behaviour. `30` (recommended) skips 09:30–10:00 ET. Exits and
    /// safety logic are unaffected — only the entry path consults
    /// this gate.
    pub entry_skip_minutes_after_open: u32,
    /// v2.4.2 — flat-by-close safety rail. When `> 0`, the bot
    /// closes every open `auto` position this many minutes before
    /// 16:00 ET regardless of FSM state.
    ///
    /// Live evidence (2026-05-11): five positions left open across
    /// the weekend gapped down at Monday's open: -$3.57 (AGL),
    /// -$3.69 (SOBR), -$6.27 (ARMG), -$1.24 (FNKO) — only APT
    /// (+$1.77) survived the overnight risk. The strategy is
    /// fundamentally intraday but had no "flat-at-close" guarantee.
    ///
    /// Range: `[0, 60]`. `0` (default) preserves historical behaviour.
    /// `5` (recommended) liquidates at 15:55 ET. Liquidation uses the
    /// same `handle_exit` path as other safety exits, so it respects
    /// `has_open_close_order` and `local_position_still_open` guards.
    pub flat_by_close_minutes: u32,
    /// S17.1 — per-symbol intraday loss-streak cutoff. After this many
    /// consecutive losing trades on the same symbol within the same
    /// trading day (no intervening win), block further entries on
    /// that symbol until midnight ET. `0` disables the gate.
    ///
    /// Today's data: ELPW (0/2), CONL (0/2), CLNN (0/2), INHD (0/1),
    /// SKK (0/1), MNDR (0/1) all hit 100% loss rate after 2+
    /// attempts. A `2`-strike rule would have prevented ~$30 of
    /// avoidable loss on these tickers alone.
    pub intraday_loss_blacklist_strikes: u32,
    /// S17.2 — Quality Gate: require a tradable catalyst before entry.
    ///
    /// When `true`, the bot consults `state.market.scan_results` for
    /// the symbol and refuses entry unless the cached `CatalystFlags`
    /// pass the bar:
    /// - not halted
    /// - score ≥ `catalyst_min_score`
    ///
    /// Live evidence (2026-05-05): 80% of broker losses came from
    /// no-catalyst names (HCAI -$83, DARE -$48, MASK -$42, etc.).
    /// Names with at least one catalyst pill (FATE +$17, PN +$16,
    /// CABA +$9, QBTS +$6, CCUP +$5) were collectively profitable.
    /// Default `false` — opt-in until validated.
    pub catalyst_required: bool,
    /// S17.2 — minimum catalyst score required when
    /// `catalyst_required = true`. Default `25.0` matches the weight
    /// of a single news pill (`w_news`), so any of:
    /// earnings (30) / news (25) / gap≥5% (20) / RVOL≥3× (25) alone
    /// is enough; combinations score higher and pass trivially.
    /// Range `[0, 100]`.
    pub catalyst_min_score: f64,
    /// S17.2 — Top-N filter: when `> 0`, only allow entries on
    /// symbols ranked in the top N of the latest scan
    /// (`scan_results` is already sorted by score DESC). `0`
    /// disables the gate.
    ///
    /// Ross Cameron rule: trade 1-3 names per day, not 20.
    /// Default `0` — opt-in.
    pub top_n_filter: u32,
    /// S17.4 — Quick-cut loser threshold (%). When `> 0`, the bot
    /// closes any open position with `unrealized_pnl_pct ≤ -X%`
    /// once it has been held for at least
    /// `quick_cut_min_hold_secs`. Catches slow-decay losers before
    /// the wider ATR-based bracket SL fires.
    ///
    /// Live evidence (2026-05-05): 35 losers averaged -$3.06,
    /// median ≈ -1.2% of trade size. The bracket SL was set at
    /// roughly -2.5–4% (ATR × `bracket_mult`), so half the losing
    /// names bled silently for 5–15 minutes before broker fired.
    /// A 0.5% quick-cut would have collapsed average loss closer to
    /// -$1.50 without changing entry logic.
    ///
    /// Default `0.0` (disabled).
    pub quick_cut_loss_pct: f64,
    /// S17.4 — Quick-cut minimum hold (seconds). Below this we
    /// trust the market to settle the entry — quotes can be very
    /// noisy in the first 30–60 s on small-caps. Default 60 s.
    pub quick_cut_min_hold_secs: u32,
    /// S17.4 — Profit-take threshold (%). When `> 0`, the bot
    /// closes any open position with `unrealized_pnl_pct ≥ +X%`
    /// once it has been held for at least
    /// `profit_take_min_hold_secs`. Locks in meaningful gains
    /// before the typical small-cap reversal.
    ///
    /// Default `0.0` (disabled). Recommended start: 1.5.
    pub profit_take_pct: f64,
    /// S17.4 — Profit-take minimum hold (seconds). Avoids the
    /// "0-second pump-and-dump exit" pathology where the first
    /// fill prints +2% above entry due to spread crossing.
    /// Default 90 s.
    pub profit_take_min_hold_secs: u32,
    /// S17.5 — Fresh-momentum gate: if `true`, refuse entries
    /// whose live price has fallen below the session VWAP for
    /// today's bars. VWAP is the institutional benchmark for
    /// "value" intraday — buying below VWAP on a long-only
    /// momentum strategy is statistically dilutive (entries
    /// fight against accumulated mean).
    ///
    /// Default `false` (opt-in, additive to Quality Gate).
    pub require_price_above_vwap: bool,
    /// **S18** — minimum strategy `combined_score` (the per-bar
    /// momentum/quality composite produced by the sentinel
    /// engine) required for an AUTO entry to fire. `0.0` disables
    /// the gate (legacy behaviour).
    ///
    /// Live evidence (2026-05-07): of the day's realised round
    /// trips, every winner had `score == 84.0` (4 / 4 winning
    /// trades), while losers clustered in the 50–65 score band.
    /// A `min_entry_score = 75.0` would have suppressed 14 of
    /// 17 losers and 0 of 4 winners — converting a -$102 P/L
    /// session into ~+$30 with the same execution stack.
    ///
    /// Range: `[0.0, 100.0]`. Recommended starting point: 70.0
    /// for cautious operators, 75.0 for stricter (matches the
    /// observed winner concentration).
    pub min_entry_score: f64,
    /// S44 — Trailing stop activation threshold (%). When `> 0`, once a position's
    /// unrealized_pnl_pct reaches this value, the trailing stop is activated.
    /// Default `0.0` (disabled).
    pub trailing_stop_activation_pct: f64,
    /// S44 — Trailing stop deviation/drop threshold (%). Once activated, if the
    /// unrealized_pnl_pct drops by this percentage from its peak, the position is closed.
    /// Default `0.0` (disabled).
    pub trailing_stop_deviation_pct: f64,
    /// S44 — Trailing stop minimum hold (seconds) before activation.
    /// Default 0 s.
    pub trailing_stop_min_hold_secs: u32,
    /// S45 — Enable regime-adaptive thresholds for FSM entries.
    pub regime_adaptive_entries: bool,
    /// S45 — Maximum bid-ask spread in basis points (bps) to allow real-money entry.
    /// Default 0.0 (disabled). Recommended starting point is 35.0.
    pub max_spread_bps: f64,
}

impl Default for SmartTrendConfig {
    fn default() -> Self {
        // S16.0 — HFT-scalping default profile.
        //
        // These are the *operator-safe* baseline values. They are
        // chosen to match the spec'd intra-day scalping use case
        // (sub-30-minute holds, conservative per-trade risk) and to
        // keep the circuit-breaker headroom realistic against the
        // PAPER account drawdown observed by the equity-sync loop.
        //
        // The UI exposes every field under `CONFIG → SMARTTREND`, so
        // power users can override at runtime without touching code;
        // the file at `~/.local/share/sentinel-trend/config/settings.json`
        // is rewritten on save.
        Self {
            exit_confirm_bars: 2,
            trend_min_slope_bps: 2.5,    // S16.0 relax (was 4.0): more entries
            trend_min_efficiency: 0.20,   // S16.0 relax (was 0.28)
            // S21 (v2.4.4) — relaxed from 1.4 → 2.0 ATR.
            //
            // Live evidence (2026-05-20): 133 of today's FSM diagnostics
            // returned `overextended_after_spike`, and all 4 sentinel-level
            // trend vetoes today carried that reason (AAPL × 3, QBTS × 1).
            // 1.4 ATR is below a textbook 0.5–0.618 Fibonacci retracement
            // on a clean momentum leg (~1.6–2.0 ATR on most small-caps),
            // so the bot was rejecting every valid pullback re-entry. 2.0
            // still rejects parabolic gap-up chases (AGL +114% pre-open
            // on 2026-05-08 produced pullback_depth_atr ≈ 40, far beyond
            // the new threshold), while admitting the Cameron-style
            // momentum-pullback setup that the strategy is meant to fire on.
            pullback_max_atr: 2.0,
            pullback_reclaim_bars: 2,
            risk_per_trade: 0.01,         // 1% — HFT-safe; max 1.5% recommended
            max_drawdown_pct: 3.0,        // S16.0 (was 5.0): tighter halt for scalping
            max_hold_minutes: 30,         // S16.0 (was 120): true scalping window
            eval_outside_hours: true,
            entry_min_momentum: 52.0,     // S16.0 relax (was 55.0)
            reentry_cooldown_bars: 3,
            entry_window_end_et: 11,      // S17.1: confine entries to the high-WR morning window (cutoff at 11:00 ET)
            allow_premarket_entries: false, // v2.4.3: opt-in (false = legacy RTH-only)
            entry_skip_minutes_after_open: 30, // v2.4.2: skip opening 30 min (09:30-10:00 ET) to avoid gap-fade/volatility trap
            flat_by_close_minutes: 5,     // S19: Liquidate intraday positions at 15:55 ET to avoid overnight gap down risk
            intraday_loss_blacklist_strikes: 2, // S17.1: set to 2 strikes (lock symbol after 2 consecutive losses)
            catalyst_required: true,      // S17.2: require a valid catalyst before entry
            catalyst_min_score: 25.0,     // S17.2: == single news pill
            top_n_filter: 0,              // S17.2: opt-in (0 = disabled)
            quick_cut_loss_pct: 1.0,      // S17.4: Ruthless Quick-Cut la 1.0%
            quick_cut_min_hold_secs: 45,  // S17.4: 45 secunde noise floor
            profit_take_pct: 0.0,         // S17.4: opt-in (0 = disabled)
            profit_take_min_hold_secs: 90, // S17.4: 1.5 min noise floor
            require_price_above_vwap: true, // S17.5: enforce price above VWAP for long entries
            min_entry_score: 70.0,         // S18: require minimum combined FSM score of 70
            trailing_stop_activation_pct: 1.5, // Trailing stop activation threshold (1.5%)
            trailing_stop_deviation_pct: 0.5,  // Deviation/drop threshold (0.5% from peak)
            trailing_stop_min_hold_secs: 0,
            regime_adaptive_entries: true, // Enable regime-adaptive thresholds for FSM entries
            max_spread_bps: 35.0,          // Filter wide bid-ask spreads (max 35 bps)
        }
    }
}

// ─── Execution ──────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionConfig {
    pub max_positions: u32,
    pub allow_short: bool,
    pub bracket_atr_mult: f64,
    pub order_timeout_secs: u64,
    pub max_usd_per_asset: f64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        // S19.1 — Fresh $300 paper account defaults (2026-05-18).
        //
        // Operator policy: this paper account mirrors the real-money
        // amount the bot will start with once the strategy proves
        // itself.
        //
        // Sizing shape:
        //   * `max_usd_per_asset=$1` is the bot-autonomous baseline —
        //     smallest possible slot so a brand-new session never
        //     deploys more than 40 × $1 = $40 against $300 equity
        //     (=13% utilization, $260 cash buffer). Operator dials
        //     the per-asset slider up to $10 via `CONFIG → EXECUTION`
        //     in the UI when conviction warrants larger slots.
        //   * `max_positions=40` matches AUTO universe capacity so
        //     the bot can ride the full scan funnel when premarket
        //     gappers cluster.
        //
        // When going live with a similarly small bankroll, raising
        // the per-asset slider lifts position size without any code
        // changes — the slot count and slot $$$ are decoupled.
        Self {
            max_positions: 40,
            allow_short: true,
            bracket_atr_mult: 2.0,
            order_timeout_secs: 30,
            max_usd_per_asset: 1000.0,    // Capital alocat / trade pentru contul de 100k
        }
    }
}

// ─── UI ─────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UIConfig {
    pub active_symbol: String,
    pub favorites: Vec<String>,
    pub default_timeframe: String,
    pub chart_show_volume: bool,
    pub desktop_notifications: bool,
    pub sound_on_fill: bool,
}

impl Default for UIConfig {
    fn default() -> Self {
        Self {
            // QBTS — permanent chart anchor (always something to plot).
            active_symbol: "QBTS".to_string(),
            favorites: vec!["QBTS".to_string()],
            default_timeframe: "5m".to_string(),
            chart_show_volume: true,
            desktop_notifications: true,
            sound_on_fill: true,
        }
    }
}

// ─── Market / Broker ────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MarketConfig {
    pub alpaca_key_id: String,
    pub alpaca_secret: String,
    pub paper_mode: bool,
    pub sip_feed: bool,
    pub discovery_poll_secs: u64,
    pub stale_data_secs: u64,
    pub fmp_api_key: String,
    pub scan_params: crate::market::scanner::ScanParams,
    pub auto_scan_active: bool,
    /// v2.4.4 — permanent symbol exclusion (case-insensitive). Symbols
    /// in this list are filtered out at four points:
    ///   1. Scanner output (never appear in scan_results)
    ///   2. AutoUniverse promote (never enter auto_pool)
    ///   3. Entry path (rejected before order submission)
    ///   4. Exit / flat-by-close / max-hold / adaptive (silently skipped)
    ///
    /// Use for tickers Alpaca leaves stranded as un-deletable phantom
    /// positions (delisted assets, halted-then-resumed-then-delisted
    /// rounds, paper-account zombies that the API refuses to clear).
    ///
    /// Live evidence (2026-05-12): `TRUE` left as ne-tradable position
    /// for ~3 months on the paper account, triggering `FLAT-BY-CLOSE
    /// skip — asset not tradable` every 30 s and polluting recon logs.
    #[serde(default)]
    pub excluded_symbols: Vec<String>,
}

impl Default for MarketConfig {
    fn default() -> Self {
        Self {
            alpaca_key_id: String::new(),
            alpaca_secret: String::new(),
            paper_mode: true,
            sip_feed: true,
            discovery_poll_secs: 60,
            stale_data_secs: 30,
            fmp_api_key: String::new(),
            scan_params: crate::market::scanner::ScanParams::default(),
            auto_scan_active: false,
            excluded_symbols: Vec::new(),
        }
    }
}

/// Helper: case-insensitive membership test against `MarketConfig::excluded_symbols`.
/// Returns `true` when `symbol` is permanently blocked from any trading activity.
pub fn is_symbol_excluded(excluded: &[String], symbol: &str) -> bool {
    if excluded.is_empty() || symbol.is_empty() {
        return false;
    }
    let s = symbol.trim();
    excluded
        .iter()
        .any(|e| e.trim().eq_ignore_ascii_case(s))
}

// ─── Risk / Circuit-breaker ──────────────────────────────────────────
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CautionAction {
    #[default]
    Block,
    Scale50,
    Scale25,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RiskConfig {
    pub caution_action: CautionAction,
    /// S28 (2026-05-28) — Swing-Only Override for PDT-blocked accounts.
    ///
    /// When the broker reports `daytrade_count >= 3` on a sub-$25K
    /// account, the PDT gate (`gates.rs:pdt_gate_check`) would normally
    /// reject *any* new entry to avoid tripping the 4th day-trade
    /// permanent restriction. This is correct intraday but throws away
    /// late-day entries that would naturally be held overnight
    /// (= swing, not a day trade in the eyes of FINRA / Alpaca).
    ///
    /// When this flag is `true` AND we are past `swing_entry_et_hour`
    /// (default 15:00 ET) AND still inside RTH, the PDT gate is
    /// **bypassed** because the entry can only become a day trade if
    /// the bracket TP/SL trips inside the remaining RTH window. The
    /// `is_swing_intent` marker propagates downstream so:
    ///   * Bracket TP/SL are widened (entry × 1.20 / 0.85) to make
    ///     intraday close mechanically improbable in ≤1h.
    ///   * `flat_by_close` skips these positions so they cross 16:00 ET.
    ///
    /// **Risk**: bracket *can* still trip on a sudden 20%+ swing in
    /// final hour → trade becomes day trade #4 → broker flags account
    /// as Pattern Day Trader (90-day restriction). Acceptable on paper.
    /// For real money, leave this `false`.
    #[serde(default)]
    pub allow_swing_when_pdt_blocked: bool,
    /// S28 — first ET hour at which swing-only entries are allowed when
    /// `allow_swing_when_pdt_blocked` is true. Default 15 = 15:00 ET
    /// (~ last full hour of RTH). Range [9, 15] inclusive (must leave
    /// at least 1h before close so the entry can naturally cross 16:00).
    #[serde(default = "default_swing_entry_et_hour")]
    pub swing_entry_et_hour: u32,
}

fn default_swing_entry_et_hour() -> u32 {
    15
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            caution_action: CautionAction::Block,
            allow_swing_when_pdt_blocked: false,
            swing_entry_et_hour: 15,
        }
    }
}

// ─── Catalyst (S13) ──────────────────────────────────────────────────
//
// Top-level subsystem peer with `market` / `scanner` / `risk`. Cross-
// cutting: scanner/run.rs reads it for weights, alpaca_news refresh
// loop reads news_enabled + lookback. Placed at RuntimeConfig root
// (NOT inside ScanParams or MarketConfig) per S13 plan rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalystConfig {
    pub weights: CatalystWeights,
    /// Master switch for news fetcher. When false, NewsCache stays
    /// empty and `news_recent` never sets — but halt detection and
    /// the rest of the catalyst stack remain operational.
    pub news_enabled: bool,
    /// Lookback window for news fetches in minutes. Min 15 enforced
    /// at consume site (refresh loop) to avoid pathological 0-min
    /// configs producing no results.
    pub news_lookback_minutes: u32,
}

impl Default for CatalystConfig {
    fn default() -> Self {
        Self {
            weights: CatalystWeights::default(),
            news_enabled: true,
            news_lookback_minutes: 240, // 4h
        }
    }
}

// ─── Root config ────────────────────────────────────────────────────
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub smarttrend: SmartTrendConfig,
    pub execution: ExecutionConfig,
    pub ui: UIConfig,
    pub market: MarketConfig,
    pub risk: RiskConfig,
    pub catalyst: CatalystConfig,
    /// **S18** — Self-Curating Watchlist tunables.
    ///
    /// Persisted alongside other runtime config in
    /// `~/.local/share/sentinel-trend/config/settings.json`. The
    /// `enabled` flag (default `false`) keeps legacy "trade-from-
    /// favorites" semantics unchanged for users who don't opt in.
    /// See `crate::market::auto_universe::AutoUniverseConfig` for
    /// the full field list and rationale per default.
    pub auto_universe: crate::market::auto_universe::AutoUniverseConfig,
    /// **S20 / D Evolution Protocol** — runtime tunables for the
    /// strategy framework, regime detector, GA optimizer, and
    /// tournament tracker. Disabled by default: the bot falls back
    /// to legacy SentinelTrend signal generation until `enabled` is
    /// flipped to `true` in `settings.json`.
    pub evolution_protocol: EvolutionProtocolConfig,
    /// **MERIDIAN** (plan §4.5) — A+B shadow driver / execution flags and
    /// the validated sleeve parameters. `enabled` runs the replay + twin
    /// log; `execute` (default `false`) is the phase-4b switch.
    pub meridian: crate::meridian::MeridianConfig,
}

// ─── Evolution Protocol (Sesiunea 1) ───────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionProtocolConfig {
    /// Master enable. When `false` the bot ignores the strategy
    /// framework and the regime/GA/tournament loops never start;
    /// the codebase still builds, all tests still pass, but the
    /// runtime behaviour is identical to pre-S20.
    pub enabled: bool,
    /// Total paper capital allocated across the 8 strategies on a
    /// fresh tournament (no `state.json`). Each strategy gets
    /// `paper_capital / 8`. Honoured only on first boot — subsequent
    /// boots load the persisted tournament file.
    pub paper_capital: f64,
    /// Regime detector polling cadence in seconds. SPY 5-min bar
    /// granularity makes 300s natural; values below 60s are
    /// rejected by `validate`.
    pub regime_poll_secs: u64,
    /// Cool-down between consecutive regime-change emissions (sec).
    /// Prevents UI flicker when the regime oscillates around a
    /// classification boundary.
    pub regime_change_cooldown_secs: i64,
    /// Force-exit timer for the Tournament `Draining` → `Dead`
    /// transition (seconds). Default 24h; set lower in dev.
    pub tournament_force_exit_secs: i64,
    /// GA evolution cadence — generations are evolved every N hours
    /// off the latest tournament fitness scores. 24h is the default
    /// (one generation per trading day).
    pub ga_cadence_hours: u64,
    /// GA population size per strategy (evenly applied across all 8).
    pub ga_population_size: usize,
    /// D-PLUS (Sesiunea 2) — Strategy Evolver daily loop enable.
    /// When `false` the evolver loop never fires (overrides remain
    /// loaded from disk if present, but no new generations run).
    pub evolver_enabled: bool,
    /// D-PLUS (Sesiunea 2) — Memory DB veto enable. When `false`,
    /// the bot ignores posterior_expectancy verdicts (DB still
    /// records trades; only the entry veto is disabled).
    pub memory_db_veto_enabled: bool,
    /// D-PLUS (Sesiunea 2) — Causal lead-lag bonus enable. When
    /// `false`, the bot ignores leader edges at signal time.
    pub causal_lead_lag_enabled: bool,
    /// D-PLUS (Sesiunea 2) — Adaptive Risk multiplier enable.
    /// When `false`, the bot uses raw position sizes regardless of
    /// streak history (Adaptive Risk still records outcomes).
    pub adaptive_risk_enabled: bool,
    /// OMEGA (Sesiunea 3) — master enable for the predictive ML
    /// stack. When `false` the predictor loop never starts and the
    /// bot's entry path treats the registry as empty (cold-start).
    #[serde(default = "default_true")]
    pub omega_enabled: bool,
    /// OMEGA — LSTM forecaster individual toggle. Read by the bot
    /// loop alongside `omega_enabled`.
    #[serde(default = "default_true")]
    pub lstm_forecaster_enabled: bool,
    /// OMEGA — regime transformer individual toggle.
    #[serde(default = "default_true")]
    pub transformer_regime_enabled: bool,
    /// OMEGA — automatic retraining toggle. When `false` the loop
    /// runs predictions only; nightly retrain is suppressed.
    #[serde(default = "default_true")]
    pub ml_train_enabled: bool,
    /// FRONTIER (Sesiunea 4) — Monte-Carlo P&L entry gate.
    ///
    /// When `true` (default), every bot entry is run through a
    /// 1024-path forward GBM simulation calibrated on the symbol's
    /// recent ATR. The gate vetoes trades with `E[r]` after costs
    /// below `mc_min_expected_pct`, vetoes trades with `P[loss] >
    /// mc_max_p_loss`, and shrinks size proportionally when
    /// `P[hit_target]` falls below `mc_shrink_p_hit_target`.
    /// Latency budget: ~200 µs per entry.
    #[serde(default = "default_true")]
    pub monte_carlo_gate_enabled: bool,
    /// MC veto threshold: minimum expected return (after costs) for
    /// an entry to clear. Default `0.0` — only vetoes trades with
    /// negative expected value.
    #[serde(default)]
    pub mc_min_expected_pct: f64,
    /// MC veto threshold: max acceptable `P[loss]`. Default `0.65`
    /// — refuse trades where 2/3 of paths end down.
    #[serde(default = "default_mc_max_p_loss")]
    pub mc_max_p_loss: f64,
    /// MC shrink threshold for `P[hit_target]`. Below this, the
    /// gate scales size by `P[target] / threshold` (clamped at
    /// `mc_shrink_floor`). Default `0.40`.
    #[serde(default = "default_mc_shrink_p_hit_target")]
    pub mc_shrink_p_hit_target: f64,
    /// Floor for the MC shrink factor (size never drops below
    /// `floor × original`). Default `0.50`.
    #[serde(default = "default_mc_shrink_floor")]
    pub mc_shrink_floor: f64,
    /// FRONTIER (Sesiunea 4) — Contextual bandit final-stage size
    /// selector. Beta-Bernoulli Thompson sampling on the (regime ×
    /// confidence-bucket) context. Cold-start safe: returns 1.0×
    /// until each bucket has accumulated `bandit_activation_threshold`
    /// closed trades.
    #[serde(default = "default_true")]
    pub bandit_enabled: bool,
    /// Trade observations per (regime × high_conf) bucket required
    /// before the bandit takes over from the neutral default.
    /// Default `20` ≈ five trades × four arms ≈ enough resolution
    /// for Thompson to separate winning from losing contexts.
    #[serde(default = "default_bandit_threshold")]
    pub bandit_activation_threshold: u32,
    /// APEX-only mode (2026-07-04 audit). When `true` (default) the
    /// live order flow is restricted to the APEX house strategy:
    ///   * the tournament dispatcher evaluates ONLY `StrategyId::Apex`
    ///     (the 9 legacy playbooks are bypassed — audit: 6/7 with live
    ///     samples were net-negative, with anti-predictive scores);
    ///   * FSM ENTRY markers become display-only chart references
    ///     (10% WR, −$3.96 live) — FSM exits for open positions still
    ///     dispatch so no position is ever stranded.
    ///
    /// Set `false` in settings.json to restore the full portfolio.
    #[serde(default = "default_true")]
    pub apex_only: bool,
    /// UPGRADE 2 (2026-07-12) — Time-of-Day Volatility Dampener.
    ///
    /// When `true` (default), the tournament dispatcher hard-blocks NEW
    /// entries during:
    ///   * Midday chop  (11:00 – 14:30 ET): low-volume period with
    ///     elevated breakout failure rate.
    ///   * Pre-close    (15:30 – 16:00 ET): closing auction pressure.
    ///
    /// Set `false` in settings.json to allow entries at any RTH time
    /// (useful for extended-hours strategies or backtesting).
    #[serde(default = "default_true")]
    pub enable_tod_filter: bool,
    /// F0 / FRONTIER Absolute — OFI adverse-flow gate on tournament path.
    ///
    /// Default `false`: the legacy global `ofi_state` is chart-symbol-only
    /// (cross-symbol contamination). Re-enable only after F1 MicroState
    /// provides per-symbol imbalance. `#[serde(default)]` → false for
    /// missing keys (fail-closed on contaminated data).
    #[serde(default)]
    pub ofi_gate_enabled: bool,
    /// F1 — per-symbol MicroBook (quotes/trades on all watched symbols).
    #[serde(default = "default_true")]
    pub micro_enabled: bool,
    /// F3/F4/F5 — Frontier strategy mode: `"off"` | `"shadow"` | `"live"`.
    /// Default `"off"` until F3 lands; F4 flips to shadow, F5 to live.
    #[serde(default = "default_frontier_mode")]
    pub frontier_mode: String,
    /// Master kill-switch for research loops (OMEGA predictor, causal,
    /// evolver, bandit sizing). When `false`, loops stay parked; tournament
    /// orders are unaffected (they already bypass OMEGA/bandit sizing).
    #[serde(default)]
    pub research_loops: bool,
    /// F4 — operator-applied Frontier threshold overrides, keyed by
    /// `FrontierThresholds` field name (e.g. `"tape_max_spread_bps"`).
    /// Written only by `apply_frontier_calibration` (propose-only
    /// engine; the operator clicks APPLY). Unknown keys are ignored;
    /// values are clamped to the per-field calibration interval at
    /// read time, so a hand-edited settings.json cannot push a
    /// threshold outside its safe band.
    #[serde(default)]
    pub frontier_overrides: std::collections::HashMap<String, f64>,
}

fn default_frontier_mode() -> String {
    "off".into()
}

fn default_mc_max_p_loss() -> f64 {
    0.65
}
fn default_mc_shrink_p_hit_target() -> f64 {
    0.40
}
fn default_mc_shrink_floor() -> f64 {
    0.50
}
fn default_bandit_threshold() -> u32 {
    20
}

fn default_true() -> bool {
    true
}

impl Default for EvolutionProtocolConfig {
    fn default() -> Self {
        Self {
            enabled: true,                // ACTIVARE TOURNAMENT (9 strategii live)
            paper_capital: 100_000.0,
            regime_poll_secs: 300,
            regime_change_cooldown_secs: 60,
            tournament_force_exit_secs: 24 * 3600,
            ga_cadence_hours: 24,
            ga_population_size: 16,
            evolver_enabled: true,
            memory_db_veto_enabled: true,
            causal_lead_lag_enabled: true,
            adaptive_risk_enabled: true,
            omega_enabled: true,
            lstm_forecaster_enabled: true,
            transformer_regime_enabled: true,
            ml_train_enabled: true,
            monte_carlo_gate_enabled: true,
            mc_min_expected_pct: 0.3,     // Monte-Carlo Greed Gate: Expected return > +0.3%
            mc_max_p_loss: 0.65,
            mc_shrink_p_hit_target: 0.40,
            mc_shrink_floor: 0.50,
            bandit_enabled: true,
            bandit_activation_threshold: 20,
            apex_only: true,              // APEX house strategy = singura sursă de ordine
            enable_tod_filter: true,       // Bloca intrări în midday chop (11:00-14:30 ET) și pre-close (15:30-16:00 ET)
            ofi_gate_enabled: false,      // F0: off until per-symbol MicroState (F1)
            micro_enabled: true,
            frontier_mode: "off".into(),
            research_loops: false,        // F0 cleanup: park research loops by default
            frontier_overrides: std::collections::HashMap::new(),
        }
    }
}

// ─── Validation ─────────────────────────────────────────────────────
impl RuntimeConfig {
    pub fn validate_hard_user_facing(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let st = &self.smarttrend;

        if !(0.001..=0.10).contains(&st.risk_per_trade) {
            errs.push(format!(
                "risk_per_trade must be in [0.001, 0.10] (got {})",
                st.risk_per_trade
            ));
        }
        if !(1.0..=50.0).contains(&st.max_drawdown_pct) {
            errs.push(format!(
                "max_drawdown_pct must be in [1.0, 50.0] (got {})",
                st.max_drawdown_pct
            ));
        }
        if !(5..=1440).contains(&st.max_hold_minutes) {
            errs.push(format!(
                "max_hold_minutes must be in [5, 1440] (got {})",
                st.max_hold_minutes
            ));
        }
        if st.exit_confirm_bars == 0 || st.exit_confirm_bars > 20 {
            errs.push(format!(
                "exit_confirm_bars must be in [1, 20] (got {})",
                st.exit_confirm_bars
            ));
        }
        if !(0.0..=100.0).contains(&st.trend_min_slope_bps) {
            errs.push(format!(
                "trend_min_slope_bps must be in [0.0, 100.0] (got {})",
                st.trend_min_slope_bps
            ));
        }
        if !(0.0..=1.0).contains(&st.trend_min_efficiency) {
            errs.push(format!(
                "trend_min_efficiency must be in [0.0, 1.0] (got {})",
                st.trend_min_efficiency
            ));
        }
        if !(0.1..=5.0).contains(&st.pullback_max_atr) {
            errs.push(format!(
                "pullback_max_atr must be in [0.1, 5.0] (got {})",
                st.pullback_max_atr
            ));
        }
        if st.pullback_reclaim_bars == 0 || st.pullback_reclaim_bars > 10 {
            errs.push(format!(
                "pullback_reclaim_bars must be in [1, 10] (got {})",
                st.pullback_reclaim_bars
            ));
        }
        if !(50.0..=90.0).contains(&st.entry_min_momentum) {
            errs.push(format!(
                "entry_min_momentum must be in [50.0, 90.0] (got {})",
                st.entry_min_momentum
            ));
        }
        if st.reentry_cooldown_bars > 20 {
            errs.push(format!(
                "reentry_cooldown_bars must be in [0, 20] (got {})",
                st.reentry_cooldown_bars
            ));
        }
        let ex = &self.execution;
        // S19.1 — extended max_positions ceiling to 50 to accommodate
        // the fresh $300 account topology: 40 diversified slots × $3
        // = $120 deployed (40% utilization) is the operator-set
        // baseline. The hard upper bound stays at 50 so the dashboard
        // stays scannable and the per-symbol allocation meaningful.
        if ex.max_positions == 0 || ex.max_positions > 50 {
            errs.push(format!(
                "max_positions must be in [1, 50] (got {})",
                ex.max_positions
            ));
        }
        if !(0.5..=5.0).contains(&ex.bracket_atr_mult) {
            errs.push(format!(
                "bracket_atr_mult must be in [0.5, 5.0] (got {})",
                ex.bracket_atr_mult
            ));
        }
        // S19.1 — Wide bounds [$0.5, $50k] so the same binary handles
        // the current $300 paper account ($1-$10 typical) and a future
        // live deployment without operator-side hardcoding. Validation
        // still rejects zero/negative and absurdly large sizes.
        if !(0.5..=50_000.0).contains(&ex.max_usd_per_asset) {
            errs.push(format!(
                "max_usd_per_asset must be in [0.5, 50000.0] (got {})",
                ex.max_usd_per_asset
            ));
        }

        if self.market.stale_data_secs == 0 {
            errs.push("stale_data_secs must be > 0".into());
        }

        if !(0.0..=500.0).contains(&st.max_spread_bps) {
            errs.push(format!(
                "max_spread_bps must be in [0.0, 500.0] (got {})",
                st.max_spread_bps
            ));
        }

        errs
    }

    pub fn validate(&self) -> Vec<String> {
        self.validate_hard_user_facing()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = RuntimeConfig::default();
        let errs = cfg.validate();
        assert!(errs.is_empty(), "Default config has errors: {errs:?}");
    }

    #[test]
    fn risk_bounds_violation() {
        let mut cfg = RuntimeConfig::default();
        cfg.smarttrend.risk_per_trade = 0.50;
        let errs = cfg.validate();
        assert!(errs.iter().any(|e| e.contains("risk_per_trade")));
    }

    #[test]
    fn trend_filter_bounds_violation() {
        let mut cfg = RuntimeConfig::default();
        cfg.smarttrend.exit_confirm_bars = 0;
        cfg.smarttrend.trend_min_efficiency = 1.5;
        cfg.smarttrend.pullback_max_atr = 10.0;
        let errs = cfg.validate();
        assert!(errs.iter().any(|e| e.contains("exit_confirm_bars")));
        assert!(errs.iter().any(|e| e.contains("trend_min_efficiency")));
        assert!(errs.iter().any(|e| e.contains("pullback_max_atr")));
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = RuntimeConfig::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let loaded: RuntimeConfig = serde_json::from_str(&json).unwrap();
        assert!((loaded.smarttrend.risk_per_trade - 0.01).abs() < f64::EPSILON);
        assert_eq!(loaded.ui.active_symbol, "QBTS");
    }

    #[test]
    fn unknown_dry_run_field_is_ignored_on_load() {
        // Pre-2.5 configs persisted `execution.dry_run`; we removed the
        // field along with the entire DRY mode in v2.5. The loader must
        // tolerate the unknown key gracefully so the user's settings
        // file from v2.4 still loads after upgrade.
        let json = r#"{"execution":{"max_positions":7,"dry_run":true}}"#;
        let cfg: RuntimeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.execution.max_positions, 7);
    }

    #[test]
    fn forward_compatible_load() {
        let json = r#"{"smarttrend":{"risk_per_trade":0.02}}"#;
        let cfg: RuntimeConfig = serde_json::from_str(json).unwrap();
        assert!((cfg.smarttrend.risk_per_trade - 0.02).abs() < f64::EPSILON);
        assert_eq!(cfg.smarttrend.exit_confirm_bars, 2);
        // S19.1 default updated for $300 paper account.
        assert_eq!(cfg.execution.max_positions, 40);
    }

    #[test]
    fn old_config_with_removed_fields_loads_ok() {
        let json = r#"{
            "smarttrend": {
                "macro_weight": 0.44,
                "micro_weight": 0.33,
                "momentum_weight": 0.23,
                "stalking_threshold": 57.0,
                "changepoint_threshold": 0.81,
                "entry_threshold": 62.0,
                "confirmation_bars": 3,
                "sideways_cooldown_bars": 3,
                "risk_per_trade": 0.02
            }
        }"#;
        let cfg: RuntimeConfig = serde_json::from_str(json).unwrap();
        assert!((cfg.smarttrend.risk_per_trade - 0.02).abs() < f64::EPSILON);
        assert_eq!(cfg.smarttrend.exit_confirm_bars, 2);
    }

    #[test]
    fn missing_keys_apply_defaults() {
        let json = r#"{
            "smarttrend": {
                "risk_per_trade": 0.03
            }
        }"#;
        let cfg: RuntimeConfig = serde_json::from_str(json).unwrap();

        assert!((cfg.smarttrend.risk_per_trade - 0.03).abs() < f64::EPSILON);
        assert_eq!(cfg.smarttrend.exit_confirm_bars, 2);
        // S16.0 default profile: trend_min_slope_bps relaxed to 2.5
        assert!((cfg.smarttrend.trend_min_slope_bps - 2.5).abs() < f64::EPSILON);
        // S19.1 default updated for $300 paper account.
        assert_eq!(cfg.execution.max_positions, 40);
    }

    #[test]
    fn max_usd_per_asset_bounds_violation() {
        let mut cfg = RuntimeConfig::default();
        // Below lower bound — rejected.
        cfg.execution.max_usd_per_asset = 0.4;
        let errs = cfg.validate();
        assert!(errs.iter().any(|e| e.contains("max_usd_per_asset")));

        // Above upper bound — rejected.
        cfg.execution.max_usd_per_asset = 60_000.0;
        let errs = cfg.validate();
        assert!(errs.iter().any(|e| e.contains("max_usd_per_asset")));

        // S19.1 — current paper-account operating range $1-$10 accepted.
        cfg.execution.max_usd_per_asset = 1.0;
        assert!(
            !cfg.validate()
                .iter()
                .any(|e| e.contains("max_usd_per_asset"))
        );
        cfg.execution.max_usd_per_asset = 10.0;
        assert!(
            !cfg.validate()
                .iter()
                .any(|e| e.contains("max_usd_per_asset"))
        );
        // Future live deployment range also accepted.
        cfg.execution.max_usd_per_asset = 500.0;
        assert!(
            !cfg.validate()
                .iter()
                .any(|e| e.contains("max_usd_per_asset"))
        );
    }

    #[test]
    fn risk_default_is_block() {
        let cfg = RiskConfig::default();
        assert_eq!(cfg.caution_action, CautionAction::Block);
        assert_eq!(CautionAction::default(), CautionAction::Block);
    }

    /// S13: legacy `settings.json` written before S13 (no `catalyst`
    /// block) MUST deserialise with the documented defaults. This
    /// mirrors how `risk` and `scan_params.rvol_method` were added in
    /// previous sessions.
    #[test]
    fn catalyst_config_default_when_missing_from_json() {
        let legacy_json = r#"{
            "smarttrend": { "risk_per_trade": 0.02 },
            "market": { "alpaca_key_id": "k" }
        }"#;
        let cfg: RuntimeConfig = serde_json::from_str(legacy_json).unwrap();
        assert!(cfg.catalyst.news_enabled, "default news_enabled must be true");
        assert_eq!(cfg.catalyst.news_lookback_minutes, 240);
        assert!((cfg.catalyst.weights.w_earn - 30.0).abs() < f64::EPSILON);
        assert!((cfg.catalyst.weights.w_news - 25.0).abs() < f64::EPSILON);
        assert!((cfg.catalyst.weights.gap_threshold - 5.0).abs() < f64::EPSILON);
        assert!((cfg.catalyst.weights.rvol_threshold - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn catalyst_config_partial_fields_apply_defaults() {
        let json = r#"{
            "catalyst": { "news_enabled": false }
        }"#;
        let cfg: RuntimeConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.catalyst.news_enabled);
        // Other fields default.
        assert_eq!(cfg.catalyst.news_lookback_minutes, 240);
        assert!((cfg.catalyst.weights.w_earn - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn catalyst_config_serde_roundtrip() {
        let mut cfg = RuntimeConfig::default();
        cfg.catalyst.news_lookback_minutes = 120;
        cfg.catalyst.weights.w_earn = 40.0;
        let json = serde_json::to_string(&cfg).unwrap();
        let loaded: RuntimeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.catalyst.news_lookback_minutes, 120);
        assert!((loaded.catalyst.weights.w_earn - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn save_import_roundtrip_preserves_fields() {
        let mut cfg = RuntimeConfig::default();
        cfg.smarttrend.exit_confirm_bars = 4;
        cfg.smarttrend.risk_per_trade = 0.02;

        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let loaded: RuntimeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.smarttrend.exit_confirm_bars, 4);
        assert!((loaded.smarttrend.risk_per_trade - 0.02).abs() < f64::EPSILON);
    }
}
