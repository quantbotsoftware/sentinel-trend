//! Tournament dispatcher — bridges the 8-strategy registry into the
//! per-bar sentinel evaluation loop.
//!
//! v2.5.0 (May 2026): the strategy framework introduced in S20 lived
//! as code without a caller — bot trades came exclusively from the
//! legacy SentinelTrend FSM. This module wires `strategies::registry()`
//! into the same evaluation tick so each closed bar triggers:
//!
//!   1. legacy FSM eval (`apply_eval_side_effects`)              — unchanged
//!   2. tournament eval (`dispatch_tournament_signals`)          — NEW
//!
//! For each symbol, every strategy whose `TradingWindow` permits the
//! current ET time is queried for an entry signal. Signals are
//! materialised as `BotAction::Entry` with `strategy_id: Some(_)` and
//! pushed onto the same `bot_tx` channel the legacy path uses — so
//! the entire downstream pipeline (handle_entry → quality gate →
//! Monte-Carlo gate → bandit → send_order) is reused bit-for-bit.
//!
//! ## Gating layers
//!
//! 1. `evolution_protocol.enabled` master kill-switch.
//! 2. `excluded_symbols` already filtered upstream by the scanner.
//! 3. Strategy `allowed_window().permits()`.
//! 4. Per-strategy `evaluate_entry()` returning `Some(_)`.
//! 5. Tournament `StrategyStatus::Active` (Draining/Dead silently skipped).
//! 6. `current_sub_wallet > 0` (no signal if capital was reclaimed).
//!
//! ## Why borrow the registry static, not own it?
//!
//! The registry is built once at startup (`OnceLock`) and never mutates.
//! Re-instantiating per tick would do 8 vtable allocations + per-strategy
//! default initialisation for zero gain. Borrowing `&'static [Box<dyn …>]`
//! is the cheapest correct option and the strategies themselves are
//! intentionally stateless (decisions are pure functions of
//! `MarketContext`).

use std::sync::atomic::Ordering;

use chrono::Datelike;
use tauri::{AppHandle, Manager};

use crate::market::market_state::MarketDataState;
use crate::smarttrend::bot::{BotAction, BotActionKind};
use crate::smarttrend::markers::create_marker;
use crate::smarttrend::strategies::{self, MarketContext};
use crate::state::{BarClosedMsg, MarkerKind, TitanState};

use super::loop_run::LOOP_NAME;

/// Stable direction label for the decision journal.
pub(crate) fn direction_str(d: crate::state::Direction) -> &'static str {
    match d {
        crate::state::Direction::Long => "long",
        crate::state::Direction::Short => "short",
    }
}

/// P1 (hospital) — true when `symbol` has hit the per-symbol daily
/// loss breaker. Reads the breaker threshold from the PRIME engine
/// (standalone mutex, dropped first) then today's ET closes from the
/// journal (Tier A, taken alone). Fail-open on any lock error.
pub(crate) fn symbol_daily_breaker_tripped(state: &TitanState, symbol: &str) -> bool {
    let max_consecutive = match state.dplus() {
        Some(dp) => match dp.prime.lock() {
            Ok(engine) if engine.enabled => engine.hospital.policy.symbol_max_consecutive_losses,
            _ => return false,
        },
        None => return false,
    };
    // ET midnight (S04 — never Utc::now().date_naive() for sessions).
    let day_start_ts = {
        let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        now_et
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|dt| dt.and_local_timezone(chrono_tz::US::Eastern).single())
            .map(|dt| dt.timestamp())
            .unwrap_or(0)
    };
    let pnls_today: Vec<f64> = {
        let Ok(journal) = state.journal.lock() else {
            return false;
        };
        let mut closes: Vec<(i64, f64)> = journal
            .entries
            .iter()
            .filter(|e| e.symbol.eq_ignore_ascii_case(symbol))
            .filter_map(|e| match (e.exit_time, e.pnl) {
                (Some(t), Some(p)) if t >= day_start_ts => Some((t, p)),
                _ => None,
            })
            .collect();
        closes.sort_by_key(|(t, _)| *t);
        closes.into_iter().map(|(_, p)| p).collect()
    };
    crate::smarttrend::prime::hospital::symbol_banned_today(&pnls_today, max_consecutive)
}

/// Build a `MarketContext` snapshot from the live market state for the
/// supplied `symbol`. Returns `None` when essential data is missing
/// (no candles, NaN price, etc.) — silent skip means strategies never
/// see degenerate inputs.
fn build_market_context(
    market: &MarketDataState,
    state: &TitanState,
    symbol: &str,
    msg: &BarClosedMsg,
    atr: f64,
) -> Option<MarketContext> {
    use chrono::Timelike;

    let timeframe: &'static str = if msg.timeframe == "1m" { "1m" } else { "5m" };

    let buf_key = format!("{symbol}:{}", msg.timeframe);
    let candles = {
        let buffers = market.candle_buffers.lock().ok()?;
        buffers.get(&buf_key)?.snapshot()
    };
    if candles.is_empty() {
        return None;
    }
    let price = msg.close_price;
    if !price.is_finite() || price <= 0.0 {
        return None;
    }

    // prev_close = the close before the last bar in our buffer. Falls
    // back to current price when we have only one bar (warmup).
    let prev_close = if candles.len() >= 2 {
        candles[candles.len() - 2].close
    } else {
        price
    };

    // session_open: first RTH bar of *today* (ET). When the buffer
    // doesn't span the open we fall back to prev_close so gap_pct is
    // simply 0 and gap-sensitive strategies refuse.
    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let session_open = candles
        .iter()
        .find(|c| {
            let bar_et = chrono::DateTime::<chrono::Utc>::from_timestamp(c.time, 0)
                .map(|d| d.with_timezone(&chrono_tz::US::Eastern));
            match bar_et {
                Some(d) => {
                    use chrono::Datelike;
                    d.year() == now_et.year()
                        && d.ordinal() == now_et.ordinal()
                        && (d.hour() * 60 + d.minute()) >= 9 * 60 + 30
                }
                None => false,
            }
        })
        .map(|c| c.open)
        .unwrap_or(prev_close);

    let gap_pct = if prev_close > 0.0 {
        ((session_open - prev_close) / prev_close) * 100.0
    } else {
        0.0
    };

    // Pull rvol / spread / catalyst score from the latest scan results.
    // The scanner produces only the top-N candidates per refresh (default
    // 100, sometimes only ~30 in slow markets), so pinned favorites and
    // open positions often miss the cut. When that happens we synthesise
    // the metrics LOCALLY from the candle buffer so strategies stay live
    // even outside the scanner's hot list. This is what unblocked
    // Tournament signalling in live trading 2026-05-14 — before this
    // fallback, every non-top-30 symbol came in with `rvol=1.0,
    // catalyst=None, spread=0` and every strategy refused.
    //
    // Local fallbacks:
    //   - rvol           = max(0.1, current_bar_volume / mean(last 20 bars))
    //   - spread_bps     = 0.0 (no quote data here; strategies that gate
    //                     on spread will gracefully accept tight, which
    //                     is what the broker validation will catch later)
    //   - catalyst_score = price-action proxy: |gap_pct| + |5bar_move_pct|
    //                     clamped 0..100. Not as rich as the news-aware
    //                     scanner score, but enough to unblock the
    //                     catalyst-only strategies (CatalystFrontRun,
    //                     ExtendedNews) when the symbol shows obvious
    //                     directional pressure.
    //   - is_halted      = false (no halt feed here; bot's halt watchdog
    //                     handles the rare event upstream).
    // AUDIT BUG 3 (S29, 2026-05-28) — Stale Scan Results gate.
    //
    // Pre-S29 the dispatcher pulled `rvol/spread/catalyst/is_halted`
    // from `market_state.scan_results` without ANY freshness check.
    // If the scanner loop crashed, the SIP feed died, or the scan-window
    // policy in `auto_scan.rs` closed the window for the session, the
    // dispatcher would silently use yesterday's catalyst data to trade
    // today's bars — exactly the failure mode flagged by audit Q30.
    //
    // S29 enforces a temporal gate: when `last_scan_ts` is older than
    // `STALE_SCAN_MAX_AGE_SECS` we treat the scan as missing and fall
    // through to the LOCAL synthesis branch (rvol from candles, catalyst
    // from price-action proxy). The fallback already exists for the
    // "symbol not in top-N" case; we now reuse it for "scan whole-file
    // stale" too.
    //
    // 900s (= 15 min) chosen because:
    //   * Default scan interval is 30s in RTH, 10 min in premarket.
    //   * Any value < 15 min covers both modes plus a 5-min grace for
    //     SIP backfill latency.
    //   * Audit Q30 explicitly says "yesterday's catalyst" → anything
    //     under an hour is conservative enough.
    const STALE_SCAN_MAX_AGE_SECS: i64 = 900;
    // PRIME F0.1 — capture the live regime in the same Tier-A pass.
    // Pre-F0 this was hardcoded `None`, which silently disabled the
    // LeveragedEtf Volatile/Panic gate and starved the conviction
    // fusion of its regime_fit term.
    let (scan_is_fresh, regime) = {
        let market_state = state.market.lock().ok()?;
        let fresh = match market_state.scan_status.last_scan_ts {
            Some(ts) => {
                let age = chrono::Utc::now().timestamp().saturating_sub(ts);
                age <= STALE_SCAN_MAX_AGE_SECS
            }
            // No scan has ever completed (cold start) — synthesize.
            None => false,
        };
        (fresh, market_state.regime)
    };
    let (rvol, spread_bps, catalyst_score, is_halted) = {
        let market_state = state.market.lock().ok()?;
        let sym_upper = symbol.to_ascii_uppercase();
        let hit = if scan_is_fresh {
            market_state
                .scan_results
                .iter()
                .find(|r| r.symbol.eq_ignore_ascii_case(&sym_upper))
        } else {
            tracing::debug!(
                component = "tournament_dispatch",
                symbol = %sym_upper,
                last_scan_ts = ?market_state.scan_status.last_scan_ts,
                "S29 scan_results STALE (>15min) — synthesizing from candles"
            );
            None
        };
        match hit {
            Some(r) => (r.rvol, r.spread_bps, Some(r.score), r.catalyst_flags.halted),
            None => {
                // Local rvol from candle volumes.
                let local_rvol = if candles.len() >= 5 {
                    let n = candles.len();
                    let current_vol = candles[n - 1].volume.max(1.0);
                    let lookback = std::cmp::min(20, n - 1);
                    let avg_vol: f64 = candles[(n - 1 - lookback)..(n - 1)]
                        .iter()
                        .map(|c| c.volume)
                        .sum::<f64>()
                        / lookback as f64;
                    if avg_vol > 0.0 {
                        (current_vol / avg_vol).clamp(0.1, 20.0)
                    } else {
                        1.0
                    }
                } else {
                    1.0
                };
                // Local catalyst proxy: blend of gap + 5-bar move magnitude.
                let recent_move_pct = if candles.len() >= 6 {
                    let anchor = &candles[candles.len() - 6];
                    if anchor.close > 0.0 {
                        ((price - anchor.close) / anchor.close * 100.0).abs()
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                let local_catalyst =
                    (gap_pct.abs() * 4.0 + recent_move_pct * 6.0).clamp(0.0, 100.0);
                let catalyst_some = if local_catalyst > 1.0 {
                    Some(local_catalyst)
                } else {
                    None
                };
                (local_rvol, 0.0, catalyst_some, false)
            }
        }
    };

    let now_et_min = now_et.hour() * 60 + now_et.minute();

    // v2.5.0 NUCLEAR (N1) — Float-shares lookup from the orphan float
    // cache (now wired). Populated by the FMP refresher; `None` until
    // the first successful fetch for the symbol.
    let float_shares = state
        .float_cache
        .get()
        .and_then(|fc| fc.lock().ok())
        .and_then(|fc| fc.get(symbol));

    // v2.5.0 NUCLEAR (N3) — Resolve companion symbol for pair / stat-arb
    // strategies. Hardcoded inverse-pair map (SPY↔SH, QQQ↔PSQ,
    // TQQQ↔SQQQ, etc.) so EtfPair and StatArb50 can actually fire.
    // Without this the snapshot always stayed at `None` and both
    // strategies were dead code for ~6 months.
    let companion = resolve_companion_snapshot(market, symbol);

    // ATR local fallback when the FSM-supplied atr is 0 (the FSM
    // doesn't seed atr from cold-start backfill, only from live tick
    // accumulation). Compute pure ATR(14) from the candle buffer
    // directly. This unblocks strategies that ctx_is_sane-permit atr=0
    // but require atr>0 for stop/target distance.
    let atr = if atr <= 0.0 && candles.len() >= 14 {
        let n = candles.len();
        let mut trs: Vec<f64> = Vec::with_capacity(14);
        for i in (n - 14)..n {
            let high = candles[i].high;
            let low = candles[i].low;
            let prev_close_i = if i == 0 {
                candles[i].open
            } else {
                candles[i - 1].close
            };
            let tr = (high - low)
                .max((high - prev_close_i).abs())
                .max((low - prev_close_i).abs());
            trs.push(tr);
        }
        let sum: f64 = trs.iter().sum();
        sum / trs.len() as f64
    } else {
        atr
    };

    let mut overrides = state
        .dplus()
        .map(|dp| dp.overrides.snapshot())
        .unwrap_or_default();
    // F4-APEX — operator calibration overrides (`apex_*` keys in
    // `frontier_overrides`) applied on top of the D+ snapshot: an
    // explicit operator APPLY beats automated tuning. Config is Tier-A
    // #1, taken alone in its own scope (guard dropped before any
    // Tier-B acquisition below).
    {
        let cfg_over = state
            .config
            .lock()
            .map(|c| c.evolution_protocol.frontier_overrides.clone())
            .unwrap_or_default();
        crate::smarttrend::strategies::apex_calibration::merge_apex_overrides(
            &mut overrides,
            &cfg_over,
        );
    }

    // PRIME F0.6 — headline polarity from the watchlist refresh loop
    // (Tier-B6, acquired alone after all Tier-A guards are dropped).
    let news_sentiment = {
        let wm = market.watchlist_metrics.lock().ok()?;
        wm.get(&symbol.to_ascii_uppercase())
            .and_then(|m| m.news_sentiment)
    };

    Some(MarketContext {
        symbol: symbol.to_string(),
        timeframe,
        price,
        prev_close,
        session_open,
        candles,
        atr,
        rvol,
        gap_pct,
        spread_bps,
        catalyst_score,
        now_et_min,
        now_et_hour: now_et.hour(),
        now_et_minute: now_et.minute(),
        // PRIME F0.1 — live regime from MarketState (written by
        // regime_loop every poll). `Some(default Calm)` before the
        // detector's first classification is acceptable: Calm is the
        // most conservative regime for every regime-gated strategy.
        regime: Some(regime),
        is_halted,
        companion,
        float_shares,
        news_sentiment,
        overrides,
    })
}

/// v2.5.0 NUCLEAR (N3) — hardcoded inverse-pair map for ETF / leveraged
/// strategies. Returns a `CompanionSnapshot` when both legs are present
/// in our candle buffers, computing the trailing 50-bar ratio mean /
/// SD / Z. Returns `None` for unmapped symbols or insufficient data —
/// pair strategies are designed to refuse on `None`.
pub(crate) fn resolve_companion_snapshot(
    market: &MarketDataState,
    symbol: &str,
) -> Option<crate::smarttrend::strategies::CompanionSnapshot> {
    const PAIRS: &[(&str, &str)] = &[
        // Broad S&P 500 inverses
        ("SPY", "SH"),
        ("SH", "SPY"),
        // Nasdaq-100 inverses
        ("QQQ", "PSQ"),
        ("PSQ", "QQQ"),
        // Leveraged S&P 500 vs inverse
        ("SPXL", "SPXS"),
        ("SPXS", "SPXL"),
        // Leveraged Nasdaq vs inverse
        ("TQQQ", "SQQQ"),
        ("SQQQ", "TQQQ"),
        // Russell 2000 inverses
        ("IWM", "RWM"),
        ("RWM", "IWM"),
        // Semis vs inverse
        ("SOXL", "SOXS"),
        ("SOXS", "SOXL"),
        // Financials leveraged
        ("FAS", "FAZ"),
        ("FAZ", "FAS"),
        // Treasury 20yr leveraged
        ("TMF", "TMV"),
        ("TMV", "TMF"),
        // Gold miners leveraged
        ("NUGT", "DUST"),
        ("DUST", "NUGT"),
        // Oil & gas leveraged
        ("GUSH", "DRIP"),
        ("DRIP", "GUSH"),
        // Cointegration sector and ETF pairs
        ("XLK", "SPY"),
        ("SPY", "XLK"),
        ("XLK", "XLF"),
        ("XLF", "XLK"),
        ("XLE", "XLK"),
        ("XLK", "XLE"),
        ("XLY", "XLP"),
        ("XLP", "XLY"),
        ("XLV", "XLU"),
        ("XLU", "XLV"),
        ("XLI", "XLB"),
        ("XLB", "XLI"),
    ];
    let sym_upper = symbol.to_ascii_uppercase();
    let companion_sym = PAIRS
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(&sym_upper))
        .map(|(_, b)| *b)?;

    let buffers = market.candle_buffers.lock().ok()?;
    let key_a = format!("{}:1m", sym_upper);
    let key_b = format!("{}:1m", companion_sym);
    let a_snap = buffers.get(&key_a)?.snapshot();
    let b_snap = buffers.get(&key_b)?.snapshot();
    if a_snap.len() < 50 || b_snap.len() < 50 {
        return None;
    }
    let a_price = a_snap.last()?.close;
    let b_price = b_snap.last()?.close;
    if a_price <= 0.0 || b_price <= 0.0 {
        return None;
    }
    // Compute ratio statistics over the last 50 aligned bars (we just
    // take the last 50 from each — alignment within a 1m TF is good
    // enough for ratio z-score; minor mis-alignment averages out).
    let len = a_snap.len().min(b_snap.len()).min(50);
    let a_tail = &a_snap[a_snap.len() - len..];
    let b_tail = &b_snap[b_snap.len() - len..];
    let ratios: Vec<f64> = a_tail
        .iter()
        .zip(b_tail.iter())
        .filter_map(|(a, b)| {
            if a.close > 0.0 && b.close > 0.0 {
                Some(a.close / b.close)
            } else {
                None
            }
        })
        .collect();
    if ratios.len() < 20 {
        return None;
    }
    let mean: f64 = ratios.iter().sum::<f64>() / ratios.len() as f64;
    let variance: f64 =
        ratios.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / ratios.len() as f64;
    let sd = variance.sqrt();
    let current_ratio = a_price / b_price;
    let z = if sd > 0.0 {
        (current_ratio - mean) / sd
    } else {
        0.0
    };
    Some(crate::smarttrend::strategies::CompanionSnapshot {
        symbol: companion_sym.to_string(),
        price: b_price,
        ratio_mean: mean,
        ratio_sd: sd,
        ratio_z: z,
    })
}

/// Evaluate every tournament strategy against `symbol` and emit
/// `BotAction::Entry` for each `Some(EntrySignal)`. Returns the count
/// of dispatched signals (0 in 95%+ of ticks). Always non-fatal.
///
/// AUDIT RISK 8 (S32, 2026-05-28) — Backpressure handling upgrade.
///
/// Pre-S32 the dispatcher used `bot_tx.try_send(action)` which drops
/// the signal silently (warn-only) when the buffer is saturated.
/// Combined with CB2's 2048-slot buffer this was rare, but on RTH open
/// when multiple strategies converge on the same symbol the bot-side
/// consumer can lag (each `handle_entry` blocks on Alpaca REST for
/// 100-200ms) and the dispatcher can outrun it.
///
/// S32 strategy: bounded retry with `send_timeout`. We give the
/// consumer up to 250ms to drain a slot — that's roughly one full
/// REST round-trip, plenty for the typical convergence burst. If
/// the timeout fires we fall back to the old drop-and-warn behaviour
/// (`bot_actions_dropped` counter increments as before).
///
/// 250ms chosen as the upper bound on a sentinel-loop tick: any longer
/// would risk evaluating stale ATR/RVOL on the NEXT bar because the
/// dispatcher would block the symbol iteration. Real-world impact:
/// previously-dropped 90% of bursts now arrive 100-150ms later (well
/// inside the same bar's signalling window).
/// P0 (audit 2026-08-11) — a bar is live only if its close is recent:
/// stale when it closed more than max(2×TF, 120s) ago. Everything
/// older is a history replay / backfill artifact and must never reach
/// live order dispatch.
pub(super) fn bar_is_stale(bar_time: i64, timeframe: &str, now: i64) -> bool {
    let tf_secs = crate::market::candle_buffer::timeframe_to_secs(timeframe);
    let bar_close = bar_time + tf_secs;
    now - bar_close > (2 * tf_secs).max(120)
}

/// F4-APEX — persist the full APEX condition vector to the decision
/// journal, transition-throttled per symbol×TF exactly like the
/// Frontier loop: a row is written only when the miss-signature (or
/// candidate state) changes, so a persistent condition doesn't spam a
/// row per bar. Near-misses (1–2 failed) and candidates (0 failed) are
/// logged; deep failures are not (no calibration value).
fn log_apex_conditions(
    symbol: &str,
    timeframe: &str,
    ctx: &crate::smarttrend::strategies::MarketContext,
    score: f64,
    direction: &str,
) {
    use std::sync::{Mutex, OnceLock};
    static LAST_KEY: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();

    let (conds, _) = crate::smarttrend::strategies::apex::Apex::default().evaluate_entry_full(ctx);
    if conds.is_empty() {
        return;
    }
    let failed: Vec<&str> = conds.iter().filter(|c| !c.passed).map(|c| c.name).collect();
    let (stage_reason, is_candidate) = match failed.len() {
        0 => ("apex_candidate".to_string(), true),
        1..=2 => (format!("cond:{}", failed.join("+")), false),
        _ => return,
    };

    let map_key = format!("{symbol}:{timeframe}");
    let lock = LAST_KEY.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    {
        let mut m = lock.lock().unwrap_or_else(|p| p.into_inner());
        // S06 — bounded: dedup keys only, cleared wholesale at cap.
        if m.len() > 512 {
            m.clear();
        }
        if m.get(&map_key).map(String::as_str) == Some(stage_reason.as_str()) {
            return;
        }
        m.insert(map_key, stage_reason.clone());
    }

    let cond_json = serde_json::to_string(&conds).unwrap_or_else(|_| "[]".into());
    // Direction "none" on near-misses keeps them out of the shadow
    // claimer (`recent_vetoed` filters direction != 'none').
    let mut rec = crate::ops::decision_log::DecisionRecord::vetoed(
        symbol,
        timeframe,
        "apex",
        if is_candidate { direction } else { "none" },
        score,
        "strategy",
        &stage_reason,
        chrono::Utc::now().timestamp(),
        ctx.price,
        crate::smarttrend::memory_db::regime_label(ctx.regime),
    );
    rec.components = Some(cond_json);
    crate::ops::decision_log::log(rec);
}

pub(super) async fn dispatch_tournament_signals(
    state: &TitanState,
    market: &MarketDataState,
    app: &AppHandle,
    bot_tx: &tokio::sync::mpsc::Sender<BotAction>,
    symbol: &str,
    msg: &BarClosedMsg,
    atr: f64,
) -> usize {
    // Master kill-switch — when Evolution Protocol is off this loop
    // is a no-op so behaviour matches the legacy pre-S20 build.
    let enabled = state
        .config
        .lock()
        .map(|c| c.evolution_protocol.enabled)
        .unwrap_or(false);
    if !enabled {
        return 0;
    }

    // Avoid pumping signals for symbols the user has banned outright.
    let excluded = state
        .config
        .lock()
        .map(|c| c.market.excluded_symbols.clone())
        .unwrap_or_default();
    if crate::config::is_symbol_excluded(&excluded, symbol) {
        return 0;
    }

    // Freshness guard (audit 2026-08-11). History loads / replays push
    // BarClosedMsg for OLD bars through bar_tx; the FSM path renders
    // them display-only, but this path used to treat them as live —
    // observed: one week-old QBTS ignition bar (Aug 3, 14:15 ET) was
    // re-dispatched to the bot on every app restart. Replays still feed
    // the FSM/markers/shadow paths upstream — only live order dispatch
    // is gated here.
    if bar_is_stale(msg.bar_time, &msg.timeframe, chrono::Utc::now().timestamp()) {
        tracing::debug!(
            component = LOOP_NAME,
            symbol,
            tf = %msg.timeframe,
            bar_time = msg.bar_time,
            "stale bar — skipping tournament dispatch (replay/backfill)"
        );
        return 0;
    }

    let ctx = match build_market_context(market, state, symbol, msg, atr) {
        Some(c) => c,
        None => return 0,
    };

    // PRIME F5a — feed this bar's close into the shadow book so open
    // counterfactuals resolve against real subsequent prices.
    if let Some(dp) = state.dplus() {
        if let Ok(mut book) = dp.shadow.lock() {
            let n = book.on_price(symbol, ctx.price, chrono::Utc::now().timestamp());
            if n > 0 {
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    resolved = n,
                    "PRIME shadow trades resolved"
                );
            }
        }
    }

    // PRIME F1 — decision journal helper. Every veto below leaves a
    // persistent, queryable row (stage + reason) instead of only a
    // debug-level tracing line.
    let regime_str = crate::smarttrend::memory_db::regime_label(ctx.regime);
    let ctx_price = ctx.price;
    let log_veto = |source: &str, direction: &str, score: f64, stage: &str, reason: &str| {
        crate::ops::decision_log::log(crate::ops::decision_log::DecisionRecord::vetoed(
            symbol,
            &msg.timeframe,
            source,
            direction,
            score,
            stage,
            reason,
            msg.bar_time,
            ctx_price,
            regime_str,
        ));
    };

    // P1 (audit 2026-05-29) — Universe quality + anti-chase rails.
    //
    // Root cause of the 26.5% historical win rate (profit factor 0.43,
    // -$2917 over 1066 trades): the aggressive scanner feeds sub-$2
    // micro-cap gappers — the entire top-of-loss book is exactly these
    // names (MASK -$350, SOBR -$316, SNES -$235, LESL -$233, AHMA -$231,
    // ATRA -$205, OSS -$193, RXT -$164) — and strategies enter AFTER the
    // move already extended, buying the top right before mean-reversion
    // stops them out. Two strategy-agnostic rails applied here so every
    // strategy inherits them without per-strategy edits:
    //
    //   1. Price floor — skip symbols priced below MIN_PRICE_USD. On
    //      sub-$2 names the bid-ask bounce alone routinely exceeds the
    //      stop distance, guaranteeing noise stop-outs.
    //   2. Anti-chase — refuse entries that join a move already extended
    //      more than MAX_RECENT_RUNUP_PCT over RUNUP_LOOKBACK_BARS in the
    //      signal's own direction (long after a run-up / short after a
    //      sell-off). Applied per-signal inside the loop once `sig.side`
    //      is known.
    const MIN_PRICE_USD: f64 = 2.0;
    const MAX_RECENT_RUNUP_PCT: f64 = 8.0;
    const RUNUP_LOOKBACK_BARS: usize = 5;

    if ctx.price < MIN_PRICE_USD {
        tracing::debug!(
            component = LOOP_NAME,
            symbol,
            price = format!("{:.4}", ctx.price),
            floor = MIN_PRICE_USD,
            "P1 universe gate — symbol below price floor (sub-$2 micro-cap)"
        );
        log_veto("tournament", "none", 0.0, "universe", "below_price_floor");
        return 0;
    }

    // P1 (hospital) — per-symbol daily breaker. A symbol that produced
    // N consecutive losing closes today (ET) is banned for the rest of
    // the day for every playbook — the audit's top-of-loss book is
    // exactly the "re-enter the same bleeding ticker" pattern (MASK
    // −$350 over repeated entries, SOBR −$316, ...). Pure function of
    // today's journal closes; resets at the ET day boundary.
    if symbol_daily_breaker_tripped(state, symbol) {
        tracing::debug!(
            component = LOOP_NAME,
            symbol,
            "P1 symbol breaker — consecutive losses today; banned until tomorrow"
        );
        log_veto(
            "tournament",
            "none",
            0.0,
            "symbol_quarantine",
            "daily_loss_breaker",
        );
        return 0;
    }

    // Recent directional run-up/draw-down magnitude (signed %) over the
    // anti-chase lookback. Positive = price rose, negative = price fell.
    let recent_move_pct: f64 = {
        let n = ctx.candles.len();
        if n > RUNUP_LOOKBACK_BARS {
            let anchor = ctx.candles[n - 1 - RUNUP_LOOKBACK_BARS].close;
            if anchor > 0.0 {
                (ctx.price - anchor) / anchor * 100.0
            } else {
                0.0
            }
        } else {
            0.0
        }
    };

    // v2.5.0 NUCLEAR (C2) — Same-symbol direction lock.
    //
    // Strategies in the registry are independent and may emit opposite
    // directions on the same symbol within the same bar (e.g. AGL on
    // 2026-05-14 had simultaneous LONG from halt_resume and SHORT from
    // catalyst_frontrun). Alpaca rejects with HTTP 403 "cannot open a
    // long buy while a short sell order is open" — 116 such errors in
    // one session. To prevent this we lock the FIRST signal's direction
    // for this symbol on this tick and silently drop the opposite-side
    // signals from other strategies. The first strategy to fire wins;
    // ordering is the registry order (gap_fade > etf_pair > ...
    // > extended_news). A future iteration could rank by score, but the
    // registry order is stable and well-known.

    // Also pre-fetch any currently open position direction on this
    // symbol so we don't fire a signal that the broker WILL reject. This
    // is cheaper than letting handle_entry race to discover via Alpaca.
    //
    // v2.5.0 NUCLEAR (HF3) — also consult `pending_entry_dirs` so that
    // in-flight orders (not-yet-filled, not-yet-seen-by-websocket) are
    // counted as a directional lock. Without this, when `trade_updates`
    // is silent, `state.trading.positions` stays empty even after the
    // broker has accepted our entry, and the next bar would fire an
    // opposite-side signal that the broker rejects with 403 — leaking
    // the just-debited cash. See 2026-05-14 incident.
    let open_pos_dir: Option<crate::state::Direction> = {
        use rust_decimal::Decimal;
        let trading = state.trading.lock().ok();
        trading.and_then(|t| {
            let from_positions = t
                .positions
                .iter()
                .find(|p| p.symbol.eq_ignore_ascii_case(symbol) && p.qty.abs() > Decimal::ZERO)
                .map(|p| p.side);
            if from_positions.is_some() {
                return from_positions;
            }
            t.pending_entry_dirs
                .iter()
                .find(|(s, _)| s.eq_ignore_ascii_case(symbol))
                .map(|(_, d)| *d)
        })
    };

    let mut candidates = Vec::new();
    let now_unix = chrono::Utc::now().timestamp();

    // APEX-only mode (2026-07-04 audit): the 9 legacy playbooks are
    // net-negative in live paper (6/7 with samples lost; scores >75 won
    // 0%). When `apex_only` is set, only the APEX house strategy and the
    // profitable HaltResume strategy may originate live orders.
    let (apex_only, tod_enabled) = state
        .config
        .lock()
        .map(|c| {
            (
                c.evolution_protocol.apex_only,
                c.evolution_protocol.enable_tod_filter,
            )
        })
        .unwrap_or((true, true));

    for strat in strategies::registry() {
        if apex_only
            && strat.id() != strategies::StrategyId::Apex
            && strat.id() != strategies::StrategyId::HaltResume
        {
            continue;
        }
        if !strat
            .allowed_window()
            .permits(ctx.now_et_hour, ctx.now_et_minute)
        {
            continue;
        }

        let sig = match strat.evaluate_entry(&ctx) {
            Some(s) => {
                // F4-APEX — candidate row with the full condition vector
                // (all-passed), so the calibration engine sees passing
                // value distributions for tightening proposals.
                if strat.id() == strategies::StrategyId::Apex {
                    log_apex_conditions(
                        symbol,
                        &msg.timeframe,
                        &ctx,
                        s.score,
                        direction_str(s.side),
                    );
                }
                s
            }
            None => {
                // F4-APEX — near-miss rows (1–2 failed conditions) carry
                // the full vector into decisions.sqlite (Frontier
                // protocol) for sole-blocker loosening analysis.
                if strat.id() == strategies::StrategyId::Apex {
                    log_apex_conditions(symbol, &msg.timeframe, &ctx, 0.0, "none");
                }
                // v2.5.0 — observability: when a strategy is window-eligible
                // but still refuses, surface the inputs so live data
                // diagnostics can pinpoint which threshold blocked.
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    price = format!("{:.4}", ctx.price),
                    atr = format!("{:.4}", ctx.atr),
                    rvol = format!("{:.2}", ctx.rvol),
                    gap_pct = format!("{:.2}", ctx.gap_pct),
                    spread_bps = format!("{:.1}", ctx.spread_bps),
                    catalyst = ?ctx.catalyst_score,
                    is_halted = ctx.is_halted,
                    candles = ctx.candles.len(),
                    "Tournament evaluate_entry returned None"
                );
                continue;
            }
        };

        // ── UPGRADE 2: Time-of-Day (TOD) Volatility Dampener ──────────────
        //
        // Blocks new entries during the two most unprofitable windows:
        //   - Midday chop   (11:30 – 14:30 ET)
        //   - Pre-close     (15:30 – 16:00 ET)
        //
        // HaltResume is exempted from this gate because it is an opportunistic,
        // event-driven strategy that trades highly-liquid resume dynamics.
        if tod_enabled && strat.id() != strategies::StrategyId::HaltResume {
            let session = crate::smarttrend::prime::conviction::session_label(ctx.now_et_min);
            if session == "mid" || session == "close" {
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    session,
                    "[TOD_GATE] Entry blocked — midday chop / pre-close defensive window"
                );
                log_veto(
                    strat.id().as_str(),
                    direction_str(sig.side),
                    sig.score,
                    "tod_gate",
                    match session {
                        "mid" => "midday_chop_block",
                        _ => "pre_close_block",
                    },
                );
                continue;
            }
        }

        // Check if the strategy is currently in an AdaptiveRisk entry cooldown
        let in_cooldown = state
            .dplus
            .get()
            .and_then(|dp| dp.adaptive_risk.lock().ok())
            .map(|ar| ar.is_in_cooldown(strat.id(), now_unix))
            .unwrap_or(false);
        if in_cooldown {
            tracing::debug!(
                component = LOOP_NAME,
                symbol,
                strategy = %strat.id(),
                "Tournament veto — strategy is in AdaptiveRisk entry cooldown"
            );
            log_veto(
                strat.id().as_str(),
                direction_str(sig.side),
                sig.score,
                "adaptive_risk",
                "entry_cooldown",
            );
            continue;
        }

        // P1 anti-chase: block continuation signals that join an
        // already-extended move. A LONG after a >MAX_RECENT_RUNUP_PCT
        // run-up (or SHORT after an equivalent sell-off) is the dominant
        // losing pattern — buying the top before mean reversion. This
        // self-selects: mean-reversion strategies (GapFade, AfternoonFade)
        // fire on the OPPOSITE side of the move (short after run-up) so
        // they are never blocked; only momentum chasers (CatalystFrontRun,
        // HaltResume, ExtendedNews) joining the move are vetoed.
        // S29.4 — Regulatorul Adaptiv Anti-Chase pentru APEX bazat pe volatilitate (ATR)
        let effective_max_runup = if strat.id() == strategies::StrategyId::Apex {
            let atr_pct = if ctx.price > 0.0 {
                (ctx.atr / ctx.price) * 100.0
            } else {
                0.0
            };
            (4.0 * atr_pct).clamp(8.0, 25.0)
        } else {
            MAX_RECENT_RUNUP_PCT
        };

        let chasing = match sig.side {
            crate::state::Direction::Long => recent_move_pct > effective_max_runup,
            crate::state::Direction::Short => recent_move_pct < -effective_max_runup,
        };
        if chasing {
            tracing::debug!(
                component = LOOP_NAME,
                symbol,
                strategy = %strat.id(),
                signal_side = ?sig.side,
                recent_move_pct = format!("{:.2}", recent_move_pct),
                threshold = effective_max_runup,
                "P1 anti-chase veto — signal joins an already-extended move"
            );
            log_veto(
                strat.id().as_str(),
                direction_str(sig.side),
                sig.score,
                "anti_chase",
                "move_already_extended",
            );
            continue;
        }

        // v2.5.0 NUCLEAR (CB1) — Per-strategy circuit breaker.
        const HIBERNATE_LOSS_STREAK: i32 = 3;
        let circuit_breaker_tripped = state
            .dplus
            .get()
            .and_then(|dp| dp.tournament.lock().ok())
            .and_then(|t| t.stats.get(&strat.id()).cloned())
            .map(|stats| stats.current_streak_len <= -HIBERNATE_LOSS_STREAK)
            .unwrap_or(false);
        if circuit_breaker_tripped {
            tracing::debug!(
                component = LOOP_NAME,
                symbol,
                strategy = %strat.id(),
                threshold = HIBERNATE_LOSS_STREAK,
                "Tournament veto — circuit breaker tripped (≥3 consecutive losses)"
            );
            log_veto(
                strat.id().as_str(),
                direction_str(sig.side),
                sig.score,
                "circuit_breaker",
                "loss_streak",
            );
            continue;
        }

        // ── UPGRADE 1: OFI (Order Flow Imbalance) Hard Gate ──────────────
        //
        // Reads the live NBBO imbalance ratio computed by the streamer from
        // real-time Alpaca SIP quote deltas. The imbalance_ratio ∈ [-1, +1]:
        //   positive = more bid pressure (buyers dominate) → favours Long
        //   negative = more ask pressure (sellers dominate) → favours Short
        //
        // Gate: if the order flow OPPOSES the signal direction beyond the
        // threshold (default -0.20), an institutional iceberg is likely
        // absorbing our entry on the wrong side. Block and log.
        //
        // Only applied when OFI state has been updated (update_count > 0);
        // at cold-start (no quote yet) we give benefit of the doubt.
        // F0: ofi_gate is OFF by default — global ofi_state is chart-only
        // and contaminates every other symbol. Re-enable via
        // `evolution_protocol.ofi_gate_enabled` only after F1 MicroState.
        const OFI_THRESHOLD: f64 = -0.20;
        let ofi_gate_on = state
            .config
            .lock()
            .map(|c| c.evolution_protocol.ofi_gate_enabled)
            .unwrap_or(false);
        if ofi_gate_on {
            // F1: prefer per-symbol MicroBook imbalance; fail-closed
            // (skip gate, do not invent) when micro missing/stale.
            let ratio_opt = {
                let book = market.micro.lock().unwrap_or_else(|p| p.into_inner());
                book.get(symbol).and_then(|s| {
                    if s.fresh && s.update_count > 0 {
                        Some(s.imbalance)
                    } else {
                        None
                    }
                })
            };
            if let Some(ratio) = ratio_opt {
                let ofi_adverse = match sig.side {
                    crate::state::Direction::Long => ratio < OFI_THRESHOLD,
                    crate::state::Direction::Short => ratio > -OFI_THRESHOLD,
                };
                if ofi_adverse {
                    tracing::info!(
                        component = LOOP_NAME,
                        symbol,
                        strategy = %strat.id(),
                        signal_side = ?sig.side,
                        imbalance_ratio = format!("{:.3}", ratio),
                        threshold = OFI_THRESHOLD,
                        "[OFI_GATE] Entry blocked — per-symbol micro imbalance opposes signal"
                    );
                    log_veto(
                        strat.id().as_str(),
                        direction_str(sig.side),
                        sig.score,
                        "ofi_gate",
                        "adverse_order_flow_imbalance",
                    );
                    continue;
                }
            }
        }

        // ── UPGRADE 3: RVOL Minimum Gate ──────────────────────────────────
        //
        // Only dispatch signals on symbols that have meaningful relative
        // volume (RVOL ≥ 1.5×). Low-RVOL symbols (RVOL < 1.5) are "dead"
        // tapes with insufficient institutional participation — breakout
        // signals on these names fail at a much higher rate because there
        // is no real buying interest behind the move.
        //
        // RVOL is sourced from ctx (filled by the scanner result or the
        // local candle-based fallback). The gate is bypassed if the ctx
        // was built from the cold-start fallback (rvol == 1.0 exactly and
        // the scanner is stale) to avoid blocking legitimate premarket
        // signals before the first watchlist refresh completes.
        const MIN_RVOL: f64 = 1.5;
        if ctx.rvol < MIN_RVOL {
            tracing::debug!(
                component = LOOP_NAME,
                symbol,
                strategy = %strat.id(),
                rvol = format!("{:.2}", ctx.rvol),
                min_rvol = MIN_RVOL,
                "[RVOL_GATE] Entry blocked — insufficient relative volume (dead tape)"
            );
            log_veto(
                strat.id().as_str(),
                direction_str(sig.side),
                sig.score,
                "rvol_gate",
                "insufficient_rvol",
            );
            continue;
        }

        candidates.push((strat, sig));
    }

    // PRIME F2d — conviction arbitration replaces the old
    // "sort by raw score, first strategy claims the direction" logic.
    //
    // All surviving candidates are fused through the conviction engine
    // (signal strength × playbook reliability × regime fit × catalyst ×
    // rvol − spread penalty). PRIME picks at most ONE winner per symbol
    // per bar: the highest-conviction candidate on the dominant side.
    // Everything else is vetoed with a journaled reason
    // (`conviction_conflict` / `below_threshold` / `outranked`).
    //
    // When PRIME is disabled (settings) or dplus is not yet initialised
    // the legacy score-sort + first-wins path runs unchanged.
    // PRIME F5d — signed causal-confluence alignment in [-1, 1].
    // Positive = this symbol's lead-lag leaders just moved bullishly
    // (correlation-weighted), negative = bearishly. Long candidates
    // earn the positive part as a conviction bonus, shorts the
    // negative part. Same math as the Bayesian `p_causal` in
    // `bot/entry.rs`, remapped from probability to alignment
    // (`2p − 1`). Computed once per symbol per bar, before the prime
    // guard is taken (dplus mutexes are standalone — never overlapped).
    let causal_alignment: f64 = if candidates.is_empty() {
        0.0
    } else {
        state
            .dplus()
            .map(|dp| {
                let causal = dp.causal.lock().unwrap_or_else(|p| p.into_inner());
                let best = causal.leaders_for(symbol).into_iter().max_by(|a, b| {
                    a.corr
                        .abs()
                        .partial_cmp(&b.corr.abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                match best {
                    Some(edge) => match causal.recent_change(&edge.leader, 10) {
                        Some(change) => {
                            let k = 1000.0;
                            let p = 1.0 / (1.0 + (-k * change * edge.corr).exp());
                            (2.0 * p - 1.0).clamp(-1.0, 1.0)
                        }
                        None => 0.0,
                    },
                    None => 0.0,
                }
            })
            .unwrap_or(0.0)
    };

    let prime_rulings: Option<Vec<crate::smarttrend::prime::Ruling>> =
        state.dplus().and_then(|dp| {
            let engine = dp.prime.lock().ok()?;
            if !engine.enabled {
                return None;
            }
            let prime_candidates: Vec<crate::smarttrend::prime::Candidate> = candidates
                .iter()
                .map(|(strat, sig)| crate::smarttrend::prime::Candidate {
                    source: strat.id().as_str().to_string(),
                    side_long: sig.side == crate::state::Direction::Long,
                    raw_score: sig.score,
                })
                .collect();
            // P5 — realized execution-cost prior: a symbol that keeps
            // filling adverse pays its measured slippage on top of the
            // quoted spread in the fusion's cost term.
            let slip_prior = crate::ops::slippage::recent_bps(symbol, now_unix);
            Some(engine.decide(
                &prime_candidates,
                ctx.regime,
                ctx.catalyst_score,
                ctx.rvol,
                ctx.spread_bps + slip_prior,
                causal_alignment,
                // P2 — session bucket for context-conditional
                // reliability (playbook|regime|session).
                crate::smarttrend::prime::conviction::session_label(ctx.now_et_min),
            ))
        });

    // Finalists carry their fused conviction; legacy path carries None.
    let mut finalists: Vec<(
        &'static Box<dyn strategies::Strategy>,
        strategies::EntrySignal,
        Option<crate::smarttrend::prime::Conviction>,
    )> = Vec::new();

    match prime_rulings {
        Some(rulings) => {
            for ((strat, sig), ruling) in candidates.into_iter().zip(rulings) {
                let mut final_ruling = ruling;
                let is_volatile_regime = matches!(
                    ctx.regime,
                    Some(crate::smarttrend::strategies::MarketRegime::Volatile)
                        | Some(crate::smarttrend::strategies::MarketRegime::Panic)
                );
                if strat.id() == strategies::StrategyId::Apex && is_volatile_regime {
                    if let crate::smarttrend::prime::Ruling::Veto {
                        conviction: _,
                        reason,
                    } = &final_ruling
                    {
                        tracing::info!(
                            component = LOOP_NAME,
                            symbol,
                            reason,
                            "S29.4 Elite Upgrade 2: PRIME conviction veto bypassed for APEX in volatile/panic regime"
                        );
                        final_ruling = crate::smarttrend::prime::Ruling::Enter(
                            crate::smarttrend::prime::Conviction {
                                value: 1.0,
                                signal: 1.0,
                                reliability: 1.0,
                                regime_fit: 1.0,
                                catalyst: 1.0,
                                rvol: 1.0,
                                spread_penalty: 0.0,
                                causal_bonus: 0.0,
                                norm: [1.0; 7],
                            },
                        );
                    }
                }

                match final_ruling {
                    crate::smarttrend::prime::Ruling::Enter(cv) => {
                        finalists.push((strat, sig, Some(cv)));
                    }
                    crate::smarttrend::prime::Ruling::Veto { conviction, reason } => {
                        tracing::debug!(
                            component = LOOP_NAME,
                            symbol,
                            strategy = %strat.id(),
                            conviction = format!("{:.3}", conviction.value),
                            threshold_reason = reason,
                            "PRIME veto — candidate lost the conviction arbitration"
                        );
                        // P3 — PRIME-stage vetoes went through fusion,
                        // so the row carries the conviction AND the
                        // normalized component vector for the
                        // Counterfactual Lab.
                        crate::ops::decision_log::log(
                            crate::ops::decision_log::DecisionRecord::vetoed(
                                symbol,
                                &msg.timeframe,
                                strat.id().as_str(),
                                direction_str(sig.side),
                                sig.score,
                                "prime",
                                reason,
                                msg.bar_time,
                                ctx_price,
                                regime_str,
                            )
                            .with_conviction(conviction.value)
                            .with_components(&conviction.norm),
                        );
                        emit_live_path_marker(
                            state,
                            app,
                            symbol,
                            msg,
                            strat.id().as_str(),
                            None,
                            sig.side,
                            ctx_price,
                            sig.score,
                            Some(conviction.value),
                            "VETO",
                            true,
                        );
                        // F5a — track the vetoed signal as a shadow
                        // trade so we can measure what this veto cost
                        // (or saved) once price resolves it.
                        if let Some(dp) = state.dplus() {
                            if let Ok(mut book) = dp.shadow.lock() {
                                book.open(crate::smarttrend::prime::shadow::ShadowTrade {
                                    symbol: symbol.to_string(),
                                    source: strat.id().as_str().to_string(),
                                    veto_stage: "prime".to_string(),
                                    veto_reason: reason.to_string(),
                                    side_long: sig.side == crate::state::Direction::Long,
                                    entry_price: ctx.price,
                                    stop: sig.stop_loss,
                                    target: sig.take_profit,
                                    opened_ts: now_unix,
                                    max_hold_minutes: sig.max_hold_minutes,
                                });
                            }
                        }
                    }
                }
            }
        }
        None => {
            // Legacy: sort by raw score descending, dispatch all.
            candidates.sort_by(|a, b| {
                b.1.score
                    .partial_cmp(&a.1.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            finalists.extend(
                candidates
                    .into_iter()
                    .map(|(strat, sig)| (strat, sig, None::<crate::smarttrend::prime::Conviction>)),
            );
        }
    }

    let mut locked_direction: Option<crate::state::Direction> = None;
    let mut dispatched = 0usize;

    for (strat, sig, conviction) in finalists {
        // C2 direction-lock: respect any pre-existing position.
        if let Some(pos_dir) = open_pos_dir {
            if pos_dir != sig.side {
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    signal_side = ?sig.side,
                    position_side = ?pos_dir,
                    "Tournament signal dropped — opposite of open position (would be rejected by broker)"
                );
                log_veto(
                    strat.id().as_str(),
                    direction_str(sig.side),
                    sig.score,
                    "direction_lock",
                    "opposite_open_position",
                );
                continue;
            }
        }

        // C2 direction-lock: first strategy this tick claims the side.
        match locked_direction {
            None => locked_direction = Some(sig.side),
            Some(d) if d == sig.side => {} // same side, allow concurrent
            Some(d) => {
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    signal_side = ?sig.side,
                    locked = ?d,
                    "Tournament signal dropped — opposite side already locked this tick"
                );
                log_veto(
                    strat.id().as_str(),
                    direction_str(sig.side),
                    sig.score,
                    "direction_lock",
                    "opposite_side_locked_this_tick",
                );
                continue;
            }
        }

        // Tournament sub-wallet checks
        let (sub_wallet_ok, needs_topup) = state
            .dplus
            .get()
            .and_then(|dp| dp.tournament.lock().ok())
            .and_then(|t| t.stats.get(&strat.id()).cloned())
            .map(|stats| {
                use crate::smarttrend::tournament::StrategyStatus;
                let active = matches!(stats.status, StrategyStatus::Active);
                let has_cash = stats.current_sub_wallet > rust_decimal::Decimal::ZERO;
                (active && has_cash, active && !has_cash)
            })
            .unwrap_or((true, false));

        if needs_topup {
            let injected = state
                .dplus
                .get()
                .and_then(|dp| dp.tournament.lock().ok())
                .and_then(|mut t| t.topup_strategy_from_pool(strat.id()));
            match injected {
                Some(amount) => {
                    tracing::warn!(
                        component = LOOP_NAME,
                        symbol,
                        strategy = %strat.id(),
                        topup = %amount,
                        "P0-A sub-wallet top-up from free_pool — strategy resurrected"
                    );
                }
                None => {
                    tracing::debug!(
                        component = LOOP_NAME,
                        symbol,
                        strategy = %strat.id(),
                        "Tournament veto — sub-wallet exhausted, free_pool empty (no phantom top-up)"
                    );
                    log_veto(
                        strat.id().as_str(),
                        direction_str(sig.side),
                        sig.score,
                        "capital",
                        "sub_wallet_exhausted",
                    );
                    continue;
                }
            }
        } else if !sub_wallet_ok {
            tracing::debug!(
                component = LOOP_NAME,
                symbol,
                strategy = %strat.id(),
                "Tournament veto — strategy not Active (Draining/Dead)"
            );
            log_veto(
                strat.id().as_str(),
                direction_str(sig.side),
                sig.score,
                "capital",
                "strategy_not_active",
            );
            continue;
        }

        // v2.5.0 NUCLEAR (CB3) — Anti-spam dispatch governance.
        const COOLDOWN_SECS_PER_SYMBOL: i64 = 300; // 5 minutes
        const MAX_DAILY_ENTRIES_PER_STRATEGY: u32 = 50;
        let now_et_day = chrono::Utc::now()
            .with_timezone(&chrono_tz::US::Eastern)
            .date_naive()
            .num_days_from_ce() as i64;
        let claim_outcome = state
            .dplus
            .get()
            .and_then(|dp| dp.tournament.lock().ok())
            .map(|mut t| {
                t.reset_daily_counters_if_new_day(now_et_day);
                t.try_claim_dispatch(
                    strat.id(),
                    symbol,
                    now_unix,
                    COOLDOWN_SECS_PER_SYMBOL,
                    MAX_DAILY_ENTRIES_PER_STRATEGY,
                )
            });
        match claim_outcome {
            Some(Ok(())) => {}
            Some(Err(reason)) => {
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    reason,
                    "Tournament signal dropped — anti-spam governance (CB3)"
                );
                log_veto(
                    strat.id().as_str(),
                    direction_str(sig.side),
                    sig.score,
                    "anti_spam",
                    reason,
                );
                continue;
            }
            None => {
                continue;
            }
        }

        let side_str = if sig.side == crate::state::Direction::Long {
            "long"
        } else {
            "short"
        };
        let vix = {
            let prices = market.last_prices.lock().unwrap();
            prices
                .get("VIX")
                .or_else(|| prices.get("$VIX"))
                .copied()
                .unwrap_or(0.0)
        };
        let (has_news, has_earnings) = {
            let wm = market.watchlist_metrics.lock().unwrap();
            if let Some(m) = wm.get(&ctx.symbol) {
                (m.catalyst_flags.news_recent, m.catalyst_flags.earnings)
            } else {
                (false, false)
            }
        };

        let setup_features = Some(crate::smarttrend::memory_db::features_from_ctx(
            &ctx,
            sig.strategy,
            side_str,
            vix,
            "unknown".to_string(), // Sesiunea 3: Sector defaulting to unknown to avoid heavy lookup overhead
            has_news,
            has_earnings,
        ));

        let action = BotAction {
            symbol: ctx.symbol.clone(),
            kind: BotActionKind::Entry,
            direction: sig.side,
            score: sig.score,
            bar_time: msg.bar_time,
            price: ctx.price,
            timeframe: msg.timeframe.clone(),
            strategy_id: Some(sig.strategy),
            stop_loss: Some(
                rust_decimal::Decimal::from_f64_retain(sig.stop_loss)
                    .unwrap_or(rust_decimal::Decimal::ZERO),
            ),
            take_profit: Some(
                rust_decimal::Decimal::from_f64_retain(sig.take_profit)
                    .unwrap_or(rust_decimal::Decimal::ZERO),
            ),
            max_hold_minutes: Some(sig.max_hold_minutes),
            setup_features,
            conviction: conviction.as_ref().map(|cv| cv.value),
        };

        // Dry-run Monte Carlo simulation for shadow testing
        {
            let side: f64 = match sig.side {
                crate::state::Direction::Long => 1.0,
                crate::state::Direction::Short => -1.0,
            };
            let mc_inputs = crate::ml::monte_carlo::build_inputs_from_entry(
                ctx.price,
                ctx.atr,
                sig.take_profit,
                sig.stop_loss,
                side,
                crate::ml::monte_carlo::DEFAULT_ROUND_TRIP_BPS,
            );
            let mc_cfg = crate::ml::monte_carlo::GbmConfig::default();
            let mc_res = crate::ml::monte_carlo::simulate(&mc_inputs, &mc_cfg);
            let mc_gate = {
                let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
                let ep = &cfg.evolution_protocol;
                crate::ml::monte_carlo::McGate {
                    min_expected_pct: ep.mc_min_expected_pct,
                    max_p_loss: ep.mc_max_p_loss,
                    shrink_p_hit_target: Some(ep.mc_shrink_p_hit_target),
                    shrink_floor: ep.mc_shrink_floor,
                }
            };
            let mc_decision = mc_gate.evaluate(&mc_res);

            // Log shadow decision
            tracing::info!(
                component = "shadow_mc",
                symbol = %ctx.symbol,
                strategy = %strat.id(),
                decision = ?mc_decision,
                mean_return = mc_res.mean_return,
                p_loss = mc_res.p_loss,
                p_hit_target = mc_res.p_hit_target,
                p_hit_stop = mc_res.p_hit_stop,
                "Monte Carlo SHADOW/Dry-Run decision evaluated"
            );

            // Record to decision log as a vetoed row with stage = "shadow_mc" if not accepted
            match mc_decision {
                crate::ml::monte_carlo::McDecision::Veto { reason } => {
                    let log_reason = format!("Veto: {}", reason);
                    crate::ops::decision_log::log(
                        crate::ops::decision_log::DecisionRecord::vetoed(
                            &ctx.symbol,
                            &msg.timeframe,
                            strat.id().as_str(),
                            direction_str(sig.side),
                            sig.score,
                            "shadow_mc",
                            &log_reason,
                            msg.bar_time,
                            ctx.price,
                            regime_str,
                        ),
                    );
                }
                crate::ml::monte_carlo::McDecision::Shrink { factor, reason } => {
                    let log_reason = format!("Shrink (factor={:.2}): {}", factor, reason);
                    crate::ops::decision_log::log(
                        crate::ops::decision_log::DecisionRecord::vetoed(
                            &ctx.symbol,
                            &msg.timeframe,
                            strat.id().as_str(),
                            direction_str(sig.side),
                            sig.score,
                            "shadow_mc",
                            &log_reason,
                            msg.bar_time,
                            ctx.price,
                            regime_str,
                        ),
                    );
                }
                crate::ml::monte_carlo::McDecision::Accept => {}
            }
        }

        match bot_tx.try_send(action) {
            Ok(_) => {
                dispatched += 1;
                // P3 — entered rows carry the normalized fusion
                // components (when PRIME arbitrated) for the
                // Counterfactual Lab's offline re-fusion.
                // Queue acceptance is not a broker fill. The bot emits
                // the authoritative `entered` row only after its
                // execution router confirms a non-zero filled quantity.
                let mut rec = crate::ops::decision_log::DecisionRecord::dispatched(
                    symbol,
                    &msg.timeframe,
                    strat.id().as_str(),
                    direction_str(sig.side),
                    sig.score,
                    conviction.as_ref().map(|cv| cv.value),
                    msg.bar_time,
                    ctx.price,
                    regime_str,
                );
                if let Some(cv) = conviction.as_ref() {
                    rec = rec.with_components(&cv.norm);
                }
                crate::ops::decision_log::log(rec);
                // F3b — live-path chart marker (APX / other strategy GO).
                emit_live_path_marker(
                    state,
                    app,
                    symbol,
                    msg,
                    strat.id().as_str(),
                    None,
                    sig.side,
                    ctx.price,
                    sig.score,
                    conviction.as_ref().map(|cv| cv.value),
                    "GO",
                    false,
                );
                tracing::info!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    side = ?sig.side,
                    score = format!("{:.1}", sig.score),
                    price = format!("{:.4}", ctx.price),
                    sl = format!("{:.4}", sig.stop_loss),
                    tp = format!("{:.4}", sig.take_profit),
                    reason = sig.reason,
                    "Tournament ENTRY signal dispatched"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(action)) => {
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    "S32 bot channel full — retrying with 250ms timeout"
                );
                match tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    bot_tx.send(action),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        dispatched += 1;
                        tracing::info!(
                            component = LOOP_NAME,
                            symbol,
                            strategy = %strat.id(),
                            "S32 Tournament ENTRY dispatched after backpressure wait"
                        );
                    }
                    Ok(Err(_)) | Err(_) => {
                        market.bot_actions_dropped.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            component = LOOP_NAME,
                            symbol,
                            strategy = %strat.id(),
                            "Tournament signal dropped — bot channel saturated >250ms"
                        );
                    }
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                market.bot_actions_dropped.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    component = LOOP_NAME,
                    symbol,
                    strategy = %strat.id(),
                    "Tournament signal dropped — bot channel CLOSED (consumer dead)"
                );
            }
        }
    }

    dispatched
}

/// F3b — push a live-path marker (APEX/Frontier/tournament) + emit alert.
#[allow(clippy::too_many_arguments)]
fn emit_live_path_marker(
    state: &TitanState,
    app: &AppHandle,
    symbol: &str,
    msg: &BarClosedMsg,
    source: &str,
    setup: Option<&str>,
    side: crate::state::Direction,
    price: f64,
    score: f64,
    conviction: Option<f64>,
    verdict: &str,
    display_only: bool,
) {
    let mut marker = create_marker(
        MarkerKind::Entry,
        side,
        msg.bar_time,
        price,
        score,
        &msg.timeframe,
        symbol,
    );
    marker.display_only = display_only;
    marker.source = Some(source.to_string());
    marker.setup = setup.map(|s| s.to_string());
    marker.conviction = conviction;
    marker.verdict = Some(verdict.to_string());

    let payload = {
        let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.markers.push(marker.clone());
        serde_json::json!({
            "symbol": symbol,
            "phase": "LivePath",
            "prev_phase": "Idle",
            "score": score,
            "direction": format!("{:?}", side),
            "bar_time": msg.bar_time,
            "timeframe": "any",
            "markers": [{
                "kind": "Entry",
                "direction": format!("{:?}", side),
                "price": price,
                "score": score,
                "bar_time": msg.bar_time,
                "display_only": display_only,
                "source": source,
                "setup": setup,
                "conviction": conviction,
                "verdict": verdict,
            }]
        })
    };
    let _ = app.emit_all("smarttrend-alert", &payload);
}

#[cfg(test)]
mod tests {
    use super::bar_is_stale;

    #[test]
    fn fresh_bar_is_not_stale() {
        let now = 1_700_000_000_i64;
        // 5m bar that closed 30s ago.
        assert!(!bar_is_stale(now - 300 - 30, "5m", now));
        // 1m bar that closed 90s ago (inside the 120s floor).
        assert!(!bar_is_stale(now - 60 - 90, "1m", now));
    }

    #[test]
    fn replayed_history_bar_is_stale() {
        let now = 1_700_000_000_i64;
        // The observed incident: a week-old 5m bar replayed at startup.
        assert!(bar_is_stale(now - 7 * 86_400, "5m", now));
        // 1m bar that closed 3 minutes ago — beyond max(2×60, 120).
        assert!(bar_is_stale(now - 60 - 180, "1m", now));
        // 5m bar that closed 11 minutes ago — beyond 2×300.
        assert!(bar_is_stale(now - 300 - 660, "5m", now));
    }
}
