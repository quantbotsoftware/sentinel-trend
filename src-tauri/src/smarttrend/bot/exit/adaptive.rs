use std::sync::Arc;

use rust_decimal::Decimal;
use tauri::{AppHandle, Manager};

use crate::execution::order_exec::{
    has_open_close_order, send_order, ExecutionMetrics, OrderRequest,
};
use crate::state::{Direction, TitanState, TradingMode, PENDING_EXIT_TIMEOUT_SECS};
use crate::smarttrend::bot::loop_run::{opposite_direction, BotMetrics};
use crate::smarttrend::bot_safety::ExitReason;
use crate::smarttrend::bot::BotAction;
use crate::smarttrend::bot_safety::BotActionKind;

use super::{alpaca_side, local_position_still_open, release_pending_exit_gate, handle_exit};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveExitParams {
    pub quick_cut_loss_pct: f64,
    pub quick_cut_min_hold_secs: i64,
    pub profit_take_pct: f64,
    pub profit_take_min_hold_secs: i64,
}

/// Pure decision: given a position's held duration and unrealized P/L
/// percentage (relative to invested cost basis), return the exit
/// reason if any adaptive rail should fire.
///
/// Both rails are off by default (`*_pct == 0.0`). When both fire on
/// the same tick (e.g. 0% threshold), profit-take wins on positive
/// P/L and quick-cut wins on negative P/L; the price sign is the
/// arbiter.
pub fn evaluate_adaptive_exit(
    held_secs: i64,
    pnl_pct: f64,
    p: &AdaptiveExitParams,
) -> Option<ExitReason> {
    if !pnl_pct.is_finite() || held_secs <= 0 {
        return None;
    }

    if p.quick_cut_loss_pct > 0.0
        && held_secs >= p.quick_cut_min_hold_secs
        && pnl_pct <= -p.quick_cut_loss_pct.abs()
    {
        return Some(ExitReason::QuickCut);
    }

    if p.profit_take_pct > 0.0
        && held_secs >= p.profit_take_min_hold_secs
        && pnl_pct >= p.profit_take_pct
    {
        return Some(ExitReason::ProfitTake);
    }

    None
}

/// Pure decision logic for Trailing Stop: tracks peak P&L per symbol and triggers exit
/// when P&L falls by deviation percentage from the peak.
pub fn evaluate_trailing_stop(
    symbol: &str,
    held_secs: i64,
    pnl_pct: f64,
    ts_activation: f64,
    ts_deviation: f64,
    ts_min_hold_secs: i64,
    peak_pnl_pcts: &mut std::collections::HashMap<String, f64>,
) -> Option<ExitReason> {
    if !pnl_pct.is_finite() || held_secs < ts_min_hold_secs || ts_activation <= 0.0 || ts_deviation <= 0.0 {
        return None;
    }
    let has_activated = peak_pnl_pcts.contains_key(symbol) || pnl_pct >= ts_activation;
    if !has_activated {
        return None;
    }
    let peak = peak_pnl_pcts
        .entry(symbol.to_string())
        .or_insert(pnl_pct);
    if pnl_pct > *peak {
        *peak = pnl_pct;
    }
    if *peak >= ts_activation && pnl_pct <= (*peak - ts_deviation) {
        return Some(ExitReason::TrailingStop);
    }
    None
}

/// PRIME F2c — asymmetric exit ladder, live-wired.
///
/// The broker bracket stop cannot be moved cheaply, so the ladder is
/// recomputed statelessly every tick from the entry, the bracket stop
/// (captured at entry, P0-B) and the favorable extreme (tracked in
/// `peak_pnl_pcts` under a `prime:`-prefixed key so the legacy
/// percent-trailing rail's activation logic is untouched). When price
/// crosses a ladder stop that is *tighter* than the bracket
/// (breakeven after +1R, 0.75R trail after +1.5R) the rail exits at
/// market with [`ExitReason::PrimeLadder`]. Crossings of the original
/// bracket stop itself stay the broker's job.
pub struct PrimeLadderInput<'a> {
    pub symbol: &'a str,
    pub is_long: bool,
    pub entry: f64,
    pub bracket_stop: f64,
    pub price: f64,
    pub pnl_pct: f64,
}

pub fn evaluate_prime_ladder(
    policy: &crate::smarttrend::prime::ExitPolicy,
    input: &PrimeLadderInput<'_>,
    peak_pnl_pcts: &mut std::collections::HashMap<String, f64>,
) -> Option<ExitReason> {
    let PrimeLadderInput {
        symbol,
        is_long,
        entry,
        bracket_stop,
        price,
        pnl_pct,
    } = *input;
    if entry <= 0.0 || bracket_stop <= 0.0 || !price.is_finite() || price <= 0.0 {
        return None;
    }
    if !pnl_pct.is_finite() {
        return None;
    }
    let initial_risk = (entry - bracket_stop).abs();
    if initial_risk <= entry * 1e-4 {
        return None;
    }

    // P0 Asymmetry Protocol — a deferred signal exit arms an immediate
    // breakeven floor (flag key set by the sentinel dispatch when it
    // suppresses an FSM exit on a winner). Read before the mutable
    // peak entry below.
    let be_armed = peak_pnl_pcts.contains_key(&format!("prime:be:{symbol}"));

    // Favorable extreme from the peak unrealized P/L percentage
    // (relative to cost basis == price-move % of entry).
    let key = format!("prime:{symbol}");
    let peak = peak_pnl_pcts.entry(key).or_insert(pnl_pct);
    if pnl_pct > *peak {
        *peak = pnl_pct;
    }
    let favorable_extreme = if is_long {
        entry * (1.0 + *peak / 100.0)
    } else {
        entry * (1.0 - *peak / 100.0)
    };

    // Trail distance: 0.75R behind the extreme (trail_atr_mult 1.5 ×
    // half the R unit) — tight enough to bank +2R runners, loose
    // enough to survive one-bar noise.
    let mut stop = crate::smarttrend::prime::exits::ladder_stop(
        policy,
        is_long,
        entry,
        bracket_stop,
        favorable_extreme,
        0.5 * initial_risk,
    );
    // Armed breakeven (P0): the position already proved itself (a
    // signal exit fired above defer_min_r) — it must never turn into
    // a loser, regardless of whether the peak reached breakeven_at_r.
    if be_armed {
        stop = if is_long { stop.max(entry) } else { stop.min(entry) };
    }
    let improved = if is_long {
        stop > bracket_stop + 1e-9
    } else {
        stop < bracket_stop - 1e-9
    };
    let crossed = if is_long { price <= stop } else { price >= stop };
    if improved && crossed {
        return Some(ExitReason::PrimeLadder);
    }
    None
}

pub async fn check_tournament_exits(
    state: &Arc<TitanState>,
    market: &Arc<crate::market::market_state::MarketDataState>,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };
    if mode != TradingMode::Auto {
        return;
    }

    let now_utc = chrono::Utc::now();
    let now_et = now_utc.with_timezone(&chrono_tz::US::Eastern);
    use chrono::Timelike;
    let now_et_min = now_et.hour() * 60 + now_et.minute();

    let excluded_for_filter: Vec<String> = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.market.excluded_symbols.clone()
    };

    let candidates = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let pending = &trading.pending_exits;
        
        trading.positions.iter()
            .filter(|p| p.qty.abs() > Decimal::ZERO)
            .filter(|p| !pending.contains_key(&p.symbol))
            .filter(|p| !crate::config::is_symbol_excluded(&excluded_for_filter, &p.symbol))
            .filter_map(|p| {
                crate::smarttrend::dplus::parse_strategy_from_source(&p.source).map(|sid| (p.clone(), sid))
            })
            .collect::<Vec<_>>()
    };

    if candidates.is_empty() {
        return;
    }

    for (pos, sid) in candidates {
        let Some(strategy) = crate::smarttrend::strategies::by_id(sid) else {
            continue;
        };

        let ctx = {
            let buf_key = format!("{}:1m", pos.symbol);
            let candles = {
                let buffers = market.candle_buffers.lock().unwrap_or_else(|p| p.into_inner());
                buffers.get(&buf_key).map(|b| b.snapshot()).unwrap_or_default()
            };
            if candles.is_empty() {
                continue;
            }
            
            let price = candles.last().unwrap().close;
            let atr = crate::smarttrend::strategies::pure_atr(&candles, 14);
            
            let companion = crate::smarttrend::sentinel::tournament_dispatch::resolve_companion_snapshot(
                market,
                &pos.symbol,
            );
            
            let overrides = state.dplus().map(|dp| dp.overrides.snapshot()).unwrap_or_default();
            
            crate::smarttrend::strategies::MarketContext {
                symbol: pos.symbol.clone(),
                timeframe: "1m",
                price,
                prev_close: price,
                session_open: price,
                candles,
                atr,
                rvol: 1.0,
                gap_pct: 0.0,
                spread_bps: 0.0,
                catalyst_score: None,
                now_et_min,
                now_et_hour: now_et.hour(),
                now_et_minute: now_et.minute(),
                regime: None,
                is_halted: false,
                companion,
                float_shares: None,
                news_sentiment: None,
                overrides,
            }
        };

        if let Some(exit_signal) = strategy.evaluate_exit(&pos, &ctx) {
            tracing::info!(
                component = "bot",
                symbol = %pos.symbol,
                strategy = %sid,
                reason = exit_signal.reason,
                "Tournament exit signal received"
            );

            // Fix 2026-08-11 (two stacked bugs — this path never
            // executed an exit before):
            //   1. Pre-inserting `pending_exits` here made
            //      `handle_exit`'s own gate see the key and skip with
            //      "already in flight". The gate is OWNED by
            //      `handle_exit`; callers must not pre-set it.
            //   2. `direction` carried the close side, but
            //      `local_position_still_open` matches the POSITION
            //      side — even without (1) the exit would abort.
            //      `handle_exit` derives the close side itself at
            //      submit time.
            let action = BotAction {
                symbol: pos.symbol.clone(),
                kind: BotActionKind::Exit(ExitReason::SignalExit),
                direction: pos.side,
                score: 100.0,
                bar_time: now_utc.timestamp(),
                price: ctx.price,
                timeframe: "1m".to_string(),
                strategy_id: Some(sid),
                stop_loss: None,
                take_profit: None,
                max_hold_minutes: None,
                setup_features: None,
                conviction: None,
            };

            // Directly handle the exit
            handle_exit(state, &action, &ExitReason::SignalExit, app, metrics, exec_metrics).await;
        }
    }
}

pub async fn check_adaptive_exits(
    state: &Arc<TitanState>,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    let (params, ts_activation, ts_deviation, ts_min_hold_secs) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            AdaptiveExitParams {
                quick_cut_loss_pct: cfg.smarttrend.quick_cut_loss_pct,
                quick_cut_min_hold_secs: i64::from(cfg.smarttrend.quick_cut_min_hold_secs),
                profit_take_pct: cfg.smarttrend.profit_take_pct,
                profit_take_min_hold_secs: i64::from(cfg.smarttrend.profit_take_min_hold_secs),
            },
            cfg.smarttrend.trailing_stop_activation_pct,
            cfg.smarttrend.trailing_stop_deviation_pct,
            i64::from(cfg.smarttrend.trailing_stop_min_hold_secs),
        )
    };

    // Bail early when both rails are disabled — keeps the hot tick
    // free of unnecessary lock acquisition + iteration.
    if params.quick_cut_loss_pct <= 0.0
        && params.profit_take_pct <= 0.0
        && ts_activation <= 0.0
        && ts_deviation <= 0.0
    {
        return;
    }

    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };
    if mode != TradingMode::Auto {
        return;
    }

    let now = chrono::Utc::now().timestamp();

    let excluded_for_filter: Vec<String> = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.market.excluded_symbols.clone()
    };

    // PRIME F2c — snapshot the exit policy BEFORE taking the trading
    // guard (the `prime` mutex is standalone; never held together with
    // Tier-A/Tier-B guards).
    let prime_exit_policy: Option<crate::smarttrend::prime::ExitPolicy> = state
        .dplus()
        .and_then(|dp| dp.prime.lock().ok().map(|p| (p.enabled, p.exit_policy)))
        .and_then(|(enabled, pol)| if enabled { Some(pol) } else { None });
    let candidates: Vec<(String, Direction, Decimal, ExitReason, f64, i64)> = {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        // Same eviction policy as check_max_hold_positions to keep the
        // pending_exits map small and consistent.
        trading
            .pending_exits
            .retain(|_, ts| now.saturating_sub(*ts) < PENDING_EXIT_TIMEOUT_SECS);
        let pending = trading.pending_exits.clone();

        let active_symbols: std::collections::HashSet<String> = trading
            .positions
            .iter()
            .map(|p| p.symbol.clone())
            .collect();
        // Evict legacy keys, PRIME `prime:`-prefixed high-water keys
        // and P0 `prime:be:`-prefixed armed-breakeven flags once the
        // position is gone.
        trading.peak_pnl_pcts.retain(|sym, _| {
            let bare = sym
                .strip_prefix("prime:be:")
                .or_else(|| sym.strip_prefix("prime:"))
                .unwrap_or(sym);
            active_symbols.contains(bare)
        });

        let mut cands = Vec::new();
        let positions = trading.positions.clone();
        for p in positions {
            if pending.contains_key(&p.symbol) {
                continue;
            }
            if crate::config::is_symbol_excluded(&excluded_for_filter, &p.symbol) {
                continue;
            }
            let anchor = p.opened_at.unwrap_or(p.first_seen);
            if anchor <= 0 {
                continue;
            }
            let held_secs = now.saturating_sub(anchor);
            use rust_decimal::prelude::ToPrimitive;
            let invested = (p.avg_entry * p.qty).to_f64().unwrap_or(0.0);
            if invested <= 0.0 {
                continue;
            }
            let pnl_pct = (p.unrealized_pnl / invested) * 100.0;

            // PRIME F2c — the asymmetric ladder gets first look: it is
            // R-based (uses the entry bracket) and strictly tighter
            // than the percent rails once a trade earns +1R.
            let mut exit_reason = prime_exit_policy.as_ref().and_then(|pol| {
                evaluate_prime_ladder(
                    pol,
                    &PrimeLadderInput {
                        symbol: &p.symbol,
                        is_long: p.side == Direction::Long,
                        entry: p.avg_entry.to_f64().unwrap_or(0.0),
                        bracket_stop: p.stop_loss,
                        price: p.current_price,
                        pnl_pct,
                    },
                    &mut trading.peak_pnl_pcts,
                )
            });

            if exit_reason.is_none() {
                exit_reason = evaluate_adaptive_exit(held_secs, pnl_pct, &params);
            }

            if exit_reason.is_none() {
                exit_reason = evaluate_trailing_stop(
                    &p.symbol,
                    held_secs,
                    pnl_pct,
                    ts_activation,
                    ts_deviation,
                    ts_min_hold_secs,
                    &mut trading.peak_pnl_pcts,
                );
            }

            if let Some(reason) = exit_reason {
                cands.push((
                    p.symbol.clone(),
                    p.side,
                    p.qty,
                    reason,
                    pnl_pct,
                    held_secs,
                ));
            }
        }
        cands
    };

    for (symbol, side, qty, reason, pnl_pct, held_secs) in candidates {
        // S17.0 — same tradable preflight as max-hold. Avoids 422
        // loops on inactive / delisted assets.
        let (key, secret) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
            )
        };
        if !key.is_empty() && !secret.is_empty() {
            let tradable = crate::market::scanner::is_tradable_now(
                &state.http_client,
                &key,
                &secret,
                &symbol,
            )
            .await;
            if !tradable {
                tracing::debug!(
                    component = "bot",
                    symbol = %symbol,
                    reason = %reason,
                    "Adaptive exit skip — asset not tradable"
                );
                continue;
            }
        }

        // v2.4.1 — Downgraded from WARN to DEBUG. Live evidence from
        // 2026-05-11: this line fires every 30 s per held position and
        // produced 5060 entries in a single trading day, of which
        // 5059 were no-ops because the bracket OCO was in flight on
        // the broker side. The actionable signal — "we just submitted
        // an exit order" — is now emitted at INFO below, right after
        // all gates clear. DEBUG retains the decision-trail audit at
        // log-level=debug without drowning normal sessions.
        tracing::debug!(
            component = "bot",
            symbol = %symbol,
            reason = %reason,
            pnl_pct = format!("{pnl_pct:.2}"),
            held_secs,
            "ADAPTIVE EXIT firing"
        );

        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_exits.insert(symbol.clone(), now);
        }

        let close_side = opposite_direction(side);

        // S17.6 — broker-side orphan-exit guard. The CRE 2026-05-06
        // incident was triggered exactly here: an earlier exit was
        // working at the broker, the local pending_exits gate had
        // expired by the next tick (30 s race, fixed in S17.6
        // bump), and `send_order` collided on `held_for_orders`.
        // With this check the adaptive rail no-ops while the orphan
        // is still pending; once it fills, position_recon clears the
        // local position before we even get here.
        if has_open_close_order(state, &symbol, alpaca_side(close_side)).await {
            tracing::info!(
                component = "bot",
                symbol = %symbol,
                reason = %reason,
                "ADAPTIVE EXIT skipped — broker already has an open close order in flight"
            );
            continue;
        }

        // v2.4.1 — local-state freshness re-check (see
        // `local_position_still_open`). This is the ATRA 2026-05-11
        // incident: candidates Vec cached `side=Long`, but in the
        // 200 ms it took to clear `has_open_close_order`,
        // `position_recon` observed the broker fill and removed the
        // position. The bot then submitted a market `sell` against
        // a flat book and Alpaca rejected it as a short attempt.
        if !local_position_still_open(state, &symbol, side) {
            tracing::info!(
                component = "bot",
                symbol = %symbol,
                reason = %reason,
                "ADAPTIVE EXIT skipped — position closed by broker between detection and submit"
            );
            release_pending_exit_gate(state, &symbol);
            continue;
        }

        // v2.4.1 — Single actionable event for the adaptive-exit
        // rail: an order is actually being submitted now (not just
        // "considered"). Companion to the DEBUG decision log above.
        tracing::info!(
            component = "bot",
            symbol = %symbol,
            reason = %reason,
            pnl_pct = format!("{pnl_pct:.2}"),
            held_secs,
            qty = %qty,
            side = ?close_side,
            "ADAPTIVE EXIT submitting order"
        );

        let order = OrderRequest {
            symbol: symbol.clone(),
            side: close_side,
            qty: qty.abs(),
            stop_loss: None,
            take_profit: None,
            source: String::new(),
            deducted_amount: Decimal::ZERO,
            order_type: "market".to_string(),
            limit_price: None,
            bar_time: None,
            notional: None,
            is_exit: true,
            is_swing_intent: false,
            entry_timeframe: String::new(),
            entry_score: None,
            conviction: None,
            expected_price: 0.0,
        };

        match send_order(Arc::clone(state), order, app.clone(), exec_metrics).await {
            Ok(result) => {
                metrics.record_exit(&reason);
                let _ = app.emit_all(
                    "bot-exit",
                    serde_json::json!({
                        "symbol": symbol,
                        "reason": reason.to_string(),
                        "qty": qty.to_string(),
                        "broker_id": result.broker_id,
                        "pnl_pct": pnl_pct,
                        "held_secs": held_secs,
                    }),
                );
            }
            Err(e) => {
                tracing::error!(
                    component = "bot",
                    symbol = %symbol,
                    reason = %reason,
                    error = %e,
                    "Adaptive exit order FAILED"
                );
                // Same cooldown discipline as max-hold (see S17.0).
                tracing::warn!(
                    component = "bot",
                    symbol = %symbol,
                    "Adaptive exit gate retained for 60 s cooldown"
                );
            }
        }
    }
}
