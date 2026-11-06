use std::sync::Arc;

use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use tauri::{AppHandle, Manager};

use crate::config::CautionAction;
use crate::execution::order_exec::{send_order_pegged_ioc, ExecutionMetrics, OrderRequest};
use crate::state::{BreakerLevel, TitanState, TradingMode};

use super::gates::{
    account_stale_check, bars_in_today_et_session, buying_power_gate_check, emit_blocked_event,
    fresh_momentum_check, pdt_gate_check, quality_gate_check, short_gate_check,
    FreshMomentumOutcome, QualityGateOutcome,
};
use super::loop_run::{compute_qty, compute_stop_loss, compute_take_profit, BotMetrics};
use super::{BotAction, SkipReason};

pub(super) async fn handle_entry(
    state: &Arc<TitanState>,
    market: &Arc<crate::market::market_state::MarketDataState>,
    action: &BotAction,
    app: &AppHandle,
    metrics: &mut BotMetrics,
    exec_metrics: &mut ExecutionMetrics,
) {
    // v2.4.4 — permanent symbol exclusion list. First-stop gate: if the
    // operator has banned this ticker (e.g. `TRUE` — Alpaca paper zombie
    // for 3+ months that no API call can clear), short-circuit BEFORE
    // any RTH check / quality gate / network round-trip. Silent trace.
    {
        let excluded = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.market.excluded_symbols.clone()
        };
        if crate::config::is_symbol_excluded(&excluded, &action.symbol) {
            tracing::trace!(
                component = "bot",
                symbol = %action.symbol,
                "Entry skip — symbol permanently excluded"
            );
            return;
        }
    }

    // ── S17.0 / S20 Gate: regular trading hours (strategy-aware) ──
    //
    // Live evidence (2026-05-04 20:22 UTC = 16:22 EDT, 22 min after
    // close): bot debited $499 attempting CLNN/BZAI entries while the
    // market was already closed. Day's wallet drained from $2,693 →
    // $5.39 in the post-close window when broker rejected orders but
    // the cleanup paths leaked funds. Hard gate: do not even debit
    // the wallet outside RTH (09:30–16:00 ET).
    //
    // **D Evolution Protocol (Sesiunea 1)** introduces strategies whose
    // `TradingWindow` is `ExtendedAndRth` (catalyst_frontrun,
    // extended_news) — for those signals we honour the strategy's
    // declared window instead of the hard RTH block. Legacy
    // SentinelTrend signals (`strategy_id == None`) keep the
    // RTH-only behaviour unchanged: this preserves the S17.0 wallet-
    // leak fix for every entry path that existed before S20.
    {
        let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        use chrono::Timelike;
        let in_rth_now = crate::market::scanner::in_rth(now_et);
        // v2.4.3 — legacy momentum (`strategy_id == None`) opts into
        // extended hours via `smarttrend.allow_premarket_entries`.
        // The 8 Tournament strategies keep their own per-strategy
        // declarations (ExtendedNews + CatalystFrontRun already run
        // 04:00–20:00 ET; the other six stay RthOnly by design).
        let legacy_allow_extended = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.smarttrend.allow_premarket_entries
        };
        let allowed_outside_rth = match action.strategy_id {
            None => {
                if legacy_allow_extended {
                    crate::smarttrend::strategies::TradingWindow::ExtendedAndRth
                        .permits(now_et.hour(), now_et.minute())
                } else {
                    false
                }
            }
            Some(id) => {
                let window = crate::smarttrend::strategies::by_id(id)
                    .map(|s| s.allowed_window())
                    .unwrap_or(crate::smarttrend::strategies::TradingWindow::RthOnly);
                window.permits(now_et.hour(), now_et.minute())
            }
        };
        if !in_rth_now && !allowed_outside_rth {
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                now_et = %now_et.format("%H:%M:%S"),
                strategy = ?action.strategy_id,
                "Entry BLOCKED — outside the strategy's trading window"
            );
            let _ = app.emit_all(
                "bot-blocked",
                serde_json::json!({
                    "symbol": action.symbol,
                    "reason": "Outside trading window",
                }),
            );
            metrics.record_entry_blocked();
            return;
        }

        // S17.1 — entry-window cutoff. Optional; defaults to 16 (= no
        // cutoff). Set to 11 in `settings.json` to suppress entries
        // during the post-morning chop window where today's empirical
        // win-rate dropped from 34% (10:00–11:00) to 17%
        // (11:00–13:00). Exits are unaffected — safety and strategy
        // exits continue to run until 16:00 ET regardless of this gate.
        //
        // v2.5.0 — Tournament strategies (`strategy_id == Some(_)`)
        // declare their own time windows in `evaluate_entry` (e.g.
        // AfternoonFade only fires 15:00-15:30, ExtendedNews only in
        // extended hours). Subjecting them to the legacy momentum
        // cutoff would silently kill AfternoonFade entirely. Skip the
        // gate for Tournament-routed signals — they already self-gate.
        let entry_window_end_et: u8 = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.smarttrend.entry_window_end_et
        };
        let cutoff = entry_window_end_et.clamp(10, 16);
        if action.strategy_id.is_none() && cutoff < 16 && now_et.hour() as u8 >= cutoff {
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                now_et = %now_et.format("%H:%M:%S"),
                cutoff,
                "Entry BLOCKED — past entry-window cutoff"
            );
            let _ = app.emit_all(
                "bot-blocked",
                serde_json::json!({
                    "symbol": action.symbol,
                    "reason": format!("Past {cutoff}:00 ET cutoff"),
                }),
            );
            metrics.record_entry_blocked();
            return;
        }

        // v2.4.2 — opening-volatility skip. The first N minutes after
        // 09:30 ET are dominated by overnight imbalance unwinds, gap
        // fades and halt auctions; the momentum-pullback FSM mis-reads
        // these tape patterns as "trend continuation" and chases the
        // top. See `entry_skip_minutes_after_open` doc for the live
        // evidence. Computed in minutes-from-09:30 ET so DST flips and
        // half-day sessions need no special handling.
        //
        // v2.5.0 — Tournament strategies bypass this gate: GapFade is
        // SPECIFICALLY designed to enter in the 09:30-10:00 window the
        // legacy skip blocks, and the other 7 strategies have their
        // own `allowed_window` declarations. Skip the legacy gate for
        // `strategy_id.is_some()`.
        let skip_minutes: u32 = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.smarttrend.entry_skip_minutes_after_open.min(120)
        };
        if action.strategy_id.is_none() && skip_minutes > 0 && in_rth_now {
            let minutes_since_open: i32 =
                (now_et.hour() as i32 - 9) * 60 + (now_et.minute() as i32 - 30);
            if minutes_since_open >= 0 && (minutes_since_open as u32) < skip_minutes {
                tracing::info!(
                    component = "bot",
                    symbol = %action.symbol,
                    now_et = %now_et.format("%H:%M:%S"),
                    minutes_since_open,
                    skip_minutes,
                    "Entry BLOCKED — opening volatility window"
                );
                let _ = app.emit_all(
                    "bot-blocked",
                    serde_json::json!({
                        "symbol": action.symbol,
                        "reason": format!(
                            "Within first {skip_minutes}min of open ({minutes_since_open}min in)"
                        ),
                    }),
                );
                metrics.record_entry_blocked();
                return;
            }
        }
    }

    // ── S18 Min-Entry-Score Gate ──
    //
    // Cheap pre-filter on the strategy's `combined_score`. Live
    // evidence (2026-05-07): all 4 winning round trips of the day
    // had `score == 84.0`, while losers clustered in the 50–65 band.
    // Suppressing entries below 75 would have eliminated 14/17
    // losers without dropping any winner. The default `0.0`
    // disables the gate so legacy behaviour is preserved; raise it
    // to 70–75 once you have a sample big enough to validate.
    //
    // v2.5.0 (NUCLEAR) — Tournament entries (`strategy_id.is_some()`)
    // carry their own quality bars in `evaluate_entry`; the legacy
    // momentum `combined_score` is not even computed for them
    // (always defaults to whatever the dispatcher seeded), so
    // letting `min_entry_score` filter them would silently kill
    // every Tournament signal that didn't accidentally clear the
    // legacy bar. Skip for `strategy_id.is_some()`.
    if action.strategy_id.is_none() {
        let min_entry_score = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.smarttrend.min_entry_score
        };
        if min_entry_score > 0.0 && action.score < min_entry_score {
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                score = action.score,
                threshold = min_entry_score,
                "Entry BLOCKED — score below S18 min_entry_score"
            );
            let _ = app.emit_all("bot-blocked", serde_json::json!({
                "symbol": action.symbol,
                "reason": format!("Score {:.1} < min_entry_score {:.1}", action.score, min_entry_score),
            }));
            metrics.record_entry_blocked();
            return;
        }
    }

    // ── S17.2 Quality Gate: catalyst-required + top-N filter ──
    //
    // The strategy's `combined_score` is momentum-only — a clean ROC
    // burst on a no-news, no-catalyst, low-volume name still fires an
    // Entry marker. Live evidence (2026-05-05): 80% of broker losses
    // came from no-catalyst names (HCAI -$83, DARE -$48, MASK -$42).
    // Names that DID have at least one catalyst pill collectively
    // produced +$54 — about $370 swing vs the full universe.
    //
    // This gate adds discipline at the bot layer (post-FSM,
    // pre-execution) using data the scanner already computes:
    //   - `state.market.scan_results` is sorted by score DESC and
    //     populated every auto_scan tick (~5 min cadence in RTH).
    //   - `CatalystFlags::score` blends news, earnings, gap, RVOL.
    //
    // Both filters are opt-in (default off) so existing behaviour is
    // preserved until live-validated. Configured via `settings.json`:
    //   smarttrend.catalyst_required = true
    //   smarttrend.catalyst_min_score = 25.0   (== single news pill)
    //   smarttrend.top_n_filter      = 3      (Ross-style focus)
    //
    // v2.5.0 — Tournament strategies bring their own quality bars (per-
    // strategy `min_catalyst_score`, `min_rvol`, `spread_bps` caps, halt
    // gates, etc. encoded in `evaluate_entry`). The catalyst-required +
    // top-N scanner gate was tuned for legacy momentum-only signals on
    // small-cap news names; applying it universally suppresses pair,
    // stat-arb, ETF and leveraged-ETF strategies whose target symbols
    // are deliberately NOT in the catalyst scanner. Skip the gate when
    // `strategy_id.is_some()` — those signals already self-validated.
    if action.strategy_id.is_none() {
        let (catalyst_required, catalyst_min_score, top_n_filter) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.smarttrend.catalyst_required,
                cfg.smarttrend.catalyst_min_score,
                cfg.smarttrend.top_n_filter,
            )
        };
        let outcome = {
            // Lock market(2) only long enough to evaluate; release
            // before any emit / log / lock acquisition further down.
            let mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
            quality_gate_check(
                &mkt.scan_results,
                &mkt.auto_universe,
                &action.symbol,
                catalyst_required,
                catalyst_min_score,
                top_n_filter,
            )
        };
        match outcome {
            QualityGateOutcome::Disabled | QualityGateOutcome::Pass => {}
            blocked => {
                let reason_str = match &blocked {
                    QualityGateOutcome::NotInScan => "Symbol not in latest scan".to_string(),
                    QualityGateOutcome::NotInTopN { rank, n } => {
                        format!("Rank {rank} ≥ top-N filter {n}")
                    }
                    QualityGateOutcome::Halted => "Halted".to_string(),
                    QualityGateOutcome::BelowMinScore {
                        score,
                        threshold,
                        pills,
                    } => {
                        format!("Catalyst score {score:.1} < {threshold:.1} (pills={pills})")
                    }
                    _ => "Quality gate".to_string(),
                };
                tracing::info!(
                    component = "bot",
                    symbol = %action.symbol,
                    outcome = ?blocked,
                    "Entry BLOCKED at Quality Gate"
                );
                let _ = app.emit_all(
                    "bot-blocked",
                    serde_json::json!({
                        "symbol": action.symbol,
                        "reason": reason_str,
                    }),
                );
                metrics.record_entry_blocked();
                return;
            }
        }
    }

    let mode = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.mode
    };

    // S15.13: refresh price from latest tick to avoid stale-price rejections
    // from Alpaca. Sentinel may emit Entry markers based on a 1m/5m bar close
    // that's already several seconds old by the time the bot loop processes
    // the action. With volatile small-caps that drift 1–4% per second around
    // the open, the SL/TP computed off `action.price` lands on the wrong side
    // of the broker's `base_price` and gets rejected (HTTP 422 code 42210000).
    //
    // Fall back to `action.price` if the latest tick is missing (rare; would
    // mean the symbol never produced a trade since boot).
    let live_price = {
        let prices = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
        prices.get(&action.symbol).copied().unwrap_or(action.price)
    };

    // ── S17.5 Fresh-momentum / VWAP gate ──
    //
    // v2.5.0 NUCLEAR — Tournament strategies (`strategy_id.is_some()`)
    // bring their own directional bias logic (GapFade goes SHORT on a
    // gap-up specifically, CatalystFrontRun goes either way based on
    // catalyst polarity, etc.). The VWAP gate is a legacy momentum
    // sanity check that vetoes 80%+ of Tournament entries — live
    // evidence 2026-05-14: 1670 of 2423 Tournament signals blocked
    // here. Skip for Tournament-attributed signals; they self-validate.
    //
    // Reject entries below today's session VWAP. Buying below VWAP on
    // a long-only momentum strategy is statistically dilutive (entries
    // fight against the day's accumulated mean). Helper does its own
    // empty-data / non-finite handling and returns Disabled when the
    // config flag is off, keeping the hot path zero-cost when not
    // requested.
    if action.strategy_id.is_none() {
        let require_vwap = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.smarttrend.require_price_above_vwap
        };
        if require_vwap {
            // Snapshot today-session bars from the candle ring buffer.
            let now_ts = chrono::Utc::now().timestamp();
            let buf_key = format!("{}:{}", action.symbol, action.timeframe);
            let today_bars = {
                let buffers = market
                    .candle_buffers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let snap = buffers
                    .get(&buf_key)
                    .map(|b| b.snapshot())
                    .unwrap_or_default();
                bars_in_today_et_session(&snap, now_ts)
            };
            let outcome = fresh_momentum_check(true, live_price, &today_bars);
            match outcome {
                FreshMomentumOutcome::Disabled | FreshMomentumOutcome::Pass => {}
                FreshMomentumOutcome::InsufficientData => {
                    tracing::info!(
                        component = "bot",
                        symbol = %action.symbol,
                        bar_count = today_bars.len(),
                        timeframe = %action.timeframe,
                        "Entry BLOCKED — VWAP gate: insufficient session data"
                    );
                    let _ = app.emit_all(
                        "bot-blocked",
                        serde_json::json!({
                            "symbol": action.symbol,
                            "reason": "VWAP gate: no session data yet",
                        }),
                    );
                    metrics.record_entry_blocked();
                    return;
                }
                FreshMomentumOutcome::BelowVwap { price, vwap } => {
                    tracing::info!(
                        component = "bot",
                        symbol = %action.symbol,
                        price,
                        vwap,
                        "Entry BLOCKED — price below session VWAP"
                    );
                    let _ = app.emit_all(
                        "bot-blocked",
                        serde_json::json!({
                            "symbol": action.symbol,
                            "reason": format!("Price ${price:.2} ≤ VWAP ${vwap:.2}"),
                        }),
                    );
                    metrics.record_entry_blocked();
                    return;
                }
            }
        }
    }
    let entry_price = if live_price > 0.0 && live_price.is_finite() {
        let drift_pct = ((live_price - action.price) / action.price).abs() * 100.0;
        if drift_pct > 5.0 {
            tracing::warn!(
                component = "bot",
                symbol = %action.symbol,
                action_price = action.price,
                live_price,
                drift_pct = format!("{:.2}", drift_pct),
                "Stale signal: price drifted >5% between FSM eval and submit; skipping"
            );
            metrics.record_entry_blocked();
            return;
        }
        live_price
    } else {
        action.price
    };

    if mode != TradingMode::Auto {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            score = action.score,
            direction = ?action.direction,
            "MANUAL mode — entry signal logged, not executed"
        );
        let _ = app.emit_all(
            "bot-signal",
            serde_json::json!({
                "symbol": action.symbol,
                "kind": "Entry",
                "direction": format!("{:?}", action.direction),
                "score": action.score,
                "mode": "manual",
            }),
        );
        return;
    }

    // ── Gate 0a: short selling permission + asset metadata (S02) ──
    let allow_short = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.execution.allow_short
    };
    // S40 (audit 2026-05-29): fold the broker's account capability into the
    // permission. Cash accounts return `shorting_enabled=false` — every
    // short order would be rejected, so drop the signal here instead.
    let allow_short = allow_short && {
        let r = state.risk.lock().unwrap_or_else(|p| p.into_inner());
        r.shorting_enabled
    };
    let asset_opt = crate::market::scanner::lookup_asset(&action.symbol).await;
    if let Some(reason) = short_gate_check(action.direction, allow_short, asset_opt.as_ref()) {
        metrics.record_skipped(&reason);
        emit_blocked_event(app, &action.symbol, &reason);
        return;
    }

    // ── Gate 0b: account snapshot freshness + PDT rule (S02) ──
    let (equity_for_gates, daytrade_count, pdt_flag, buying_power, last_sync_at, is_cash_account) = {
        let mut r = state.risk.lock().unwrap_or_else(|p| p.into_inner());
        // S40: cash account ⇔ multiplier == 1. Default 0.0 (unsynced) is
        // treated as margin so PDT protection is never dropped before a
        // confirming /v2/account sync.
        let is_cash = (r.account_multiplier - 1.0).abs() < 0.01;
        let now_ts = chrono::Utc::now().timestamp();
        let unsettled_total = if is_cash {
            r.get_unsettled_funds_total(now_ts)
        } else {
            0.0
        };
        let bp = if is_cash {
            (r.buying_power - unsettled_total).max(0.0)
        } else {
            r.buying_power
        };
        (
            r.session_equity,
            r.daytrade_count,
            r.pattern_day_trader,
            bp,
            r.last_account_sync_at,
            is_cash,
        )
    };
    let now_ts = chrono::Utc::now().timestamp();
    if let Some(reason) = account_stale_check(now_ts, last_sync_at) {
        metrics.record_skipped(&reason);
        emit_blocked_event(app, &action.symbol, &reason);
        return;
    }
    // S28 — read swing-only override config for the PDT gate.
    let (allow_swing, swing_hour_start) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.risk.allow_swing_when_pdt_blocked,
            cfg.risk.swing_entry_et_hour,
        )
    };
    let now_et_hour: u32 = {
        use chrono::Timelike;
        chrono::Utc::now()
            .with_timezone(&chrono_tz::US::Eastern)
            .hour()
    };
    if let Some(reason) = pdt_gate_check(
        equity_for_gates,
        daytrade_count,
        pdt_flag,
        now_et_hour,
        allow_swing,
        swing_hour_start,
        is_cash_account,
    ) {
        metrics.record_skipped(&reason);
        emit_blocked_event(app, &action.symbol, &reason);
        return;
    }
    // S28 — when PDT count ≥ 3 and the swing window allowed the entry,
    // mark the trade as swing-intent so downstream bracket sizing and
    // `flat_by_close` treat the position as overnight-only.
    let is_swing_intent = crate::smarttrend::bot::gates::is_swing_intent_entry(
        equity_for_gates,
        daytrade_count,
        pdt_flag,
        now_et_hour,
        allow_swing,
        swing_hour_start,
        is_cash_account,
    );
    if is_swing_intent {
        tracing::warn!(
            component = "bot",
            symbol = %action.symbol,
            daytrade_count,
            equity = equity_for_gates,
            now_et_hour,
            "S28 SWING-INTENT entry — PDT count≥3, bypass enabled, late-RTH window. \
             Bracket widened, flat_by_close skipped."
        );
    }

    // ── Gate 1: max positions ──
    let (max_pos, current_count) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.execution.max_positions as usize,
            trading.positions.len(),
        )
    };
    if current_count >= max_pos {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            current = current_count,
            max = max_pos,
            "Entry BLOCKED — max positions reached"
        );
        let _ = app.emit_all(
            "bot-blocked",
            serde_json::json!({
                "symbol": action.symbol,
                "reason": format!("Max positions ({}/{})", current_count, max_pos),
            }),
        );
        metrics.entries_blocked_max_pos += 1;
        metrics.record_entry_blocked();
        return;
    }

    // ── Gate 2: duplicate symbol (S15.14 + S17.0 race fix) ──
    //
    // Check + claim are done in the SAME critical section. Previously
    // the `pending_entries.insert()` happened ~270 lines later (after
    // the await boundary on `send_order`), giving 4+ concurrent bot
    // loop iterations a window to all pass the gate AND debit the
    // wallet for the same symbol. Live evidence (2026-05-04 19:41:39):
    //
    //   19:41:39.887 Debited QBTS est_cost=480.58 (BUY 1)
    //   19:41:40.020 Debited QBTS est_cost=481.50 (BUY 2 dup)
    //   19:41:40.152 Debited QBTS est_cost=481.50 (BUY 3 dup)
    //   19:41:40.283 Debited QBTS est_cost=481.50 (BUY 4 dup)
    //
    // Only one position opened at the broker — the other 3 leaked
    // funds when the broker rejected dups but the cleanup was not
    // atomic. Claim the slot before any further work / await calls.
    let blacklist_strikes: u32 = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.smarttrend.intraday_loss_blacklist_strikes
    };
    {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let now = chrono::Utc::now().timestamp();
        trading
            .pending_entries
            .retain(|_, ts| now.saturating_sub(*ts) < crate::state::PENDING_ENTRY_TIMEOUT_SECS);
        // v2.5.0 NUCLEAR (HF3) — keep the direction-lock map in lock-
        // step with the pending_entries TTL eviction so stale locks
        // can't permanently veto opposite-side signals on a symbol.
        let live_pending: std::collections::HashSet<String> =
            trading.pending_entries.keys().cloned().collect();
        trading
            .pending_entry_dirs
            .retain(|s, _| live_pending.contains(s));
        let already_open = trading.positions.iter().any(|p| p.symbol == action.symbol);
        let already_pending = trading.pending_entries.contains_key(&action.symbol);
        if already_open || already_pending {
            let reason = if already_open {
                "Position already open"
            } else {
                "Entry already pending"
            };
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                already_open,
                already_pending,
                "Entry BLOCKED — {reason}"
            );
            let _ = app.emit_all(
                "bot-blocked",
                serde_json::json!({
                    "symbol": action.symbol,
                    "reason": reason,
                }),
            );
            metrics.entries_blocked_duplicate += 1;
            metrics.record_entry_blocked();
            return;
        }

        // S17.1 — intraday loss-streak blacklist. Held inside the same
        // critical section as the duplicate-symbol gate so the check
        // and any subsequent claim are atomic. If today's losing
        // streak on this symbol meets or exceeds the configured
        // strike count, refuse the entry without claiming the slot.
        // Streak is reset to 0 on any winning close (see
        // `ops::journal::update_loss_streak`).
        //
        // v2.5.0 NUCLEAR (N4) — Tournament strategies bypass this gate.
        // Each Tournament strategy has its own sub-wallet accounting
        // and is allowed to keep firing on a symbol that's been
        // unprofitable on the day; coupling Tournament to FSM's
        // loss-streak counter killed 39 valid Tournament signals on
        // 2026-05-14.
        if action.strategy_id.is_none() && blacklist_strikes > 0 {
            let streak = trading
                .symbol_loss_streak
                .get(&action.symbol.trim().to_uppercase())
                .map(|(n, _)| *n)
                .unwrap_or(0);
            if streak >= blacklist_strikes {
                tracing::info!(
                    component = "bot",
                    symbol = %action.symbol,
                    streak,
                    threshold = blacklist_strikes,
                    "Entry BLOCKED — intraday loss-streak blacklist"
                );
                let _ = app.emit_all(
                    "bot-blocked",
                    serde_json::json!({
                        "symbol": action.symbol,
                        "reason": format!("Loss streak {streak} ≥ {blacklist_strikes}"),
                    }),
                );
                metrics.record_entry_blocked();
                return;
            }
        }
        // Claim the slot atomically with the gate check. Any concurrent
        // task entering the gate after this point will see the symbol
        // pending and bail. If any later step (gates, wallet debit,
        // order submit) fails, the slot must be released — see all
        // `metrics.record_entry_blocked()` returns below this point and
        // the failure branch of `send_order`.
        trading.pending_entries.insert(action.symbol.clone(), now);
        // v2.5.0 NUCLEAR (HF3) — also tag the in-flight direction so
        // the Tournament dispatcher can drop opposite-direction signals
        // even if the trade_updates websocket is silent and
        // state.trading.positions is stale.
        trading
            .pending_entry_dirs
            .insert(action.symbol.clone(), action.direction);
    }
    // Helper closure: any post-claim early-return must release the
    // pending slot or the symbol stays blocked for 60s.
    let release_pending = |state: &Arc<TitanState>| {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading.pending_entries.remove(&action.symbol);
        trading.pending_entry_dirs.remove(&action.symbol);
    };

    // S24 (2026-05-21) — `risk_per_trade` removed from the legacy sizing
    // path. It is retained in config for tournament strategy sizing,
    // daily-loss telemetry, and ATR-based stop scaling, but the
    // per-trade USD envelope is now governed exclusively by
    // `execution.max_usd_per_asset`. See line 690.
    let (bracket_mult, max_usd_per_asset) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.execution.bracket_atr_mult,
            cfg.execution.max_usd_per_asset,
        )
    };

    // ── Gate 3: bot wallet balance ──
    let bot_balance = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        st.bot_wallet.balance
    };
    let min_balance = Decimal::from_f64(25.0).unwrap_or(Decimal::ZERO);
    if bot_balance < min_balance {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            %bot_balance,
            "Entry BLOCKED — bot wallet insufficient (< $25)"
        );
        let _ = app.emit_all(
            "toast-warning",
            serde_json::json!({
                "message": format!("BOT: Insufficient balance (${bot_balance})"),
            }),
        );
        metrics.record_entry_blocked();
        release_pending(state);
        return;
    }

    let account_snap = state
        .smarttrend
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .account
        .clone();
    // S15.13: derive atr/qty/SL/TP from `entry_price` (live tick) instead of
    // `action.price` (FSM-eval-time). Keeps the broker-side `base_price`
    // and our limit prices on the same side.
    //
    // v2.4.2 — use a REAL ATR from the candle buffer instead of the
    // historical `entry_price * 0.01` placeholder.
    //
    // The 1%-of-price fake had two failure modes in live trading:
    //   1. Small-caps move 3–5% in minutes (premarket gappers, halts).
    //      A 1% ATR proxy → 2%-wide bracket stop → instant tag on
    //      normal noise. 2026-05-12 AGL: action_price=$60.50,
    //      fill=$62.19 (chase the pop), SL set off action_price at
    //      $58.74 i.e. −5.5% from actual fill. Bracket tagged in 2m.
    //   2. Low-volatility names (mega-caps, ETFs) get oversized stops
    //      that never trigger — we hold losers to max-hold instead of
    //      cutting on the strategy-implied volatility budget.
    //
    // Pull the same `calculate_atr_sma_last(&candles, 14)` the sentinel
    // engine uses so SL/TP are consistent with the diagnostic that
    // greenlit the entry. Lock the buffer briefly, snapshot, release.
    // Try 5m first (default decision timeframe), fall back to 1m, fall
    // back to the legacy 1% proxy only when both buffers are empty.
    let atr_val = {
        let buffers = market
            .candle_buffers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut atr_real = 0.0;
        for tf in ["5m", "1m"] {
            let key = format!("{}:{}", action.symbol, tf);
            if let Some(buf) = buffers.get(&key) {
                let snap = buf.snapshot();
                if let Some(a) = titan_indicators::calculate_atr_sma_last(&snap, 14) {
                    if a.is_finite() && a > 0.0 {
                        atr_real = a;
                        break;
                    }
                }
            }
        }
        if atr_real > 0.0 {
            atr_real
        } else {
            tracing::debug!(
                component = "bot",
                symbol = %action.symbol,
                price = entry_price,
                "Real ATR unavailable from candle buffer; falling back to 1%-of-price proxy"
            );
            entry_price * 0.01
        }
    };

    let mut entry_price = entry_price;
    if entry_price <= 0.0 || !entry_price.is_finite() {
        tracing::warn!(
            component = "bot",
            symbol = %action.symbol,
            price = entry_price,
            "Invalid price, skipping entry"
        );
        metrics.record_entry_blocked();
        release_pending(state);
        return;
    }

    // S29.4 — Upgrade de Elită 3: Micro-Pullback Price Improvement Gate pentru APEX
    if action.strategy_id == Some(crate::smarttrend::strategies::StrategyId::Apex) {
        let pb_target_offset = 0.15 * atr_val;
        if pb_target_offset > 0.0 {
            let target_price = match action.direction {
                crate::state::Direction::Long => entry_price - pb_target_offset,
                crate::state::Direction::Short => entry_price + pb_target_offset,
            };
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                initial_price = entry_price,
                target_price,
                "S29.4 Elite Upgrade 3: Micro-Pullback Gate armed. Waiting up to 10s for price improvement..."
            );

            let start_wait = std::time::Instant::now();
            let mut got_improvement = false;
            while start_wait.elapsed().as_secs() < 10 {
                let current_live_price = {
                    let prices = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
                    prices.get(&action.symbol.to_ascii_uppercase()).cloned()
                };
                if let Some(clp) = current_live_price {
                    let condition_met = match action.direction {
                        crate::state::Direction::Long => clp <= target_price,
                        crate::state::Direction::Short => clp >= target_price,
                    };
                    if condition_met {
                        tracing::info!(
                            component = "bot",
                            symbol = %action.symbol,
                            initial_price = entry_price,
                            improved_price = clp,
                            "Micro-pullback target met! Triggering entry order."
                        );
                        entry_price = clp;
                        got_improvement = true;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if !got_improvement {
                let current_live_price = {
                    let prices = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
                    prices.get(&action.symbol.to_ascii_uppercase()).cloned()
                };
                if let Some(clp) = current_live_price {
                    entry_price = clp;
                }
                tracing::info!(
                    component = "bot",
                    symbol = %action.symbol,
                    final_price = entry_price,
                    "Micro-pullback timeout. Chasing entry at current market price."
                );
            }
        }
    }

    let bot_bal_f64 = bot_balance.to_f64().unwrap_or(0.0);
    let max_size = bot_bal_f64.min(max_usd_per_asset);
    // S24 (2026-05-21) — removed `account_snap.compute_size_usd(risk_per_trade)`
    // as a second limiter. On small accounts (paper $300 baseline) the
    // formula `allocated_capital × risk_per_trade` collapsed sizing to
    // sub-1-share even when `max_usd_per_asset` was correctly set:
    //   * allocated_capital ≈ broker_equity (set by `equity_sync.rs:166`)
    //   * risk_per_trade clamped to ≤ 0.10 by config validation
    //   ⇒ max produceable size ≈ $30, frequently below per-share price
    //     and silently capped further by `account_snap` snapshot lag.
    // Operator-facing intent (`max_usd_per_asset` in UI) is now the
    // *single source of truth* for per-trade USD exposure. `risk_per_trade`
    // remains in use for: tournament strategy `position_size()`,
    // `position_size_risk_based`, and the daily-loss governor.
    let mut size_usd = max_size;
    // Keep `account_snap` alive: the silent dependency on `allocated_capital`
    // is removed but downstream telemetry / P&L still reads it.
    let _ = &account_snap;

    // v2.5.0 — Tournament-aware sizing. When the action came from a
    // strategy (`strategy_id == Some(_)`), the strategy itself
    // declared a per-trade risk envelope (`risk_dollars`,
    // `sub_wallet_cap_pct`, `max_loss_pct`) tied to its allocated
    // sub-wallet. We honour the strategy's `position_size()` and
    // shrink to that — never grow above the global bot cap. This
    // keeps:
    //   * strategy-internal risk constraints (1×ATR stop @ 0.5% loss
    //     for CatalystFrontRun, 1.5×ATR @ wider hold for ExtendedNews)
    //   * Tournament sub-wallet isolation (a single strategy can't
    //     drain the whole bot_wallet)
    //   * global max_usd_per_asset rail (defensive ceiling unchanged)
    if let Some(strategy_id) = action.strategy_id {
        if let Some(strat) = crate::smarttrend::strategies::by_id(strategy_id) {
            let sub_wallet = state
                .dplus
                .get()
                .and_then(|dp| dp.tournament.lock().ok())
                .and_then(|t| t.stats.get(&strategy_id).cloned())
                .map(|s| {
                    if s.pending_sandbox {
                        s.current_sub_wallet
                            * Decimal::from_f64_retain(0.5).unwrap_or_else(|| Decimal::new(5, 1))
                    } else {
                        s.current_sub_wallet
                    }
                })
                .unwrap_or_else(|| Decimal::from_f64_retain(bot_bal_f64).unwrap_or(Decimal::ZERO));
            if sub_wallet > Decimal::ZERO {
                let buf_key = format!("{}:{}", action.symbol, action.timeframe);
                let candles = market
                    .candle_buffers
                    .lock()
                    .ok()
                    .and_then(|b| b.get(&buf_key).map(|x| x.snapshot()))
                    .unwrap_or_default();

                let now_utc = chrono::Utc::now();
                let now_et = now_utc.with_timezone(&chrono_tz::US::Eastern);
                use chrono::TimeZone;
                use chrono::Timelike;
                let now_et_hour = now_et.hour();
                let now_et_minute = now_et.minute();
                let now_et_min = now_et_hour * 60 + now_et_minute;

                let is_halted_from_map = {
                    let key = action.symbol.to_ascii_uppercase();
                    let halts = market.halts.lock().unwrap_or_else(|p| p.into_inner());
                    // F2: resumed rows stay in the map — only active halts block.
                    halts
                        .get(&key)
                        .map(|h| h.resumed_at.is_none())
                        .unwrap_or(false)
                };

                let wm = {
                    let wms = market
                        .watchlist_metrics
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    wms.get(&action.symbol.to_ascii_uppercase()).cloned()
                };

                let scan_is_fresh = {
                    let market_state = state.market.lock().unwrap_or_else(|p| p.into_inner());
                    match market_state.scan_status.last_scan_ts {
                        Some(ts) => {
                            let age = chrono::Utc::now().timestamp().saturating_sub(ts);
                            age <= 900
                        }
                        None => false,
                    }
                };

                let hit = if scan_is_fresh {
                    let market_state = state.market.lock().unwrap_or_else(|p| p.into_inner());
                    market_state
                        .scan_results
                        .iter()
                        .find(|r| r.symbol.eq_ignore_ascii_case(&action.symbol))
                        .cloned()
                } else {
                    None
                };

                let news_sentiment = wm.as_ref().and_then(|m| m.news_sentiment);

                let (rvol, spread_bps, catalyst_score, is_halted, gap_pct) = if let Some(r) = hit {
                    (
                        r.rvol,
                        r.spread_bps,
                        Some(r.score),
                        r.catalyst_flags.halted || is_halted_from_map,
                        r.gap_percent,
                    )
                } else if let Some(m) = wm {
                    (
                        m.rvol,
                        m.spread_bps,
                        Some(m.catalyst_flags.score),
                        m.catalyst_flags.halted || is_halted_from_map,
                        m.gap_percent,
                    )
                } else {
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
                    (local_rvol, 0.0, None, is_halted_from_map, 0.0)
                };

                let regime = {
                    let m = state.market.lock().unwrap_or_else(|p| p.into_inner());
                    Some(m.regime)
                };

                let float_shares = state
                    .float_cache
                    .get()
                    .and_then(|fc| fc.lock().ok())
                    .and_then(|fc| fc.get(&action.symbol));

                let companion =
                    crate::smarttrend::sentinel::tournament_dispatch::resolve_companion_snapshot(
                        market,
                        &action.symbol,
                    );

                let prev_close = if candles.len() >= 2 {
                    candles[candles.len() - 2].close
                } else {
                    entry_price
                };

                let session_open = {
                    let mut open_val = entry_price;
                    if !candles.is_empty() {
                        let today_date = now_et.date_naive();
                        for c in &candles {
                            if let Some(c_et) =
                                chrono_tz::US::Eastern.timestamp_opt(c.time, 0).single()
                            {
                                if c_et.date_naive() == today_date {
                                    open_val = c.open;
                                    break;
                                }
                            }
                        }
                    }
                    open_val
                };

                let overrides = state
                    .dplus()
                    .map(|dp| dp.overrides.snapshot())
                    .unwrap_or_default();

                let ctx = crate::smarttrend::strategies::MarketContext {
                    symbol: action.symbol.clone(),
                    timeframe: if action.timeframe == "1m" { "1m" } else { "5m" },
                    price: entry_price,
                    prev_close,
                    session_open,
                    candles,
                    atr: atr_val,
                    rvol,
                    gap_pct,
                    spread_bps,
                    catalyst_score,
                    now_et_min,
                    now_et_hour,
                    now_et_minute,
                    regime,
                    is_halted,
                    companion,
                    float_shares,
                    news_sentiment,
                    overrides,
                };

                let strategy_size = strat.position_size(&ctx, sub_wallet.to_f64().unwrap_or(0.0));
                if strategy_size > 0.0 && strategy_size.is_finite() {
                    let pre = size_usd;
                    size_usd = size_usd.min(strategy_size);

                    if let Some(dp) = state.dplus() {
                        if let Ok(ar) = dp.adaptive_risk.lock() {
                            let mult = ar.size_multiplier(strategy_id);
                            if mult < 1.0 {
                                let size_pre_mult = size_usd;
                                size_usd *= mult;
                                tracing::info!(
                                    component = "bot",
                                    symbol = %action.symbol,
                                    strategy = %strategy_id,
                                    size_pre = size_pre_mult,
                                    size_post = size_usd,
                                    size_mult = mult,
                                    "D-PLUS size multiplier applied to size_usd"
                                );
                            }
                        }
                    }

                    // S29.3 — Regulatorul Dinamic de Spread Penalty pentru APEX
                    if strategy_id == crate::smarttrend::strategies::StrategyId::Apex
                        && spread_bps > 40.0
                    {
                        let spread_mult = (40.0 / spread_bps).clamp(0.1, 1.0);
                        let size_pre_penalty = size_usd;
                        size_usd *= spread_mult;
                        tracing::info!(
                            component = "bot",
                            symbol = %action.symbol,
                            strategy = %strategy_id,
                            spread_bps,
                            spread_mult,
                            size_pre = size_pre_penalty,
                            size_post = size_usd,
                            "S29.3 Spread penalty sizing multiplier applied to size_usd"
                        );
                    }

                    // SPY Macro Bearish Trend Sizer: scale down Long size by 50% when SPY is trading below its 30-min VWAP
                    if action.direction == crate::state::Direction::Long && is_spy_bearish(market) {
                        let size_pre = size_usd;
                        size_usd *= 0.50;
                        tracing::info!(
                            component = "bot",
                            symbol = %action.symbol,
                            size_pre,
                            size_post = size_usd,
                            "SPY macro bearish trend detected — Long position size scaled down by 50%"
                        );
                    }

                    // v2.5.0 NUCLEAR (C3 + C3.1) — global hard-cap per
                    // Tournament trade.
                    //
                    // C3 (2026-05-14): live evidence LESL -$224 single-
                    // trade loss → introduced $40 cap.
                    //
                    // C3.1 (2026-05-15 post-mortem): the cap was
                    // mathematically inactive because:
                    //   - When `atr_val == 0` (very common during low-
                    //     volume bars / first 14 candles), the fallback
                    //     was `entry_price * 0.005` (0.5% stop). At an
                    //     entry of $20 that is $0.10. Cap = $40 / $0.10
                    //     × $20 = $8,000 → typical $600 sized trade
                    //     never tripped the cap.
                    //   - Result Friday: LESL closed -$125, RPGL -$72,
                    //     ORGN -$30 — all > supposed $40 cap.
                    //
                    // C3.1 fix (this block):
                    //   1. Lower the ceiling: $40 → $20 hard cap.
                    //   2. Lift the ATR fallback floor: 0.5% → 1.5%.
                    //      A 1.5% stop on a $20 entry = $0.30 → cap
                    //      = $20 / $0.30 × $20 = $1,333. With size
                    //      $600 the cap is INACTIVE only when stops
                    //      are <0.7% of price (i.e. extremely tight,
                    //      genuinely bounded losses). Otherwise it
                    //      kicks in and bounds qty so the broker
                    //      cannot fill more than $20 worth of risk
                    //      per trade.
                    const MAX_LOSS_PER_TRADE_USD: f64 = 20.0;
                    if entry_price > 0.0 && size_usd > 0.0 {
                        let approx_stop_dist = (atr_val * 1.0).max(entry_price * 0.015);
                        if approx_stop_dist > 0.0 {
                            let max_size_by_loss =
                                MAX_LOSS_PER_TRADE_USD / approx_stop_dist * entry_price;
                            if max_size_by_loss < size_usd {
                                tracing::info!(
                                    component = "bot",
                                    symbol = %action.symbol,
                                    strategy = %strategy_id,
                                    size_pre = size_usd,
                                    size_capped = max_size_by_loss,
                                    approx_stop_dist,
                                    "Tournament size capped by MAX_LOSS_PER_TRADE_USD=${}",
                                    MAX_LOSS_PER_TRADE_USD as i32
                                );
                                size_usd = max_size_by_loss;
                            }
                        }
                    }

                    tracing::info!(
                        component = "bot",
                        symbol = %action.symbol,
                        strategy = %strategy_id,
                        sub_wallet = %sub_wallet,
                        strategy_size,
                        size_pre = pre,
                        size_post = size_usd,
                        "Tournament position sizing applied"
                    );
                } else {
                    // v2.5.0 — Strategy::position_size() == 0.0 (or NaN/Inf)
                    // is the trait's documented signal for "refuse this trade
                    // size-side". Without this branch the bot fell back to
                    // legacy sizing and submitted a Tournament-attributed
                    // entry at full bot-wallet allocation against the
                    // strategy's explicit veto — fund-leak risk + Tournament
                    // attribution recording the P/L against a strategy that
                    // refused the trade. Cross-validated by peer audits 2026-05-12.
                    tracing::info!(
                        component = "bot",
                        symbol = %action.symbol,
                        strategy = %strategy_id,
                        sub_wallet = %sub_wallet,
                        strategy_size,
                        "Tournament entry refused — strategy_size invalid (0 / NaN / inf)"
                    );
                    metrics.record_entry_blocked();
                    release_pending(state);
                    return;
                }
            }
        }
    }

    // ── PRIME F2b — conviction-scaled sizing ──
    //
    // When the action carries a fused conviction (PRIME arbitration
    // winner), the risk budget scales with it: 0.5× at threshold,
    // linear up to 1.5× at conviction 1.0 — capped by the bot balance
    // so scaling can never buy more than the wallet holds. Empirical
    // driver: flat sizing paid full risk on marginal signals (avg win
    // $4.20 vs avg loss $9.20). Legacy actions (`conviction: None`)
    // are untouched.
    if let Some(cv) = action.conviction {
        let threshold = state
            .dplus()
            .and_then(|dp| dp.prime.lock().ok())
            .map(|p| p.weights.threshold)
            .unwrap_or(0.55);
        let mult = crate::smarttrend::prime::capital::conviction_multiplier(cv, threshold);
        if mult > 0.0 {
            let pre = size_usd;
            size_usd = (size_usd * mult).min(max_size);
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                conviction = format!("{:.3}", cv),
                multiplier = format!("{:.2}", mult),
                size_pre = pre,
                size_post = size_usd,
                "PRIME conviction sizing applied"
            );
        }
    }

    // ── P4 — correlation-aware capital (portfolio heat) ──
    //
    // The lead-lag graph already measures co-movement between the
    // streamed symbols. When the incoming entry is strongly correlated
    // with something ALREADY open, the two positions are one
    // concentrated bet wearing two tickers — the size gets a haircut
    // (down to 0.4× at |corr| ≥ 0.85). Lock discipline: trading
    // (Tier A) is taken and dropped, THEN the standalone causal mutex —
    // never overlapped.
    {
        let open_symbols: Vec<String> = {
            let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading
                .positions
                .iter()
                .filter(|p| p.symbol != action.symbol)
                .map(|p| p.symbol.clone())
                .collect()
        };
        if !open_symbols.is_empty() {
            let max_abs_corr = state
                .dplus()
                .and_then(|dp| dp.causal.lock().ok().map(|c| c.snapshot()))
                .map(|graph| {
                    graph
                        .edges
                        .iter()
                        .filter(|e| {
                            (e.leader == action.symbol && open_symbols.contains(&e.lagger))
                                || (e.lagger == action.symbol && open_symbols.contains(&e.leader))
                        })
                        .map(|e| e.corr.abs())
                        .fold(0.0_f64, f64::max)
                })
                .unwrap_or(0.0);
            let haircut = crate::smarttrend::prime::capital::correlation_haircut(max_abs_corr);
            if haircut < 1.0 {
                let pre = size_usd;
                size_usd *= haircut;
                tracing::info!(
                    component = "bot",
                    symbol = %action.symbol,
                    max_abs_corr = format!("{:.2}", max_abs_corr),
                    haircut = format!("{:.2}", haircut),
                    size_pre = pre,
                    size_post = size_usd,
                    "P4 correlation haircut — clustered with open position(s)"
                );
            }
        }
    }

    // ── OMEGA (Sesiunea 3) — confidence sizing + adverse veto ──
    //
    // Cold-start contract: when the predictor registry has no warm
    // models (samples_trained < COLD_START_MIN_SAMPLES) the lazy
    // helper returns the default decision (1.0× size, no veto)
    // without even building a MarketContext. So this block is
    // strictly additive — bot behaviour identical to v2.1 until
    // MemoryDb accumulates enough closed setups to trigger
    // automated retraining at 21:00 ET.
    let mut omega_decision = crate::ml::predictor_loop::OmegaEntryDecision::default();
    if let Some(dplus) = state.dplus() {
        let intended_side: f64 = match action.direction {
            crate::state::Direction::Long => 1.0,
            crate::state::Direction::Short => -1.0,
        };
        let symbol = action.symbol.clone();
        let timeframe = action.timeframe.clone();
        let market_for_ctx = Arc::clone(market);
        omega_decision =
            crate::ml::predictor_loop::evaluate_omega_at_entry_lazy(dplus, intended_side, || {
                // Build a minimal MarketContext from the live
                // candle ring buffer. The strategy framework
                // would normally provide a richer context — for
                // OMEGA-at-bot the close+volume series is enough.
                let buf_key = format!("{symbol}:{timeframe}");
                let buffers = market_for_ctx
                    .candle_buffers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let candles = buffers
                    .get(&buf_key)
                    .map(|b| b.snapshot())
                    .unwrap_or_default();
                if candles.is_empty() {
                    return None;
                }
                let overrides = dplus.overrides.snapshot();
                Some(crate::smarttrend::strategies::MarketContext {
                    symbol: symbol.clone(),
                    timeframe: if timeframe == "1m" { "1m" } else { "5m" },
                    price: entry_price,
                    prev_close: entry_price,
                    session_open: entry_price,
                    candles,
                    atr: atr_val,
                    rvol: 1.0,
                    gap_pct: 0.0,
                    spread_bps: 0.0,
                    catalyst_score: None,
                    now_et_min: 0,
                    now_et_hour: 0,
                    now_et_minute: 0,
                    regime: None,
                    is_halted: false,
                    companion: None,
                    float_shares: None,
                    news_sentiment: None,
                    overrides,
                })
            });
        // v2.5.0 NUCLEAR — Tournament strategies are explicit, deterministic
        // and have their own internal risk envelopes (per-strategy
        // `max_loss_pct`, `atr_stop_mult`, `sub_wallet_cap_pct`). OMEGA
        // is a *learned* predictor on the legacy momentum FSM feature
        // set; vetoing a Tournament signal because OMEGA's confidence
        // dropped below threshold means a probabilistic ML layer that
        // wasn't trained on these setups is silently killing them.
        // Per user directive 2026-05-12 ("nuclear risk maxim, totul
        // sau nimic, bani virtuali"): OMEGA veto applies ONLY to
        // legacy `strategy_id.is_none()` momentum entries. Tournament
        // signals also bypass the size multiplier — strategies size
        // themselves via `position_size()` above; OMEGA shouldn't
        // shrink an already-sub-wallet-bounded position.
        if action.strategy_id.is_none() && omega_decision.vetoed {
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                diag = %omega_decision.diag,
                "Entry BLOCKED — OMEGA adverse veto"
            );
            let _ = app.emit_all(
                "bot-blocked",
                serde_json::json!({
                    "symbol": action.symbol,
                    "reason": format!("OMEGA veto ({})", omega_decision.diag),
                }),
            );
            metrics.record_entry_blocked();
            release_pending(state);
            return;
        }
        let pre = size_usd;
        if action.strategy_id.is_none() {
            size_usd *= omega_decision.size_multiplier;
        }
        if !omega_decision.diag.is_empty() {
            tracing::debug!(
                component = "bot",
                symbol = %action.symbol,
                size_pre = pre,
                size_post = size_usd,
                diag = %omega_decision.diag,
                "OMEGA size modifier applied"
            );
        }
    }

    // ── FRONTIER (Sesiunea 4) — Monte-Carlo P&L gate ──
    //
    // Forward-simulates 1024 GBM paths × 20 bars calibrated on the
    // symbol's ATR/price ratio. Vetoes negative-expected-value
    // entries and shrinks size when target probability is too low.
    // Compute SL / TP up front so the gate evaluates the *actual*
    // bracket levels the order will use (re-used at line ~745
    // when the bracket is finally constructed). Cold-start contract:
    // when `monte_carlo_gate_enabled = false`, the entire block is
    // skipped and the bot behaves identically to v2.3.5.
    let stop_loss = if let Some(sl) = action.stop_loss {
        sl
    } else {
        compute_stop_loss(entry_price, atr_val, bracket_mult, action.direction)
    };
    let take_profit = if let Some(tp) = action.take_profit {
        tp
    } else {
        compute_take_profit(entry_price, atr_val, bracket_mult, action.direction)
    };
    // S28 — Swing-Intent bracket widening.
    //
    // When the PDT gate allowed this entry only because we're inside the
    // late-RTH swing window, we deliberately widen the bracket so it
    // *mechanically cannot trip* during the remaining intraday window
    // (≤ 1h) under typical volatility. Geometry:
    //   * TP = entry × 1.20 (+20% — would be a major move in 1h)
    //   * SL = entry × 0.85 (-15% safety net for overnight gap-down)
    // The position is intentionally held across the close and exited the
    // next session by `flat_by_close`-equivalent logic (deferred exit at
    // RTH open) or by sentinel exit signals on the next bar evaluation.
    //
    // Trade-off accepted: if a sudden +20% rip happens in the last
    // 60 minutes (rare but not impossible on small-cap catalysts), the
    // TP will fill intraday → this becomes day-trade #4 → broker flags
    // account PDT. Operator accepts this risk because:
    //   1. Paper account — PDT flag has zero real cost.
    //   2. A +20% intraday move on a $25 sized trade = +$5 P&L,
    //      meaningful vs. baseline +/- $1/day so worth the risk.
    let (stop_loss, take_profit) = if is_swing_intent {
        use rust_decimal::Decimal;
        let entry_dec = Decimal::from_f64(entry_price).unwrap_or(Decimal::ZERO);
        let one_twenty = Decimal::from_f64(1.20).unwrap_or(Decimal::ONE);
        let eighty_five = Decimal::from_f64(0.85).unwrap_or(Decimal::ONE);
        let (sl_swing, tp_swing) = match action.direction {
            crate::state::Direction::Long => (entry_dec * eighty_five, entry_dec * one_twenty),
            // Short: SL above entry (× 1.15), TP below (× 0.80)
            crate::state::Direction::Short => {
                let one_fifteen = Decimal::from_f64(1.15).unwrap_or(Decimal::ONE);
                let eighty = Decimal::from_f64(0.80).unwrap_or(Decimal::ONE);
                (entry_dec * one_fifteen, entry_dec * eighty)
            }
        };
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            sl_orig = %stop_loss,
            tp_orig = %take_profit,
            sl_swing = %sl_swing,
            tp_swing = %tp_swing,
            "S28 SWING-INTENT bracket widened — intraday trip mechanically improbable"
        );
        (sl_swing, tp_swing)
    } else {
        (stop_loss, take_profit)
    };
    {
        let (mc_enabled, mc_gate) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            let ep = &cfg.evolution_protocol;
            (
                ep.monte_carlo_gate_enabled,
                crate::ml::monte_carlo::McGate {
                    min_expected_pct: ep.mc_min_expected_pct,
                    max_p_loss: ep.mc_max_p_loss,
                    shrink_p_hit_target: Some(ep.mc_shrink_p_hit_target),
                    shrink_floor: ep.mc_shrink_floor,
                },
            )
        };
        // v2.5.0 NUCLEAR — Monte Carlo gate skips Tournament entries.
        // The gate is a GBM forward-sim calibrated on legacy momentum
        // ATR-stop bracket geometry; Tournament strategies build their
        // own bracket (per-strategy `atr_stop_mult`/`atr_target_mult`)
        // and the GBM assumption (lognormal returns, constant vol)
        // mis-prices mean-reverting and pair strategies hardcore. Per
        // user directive 2026-05-12: nuclear risk, virtual capital,
        // protect nothing — strategy-internal risk envelopes are the
        // only authority on Tournament trades.
        if mc_enabled && action.strategy_id.is_none() {
            let side: f64 = match action.direction {
                crate::state::Direction::Long => 1.0,
                crate::state::Direction::Short => -1.0,
            };
            let mc_inputs = crate::ml::monte_carlo::build_inputs_from_entry(
                entry_price,
                atr_val,
                take_profit.to_f64().unwrap_or(entry_price),
                stop_loss.to_f64().unwrap_or(entry_price),
                side,
                crate::ml::monte_carlo::DEFAULT_ROUND_TRIP_BPS,
            );
            let mc_cfg = crate::ml::monte_carlo::GbmConfig::default();
            let mc_res = crate::ml::monte_carlo::simulate(&mc_inputs, &mc_cfg);
            let decision = mc_gate.evaluate(&mc_res);
            tracing::debug!(
                component = "bot",
                symbol = %action.symbol,
                mean_return = mc_res.mean_return,
                p_loss = mc_res.p_loss,
                p_hit_target = mc_res.p_hit_target,
                p_hit_stop = mc_res.p_hit_stop,
                elapsed_us = mc_res.elapsed_us,
                "MC simulation"
            );
            match decision {
                crate::ml::monte_carlo::McDecision::Veto { reason } => {
                    tracing::info!(
                        component = "bot",
                        symbol = %action.symbol,
                        reason = %reason,
                        "Entry BLOCKED — Monte Carlo gate veto"
                    );
                    let _ = app.emit_all(
                        "bot-blocked",
                        serde_json::json!({
                            "symbol": action.symbol,
                            "reason": format!("MC veto ({reason})"),
                        }),
                    );
                    metrics.record_entry_blocked();
                    release_pending(state);
                    return;
                }
                crate::ml::monte_carlo::McDecision::Shrink { factor, reason } => {
                    let pre = size_usd;
                    size_usd *= factor;
                    tracing::info!(
                        component = "bot",
                        symbol = %action.symbol,
                        size_pre = pre,
                        size_post = size_usd,
                        reason = %reason,
                        "MC shrink applied"
                    );
                }
                crate::ml::monte_carlo::McDecision::Accept => {}
            }
        }
    }

    // ── FRONTIER (Sesiunea 4) — Contextual bandit final-stage sizer ──
    //
    // The bandit reads OMEGA's confidence proxy and picks a size
    // multiplier ∈ {0.5, 0.75, 1.0} from a Beta-Bernoulli posterior
    // it learned from past trade outcomes. Cold-start contract:
    // until each `(regime, confidence_bucket)` combo accumulates
    // `bandit_activation_threshold` closed trades, the bandit
    // returns 1.0× — bot behaviour identical to v2.3.9.
    //
    // We stash the chosen arm + context in `state.bandit_pending`
    // keyed by symbol so the close handler can call `bandit.update()`
    // with the same bucket. The bandit can ONLY shrink size, never
    // grow it (max arm = 1.0×) — defensive choice given OMEGA + MC
    // already provided a blessed size.
    {
        let real_regime = state
            .market
            .lock()
            .ok()
            .map(|m| m.regime)
            .unwrap_or(crate::smarttrend::strategies::MarketRegime::Calm);
        let real_confidence = omega_decision.confidence;

        let hawkes_intensity = {
            let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            let key = crate::state::fsm_key(&action.symbol, &action.timeframe);
            st.fsm.get(&key).map(|f| f.hawkes_intensity).unwrap_or(0.05)
        };

        let bandit_decision = {
            let bandit = state.bandit.lock().unwrap_or_else(|p| p.into_inner());
            bandit.select(real_regime, real_confidence, hawkes_intensity)
        };

        // Calculate portfolio stop risk (percentage of Net Asset Value) for CVaR check.
        // Tier-A ascending: smarttrend(3) then trading(4). Drop each before next.
        let portfolio_stop_risk_pct = {
            let nav = {
                let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                st.account.allocated_capital.to_f64().unwrap_or(100_000.0)
            };
            let total_risk: f64 = {
                let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                trading
                    .positions
                    .iter()
                    .map(|p| {
                        let qty = p.qty.to_f64().unwrap_or(0.0);
                        let entry = p.avg_entry.to_f64().unwrap_or(0.0);
                        let stop = if p.stop_loss != 0.0 {
                            p.stop_loss
                        } else {
                            entry
                        };
                        (entry - stop).abs() * qty
                    })
                    .sum()
            };
            if nav > 0.0 {
                (total_risk / nav) * 100.0
            } else {
                100.0
            }
        };

        let mut multiplier = bandit_decision.multiplier;
        if multiplier > 1.0 && portfolio_stop_risk_pct >= 1.5 {
            multiplier = 1.0;
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                risk = format!("{:.2}%", portfolio_stop_risk_pct),
                "Bandit sizing booster CLAMPED to 1.0x due to portfolio stop risk >= 1.5%"
            );
        }

        if action.strategy_id.is_none() && !bandit_decision.cold_start {
            if (multiplier - 1.0).abs() > 1e-9 {
                let pre = size_usd;
                size_usd *= multiplier;
                tracing::info!(
                    component = "bot",
                    symbol = %action.symbol,
                    size_pre = pre,
                    size_post = size_usd,
                    arm = bandit_decision.arm_idx,
                    multiplier,
                    diag = %bandit_decision.diag,
                    "Bandit size multiplier applied"
                );
            }
        } else {
            tracing::debug!(
                component = "bot",
                symbol = %action.symbol,
                arm = bandit_decision.arm_idx as i64,
                multiplier = bandit_decision.multiplier,
                cold_start = bandit_decision.cold_start,
                diag = %bandit_decision.diag,
                "Bandit decision (no sizing change)"
            );
        }

        if bandit_decision.arm_idx < crate::ml::bandit::NUM_ARMS {
            let ctx = crate::ml::bandit::BanditPendingContext {
                arm_idx: bandit_decision.arm_idx,
                regime: real_regime,
                confidence: real_confidence,
                recorded_at: chrono::Utc::now().timestamp(),
            };
            let mut pending = state
                .bandit_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            pending.insert(action.symbol.clone(), ctx);
        }
    }

    // ── Bayesian Decision Fusion Layer & OFI-Gating ──
    let ofi_state = market.ofi_state.load();

    // 1. Spread Gating
    //
    // PRIME F0.4 — per-strategy spread cap. The old global 15 bps was
    // tuned for liquid RTH momentum names and silently blocked the
    // wide-spread specialists (HaltResume allows 100 bps, ExtendedNews
    // 250 bps in `evaluate_entry` — then died here). Tournament
    // signals now use their own strategy's `max_spread_bps()`; the
    // legacy FSM path keeps the original 15 bps.
    let current_spread_bps = ofi_state.spread_bps;
    let live_rvol = {
        let metrics = market.watchlist_metrics.lock().unwrap();
        metrics.get(&action.symbol).map(|m| m.rvol).unwrap_or(1.0)
    };
    let mut spread_cap_bps = action
        .strategy_id
        .and_then(crate::smarttrend::strategies::by_id)
        .map(|s| s.max_spread_bps())
        .unwrap_or(15.0);

    // S29.3 — Regulatorul Dinamic de Spread pentru APEX în entry gating
    if action.strategy_id == Some(crate::smarttrend::strategies::StrategyId::Apex)
        && action.price < 5.0
        && live_rvol >= 5.0
    {
        spread_cap_bps = 60.0;
    }

    // F3 — Frontier owns spread via per-symbol MicroBook (fail-closed).
    // Global ofi_state.spread_bps is chart-symbol only and must not gate it.
    let frontier_spread =
        if action.strategy_id == Some(crate::smarttrend::strategies::StrategyId::Frontier) {
            let key = action.symbol.to_ascii_uppercase();
            let book = market.micro.lock().unwrap_or_else(|p| p.into_inner());
            book.get(&key).and_then(|s| {
                if s.fresh && s.spread_bps > 0.0 {
                    Some(s.spread_bps)
                } else {
                    None
                }
            })
        } else {
            None
        };
    let spread_blocked = match frontier_spread {
        Some(sp) => sp > spread_cap_bps,
        None if action.strategy_id == Some(crate::smarttrend::strategies::StrategyId::Frontier) => {
            true // fail-closed: no micro → no Frontier entry
        }
        None => current_spread_bps > spread_cap_bps,
    };
    if spread_blocked {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            spread_bps = frontier_spread.unwrap_or(current_spread_bps),
            cap_bps = spread_cap_bps,
            "Entry BLOCKED — spread too wide"
        );
        crate::ops::decision_log::log(crate::ops::decision_log::DecisionRecord::vetoed(
            &action.symbol,
            &action.timeframe,
            action.strategy_id.map(|s| s.as_str()).unwrap_or("fsm"),
            match action.direction {
                crate::state::Direction::Long => "long",
                crate::state::Direction::Short => "short",
            },
            action.score,
            "spread",
            &format!("{current_spread_bps:.1}bps > {spread_cap_bps:.0}bps"),
            action.bar_time,
            action.price,
            "unknown",
        ));
        let _ = app.emit_all("bot-blocked", serde_json::json!({
            "symbol": action.symbol,
            "reason": format!("Spread too wide ({current_spread_bps:.1} bps > {spread_cap_bps:.0} bps)"),
        }));
        metrics.record_entry_blocked();
        release_pending(state);
        return;
    }

    // 2. Probabilities calculation
    let p_fsm = (action.score / 100.0).clamp(0.0, 1.0);

    let p_omega_regime = if let Some(dplus) = state.dplus() {
        let registry = dplus.omega.load();
        if let Some(transformer) = registry.get("regime_xfmr") {
            if let Some(probs) = transformer.last_probabilities() {
                // Calm (0) + Trending (1) probability
                (probs[0] + probs[1]) as f64
            } else {
                0.5
            }
        } else {
            0.5
        }
    } else {
        0.5
    };

    let p_ofi = {
        let ratio = ofi_state.imbalance_ratio;
        match action.direction {
            crate::state::Direction::Long => (ratio + 1.0) / 2.0,
            crate::state::Direction::Short => (1.0 - ratio) / 2.0,
        }
    }
    .clamp(0.0, 1.0);

    let p_causal = if let Some(dplus) = state.dplus() {
        let causal_lock = dplus.causal.lock().unwrap_or_else(|p| p.into_inner());
        let leaders = causal_lock.leaders_for(&action.symbol);
        if !leaders.is_empty() {
            let mut best_leader_edge: Option<crate::smarttrend::causal::LeadLagEdge> = None;
            for edge in &leaders {
                if best_leader_edge.is_none()
                    || edge.corr.abs() > best_leader_edge.as_ref().unwrap().corr.abs()
                {
                    best_leader_edge = Some(edge.clone());
                }
            }
            if let Some(edge) = best_leader_edge {
                if let Some(change) = causal_lock.recent_change(&edge.leader, 10) {
                    let alignment = change * edge.corr;
                    let k = 1000.0;
                    let p = 1.0 / (1.0 + (-k * alignment).exp());
                    p.clamp(0.0, 1.0)
                } else {
                    0.5
                }
            } else {
                0.5
            }
        } else {
            0.5
        }
    } else {
        0.5
    };

    let p_memory = if let Some(dplus) = state.dplus() {
        if let Some(features) = &action.setup_features {
            if let Ok(Some(post)) = dplus.memory_db.posterior_expectancy(features) {
                post.win_rate
            } else {
                0.5
            }
        } else {
            0.5
        }
    } else {
        0.5
    };

    let joint_p_win = (p_fsm + p_omega_regime + p_ofi + p_causal + p_memory) / 5.0;

    tracing::info!(
        component = "bot",
        symbol = %action.symbol,
        p_fsm,
        p_omega_regime,
        p_ofi,
        p_causal,
        p_memory,
        joint_p_win,
        "Bayesian Decision Fusion complete"
    );

    let sized_usd_opt =
        evaluate_bayesian_fusion(p_fsm, p_omega_regime, p_ofi, p_causal, p_memory, size_usd);
    if sized_usd_opt.is_none() {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            joint_p_win,
            "Entry BLOCKED — Bayesian joint probability below 0.45"
        );
        let _ = app.emit_all(
            "bot-blocked",
            serde_json::json!({
                "symbol": action.symbol,
                "reason": format!("Bayesian joint P(Win) {:.2} < 0.45", joint_p_win),
            }),
        );
        metrics.record_entry_blocked();
        release_pending(state);
        return;
    }
    let new_size_usd = sized_usd_opt.unwrap();

    // Adaptive continuous sizing
    let pre_size = size_usd;
    size_usd = new_size_usd;
    tracing::info!(
        component = "bot",
        symbol = %action.symbol,
        size_pre = pre_size,
        size_post = size_usd,
        "Adaptive continuous sizing applied via joint probability"
    );

    // ── Pilonul 2: Scutul de Risc prin Covarianță (CVaR Portfolio Gate) ──
    match evaluate_portfolio_var_gate(state, market, &action.symbol, size_usd, &action.timeframe) {
        Err(e) => {
            tracing::info!(
                component = "risk_shield",
                symbol = %action.symbol,
                error = %e,
                "Entry BLOCKED — Covariance VaR Gate veto applied"
            );
            let _ = app.emit_all(
                "bot-blocked",
                serde_json::json!({
                    "symbol": action.symbol,
                    "reason": format!("Risk Shield: {}", e),
                }),
            );
            metrics.record_entry_blocked();
            release_pending(state);
            return;
        }
        Ok(scaled_size) => {
            if scaled_size < size_usd {
                tracing::info!(
                    component = "risk_shield",
                    symbol = %action.symbol,
                    size_pre = size_usd,
                    size_post = scaled_size,
                    "Covariance VaR Gate scaled down size"
                );
                size_usd = scaled_size;
            }
        }
    }

    let mut qty = compute_qty(size_usd, entry_price);

    // S15.12: Alpaca rejects `bracket + fractional qty` (HTTP 422 code 42210000:
    // "fractional orders must be simple orders"). Since S15.11 made bracket
    // mandatory for AUTO entries, we truncate qty to an integer here. This also
    // subsumes the NotFractionable metadata check below for assets that don't
    // support fractional trading at all.
    let pre_trunc_qty = qty;
    qty = qty.trunc();
    if qty != pre_trunc_qty {
        tracing::info!(
            component = "bot",
            symbol = %action.symbol,
            original = %pre_trunc_qty,
            truncated = %qty,
            "Qty truncated to integer (Alpaca bracket requirement)"
        );
    }

    let min_qty = Decimal::ONE;
    if qty < min_qty {
        // S45 (2026-05-29) — small-account "concentrate" 1-share floor.
        //
        // With a $300 cash/paper account, notional sizing routinely
        // computes < 1 share for $13–50 names (live: $11.25 / $46 = 0.24),
        // and Alpaca bracket orders require WHOLE shares. Truncating to 0
        // and skipping silently killed every Tournament entry (live
        // 2026-05-29: 50 catalyst_frontrun signals → 0 positions opened).
        //
        // Realistic small-account behaviour: buy exactly 1 whole share
        // when (a) it is affordable — the buying-power gate just below
        // enforces this — and (b) a single share's worst-case stop loss
        // stays within the per-trade risk bound. For a $1500+ name a 1.5%
        // stop already exceeds the bound, so we still (correctly) skip.
        const MAX_LOSS_PER_TRADE_USD: f64 = 20.0;
        let approx_stop_dist = (atr_val * 1.0).max(entry_price * 0.015);
        if entry_price > 0.0 && approx_stop_dist > 0.0 && approx_stop_dist <= MAX_LOSS_PER_TRADE_USD
        {
            qty = Decimal::ONE;
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                size_usd,
                price = entry_price,
                one_share_loss = approx_stop_dist,
                "Sub-1-share notional floored to 1 whole share (small-account concentrate)"
            );
        } else {
            tracing::warn!(
                component = "bot",
                symbol = %action.symbol,
                size_usd,
                price = entry_price,
                one_share_loss = approx_stop_dist,
                "Skipping entry — 1 share exceeds per-trade risk bound (price too high for $300 account)"
            );
            metrics.record_entry_blocked();
            release_pending(state);
            return;
        }
    }

    let est_cost_f64 = qty.to_f64().unwrap_or(0.0) * entry_price;

    // ── Gate 0c: broker buying power (S02) ──
    if let Some(reason) = buying_power_gate_check(est_cost_f64, buying_power) {
        metrics.record_skipped(&reason);
        emit_blocked_event(app, &action.symbol, &reason);
        release_pending(state);
        return;
    }

    // ── Gate 0d: fractionable check for sized qty (S02) ──
    // `asset_opt` is from Gate 0a above. order_exec also performs this check
    // (catching manual orders too) — duplicating here gives a dimensional
    // counter for bot-driven entries and an early bot-blocked event.
    if let Some(a) = asset_opt.as_ref() {
        if !a.fractionable && qty.fract() != Decimal::ZERO {
            metrics.record_skipped(&SkipReason::NotFractionable);
            emit_blocked_event(app, &action.symbol, &SkipReason::NotFractionable);
            release_pending(state);
            return;
        }
    }
    // Note: if asset_opt is None for a Long entry, Gate 0a doesn't reject
    // (short_gate_check returns None for Long). order_exec.rs::send_order
    // will reject with `AssetUnknown` Validation error a few lines below.

    // ── Gate 0e: circuit breaker (S03) ──
    {
        let breaker = { state.risk.lock().unwrap_or_else(|p| p.into_inner()).breaker };
        let caution_action = {
            state
                .config
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .risk
                .caution_action
        };

        match breaker {
            BreakerLevel::Panic | BreakerLevel::Halt => {
                metrics.record_skipped(&SkipReason::BreakerHalt);
                emit_blocked_event(app, &action.symbol, &SkipReason::BreakerHalt);
                release_pending(state);
                return;
            }
            BreakerLevel::Caution => match caution_action {
                CautionAction::Block => {
                    metrics.record_skipped(&SkipReason::CautionBlock);
                    emit_blocked_event(app, &action.symbol, &SkipReason::CautionBlock);
                    release_pending(state);
                    return;
                }
                CautionAction::Scale50 => {
                    // S15.12: keep integer for bracket compatibility
                    qty = (qty * Decimal::from_f64(0.50).unwrap_or(Decimal::ZERO)).trunc();
                    tracing::info!(component = "bot", symbol = %action.symbol, %qty, "Caution: qty scaled to 50% (int)");
                }
                CautionAction::Scale25 => {
                    qty = (qty * Decimal::from_f64(0.25).unwrap_or(Decimal::ZERO)).trunc();
                    tracing::info!(component = "bot", symbol = %action.symbol, %qty, "Caution: qty scaled to 25% (int)");
                }
            },
            BreakerLevel::Normal => {}
        }
    }

    if qty < min_qty {
        tracing::warn!(component = "bot", symbol = %action.symbol, %qty, "Qty scaled below 1 share by caution; skipping");
        release_pending(state);
        return;
    }

    // Recompute est_cost after potential caution scaling (S15.13: live price)
    let est_cost =
        Decimal::from_f64(qty.to_f64().unwrap_or(0.0) * entry_price).unwrap_or(Decimal::ZERO);

    // S15.11+S15.13: bracket SL+TP based on live tick — already
    // computed above for the Monte Carlo gate; reuse those values
    // here so the bracket levels and the gate evaluation always
    // reference the same SL/TP. Alpaca rejects single-leg bracket
    // orders (HTTP 422 code 40010001) and rejects brackets where
    // SL/TP land on the wrong side of `base_price` (HTTP 422 code
    // 42210000).

    // Debit bot wallet before sending order
    // S05: release smarttrend(3) before any emit_all
    let wallet_err: Option<String> = {
        let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        // S16.0 — use `debit()` rather than `withdraw()` for trade
        // entries. Trade debits are internal cash-flow events; they
        // must NOT inflate `total_withdrawn` (which the UI uses to
        // compute lifetime wallet P/L). The historical use of
        // `withdraw()` produced spurious +$25K-style "P/L" displays
        // because every entry-then-fail cycle bumped the counter.
        match st.bot_wallet.debit(est_cost) {
            Err(e) => Some(e),
            Ok(()) => {
                tracing::info!(
                    component = "bot",
                    symbol = %action.symbol,
                    %est_cost,
                    balance = %st.bot_wallet.balance,
                    "Debited from bot wallet",
                );
                None
            }
        }
    };
    if let Some(e) = wallet_err {
        tracing::warn!(component = "bot", symbol = %action.symbol, error = %e, "Wallet withdraw failed");
        let _ = app.emit_all(
            "toast-warning",
            serde_json::json!({
                "message": format!("BOT: {e}"),
            }),
        );
        metrics.record_entry_blocked();
        release_pending(state);
        return;
    }
    let _ = app.emit_all("wallet-changed", serde_json::json!({}));

    // v2.5.0 — strategy attribution. When the action originates from
    // the Tournament dispatcher (`strategy_id == Some(_)`) we tag the
    // OrderRequest source as `"s2:{strategy}"` so the D-PLUS close hook
    // (`dplus::on_trade_close`) routes the realised P/L to:
    //   - Tournament.record_trade_close  (sub-wallet accounting)
    //   - AdaptiveRisk.record_outcome    (per-strategy size scaling)
    //   - MemoryDb.record_close           (per-setup ML samples)
    // Legacy momentum signals (`strategy_id == None`) keep source="bot"
    // bit-for-bit, preserving pre-v2.5 attribution semantics.
    let source_tag = match action.strategy_id {
        Some(id) => crate::smarttrend::dplus::source_for_strategy(id),
        None => "bot".into(),
    };

    let is_apex = action.strategy_id == Some(crate::smarttrend::strategies::StrategyId::Apex);
    let (order_type, limit_price) = if is_apex {
        let offset = (0.10 * atr_val).clamp(0.01, 0.15);
        let limit = match action.direction {
            crate::state::Direction::Long => entry_price + offset,
            crate::state::Direction::Short => entry_price - offset,
        };
        (
            "limit".to_string(),
            Some(
                rust_decimal::Decimal::from_f64(limit)
                    .unwrap_or(rust_decimal::Decimal::ZERO)
                    .round_dp(2),
            ),
        )
    } else {
        ("market".to_string(), None)
    };

    let order = OrderRequest {
        symbol: action.symbol.clone(),
        side: action.direction,
        qty,
        stop_loss: Some(stop_loss),
        take_profit: Some(take_profit),
        source: source_tag,
        deducted_amount: est_cost,
        order_type,
        limit_price,
        bar_time: Some(action.bar_time),
        notional: None,
        is_exit: false,
        is_swing_intent,
        // S37 (2026-05-28 22:55 ET): capture the TF of the FSM that
        // emitted this entry signal. Propagates through PendingOrder →
        // Position so `handle_exit` can gate `SignalExit` against
        // cross-TF FSM closes (1m FSM no longer closes a 5m-opened
        // position). Stop/TP/max-hold/EOD exits unaffected.
        entry_timeframe: action.timeframe.clone(),
        // Record the real strategy-side entry score so the journal row
        // survives even when the paper WebSocket fill stream is silent
        // and the open is materialised via position_recon (which would
        // otherwise fall back to 0.0 for non-FSM Tournament strategies).
        // Legacy momentum entries (`strategy_id == None`) keep `None`
        // here and continue to be scored via the FSM in recon.
        entry_score: action.strategy_id.map(|_| action.score),
        // PRIME F3 — carry the fused conviction into the order path so
        // the journal (and the chart marker) can attribute it.
        conviction: action.conviction,
        // P5 — the live price this entry decision was sized at; the
        // slippage ledger scores the eventual fill against it.
        expected_price: entry_price,
    };

    tracing::info!(
        component = "bot",
        symbol = %action.symbol,
        side = ?action.direction,
        %qty,
        action_price = action.price,
        live_price = entry_price,
        %stop_loss,
        %take_profit,
        score = action.score,
        "Submitting AUTO entry order"
    );

    // S17.0 — pending_entries claim already happened at Gate 2 above.
    // Any subsequent sentinel signal for the same symbol will hit the
    // duplicate gate via `pending_entries`. The slot is released only
    // by trade_updates fill (success path) or send_order error branch.

    match send_order_pegged_ioc(Arc::clone(state), order, app.clone(), exec_metrics).await {
        Ok(result) => {
            metrics.record_entry_executed();
            // S34 — decision journal lifecycle: this is the first
            // point where `entered` is truthful. The dispatcher logged
            // `dispatched` when the action entered the bot queue; the
            // pegged IOC router only returns Ok after it confirms a
            // non-zero broker fill.
            let source = action.strategy_id.map(|id| id.as_str()).unwrap_or("fsm");
            let regime = state
                .market
                .lock()
                .map(|m| crate::smarttrend::memory_db::regime_label(Some(m.regime)))
                .unwrap_or("unknown");
            crate::ops::decision_log::log(crate::ops::decision_log::DecisionRecord::entered(
                &action.symbol,
                &action.timeframe,
                source,
                if action.direction == crate::state::Direction::Long {
                    "long"
                } else {
                    "short"
                },
                action.score,
                action.conviction,
                action.bar_time,
                entry_price,
                regime,
            ));
            tracing::info!(
                component = "bot",
                symbol = %action.symbol,
                broker_id = %result.broker_id,
                filled_qty = %result.qty,
                status = %result.status,
                "Entry order filled"
            );

            // Sesiunea 3 — ML hook: Record trade open for Tournament strategies so OMEGA can learn
            if let Some(dplus) = state.dplus() {
                if let Some(strat_id) = action.strategy_id {
                    // S45 (2026-05-29) — count the daily entry against the
                    // per-strategy cap ONLY now that the broker accepted the
                    // order (real fill). Moved off the dispatch/claim path so
                    // a rejected entry (qty<1 etc.) no longer burns the cap.
                    if let Ok(mut t) = dplus.tournament.lock() {
                        t.record_daily_entry(strat_id);
                    }
                    if let Some(features) = &action.setup_features {
                        let setup_id_res = dplus.memory_db.record_open(features, action.bar_time);
                        match setup_id_res {
                            Ok(sid) => {
                                dplus.on_trade_open(&action.symbol, Some(strat_id), Some(sid));

                                // OMEGA Continuous Live-Learning: Record live feature vectors
                                let buf_key = format!("{}:{}", action.symbol, action.timeframe);
                                let candles = {
                                    let buffers = market
                                        .candle_buffers
                                        .lock()
                                        .unwrap_or_else(|p| p.into_inner());
                                    buffers
                                        .get(&buf_key)
                                        .map(|b| b.snapshot())
                                        .unwrap_or_default()
                                };
                                if !candles.is_empty() {
                                    let regime_val =
                                        state.market.lock().ok().map(|m| m.regime).unwrap_or(
                                            crate::smarttrend::strategies::MarketRegime::Calm,
                                        );
                                    let overrides = dplus.overrides.snapshot();
                                    let ctx = crate::smarttrend::strategies::MarketContext {
                                        symbol: action.symbol.clone(),
                                        timeframe: if action.timeframe == "1m" {
                                            "1m"
                                        } else {
                                            "5m"
                                        },
                                        price: entry_price,
                                        prev_close: entry_price,
                                        session_open: entry_price,
                                        candles,
                                        atr: atr_val,
                                        rvol: 1.0,
                                        gap_pct: 0.0,
                                        spread_bps: 0.0,
                                        catalyst_score: None,
                                        now_et_min: 0,
                                        now_et_hour: 0,
                                        now_et_minute: 0,
                                        regime: Some(regime_val),
                                        is_halted: false,
                                        companion: None,
                                        float_shares: None,
                                        news_sentiment: None,
                                        overrides,
                                    };
                                    let fv = crate::ml::features::extract_features(&ctx);
                                    let now_ts = chrono::Utc::now().timestamp();
                                    if let Ok(ml_sample_id) = dplus
                                        .ml_logger
                                        .record_sample(
                                            fv.0,
                                            regime_val,
                                            now_ts,
                                            action.symbol.clone(),
                                            sid,
                                            "live_paper".to_string(),
                                        )
                                        .await
                                    {
                                        dplus.pending_entry_details.lock().unwrap().insert(
                                            sid,
                                            (
                                                entry_price,
                                                atr_val,
                                                qty.to_f64().unwrap_or(0.0),
                                                ml_sample_id,
                                            ),
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    component = "bot",
                                    symbol = %action.symbol,
                                    "Failed to record setup open in MemoryDb: {e}"
                                );
                            }
                        }
                    }
                }
            }
            let _ = app.emit_all(
                "bot-order",
                serde_json::json!({
                    "symbol": action.symbol,
                    "kind": "Entry",
                    "direction": format!("{:?}", action.direction),
                    "qty": qty.to_string(),
                    "stop_loss": stop_loss.to_string(),
                    "take_profit": take_profit.to_string(),
                    "broker_id": result.broker_id,
                    "score": action.score,
                }),
            );
        }
        Err(e) => {
            // S34 — preserve execution failures separately from a
            // strategy veto. This makes "signal -> dispatched ->
            // unfilled" auditable without falsely counting it as a
            // filled APEX trade or a failed signal gate.
            let source = action.strategy_id.map(|id| id.as_str()).unwrap_or("fsm");
            let regime = state
                .market
                .lock()
                .map(|m| crate::smarttrend::memory_db::regime_label(Some(m.regime)))
                .unwrap_or("unknown");
            crate::ops::decision_log::log(
                crate::ops::decision_log::DecisionRecord::execution_failed(
                    &action.symbol,
                    &action.timeframe,
                    source,
                    if action.direction == crate::state::Direction::Long {
                        "long"
                    } else {
                        "short"
                    },
                    action.score,
                    action.conviction,
                    action.bar_time,
                    entry_price,
                    regime,
                    &e.to_string(),
                ),
            );
            // Refund bot wallet on order failure
            {
                let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                st.bot_wallet.credit(est_cost);
                tracing::warn!(
                    component = "bot",
                    symbol = %action.symbol,
                    %est_cost,
                    balance = %st.bot_wallet.balance,
                    "Refunded to bot wallet",
                );
            }
            // S15.14: release the pending claim — broker rejected so
            // we want a future sentinel signal to be able to retry.
            {
                let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
                trading.pending_entries.remove(&action.symbol);
                trading.pending_entry_dirs.remove(&action.symbol);
            }
            let _ = app.emit_all("wallet-changed", serde_json::json!({}));

            metrics.record_entry_blocked();
            tracing::error!(
                component = "bot",
                symbol = %action.symbol,
                error = %e,
                "Entry order FAILED"
            );
            let _ = app.emit_all(
                "bot-error",
                serde_json::json!({
                    "symbol": action.symbol,
                    "error": e.to_string(),
                }),
            );
        }
    }
}

fn evaluate_portfolio_var_gate(
    state: &TitanState,
    market: &crate::market::market_state::MarketDataState,
    new_symbol: &str,
    new_size_usd: f64,
    timeframe: &str,
) -> Result<f64, String> {
    // 1. Get active positions from state.trading
    let active_positions = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading.positions.clone()
    };

    let mut symbols_to_check: Vec<String> =
        active_positions.iter().map(|p| p.symbol.clone()).collect();
    if !symbols_to_check.contains(&new_symbol.to_string()) {
        symbols_to_check.push(new_symbol.to_string());
    }

    // 2. Lock market.candle_buffers and build returns_map
    let mut returns_map = std::collections::HashMap::new();
    {
        let buffers = market
            .candle_buffers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for sym in &symbols_to_check {
            let mut snap_opt = None;

            let tfs_to_try = if sym == new_symbol {
                vec![timeframe, "5m", "1m"]
            } else {
                let pos_tf = active_positions
                    .iter()
                    .find(|p| p.symbol == *sym)
                    .map(|p| p.entry_timeframe.as_str())
                    .unwrap_or("");
                if !pos_tf.is_empty() {
                    vec![pos_tf, "5m", "1m"]
                } else {
                    vec!["5m", "1m"]
                }
            };

            for tf in tfs_to_try {
                let key = format!("{}:{}", sym, tf);
                if let Some(buf) = buffers.get(&key) {
                    let snap = buf.snapshot();
                    if snap.len() >= 2 {
                        snap_opt = Some(snap);
                        break;
                    }
                }
            }

            if let Some(snap) = snap_opt {
                let closes: Vec<f64> = snap.iter().map(|c| c.close).collect();
                let log_returns = titan_risk::compute_returns(&closes);
                returns_map.insert(sym.clone(), log_returns);
            }
        }
    }

    // 3. Build current_positions slice for titan_risk
    let current_positions: Vec<titan_risk::PortfolioPosition> = active_positions
        .iter()
        .map(|p| {
            let notional = (p.qty * p.avg_entry).to_f64().unwrap_or(0.0);
            titan_risk::PortfolioPosition {
                symbol: p.symbol.clone(),
                notional,
            }
        })
        .collect();

    // 4. Check if adding the new position pushes VaR above threshold
    let mut proposed_symbols: Vec<String> =
        current_positions.iter().map(|p| p.symbol.clone()).collect();
    let mut proposed_notionals: Vec<f64> = current_positions.iter().map(|p| p.notional).collect();
    if let Some(idx) = proposed_symbols.iter().position(|s| *s == *new_symbol) {
        proposed_notionals[idx] += new_size_usd;
    } else {
        proposed_symbols.push(new_symbol.to_string());
        proposed_notionals.push(new_size_usd);
    }

    let returns_refs: Vec<&[f64]> = proposed_symbols
        .iter()
        .map(|s| returns_map.get(s).map(|v| v.as_slice()).unwrap_or(&[]))
        .collect();

    let has_data = returns_refs.iter().any(|r| r.len() >= 2);
    if !has_data {
        return Ok(new_size_usd);
    }

    let cov = titan_risk::covariance_matrix(&returns_refs);
    let proposed_var = titan_risk::parametric_var(&proposed_notionals, &cov, 0.95);

    if proposed_var > 0.02 {
        return Err(format!(
            "Portfolio VaR of {:.2}% exceeds safety threshold of 2.0%",
            proposed_var * 100.0
        ));
    }

    let scaled_size = if proposed_var > 0.01 {
        let scale = (0.02 - proposed_var) / 0.01;
        let scaled = new_size_usd * scale;
        tracing::info!(
            component = "risk_shield",
            symbol = %new_symbol,
            proposed_var = %format!("{:.2}%", proposed_var * 100.0),
            scale = %format!("{:.2}%", scale * 100.0),
            size_pre = %new_size_usd,
            size_post = %scaled,
            "VaR Portfolio Gate: high risk detected, sizing scaled down"
        );
        scaled
    } else {
        new_size_usd
    };

    Ok(scaled_size)
}

pub fn evaluate_bayesian_fusion(
    p_fsm: f64,
    p_omega: f64,
    p_ofi: f64,
    p_causal: f64,
    p_memory: f64,
    size_usd: f64,
) -> Option<f64> {
    let joint_p_win = (p_fsm + p_omega + p_ofi + p_causal + p_memory) / 5.0;
    if joint_p_win < 0.45 {
        None
    } else {
        Some(size_usd * joint_p_win)
    }
}

fn is_spy_bearish(market: &crate::market::market_state::MarketDataState) -> bool {
    let buffers = match market.candle_buffers.lock() {
        Ok(b) => b,
        Err(p) => p.into_inner(),
    };
    let Some(spy_buf) = buffers.get("SPY:1m") else {
        return false; // Fail-safe: if SPY is not loaded, assume normal
    };
    let candles = spy_buf.snapshot();
    if candles.len() < 30 {
        return false;
    }
    let price = candles.last().unwrap().close;
    let vwap = crate::smarttrend::strategies::apex::rolling_vwap(&candles, 30);
    vwap > 0.0 && price < vwap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_spy_bearish() {
        use crate::market::candle_buffer::CandleRingBuffer;
        use crate::market::market_state::MarketDataState;
        use titan_types::Candle;

        let market = MarketDataState::new();

        // 1. No SPY buffer -> should be false
        assert!(!super::is_spy_bearish(&market));

        // 2. Seed SPY:1m with less than 30 candles -> should be false
        {
            let mut cb = market.candle_buffers.lock().unwrap();
            let mut spy_buf = CandleRingBuffer::new(60);
            spy_buf.seed(vec![Candle {
                time: 100,
                open: 500.0,
                high: 501.0,
                low: 499.0,
                close: 500.0,
                volume: 1000.0,
            }]);
            cb.insert("SPY:1m".to_string(), spy_buf);
        }
        assert!(!super::is_spy_bearish(&market));

        // 3. Seed 30 flat candles at $500. Last close at $490 (bearish) -> should be true
        {
            let mut cb = market.candle_buffers.lock().unwrap();
            let mut spy_buf = CandleRingBuffer::new(60);
            let mut candles = (0..30)
                .map(|i| Candle {
                    time: i * 60,
                    open: 500.0,
                    high: 501.0,
                    low: 499.0,
                    close: 500.0,
                    volume: 1000.0,
                })
                .collect::<Vec<_>>();
            // Set last candle close below VWAP (which is 500.0)
            if let Some(last) = candles.last_mut() {
                last.close = 490.0;
            }
            spy_buf.seed(candles);
            cb.insert("SPY:1m".to_string(), spy_buf);
        }
        assert!(super::is_spy_bearish(&market));

        // 4. Seed 30 flat candles at $500. Last close at $510 (bullish) -> should be false
        {
            let mut cb = market.candle_buffers.lock().unwrap();
            let mut spy_buf = CandleRingBuffer::new(60);
            let mut candles = (0..30)
                .map(|i| Candle {
                    time: i * 60,
                    open: 500.0,
                    high: 501.0,
                    low: 499.0,
                    close: 500.0,
                    volume: 1000.0,
                })
                .collect::<Vec<_>>();
            // Set last candle close above VWAP
            if let Some(last) = candles.last_mut() {
                last.close = 510.0;
            }
            spy_buf.seed(candles);
            cb.insert("SPY:1m".to_string(), spy_buf);
        }
        assert!(!super::is_spy_bearish(&market));
    }

    #[test]
    fn test_evaluate_bayesian_fusion_veto() {
        // Average is (0.4 + 0.4 + 0.4 + 0.4 + 0.4) / 5 = 0.4 < 0.45 -> vetoed (None)
        assert_eq!(
            evaluate_bayesian_fusion(0.4, 0.4, 0.4, 0.4, 0.4, 1000.0),
            None
        );
    }

    #[test]
    fn test_evaluate_bayesian_fusion_pass_and_sizing() {
        // Average is (0.8 + 0.8 + 0.8 + 0.8 + 0.8) / 5 = 0.8 >= 0.45 -> passes, size scaled to 800.0
        assert_eq!(
            evaluate_bayesian_fusion(0.8, 0.8, 0.8, 0.8, 0.8, 1000.0),
            Some(800.0)
        );

        // Average is (0.5 + 0.5 + 0.5 + 0.5 + 0.5) / 5 = 0.5 >= 0.45 -> passes, size scaled to 500.0
        assert_eq!(
            evaluate_bayesian_fusion(0.5, 0.5, 0.5, 0.5, 0.5, 1000.0),
            Some(500.0)
        );
    }

    #[test]
    fn test_evaluate_portfolio_var_gate() {
        use crate::config::RuntimeConfig;
        use crate::market::candle_buffer::CandleRingBuffer;
        use crate::state::Position;
        use titan_types::Candle;

        let state = TitanState::new(RuntimeConfig::default());
        let market = std::sync::Arc::new(crate::market::market_state::MarketDataState::new());

        // 1. Populate some positions
        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions.push(Position {
                symbol: "AAPL".to_string(),
                qty: Decimal::from(10),
                avg_entry: Decimal::from(150),
                current_price: 150.0,
                unrealized_pnl: 0.0,
                side: crate::state::Direction::Long,
                opened_at: Some(12345),
                source: "s2:gap_fade".to_string(),
                first_seen: 12345,
                version: 1,
                trade_id: "trade-aapl".to_string(),
                is_swing_intent: false,
                entry_timeframe: "5m".to_string(),
                stop_loss: 140.0,
                take_profit: 160.0,
            });
        }

        // 2. Populate candle buffers for AAPL and MSFT
        {
            let mut cb = market.candle_buffers.lock().unwrap();

            let mut aapl_buf = CandleRingBuffer::new(300);
            aapl_buf.seed(vec![
                Candle {
                    time: 100,
                    open: 150.0,
                    high: 151.0,
                    low: 149.0,
                    close: 150.0,
                    volume: 1000.0,
                },
                Candle {
                    time: 200,
                    open: 150.0,
                    high: 152.0,
                    low: 150.0,
                    close: 151.0,
                    volume: 1000.0,
                },
                Candle {
                    time: 300,
                    open: 151.0,
                    high: 152.0,
                    low: 149.0,
                    close: 149.5,
                    volume: 1000.0,
                },
            ]);
            cb.insert("AAPL:5m".to_string(), aapl_buf);

            let mut msft_buf = CandleRingBuffer::new(300);
            msft_buf.seed(vec![
                Candle {
                    time: 100,
                    open: 250.0,
                    high: 251.0,
                    low: 249.0,
                    close: 250.0,
                    volume: 1000.0,
                },
                Candle {
                    time: 200,
                    open: 250.0,
                    high: 252.0,
                    low: 250.0,
                    close: 252.0,
                    volume: 1000.0,
                },
                Candle {
                    time: 300,
                    open: 252.0,
                    high: 253.0,
                    low: 251.0,
                    close: 251.5,
                    volume: 1000.0,
                },
            ]);
            cb.insert("MSFT:5m".to_string(), msft_buf);
        }

        // Evaluate VaR gate with a small new position (should be allowed)
        let res = evaluate_portfolio_var_gate(&state, &market, "MSFT", 500.0, "5m");
        assert!(res.is_ok());
        let scaled = res.unwrap();
        assert!(scaled > 0.0);

        // Evaluate VaR gate with a huge highly volatile position (should trigger warning/scale or block)
        {
            let mut cb = market.candle_buffers.lock().unwrap();
            let mut volatile_buf = CandleRingBuffer::new(300);
            volatile_buf.seed(vec![
                Candle {
                    time: 100,
                    open: 10.0,
                    high: 15.0,
                    low: 9.0,
                    close: 10.0,
                    volume: 1000.0,
                },
                Candle {
                    time: 200,
                    open: 10.0,
                    high: 20.0,
                    low: 8.0,
                    close: 15.0,
                    volume: 1000.0,
                },
                Candle {
                    time: 300,
                    open: 15.0,
                    high: 25.0,
                    low: 7.0,
                    close: 8.0,
                    volume: 1000.0,
                },
            ]);
            cb.insert("VOLATILE:5m".to_string(), volatile_buf);
        }

        let res_blocked = evaluate_portfolio_var_gate(&state, &market, "VOLATILE", 50000.0, "5m");
        assert!(
            res_blocked.is_err(),
            "Extreme volatility and size must be blocked by VaR Gate"
        );
    }
}
