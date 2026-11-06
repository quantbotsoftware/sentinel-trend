use titan_indicators::{calculate_atr_sma_last, calculate_ema_last, IndicatorPeriods};

use crate::config::SmartTrendConfig;
use crate::market::market_state::MarketDataState;
use crate::state::{fsm_key, BarClosedMsg, FsmPhase, MarkerKind, SymbolFsm, TitanState};
use crate::smarttrend::strategies::MarketRegime;

use crate::smarttrend::engine::{evaluate, EvalResult, MomentumInput, SmartTrendInput, Timeframe};

use super::loop_run::LOOP_NAME;

fn assemble_momentum_input(market: &MarketDataState, symbol: &str, tf: &str) -> MomentumInput {
    let buffers = market.candle_buffers.lock().unwrap();
    let buf_key = format!("{symbol}:{tf}");
    let candles = buffers.get(&buf_key).map(|b| b.snapshot());
    drop(buffers);

    let (rvol, roc) = match candles {
        Some(ref c) if c.len() >= 5 => {
            let volumes: Vec<f64> = c.iter().map(|bar| bar.volume).collect();
            let vol_window = 20usize.min(volumes.len());
            let vol_sma: f64 = volumes.iter().rev().take(vol_window).sum::<f64>() / vol_window as f64;
            let last_vol = volumes.last().copied().unwrap_or(1.0);
            let computed_rvol = if vol_sma > 0.0 {
                last_vol / vol_sma
            } else {
                1.0
            };

            let closes: Vec<f64> = c.iter().map(|bar| bar.close).collect();
            let n = closes.len();
            let roc_periods: usize = match tf {
                "1m" => 10,
                "5m" => 5,
                _ => 3,
            };
            let lookback = roc_periods.min(n.saturating_sub(1));
            let base_idx = n.saturating_sub(lookback + 1);
            let computed_roc = if lookback >= 1 && closes[base_idx] > 0.0 {
                ((closes[n - 1] - closes[base_idx]) / closes[base_idx]) * 100.0
            } else {
                0.0
            };

            (computed_rvol, computed_roc)
        }
        _ => (1.0, 0.0),
    };

    MomentumInput { rvol, roc }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TrendMetrics {
    pub(super) is_bullish_trend: bool,
    pub(super) is_sideways: bool,
    pub(super) pullback_valid: bool,
    pub(super) slope_bps: f64,
    pub(super) efficiency_ratio: f64,
    pub(super) pullback_depth_atr: f64,
    /// v2.4.3 — `true` only when the candle buffer had ≥40 bars at
    /// the time `compute_trend_metrics` ran. False on default-fallback
    /// (`candles.len() < 40`, EMA collapsed, or no buffer at all).
    /// Live evidence (MGNX 2026-05-12 11:35 ET): 1m signal entered
    /// `phase=Active` while the 5m buffer was still warming up, so
    /// every TrendMetrics field was the silent Default. The 5m
    /// historical replay 80 s later showed slope_bps=-56.91 i.e. the
    /// symbol was bearish all along — the bot just didn't know yet.
    pub(super) has_sufficient_history: bool,
    pub(super) above_ema_fast: bool,
    pub(super) above_ema_slow: bool,
}

impl Default for TrendMetrics {
    fn default() -> Self {
        Self {
            is_bullish_trend: false,
            is_sideways: true,
            pullback_valid: false,
            slope_bps: 0.0,
            efficiency_ratio: 0.0,
            pullback_depth_atr: 0.0,
            has_sufficient_history: false,
            above_ema_fast: false,
            above_ema_slow: false,
        }
    }
}


fn compute_efficiency_ratio(closes: &[f64], lookback: usize) -> f64 {
    if closes.len() <= lookback || lookback == 0 {
        return 0.0;
    }
    let start = closes.len() - lookback - 1;
    let end = closes.len() - 1;
    let direction = (closes[end] - closes[start]).abs();
    let mut volatility = 0.0;
    for i in (start + 1)..=end {
        volatility += (closes[i] - closes[i - 1]).abs();
    }
    if volatility > 0.0 {
        (direction / volatility).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn compute_trend_metrics(
    market: &MarketDataState,
    symbol: &str,
    tf: &str,
    current_price: f64,
    cfg: &SmartTrendConfig,
) -> TrendMetrics {
    let candles = {
        let buffers = market.candle_buffers.lock().unwrap();
        let key = format!("{symbol}:{tf}");
        buffers.get(&key).map(|b| b.snapshot())
    };

    let candles = match candles {
        Some(c) if c.len() >= 40 => c,
        _ => return TrendMetrics::default(),
    };

    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let ema_fast = calculate_ema_last(&closes, 8);
    let ema_slow = calculate_ema_last(&closes, 21);
    if ema_slow <= 0.0 || !ema_slow.is_finite() {
        return TrendMetrics::default();
    }

    let ema_slow_prev = if closes.len() > 25 {
        calculate_ema_last(&closes[..closes.len() - 5], 21)
    } else {
        ema_slow
    };
    let slope_bps = if ema_slow_prev > 0.0 {
        ((ema_slow - ema_slow_prev) / ema_slow_prev) * 10_000.0
    } else {
        0.0
    };

    let efficiency_ratio = compute_efficiency_ratio(&closes, 20);
    let atr = calculate_atr_sma_last(&candles, 14).unwrap_or(0.0);

    let recent = &candles[candles.len() - 20..];
    let recent_high = recent
        .iter()
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let recent_low = recent.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);

    let ema_spread_bps = ((ema_fast - ema_slow).abs() / ema_slow) * 10_000.0;
    let slope_ok = slope_bps >= cfg.trend_min_slope_bps;
    let efficiency_ok = efficiency_ratio >= cfg.trend_min_efficiency;
    let is_bullish_trend =
        ema_fast > ema_slow && current_price >= ema_slow && slope_ok && efficiency_ok;

    let low_slope = slope_bps.abs() < cfg.trend_min_slope_bps;
    let low_efficiency = efficiency_ratio < cfg.trend_min_efficiency;
    // v2.4.4 (S21) — tight_range factor relaxed from 2.2 → 1.5 ATR.
    //
    // Live evidence (2026-05-20): 150 of today's FSM diagnostics
    // classified as `sideways_regime`. The compound condition
    // `compressed_emas && tight_range` was the dominant path. With
    // factor 2.2, a 20-bar range of only ~2 ATR (≈ 0.4% of price
    // on a typical small-cap) reads as sideways even when the EMAs
    // are merely consolidating before continuation. Empirically a
    // 1.5 ATR range over 20 bars is genuinely flat tape — the
    // distinction matters because the sideways diagnostic vetoes
    // entries on the >=5m timeframe (see `TREND_VETO_REASONS`).
    //
    // Other component (`low_slope && low_efficiency`) is unchanged,
    // so symbols with both flat slope AND no efficiency still get
    // tagged sideways regardless of range width.
    let tight_range = atr > 0.0 && ((recent_high - recent_low) / atr) < 1.5;
    let compressed_emas = ema_spread_bps < cfg.trend_min_slope_bps;
    let is_sideways = (low_slope && low_efficiency) || (compressed_emas && tight_range);

    let pullback_depth_atr = if atr > 0.0 {
        (recent_high - current_price) / atr
    } else {
        0.0
    };
    let reclaim_bars = cfg.pullback_reclaim_bars.max(1) as usize;
    let reclaim_start = closes.len().saturating_sub(reclaim_bars);
    let reclaim_ok = closes[reclaim_start..].iter().all(|c| *c >= ema_fast);
    let pulled_back = pullback_depth_atr >= 0.05 && pullback_depth_atr <= cfg.pullback_max_atr;
    let above_ema_fast = current_price >= ema_fast;
    let above_ema_slow = current_price >= ema_slow;
    let pullback_valid = is_bullish_trend && pulled_back && reclaim_ok && !is_sideways;

    TrendMetrics {
        is_bullish_trend,
        is_sideways,
        pullback_valid,
        slope_bps,
        efficiency_ratio,
        pullback_depth_atr,
        // reached the end of the function ⇒ candles.len() >= 40 and
        // ema_slow > 0 ⇒ every field above is real, not Default.
        has_sufficient_history: true,
        above_ema_fast,
        above_ema_slow,
    }
}

fn classify_entry_diagnostic_reason(
    current_phase: FsmPhase,
    trend_metrics: &TrendMetrics,
    cfg: &SmartTrendConfig,
    tf: &str,
) -> Option<&'static str> {
    // v2.4.2 — Idle-gating was disastrous in live trading. Engine
    // emits an `Entry` marker at the Confirming→Active transition,
    // *not* at Idle→Confirming. With the historical
    // `if current_phase != Idle return None` early-exit the
    // diagnostic was None at the exact instant the marker fired, so
    // the post-eval veto in `evaluate_symbol_data_only` never ran.
    //
    // Live evidence (2026-05-12 10:38 ET, AGL):
    //   t+0  ms  phase=Idle       pullback_atr=46.70  diag=overextended_after_spike  (no marker, no veto needed)
    //   t+0.1ms  phase=Confirming pullback_atr=46.01  diag=None ← Idle-gate trips here
    //   t+0.2ms  phase=Active     markers=1           diag=None ← marker emitted, veto missed
    //
    // AGL filled $62.19, bracket stop tagged $58.76 → −$6.86 on a
    // stock whose own diagnostic had been screaming overextended
    // for the previous 200 µs. Removing the gate makes the veto
    // honour the *original* intent: keep the bot out of parabolic
    // gap-ups regardless of how fast the FSM advanced through
    // Confirming.
    //
    // `current_phase` retained as an argument because future
    // phase-specific veto rules (e.g. tighten when Active to slow
    // re-entries) will need it; today it goes unused.
    let _ = current_phase;

    // S35 (2026-05-28) — Per-TF threshold relaxation for catalyst entries.
    //
    // **Operator intent**: manual trader (Tony) needs to see FSM
    // signals during catalyst-driven pumps (QBTS-style premarket
    // explosions). On 1m/5m these pumps create asymmetric metrics:
    //   • During the ramp: slope_bps strongly positive (bullish) ✓
    //   • After peak: slope flips slightly negative (–10 to –40 bps)
    //     → previously classified `bearish_trend` → FSM=Idle → no
    //     marker → manual trader sees nothing on the chart.
    //   • Pullback depth: a single 3-5 ATR spike on the open is the
    //     SIGNATURE of a real catalyst (news, M&A, FDA), but pre-S35
    //     `pullback_max_atr=2.0` blocked every such bar.
    //
    // Fix: on 1m/5m relax both gates so the FSM cycles through
    // Idle→Confirming→Active→Exit and emits BUY/SELL signals on
    // the chart. The relaxation is SIGNAL-ONLY for these TFs:
    //   1. `should_veto_quality` (in `evaluate_symbol_data_only`)
    //      still vetoes 1m `overextended_after_spike` for real entry
    //      submission via `TREND_VETO_REASONS` on 5m+.
    //   2. PDT gate + quality gate + score >= min_entry_score still
    //      protect real-money fires on these signals.
    //
    // Net effect: Tony sees more FSM BUY / FSM SELL on 1m/5m as
    // manual-trading reference; bot still safe-by-default for live
    // entries.
    //
    // S36 (2026-05-28 18:46 ET) — Extended the relaxations to 10m
    // and added a softer middle-tier for 15m/30m. With per-TF FSM
    // keying re-instated, each TF needs its own permissive ladder
    // so 5m/10m can transition independently. 15m/30m get a 1.5×
    // pullback bump (less aggressive than the small-TF 2.5×) so
    // they're not strict-strict but still demand more structure
    // than 1m/5m/10m.
    let is_short_tf = matches!(tf, "1m" | "5m" | "10m");
    let is_mid_tf = matches!(tf, "15m" | "30m");

    // On short TFs, bearish_trend is too noisy: a single red bar
    // flips slope negative even when the macro context is bullish.
    // Skip the bearish classification entirely on 1m/5m/10m so the
    // FSM can keep evaluating Confirming→Active.
    let bearish_trend = if is_short_tf {
        false
    } else {
        trend_metrics.slope_bps <= -cfg.trend_min_slope_bps
            && trend_metrics.efficiency_ratio >= cfg.trend_min_efficiency
    };

    // Per-TF pullback ceiling for "overextended after spike".
    //   1m/5m/10m → 2.5× (catalyst spikes regularly hit 3-5 ATR)
    //   15m/30m   → 1.5× (still permissive but demands structure)
    //   1h/4h/1D  → 1.0× (strict — no relaxation)
    let pullback_ceiling = if is_short_tf {
        cfg.pullback_max_atr * 2.5
    } else if is_mid_tf {
        cfg.pullback_max_atr * 1.5
    } else {
        cfg.pullback_max_atr
    };
    let overextended_after_spike = trend_metrics.pullback_depth_atr > pullback_ceiling;

    if bearish_trend {
        Some("bearish_trend")
    } else if trend_metrics.is_sideways {
        Some("sideways_regime")
    } else if overextended_after_spike {
        Some("overextended_after_spike")
    } else if !trend_metrics.is_bullish_trend {
        Some("trend_not_bullish")
    } else if !trend_metrics.pullback_valid {
        Some("pullback_not_valid")
    } else {
        None
    }
}

/// S37 (2026-05-28 22:55 ET) — STRICT classifier for real-money gating.
///
/// Companion to `classify_entry_diagnostic_reason` which carries per-TF
/// relaxations (S35/S36) that are correct for *display visibility* on the
/// chart but too permissive for actually firing real trades:
///   - `bearish_trend` is suppressed on 1m/5m/10m
///   - `pullback_ceiling` is multiplied by 2.5× on short TFs and 1.5× on mid TFs
///
/// Combined with `confirm_target_for_tf=1` on 5m/10m (engine.rs), this
/// produced a documented gap where a single bullish bar in a bearish trend
/// would transition the FSM to Active, emit an Entry marker that was NOT
/// `display_only`-flagged, and dispatch as a real `BotAction::Entry`.
/// Live screenshot evidence (2026-05-28 22:50 ET QBTS 1m): after-hours
/// $27→$29.50 pump shown on chart with zero new markers because the FSM
/// was stuck in Exit phase post-morning, AND the entry-eligibility gate
/// had silently widened — exactly the Thesis 2 failure mode.
///
/// This strict classifier ignores all per-TF relaxations: always evaluates
/// `bearish_trend` regardless of TF and uses the unscaled `pullback_max_atr`
/// ceiling. Whenever it returns `Some(_)` the corresponding Entry marker
/// is flagged `display_only=true` so it remains visible on the chart
/// (manual-trading reference preserved) but the dispatcher skips it for
/// `BotAction` (no real fire).
pub(super) fn classify_entry_diagnostic_reason_strict(
    trend_metrics: &TrendMetrics,
    cfg: &SmartTrendConfig,
) -> Option<&'static str> {
    let bearish_trend = trend_metrics.slope_bps <= -cfg.trend_min_slope_bps
        && trend_metrics.efficiency_ratio >= cfg.trend_min_efficiency;
    let overextended_after_spike = trend_metrics.pullback_depth_atr > cfg.pullback_max_atr;

    if bearish_trend {
        Some("bearish_trend")
    } else if trend_metrics.is_sideways {
        Some("sideways_regime")
    } else if overextended_after_spike {
        Some("overextended_after_spike")
    } else if !trend_metrics.is_bullish_trend {
        Some("trend_not_bullish")
    } else if !trend_metrics.pullback_valid {
        Some("pullback_not_valid")
    } else {
        None
    }
}

fn check_macro_consensus(market: &MarketDataState, symbol: &str, current_tf: &str) -> bool {
    let anchor_tf = match current_tf {
        "1m" | "5m" => "15m",
        "10m" | "15m" => "1h",
        _ => return true, // higher TFs don't have macro anchors
    };

    let key = format!("{symbol}:{anchor_tf}");
    let snap = {
        let buffers = market.candle_buffers.lock().unwrap();
        buffers.get(&key).map(|b| b.snapshot())
    };

    if let Some(candles) = snap {
        if candles.len() >= 40 {
            let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
            let ema_fast = calculate_ema_last(&closes, 8);
            let ema_slow = calculate_ema_last(&closes, 21);
            return ema_fast > ema_slow;
        }
    }
    true // safe fallback
}

pub(super) struct SymbolEvalInputs<'a> {
    pub(super) input: SmartTrendInput<'a>,
    pub(super) current_fsm: SymbolFsm,
    pub(super) prev_phase: FsmPhase,
    pub(super) trend_metrics: TrendMetrics,
    pub(super) entry_diagnostic_reason: Option<&'static str>,
    /// S37 — STRICT (no per-TF relaxation) version of `entry_diagnostic_reason`.
    /// Drives `display_only` flagging in `evaluate_symbol_data_only` so
    /// the entry-eligibility gate is tight while marker visibility stays
    /// relaxed per S35/S36.
    pub(super) entry_diagnostic_reason_strict: Option<&'static str>,
    pub(super) short_efficiency: f64,
    pub(super) regime: MarketRegime,
    pub(super) spread_bps: f64,
}

pub(super) fn build_symbol_eval_inputs<'a>(
    market: &MarketDataState,
    state: &TitanState,
    symbol: &'a str,
    msg: &'a BarClosedMsg,
    st_config: &SmartTrendConfig,
) -> SymbolEvalInputs<'a> {
    let regime = {
        let m = state.market.lock().unwrap();
        m.regime
    };
    let active_symbol = {
        let m = state.market.lock().unwrap();
        m.active_symbol.clone()
    };
    let spread_bps = if active_symbol == symbol {
        market.ofi_state.load().spread_bps
    } else {
        let metrics = market.watchlist_metrics.lock().unwrap();
        metrics.get(symbol).map(|m| m.spread_bps).unwrap_or(0.0)
    };

    refresh_indicator_cache(market, symbol, &msg.timeframe);

    let current_fsm = {
        let st = state.smarttrend.lock().unwrap();
        // S36 (2026-05-28 18:46 ET) — RE-INSTATED per-TF FSM keying.
        //
        // Live data showed S35.6 symbol-only keying was strictly worse
        // than S35.4 per-TF for Tony's manual-trading use case:
        //   1m  : 12 of 13 watchlist symbols had markers (only TF that
        //         was even partially functional under symbol-only)
        //   5m  : 1 of 13 had markers (AIRO catalyst pump only)
        //   10m : 0 of 13 had markers (charts entirely empty)
        //
        // Root cause: markers are stored WITH their TF of origin and
        // the chart filters by TF. Under symbol-only keying, whichever
        // TF's bar transitioned the FSM "first" emitted the marker;
        // subsequent bars on other TFs saw the already-Active phase,
        // had no transition, and emitted nothing. The S35.6 promise
        // of "1m transition leaks onto 5m chart" was not realised
        // because the marker is tagged 1m and never appears on a 5m
        // chart query.
        //
        // S36 fix: each `(symbol, tf)` carries its own FSM, evolves
        // independently on its own bar cadence, and emits markers on
        // its own transitions. Combined with `confirm_target_for_tf`
        // (engine.rs) and extended relaxations in
        // `classify_entry_diagnostic_reason` for 5m/10m, this restores
        // marker visibility across all small TFs without requiring
        // catalyst-level pumps to trigger.
        //
        // Real-money safety unchanged: dispatch.rs still filters
        // display_only markers from BotAction; bot.rs quality gate,
        // min_entry_score, scan inclusion, slot availability and PDT
        // gate are independent of the FSM hashmap key shape.
        st.fsm
            .get(&fsm_key(symbol, &msg.timeframe))
            .cloned()
            .unwrap_or_default()
    };
    let prev_phase = current_fsm.phase;

    let trend_metrics =
        compute_trend_metrics(market, symbol, &msg.timeframe, msg.close_price, st_config);
    let entry_diagnostic_reason = classify_entry_diagnostic_reason(
        current_fsm.phase,
        &trend_metrics,
        st_config,
        &msg.timeframe,
    );
    // S37 (2026-05-28 22:55 ET) — strict classifier computed in parallel.
    // Used by `evaluate_symbol_data_only` to flag `display_only=true` on
    // Entry markers whenever the strict gate would have vetoed, even if
    // the relaxed per-TF reason above is `None`. See doc-comment on
    // `classify_entry_diagnostic_reason_strict` for the full rationale.
    let entry_diagnostic_reason_strict =
        classify_entry_diagnostic_reason_strict(&trend_metrics, st_config);

    let momentum_input = assemble_momentum_input(market, symbol, &msg.timeframe);

    let (atr, short_efficiency) = {
        let buffers = market.candle_buffers.lock().unwrap();
        let key = format!("{symbol}:{}", msg.timeframe);
        let snap = buffers.get(&key).map(|b| b.snapshot());
        let atr_val = snap.as_ref().and_then(|c| calculate_atr_sma_last(c, 14)).unwrap_or(0.0);
        let er_val = snap.as_ref().map(|c| {
            let closes: Vec<f64> = c.iter().map(|bar| bar.close).collect();
            compute_efficiency_ratio(&closes, 10)
        }).unwrap_or(0.0);
        (atr_val, er_val)
    };

    let macro_trend_bullish = check_macro_consensus(market, symbol, &msg.timeframe);

    let news_sentiment = {
        let metrics = market.watchlist_metrics.lock().unwrap();
        metrics.get(symbol).and_then(|m| m.news_sentiment)
    };

    let tf_enum = Timeframe::from_str(&msg.timeframe).unwrap_or(Timeframe::M5);
    let input = SmartTrendInput {
        momentum_input,
        current_price: msg.close_price,
        prev_close: current_fsm.prev_close,
        bar_time: msg.bar_time,
        timeframe: tf_enum,
        symbol,
        atr,
        regime,
        macro_trend_bullish,
        above_ema_fast: trend_metrics.above_ema_fast,
        above_ema_slow: trend_metrics.above_ema_slow,
        news_sentiment,
    };

    SymbolEvalInputs {
        input,
        current_fsm,
        prev_phase,
        trend_metrics,
        entry_diagnostic_reason,
        entry_diagnostic_reason_strict,
        short_efficiency,
        regime,
        spread_bps,
    }
}

const TREND_VETO_REASONS: &[&str] = &[
    "bearish_trend",
    "sideways_regime",
    "overextended_after_spike",
];

fn should_veto_quality(_atr: f64, _price: f64, _short_efficiency: f64, max_spread_bps: f64, spread_bps: f64) -> bool {
    if max_spread_bps > 0.0 && spread_bps > max_spread_bps {
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
pub(super) fn evaluate_symbol_data_only<'a>(
    input: &SmartTrendInput<'a>,
    current_fsm: &SymbolFsm,
    st_config: &SmartTrendConfig,
    entry_diagnostic_reason: Option<&'static str>,
    entry_diagnostic_reason_strict: Option<&'static str>,
    short_efficiency: f64,
    has_sufficient_history: bool,
    regime: MarketRegime,
    spread_bps: f64,
) -> EvalResult {
    let mut adjusted_config = st_config.clone();
    if st_config.regime_adaptive_entries {
        match regime {
            MarketRegime::Trending => {
                adjusted_config.pullback_max_atr *= 1.2;
            }
            MarketRegime::Volatile | MarketRegime::Panic => {
                adjusted_config.entry_min_momentum *= 1.1;
                adjusted_config.pullback_max_atr *= 0.8;
            }
            MarketRegime::Calm => {
                adjusted_config.entry_min_momentum *= 1.1;
            }
        }
    }

    let mut result = evaluate(input, current_fsm, &adjusted_config);

    // v2.4.3 — refuse to emit Entry markers when the trend-metrics
    // computation fell back to `Default` (candle buffer < 40 bars,
    // EMA collapsed, or no buffer at all). Otherwise we trade on
    // hallucinated trend context.
    //
    // Live evidence (2026-05-12 11:35 ET, MGNX): 1m FSM transitioned
    // Idle→Confirming→Active on a momentum spike while 5m had < 40
    // bars warmed up. All trend metrics were `Default`
    // (is_sideways=true by definition), so neither the 1m nor 5m
    // diagnostic could veto the entry. 80 s later the historical
    // replay arrived showing slope_bps=-56.91 — bearish_trend. Bot
    // bought $148.625, sold for a loss; would have skipped entirely
    // if we'd known the symbol's 5m history said "no thanks".
    // S35.1 (2026-05-28) — On short TFs (1m / 5m / 10m / 15m), vetoed
    // Entry markers are NOT discarded — they're flagged
    // `display_only=true` and kept in the result so they appear on the
    // chart as manual-trading reference. The dispatcher skips
    // display_only markers when building `BotAction`, so no real trade
    // fires.
    //
    // S35.2 (2026-05-28 14:15 ET) — extended from {1m,5m} to
    // {1m,5m,10m,15m} after Tony confirmed via screenshots that 5m
    // captured the QBTS catalyst pump perfectly but 10m/15m showed
    // FSM SELL markers only — Entry markers were vetoed (and cleared)
    // on those TFs, leaving the chart asymmetric and unreadable.
    //
    // S35.3 (2026-05-28 14:50 ET) — extended to ALL timeframes. Tony's
    // round-3 screenshots showed 30m / 4h / 1D still had Entry markers
    // vetoed-and-cleared (lipsă FSM BUY), leaving an asymmetric chart
    // with only Exit markers visible. Real-money safety is unchanged
    // because:
    //   - `dispatch.rs` filters display_only markers from BotAction
    //   - `bot.rs` quality gate + min_entry_score + scan inclusion +
    //     slot availability + PDT gate are all independent of FSM
    //     marker visibility.
    // The historical "clear entirely" behaviour was conservative
    // overhead that hurt the manual-trading reference use-case
    // without adding any extra safety on top of the existing gates.
    let is_short_tf = true;
    let _ = input.timeframe.as_str(); // explicit no-op for the lint

    let veto_or_flag = |marker: &mut Option<crate::state::SmartTrendMarker>| -> bool {
        let mut had_entry = false;
        if let Some(ref mut m) = marker {
            if m.kind == MarkerKind::Entry {
                if is_short_tf {
                    if !m.display_only {
                        m.display_only = true;
                        had_entry = true;
                    }
                } else {
                    had_entry = true;
                    *marker = None;
                }
            }
        }
        had_entry
    };

    // PRIME F1 — persist each FSM entry veto to the decision journal.
    let log_fsm_veto = |stage: &str, reason: &str, score: f64| {
        crate::ops::decision_log::log(crate::ops::decision_log::DecisionRecord::vetoed(
            input.symbol,
            &input.timeframe.to_string(),
            "fsm",
            "long",
            score,
            stage,
            reason,
            input.bar_time,
            input.current_price,
            crate::smarttrend::memory_db::regime_label(Some(regime)),
        ));
    };

    if !has_sufficient_history {
        let vetoed = veto_or_flag(&mut result.marker);
        if vetoed {
            tracing::info!(
                component = LOOP_NAME,
                symbol = %input.symbol,
                tf = %input.timeframe,
                display_only = is_short_tf,
                "Entry vetoed — insufficient candle history (< 40 bars on this TF)"
            );
            log_fsm_veto("fsm_history", "insufficient_history", result.diagnostics.combined_score);
        }
    }

    if let Some(reason) = entry_diagnostic_reason {
        let tf = input.timeframe.as_str();
        // 1m timeframe deliberately skips most trend vetoes because
        // sideways / bearish_trend reasons are noisy on a 1m bar
        // (a single red candle would veto valid pullbacks). However
        // `overextended_after_spike` is **structural**, not noise:
        // the parabolic gap-up is detectable on any timeframe and
        // chasing it on 1m blows up in seconds.
        //
        // Live evidence (2026-05-08 09:32 ET): AGL gapped +114%
        // pre-open. 5m sentinel correctly flagged
        // `overextended_after_spike`, but the 1m FSM transitioned
        // anyway and the bot fired an entry at $60.07. Bracket SL
        // tagged at $58.67 in 4 seconds — net -$5.24 on a name
        // whose own diagnostic had warned us off.
        let should_veto = if tf == "1m" {
            reason == "overextended_after_spike"
        } else {
            TREND_VETO_REASONS.contains(&reason)
        };
        if should_veto {
            let vetoed = veto_or_flag(&mut result.marker);
            if vetoed {
                tracing::info!(
                    component = LOOP_NAME,
                    symbol = %input.symbol,
                    reason,
                    display_only = is_short_tf,
                    "Entry vetoed by trend diagnostic"
                );
                log_fsm_veto("fsm_trend", reason, result.diagnostics.combined_score);
            }
        }
    }

    if should_veto_quality(input.atr, input.current_price, short_efficiency, adjusted_config.max_spread_bps, spread_bps) {
        let had_entry = veto_or_flag(&mut result.marker);
        if had_entry {
            tracing::info!(
                component = LOOP_NAME,
                symbol = %input.symbol,
                atr = input.atr,
                price = input.current_price,
                efficiency = format!("{:.3}", short_efficiency),
                display_only = is_short_tf,
                "Entry vetoed by quality gate"
            );
            log_fsm_veto("fsm_quality", "atr_efficiency_spread", result.diagnostics.combined_score);
        }
    }

    // S37 (2026-05-28 22:55 ET) — STRICT entry-eligibility gate.
    //
    // The veto checks above use `entry_diagnostic_reason` which carries
    // per-TF relaxations (S35/S36) tuned for chart marker visibility on
    // catalyst-driven pumps. Those relaxations are correct for the
    // *display* axis but silently widen real-money entry eligibility on
    // 1m/5m/10m by suppressing the `bearish_trend` veto and inflating the
    // pullback ceiling — exactly the Thesis 2 failure mode documented in
    // the external architectural review.
    //
    // This block enforces the strict gate as a final pass: whenever the
    // STRICT classifier would have vetoed (any of the five reasons,
    // including `trend_not_bullish` and `pullback_not_valid` which the
    // legacy TREND_VETO_REASONS list omitted), flag any surviving Entry
    // markers as `display_only=true`. The marker stays on the chart for
    // Tony's manual-trading reference (cyan FSM SIM badge) but the
    // dispatcher (`dispatch.rs:81`) skips it for `BotAction::Entry` — no
    // real fire.
    //
    // Net effect: marker density on 1m/5m/10m is preserved (S35/S36 wins
    // retained), but the silent entry-widening is closed. Display and
    // entry-eligibility are now decoupled exactly as Opus 4.8 proposed
    // in §E#1 R2.
    if let Some(strict_reason) = entry_diagnostic_reason_strict {
        let mut flagged = 0u32;
        if let Some(ref mut m) = result.marker {
            if m.kind == MarkerKind::Entry && !m.display_only {
                m.display_only = true;
                flagged += 1;
            }
        }
        if flagged > 0 {
            tracing::info!(
                component = LOOP_NAME,
                symbol = %input.symbol,
                tf = %input.timeframe,
                strict_reason,
                flagged,
                "Entry flagged display_only by strict gate (visible, no real fire)"
            );
            log_fsm_veto("fsm_strict", strict_reason, result.diagnostics.combined_score);
        }
    }

    result
}

fn refresh_indicator_cache(market: &MarketDataState, symbol: &str, tf: &str) {
    let candles = {
        let buffers = market.candle_buffers.lock().unwrap();
        let key = format!("{symbol}:{tf}");
        buffers.get(&key).map(|b| b.snapshot())
    };

    let candles = match candles {
        Some(c) if !c.is_empty() => c,
        _ => return,
    };

    let periods = IndicatorPeriods::default();

    let mut cache = market.indicator_cache.lock().unwrap();
    let _ = cache.get_or_compute(symbol, tf, &candles, &periods);
}

#[cfg(test)]
mod tests {
    use crate::config::RuntimeConfig;
    use crate::market::market_state::MarketDataState;
    use crate::state::{FsmPhase, MarkerKind, SymbolFsm, TitanState};
    use crate::smarttrend::strategies::MarketRegime;

    use crate::smarttrend::engine::{evaluate, MomentumInput, SmartTrendInput, Timeframe};

    use super::{
        assemble_momentum_input, classify_entry_diagnostic_reason, compute_trend_metrics,
        evaluate_symbol_data_only, TrendMetrics,
    };
    use super::super::dispatch::apply_eval_result;

    #[test]
    fn momentum_input_no_candles() {
        let market = MarketDataState::new();
        let input = assemble_momentum_input(&market, "AAPL", "5m");
        assert!((input.rvol - 1.0).abs() < f64::EPSILON);
        assert!(input.roc.abs() < f64::EPSILON);
    }

    #[test]
    fn momentum_input_with_candles() {
        let market = MarketDataState::new();
        {
            let mut buffers = market.candle_buffers.lock().unwrap();
            let mut buf = crate::market::candle_buffer::CandleRingBuffer::new(300);
            let candles: Vec<titan_types::Candle> = (0..25)
                .map(|i| titan_types::Candle {
                    time: (i + 1) * 300,
                    open: 100.0 + i as f64,
                    high: 101.0 + i as f64,
                    low: 99.0 + i as f64,
                    close: 100.5 + i as f64,
                    volume: 1000.0 + (i as f64 * 100.0),
                })
                .collect();
            buf.seed(candles);
            buffers.insert("AAPL:5m".to_string(), buf);
        }

        let input = assemble_momentum_input(&market, "AAPL", "5m");
        assert!(input.rvol > 0.0);
    }

    #[test]
    fn data_only_eval_does_not_force_idle_on_unfavorable_diagnostic() {
        let cfg = RuntimeConfig::default().smarttrend;
        let input = strong_bullish_input("AAPL");
        let current_fsm = SymbolFsm {
            prev_close: 100.0,
            ..SymbolFsm::default()
        };

        let engine_result = evaluate(&input, &current_fsm, &cfg);
        assert_ne!(engine_result.new_fsm.phase, FsmPhase::Idle);

        let sentinel_result = evaluate_symbol_data_only(
            &input,
            &current_fsm,
            &cfg,
            Some("trend_not_bullish"),
            // S37: strict_reason None here — this test asserts that the
            // (relaxed) data-only eval doesn't force Idle even on an
            // unfavorable per-TF reason. The strict-gate flagging is
            // tested separately in `s37_strict_gate_flags_display_only`.
            None,
            1.0,
            true,
            MarketRegime::Calm,
            0.0,
        );

        assert_eq!(sentinel_result.new_fsm.phase, engine_result.new_fsm.phase);
        assert_eq!(
            sentinel_result.new_fsm.confirm_count,
            engine_result.new_fsm.confirm_count
        );
    }

    #[test]
    fn data_only_eval_preserves_engine_markers() {
        let cfg = RuntimeConfig::default().smarttrend;
        let input = strong_bullish_input("AAPL");
        let current_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            confirm_count: 1,
            prev_close: 100.0,
            ..Default::default()
        };

        let engine_result = evaluate(&input, &current_fsm, &cfg);
        assert!(engine_result.marker.is_some());

        let sentinel_result = evaluate_symbol_data_only(
            &input,
            &current_fsm,
            &cfg,
            Some("pullback_not_valid"),
            None,
            1.0,
            true,
            MarketRegime::Calm,
            0.0,
        );

        assert_eq!(sentinel_result.marker.is_some(), engine_result.marker.is_some());
        assert_eq!(
            sentinel_result.marker.as_ref().map(|m| m.kind),
            engine_result.marker.as_ref().map(|m| m.kind)
        );
    }

    #[test]
    fn data_only_applies_engine_result_identically() {
        let state = TitanState::new(RuntimeConfig::default());
        let cfg = RuntimeConfig::default().smarttrend;
        let input = strong_bullish_input("AAPL");
        let prev_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            confirm_count: 1,
            prev_close: 100.0,
            ..Default::default()
        };

        let result = evaluate_symbol_data_only(
            &input,
            &prev_fsm,
            &cfg,
            Some("sideways_regime"),
            None,
            1.0,
            true,
            MarketRegime::Calm,
            0.0,
        );
        let phase_changed = apply_eval_result(&state, "AAPL", "5m", &result, prev_fsm.phase);
        let expected_phase_change = prev_fsm.phase != result.new_fsm.phase;

        let st = state.smarttrend.lock().unwrap();
        // S36 — FSM is stored under `fsm_key(symbol, tf)` not bare symbol.
        let stored_fsm = st
            .fsm
            .get(&crate::state::fsm_key("AAPL", "5m"))
            .expect("FSM must be written");
        assert_eq!(stored_fsm.phase, result.new_fsm.phase);
        assert_eq!(stored_fsm.confirm_count, result.new_fsm.confirm_count);
        let stored_markers = st
            .markers
            .iter()
            .filter(|m| m.symbol == "AAPL" && m.bar_time == input.bar_time)
            .count();
        assert_eq!(stored_markers, if result.marker.is_some() { 1 } else { 0 });
        assert_eq!(phase_changed, expected_phase_change);
    }

    #[test]
    fn trend_metrics_detects_bullish_pullback() {
        let market = MarketDataState::new();
        let cfg = RuntimeConfig::default().smarttrend;

        let mut candles: Vec<titan_types::Candle> = Vec::new();
        for i in 0..80 {
            let trend = 100.0 + i as f64 * 0.25;
            let close = if (60..66).contains(&i) {
                trend - 1.4
            } else if i >= 66 {
                trend - 0.4
            } else {
                trend
            };
            let open = if i > 0 {
                candles[i - 1].close
            } else {
                close - 0.2
            };
            candles.push(titan_types::Candle {
                time: (i as i64 + 1) * 300,
                open,
                high: close + 0.35,
                low: close - 0.35,
                close,
                volume: 1500.0 + i as f64 * 20.0,
            });
        }

        seed_symbol_candles(&market, "AAPL", "5m", candles.clone());
        let current_price = candles.last().map(|c| c.close).unwrap_or(0.0);
        let metrics = compute_trend_metrics(&market, "AAPL", "5m", current_price, &cfg);

        assert!(metrics.is_bullish_trend);
        assert!(!metrics.is_sideways);
        assert!(metrics.pullback_valid);
    }

    #[test]
    fn trend_metrics_detects_sideways_regime() {
        let market = MarketDataState::new();
        let cfg = RuntimeConfig::default().smarttrend;

        let candles: Vec<titan_types::Candle> = (0..80)
            .map(|i| {
                let wobble = if i % 2 == 0 { 0.08 } else { -0.08 };
                let close = 100.0 + wobble;
                titan_types::Candle {
                    time: (i as i64 + 1) * 300,
                    open: 100.0 - wobble,
                    high: close + 0.12,
                    low: close - 0.12,
                    close,
                    volume: 1200.0,
                }
            })
            .collect();

        seed_symbol_candles(&market, "MSFT", "5m", candles.clone());
        let current_price = candles.last().map(|c| c.close).unwrap_or(0.0);
        let metrics = compute_trend_metrics(&market, "MSFT", "5m", current_price, &cfg);

        assert!(metrics.is_sideways);
        assert!(!metrics.is_bullish_trend);
        assert!(!metrics.pullback_valid);
    }

    #[test]
    fn classify_reason_sideways_on_default_metrics() {
        let cfg = RuntimeConfig::default().smarttrend;
        let reason =
            classify_entry_diagnostic_reason(FsmPhase::Idle, &TrendMetrics::default(), &cfg, "5m");
        assert_eq!(reason, Some("sideways_regime"));
    }

    /// S35 (2026-05-28) — On short TFs (1m/5m) the bearish_trend
    /// classifier MUST be skipped so the FSM cycles freely after
    /// catalyst-driven peaks (post-spike retracements are normal
    /// price action, not a regime flip). The pullback ceiling is
    /// also widened 2.5x to admit pre-market gap-up signatures.
    #[test]
    fn classify_reason_1m_skips_bearish_trend() {
        let cfg = RuntimeConfig::default().smarttrend;
        let bearish_metrics = TrendMetrics {
            is_bullish_trend: true,
            is_sideways: false,
            pullback_valid: true,
            slope_bps: -50.0, // strongly bearish on 1m
            efficiency_ratio: 0.5,
            pullback_depth_atr: 1.0, // within ceiling
            ..Default::default()
        };
        // 1m should NOT classify bearish_trend → falls through to None
        let reason_1m = classify_entry_diagnostic_reason(
            FsmPhase::Confirming,
            &bearish_metrics,
            &cfg,
            "1m",
        );
        assert_eq!(reason_1m, None, "1m must skip bearish_trend gate");

        // 15m STILL classifies bearish_trend (strict on higher TFs)
        let reason_15m = classify_entry_diagnostic_reason(
            FsmPhase::Confirming,
            &bearish_metrics,
            &cfg,
            "15m",
        );
        assert_eq!(reason_15m, Some("bearish_trend"));
    }

    #[test]
    fn classify_reason_5m_relaxes_pullback_ceiling() {
        let cfg = RuntimeConfig::default().smarttrend; // pullback_max_atr = 2.0
        let wide_pullback = TrendMetrics {
            is_bullish_trend: true,
            is_sideways: false,
            pullback_valid: true,
            slope_bps: 5.0, // bullish
            efficiency_ratio: 0.5,
            pullback_depth_atr: 4.5, // 4.5 ATR — within 5.0 (2.0 * 2.5) on 1m/5m/10m
            ..Default::default()
        };
        // 5m admits (ceiling = 2.0 * 2.5 = 5.0 ATR, depth = 4.5)
        let reason_5m = classify_entry_diagnostic_reason(
            FsmPhase::Confirming,
            &wide_pullback,
            &cfg,
            "5m",
        );
        assert_eq!(reason_5m, None, "5m must admit 4.5 ATR pullback");

        // 15m rejects with S36 mid-tier ceiling (2.0 * 1.5 = 3.0 ATR, depth = 4.5)
        let reason_15m = classify_entry_diagnostic_reason(
            FsmPhase::Confirming,
            &wide_pullback,
            &cfg,
            "15m",
        );
        assert_eq!(reason_15m, Some("overextended_after_spike"));
    }

    /// S36 (2026-05-28) — 10m must behave like 1m/5m: skip bearish_trend
    /// gate and accept up to 5.0 ATR pullbacks. Pre-S36 the 10m chart
    /// was the worst-affected (zero markers across the entire 13-symbol
    /// watchlist on 2026-05-28) because it inherited the strict 1.0× ceiling.
    #[test]
    fn classify_reason_10m_matches_short_tf_relaxations() {
        let cfg = RuntimeConfig::default().smarttrend;
        let bearish_with_wide_pullback = TrendMetrics {
            is_bullish_trend: true,
            is_sideways: false,
            pullback_valid: true,
            slope_bps: -50.0, // strongly bearish — would trip on 15m+
            efficiency_ratio: 0.5,
            pullback_depth_atr: 4.5, // 4.5 ATR — within 5.0 ceiling on small TFs
            ..Default::default()
        };
        // 10m must skip both bearish_trend AND admit the 4.5 ATR pullback.
        let reason_10m = classify_entry_diagnostic_reason(
            FsmPhase::Confirming,
            &bearish_with_wide_pullback,
            &cfg,
            "10m",
        );
        assert_eq!(
            reason_10m, None,
            "10m must skip bearish_trend AND admit 4.5 ATR pullback"
        );
    }

    /// S36 — 15m/30m get a softer middle-tier ceiling (1.5×) so they're
    /// not strict-strict but still demand more structure than 1m/5m/10m.
    /// Pullbacks under 3.0 ATR pass on 15m/30m; over 3.0 ATR rejected.
    #[test]
    fn classify_reason_30m_uses_middle_tier_ceiling() {
        let cfg = RuntimeConfig::default().smarttrend; // pullback_max_atr = 2.0
        let medium_pullback = TrendMetrics {
            is_bullish_trend: true,
            is_sideways: false,
            pullback_valid: true,
            slope_bps: 5.0,
            efficiency_ratio: 0.5,
            pullback_depth_atr: 2.5, // within 3.0 mid-tier ceiling
            ..Default::default()
        };
        let reason_30m = classify_entry_diagnostic_reason(
            FsmPhase::Confirming,
            &medium_pullback,
            &cfg,
            "30m",
        );
        assert_eq!(reason_30m, None, "30m must admit 2.5 ATR pullback under 3.0× ceiling");

        let mut wider = medium_pullback.clone();
        wider.pullback_depth_atr = 3.5; // exceeds 3.0 mid-tier ceiling
        let reason_30m_wide = classify_entry_diagnostic_reason(
            FsmPhase::Confirming,
            &wider,
            &cfg,
            "30m",
        );
        assert_eq!(
            reason_30m_wide,
            Some("overextended_after_spike"),
            "30m rejects 3.5 ATR pullback past mid-tier ceiling"
        );
    }

    /// v2.4.2 — replaces `classify_reason_none_when_not_idle`. The
    /// historical contract was "diagnostic returns None unless we're
    /// still in Idle" — that turned out to neuter the post-eval veto
    /// at the Confirming→Active step where the Entry marker actually
    /// emits (see the comment block in `classify_entry_diagnostic_reason`).
    /// New contract: diagnostic reflects trend metrics regardless of
    /// the current phase. The veto pipeline only cares that we surface
    /// a reason at the moment the marker exists.
    #[test]
    fn classify_reason_emitted_regardless_of_phase() {
        let cfg = RuntimeConfig::default().smarttrend;
        for phase in [
            FsmPhase::Idle,
            FsmPhase::Confirming,
            FsmPhase::Active,
            FsmPhase::Exit,
            FsmPhase::Stalking,
        ] {
            // default TrendMetrics are flat → "sideways_regime" is the
            // expected reason. The point of the test is that it does
            // NOT vanish to `None` just because we're past Idle.
            let reason =
                classify_entry_diagnostic_reason(phase, &TrendMetrics::default(), &cfg, "5m");
            assert_eq!(
                reason,
                Some("sideways_regime"),
                "phase {phase:?} unexpectedly returned None"
            );
        }
    }

    /// Regression for the MGNX 2026-05-12 11:35 ET incident. When
    /// the candle buffer is < 40 bars (newly promoted symbol,
    /// reconnect after streamer hiccup, etc.) every `TrendMetrics`
    /// field is the silent `Default` — `is_sideways=true` by
    /// definition, slope/efficiency/pullback all 0.0. The 1m TF
    /// veto path only fires on `overextended_after_spike`, so the
    /// fake-sideways diagnostic doesn't block the entry. The fix is
    /// `has_sufficient_history`: when false, any Entry marker
    /// produced by `evaluate` is yanked regardless of TF / reason.
    #[test]
    fn data_only_vetoes_entry_when_history_is_insufficient() {
        let cfg = RuntimeConfig::default().smarttrend;
        let input = strong_bullish_input("MGNX");
        let current_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            confirm_count: 1,
            prev_close: 100.0,
            ..Default::default()
        };
        let engine_result = evaluate(&input, &current_fsm, &cfg);
        assert!(
            engine_result.marker.is_some(),
            "precondition: engine must emit an Entry marker so the veto has something to flag"
        );

        let sentinel_result = evaluate_symbol_data_only(
            &input,
            &current_fsm,
            &cfg,
            None, // no diag reason — only history flag should matter here
            None, // no strict reason either — test focuses on history veto
            1.0,
            false, // ← key: insufficient history
            MarketRegime::Calm,
            0.0,
        );

        // S35.3+ contract: the veto NO LONGER removes Entry markers —
        // it flags them `display_only = true` so the chart still shows
        // them as a manual-trading reference, while `dispatch.rs`
        // strips display-only markers from the BotAction event so
        // real-money fires are still suppressed. Verify the FLAG, not
        // the absence.
        let entry_markers: Vec<_> = sentinel_result
            .marker
            .iter()
            .filter(|m| m.kind == MarkerKind::Entry)
            .collect();
        assert!(
            !entry_markers.is_empty(),
            "Entry markers must be retained for chart visibility"
        );
        assert!(
            entry_markers.iter().all(|m| m.display_only),
            "Entry markers must be flagged display_only when has_sufficient_history is false"
        );
    }

    /// Regression for the AGL 2026-05-12 incident. With pullback
    /// depth ≫ `pullback_max_atr` the diagnostic *must* fire even
    /// from the Confirming phase so the eval pipeline can yank the
    /// Entry marker before it reaches the bot.
    #[test]
    fn classify_reason_overextended_fires_from_confirming() {
        let cfg = RuntimeConfig::default().smarttrend;
        let metrics = TrendMetrics {
            is_bullish_trend: true,
            is_sideways: false,
            slope_bps: 200.0,
            efficiency_ratio: 0.3,
            pullback_valid: false,
            pullback_depth_atr: 40.0, // gap-up, ≫ cfg.pullback_max_atr (1.4)
            has_sufficient_history: true,
            above_ema_fast: true,
            above_ema_slow: true,
        };
        // Test on 15m where pullback_max_atr is strict (2.0 ATR).
        // A 40 ATR gap-up triggers `overextended_after_spike` on any TF.
        let reason =
            classify_entry_diagnostic_reason(FsmPhase::Confirming, &metrics, &cfg, "15m");
        assert_eq!(reason, Some("overextended_after_spike"));
    }

    fn strong_bullish_input<'a>(symbol: &'a str) -> SmartTrendInput<'a> {
        SmartTrendInput {
            momentum_input: MomentumInput {
                rvol: 1.5,
                roc: 2.0,
            },
            current_price: 101.0,
            prev_close: 100.0,
            bar_time: 1700000000,
            timeframe: Timeframe::M5,
            symbol,
            atr: 0.0,
            regime: MarketRegime::Calm,
            macro_trend_bullish: true,
            above_ema_fast: true,
            above_ema_slow: true,
            news_sentiment: None,
        }
    }

    #[allow(dead_code)]
    fn make_test_candles(count: usize, base_price: f64) -> Vec<titan_types::Candle> {
        (0..count)
            .map(|i| titan_types::Candle {
                time: (i as i64 + 1) * 300,
                open: base_price + (i as f64 * 0.1),
                high: base_price + (i as f64 * 0.1) + 0.5,
                low: base_price + (i as f64 * 0.1) - 0.3,
                close: base_price + (i as f64 * 0.1) + 0.2,
                volume: 1000.0 + (i as f64 * 10.0),
            })
            .collect()
    }

    fn seed_symbol_candles(
        market: &MarketDataState,
        symbol: &str,
        timeframe: &str,
        candles: Vec<titan_types::Candle>,
    ) {
        let mut buffers = market.candle_buffers.lock().unwrap();
        let mut buf = crate::market::candle_buffer::CandleRingBuffer::new(300);
        buf.seed(candles);
        buffers.insert(format!("{symbol}:{timeframe}"), buf);
    }

    #[test]
    fn test_spread_guard_veto() {
        let mut cfg = RuntimeConfig::default().smarttrend;
        cfg.max_spread_bps = 25.0; // max spread 25 bps

        let input = strong_bullish_input("AAPL");
        // We create an FSM in Confirming state so it would transition to Active and emit Entry marker
        let current_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            confirm_count: 1,
            prev_close: 100.0,
            ..Default::default()
        };

        // Case 1: Spread (30 bps) > max_spread_bps (25 bps) => entry marker should be flagged display_only
        let res_vetoed = evaluate_symbol_data_only(
            &input,
            &current_fsm,
            &cfg,
            None,
            None,
            1.0,
            true,
            MarketRegime::Calm,
            30.0, // spread 30 bps
        );
        let entry_markers: Vec<_> = res_vetoed.marker.iter().filter(|m| m.kind == MarkerKind::Entry).collect();
        assert!(!entry_markers.is_empty(), "Engine should produce Entry marker");
        assert!(entry_markers.iter().all(|m| m.display_only), "Entry markers must be display_only due to Spread Guard");

        // Case 2: Spread (20 bps) <= max_spread_bps (25 bps) => entry marker should NOT be display_only
        let res_ok = evaluate_symbol_data_only(
            &input,
            &current_fsm,
            &cfg,
            None,
            None,
            1.0,
            true,
            MarketRegime::Calm,
            20.0, // spread 20 bps
        );
        let entry_markers_ok: Vec<_> = res_ok.marker.iter().filter(|m| m.kind == MarkerKind::Entry).collect();
        assert!(!entry_markers_ok.is_empty(), "Engine should produce Entry marker");
        assert!(entry_markers_ok.iter().all(|m| !m.display_only), "Entry markers must NOT be display_only");
    }

    #[test]
    fn test_regime_adaptive_calm_momentum_gate() {
        let mut cfg = RuntimeConfig::default().smarttrend;
        cfg.regime_adaptive_entries = true;
        cfg.entry_min_momentum = 52.0;

        // On 1h timeframe, entry_min_momentum is base (52.0).
        // With Calm regime adaptive entries enabled, entry_min_momentum is scaled by 1.1x to 57.2.
        // We set input momentum score to 55.0 (which is > 52.0 but < 57.2).
        // rvol = 1.0, roc = 0.25 => score = 55.0
        let mut input = strong_bullish_input("AAPL");
        input.timeframe = Timeframe::H1;
        input.momentum_input = MomentumInput {
            rvol: 1.0,
            roc: 0.25,
        };

        let current_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            confirm_count: 1,
            prev_close: 100.0,
            ..Default::default()
        };

        // Case 1: Calm regime => entry_min_momentum = 57.2 => score (55.0) is not enough => no entry transition (no entry marker)
        let res_calm = evaluate_symbol_data_only(
            &input,
            &current_fsm,
            &cfg,
            None,
            None,
            1.0,
            true,
            MarketRegime::Calm,
            0.0,
        );
        let entry_markers_calm: Vec<_> = res_calm.marker.iter().filter(|m| m.kind == MarkerKind::Entry).collect();
        assert!(entry_markers_calm.is_empty(), "Should not emit Entry marker in Calm regime due to increased momentum floor");

        // Case 2: Trending regime => entry_min_momentum = 52.0 => score (55.0) is enough => entry transition (Entry marker emitted)
        let res_trending = evaluate_symbol_data_only(
            &input,
            &current_fsm,
            &cfg,
            None,
            None,
            1.0,
            true,
            MarketRegime::Trending,
            0.0,
        );
        let entry_markers_trending: Vec<_> = res_trending.marker.iter().filter(|m| m.kind == MarkerKind::Entry).collect();
        assert!(!entry_markers_trending.is_empty(), "Should emit Entry marker in Trending regime since momentum floor is not scaled");
    }

}
