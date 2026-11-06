use tauri::AppHandle;

use crate::market::market_state::MarketDataState;
use crate::state::{fsm_key, BarClosedMsg, Direction, FsmPhase, MarkerKind, TitanState};

use crate::smarttrend::bot::{BotAction, BotActionKind, ExitReason};
use crate::smarttrend::engine::EvalResult;

use super::alerts::{emit_alert_event, emit_eval_event, send_desktop_notification};
use super::loop_run::LOOP_NAME;

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_eval_side_effects(
    state: &TitanState,
    market: &MarketDataState,
    app: &AppHandle,
    bot_tx: &tokio::sync::mpsc::Sender<BotAction>,
    symbol: &str,
    msg: &BarClosedMsg,
    result: &EvalResult,
    prev_phase: FsmPhase,
    stream_stale: bool,
    _atr: f64,
) -> bool {
    let phase_changed = apply_eval_result(state, symbol, &msg.timeframe, result, prev_phase);

    emit_eval_event(app, symbol, &result.diagnostics);

    if phase_changed || result.marker.is_some() {
        emit_alert_event(
            app,
            symbol,
            result,
            prev_phase,
            &msg.timeframe,
            msg.bar_time,
        );

        let temp_markers = result.marker.clone().into_iter().collect::<Vec<_>>();
        send_desktop_notification(
            app,
            symbol,
            &temp_markers,
            &state.config.lock().unwrap().ui,
        );

        tracing::info!(
            component = LOOP_NAME,
            symbol,
            prev = ?prev_phase,
            new = ?result.new_fsm.phase,
            score = format!("{:.1}", result.diagnostics.combined_score),
            markers = if result.marker.is_some() { 1 } else { 0 },
            "FSM transition"
        );

        // PRIME F2d — FSM as conviction candidate.
        //
        // History: v2.5.0 planned "tournament-only mode" (FSM entries
        // suppressed when evolution_protocol.enabled) but the flag was
        // captured and never enforced (`_tournament_active`), so both
        // engines kept firing independently. PRIME resolves this
        // properly: when the tournament is active, an FSM ENTRY marker
        // is no longer an unconditional order — it is fused through
        // the PRIME conviction engine (source = "fsm") and dispatches
        // only when conviction clears the threshold. The chart marker
        // itself always survives (pushed in `apply_eval_result`), so
        // the operator still sees what the FSM wanted to do.
        let (tournament_active, apex_only) = state
            .config
            .lock()
            .map(|c| (c.evolution_protocol.enabled, c.evolution_protocol.apex_only))
            .unwrap_or((false, true));
        // Live regime for the fusion — read once, Tier-A guard dropped
        // immediately (S05-compatible; no emits in this scope).
        let regime = state.market.lock().ok().map(|m| m.regime);

        #[allow(for_loops_over_fallibles)]
        for marker in &result.marker {
            // S35.1 (2026-05-28) — `display_only` markers are FSM
            // simulation signals kept on the chart for manual-trading
            // reference but vetoed by post-eval gates (quality/trend/
            // history). They must NOT trigger BotAction so no real
            // trade fires. Only Entry markers get the display_only
            // treatment — Exits always dispatch (vetoing an exit
            // signal would strand the bot in an open position).
            if marker.display_only && marker.kind == MarkerKind::Entry {
                tracing::debug!(
                    component = LOOP_NAME,
                    symbol,
                    tf = %marker.timeframe,
                    "FSM ENTRY skipped — display_only marker (manual-trading reference only)"
                );
                continue;
            }
            let bot_kind = match marker.kind {
                MarkerKind::Entry => {
                    // APEX-only mode (2026-07-04 audit): FSM entries are
                    // 10% WR / −$3.96 live. The marker stays on the chart
                    // as a visual reference (already pushed by
                    // `apply_eval_result`), but no order fires. FSM exits
                    // below still dispatch — vetoing an exit would strand
                    // an open position.
                    if apex_only && tournament_active {
                        tracing::debug!(
                            component = LOOP_NAME,
                            symbol,
                            tf = %marker.timeframe,
                            "FSM ENTRY skipped — apex_only mode (chart reference only)"
                        );
                        continue;
                    }
                    // P1 (hospital) — per-symbol daily loss breaker
                    // applies to FSM entries exactly as to tournament
                    // entries (same bleeding-ticker failure mode).
                    if super::tournament_dispatch::symbol_daily_breaker_tripped(state, symbol) {
                        tracing::debug!(
                            component = LOOP_NAME,
                            symbol,
                            tf = %marker.timeframe,
                            "P1 symbol breaker — FSM ENTRY banned for today"
                        );
                        crate::ops::decision_log::log(
                            crate::ops::decision_log::DecisionRecord::vetoed(
                                symbol,
                                &marker.timeframe,
                                "fsm",
                                if marker.direction == Direction::Long { "long" } else { "short" },
                                marker.score,
                                "symbol_quarantine",
                                "daily_loss_breaker",
                                marker.bar_time,
                                marker.price,
                                crate::smarttrend::memory_db::regime_label(regime),
                            ),
                        );
                        continue;
                    }
                    BotActionKind::Entry
                }
                MarkerKind::Exit => {
                    // Phantom-exit guard (2026-08-11). In apex_only the
                    // FSM never opens real positions, so a sim exit with
                    // no locally-held position would dispatch anyway and
                    // cost one broker REST query per eval (1 Hz) before
                    // `handle_exit` aborts — live evidence: 137 aborted
                    // QBTS/SOXS exits in 2.5 min at startup. Skip at the
                    // source; `handle_exit` still re-checks freshness
                    // for the held case.
                    if !holds_local_position(state, symbol) {
                        tracing::debug!(
                            component = LOOP_NAME,
                            symbol,
                            tf = %marker.timeframe,
                            "FSM EXIT skipped — no local position (sim-only trade)"
                        );
                        continue;
                    }
                    // P0 Asymmetry Protocol — exit-side arbitration.
                    // Empirics (2026-07-03, 58 closed trades): signal
                    // exits cut winners at avg +$1.09 while losers ran
                    // to avg −$2.45. A signal exit on a *winner* above
                    // `defer_min_r` is deferred: the PRIME ladder takes
                    // over with an armed breakeven floor, so the trade
                    // can no longer lose but can still run. Losers and
                    // degenerate positions (no bracket stop) execute
                    // immediately — fast loss-cutting is preserved.
                    if defer_signal_exit_if_winner(state, symbol, marker, regime) {
                        continue;
                    }
                    BotActionKind::Exit(ExitReason::SignalExit)
                }
                _ => continue,
            };

            // PRIME arbitration applies to ENTRY markers only — vetoing
            // an exit would strand an open position (S35.1 rationale).
            let mut fsm_conviction: Option<f64> = None;
            if bot_kind == BotActionKind::Entry && tournament_active {
                // S29.2 — extract live market context (spread and rvol) for FSM
                // to align Prime decisions with actual broker state.
                let (live_spread, live_rvol) = {
                    let active_symbol = state.market.lock().ok()
                        .map(|m| m.active_symbol.clone())
                        .unwrap_or_default();
                    
                    if active_symbol.eq_ignore_ascii_case(symbol) {
                        (market.ofi_state.load().spread_bps, 1.0)
                    } else {
                        let metrics = market.watchlist_metrics.lock().unwrap();
                        metrics.get(symbol)
                            .map(|m| (m.spread_bps, m.rvol))
                            .unwrap_or((0.0, 1.0))
                    }
                };

                let ruling = state.dplus().and_then(|dp| {
                    let engine = dp.prime.lock().ok()?;
                    if !engine.enabled {
                        return None;
                    }
                    let candidate = crate::smarttrend::prime::Candidate {
                        source: "fsm".to_string(),
                        side_long: marker.direction == Direction::Long,
                        raw_score: marker.score,
                    };
                    // P2 — session bucket for context-conditional
                    // reliability.
                    let session = {
                        use chrono::Timelike;
                        let now_et =
                            chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
                        crate::smarttrend::prime::conviction::session_label(
                            now_et.hour() * 60 + now_et.minute(),
                        )
                    };
                    // P5 — realized execution-cost prior stands in for
                    // the (unavailable) quoted spread on this path.
                    let slip_prior = crate::ops::slippage::recent_bps(
                        symbol,
                        chrono::Utc::now().timestamp(),
                    );
                    engine
                        .decide(
                            &[candidate],
                            regime,
                            None,
                            live_rvol,
                            live_spread + slip_prior,
                            0.0,
                            session,
                        )
                        .into_iter()
                        .next()
                });
                match ruling {
                    Some(crate::smarttrend::prime::Ruling::Enter(cv)) => {
                        fsm_conviction = Some(cv.value);
                        // P3 — components recorded for the
                        // Counterfactual Lab's offline re-fusion.
                        crate::ops::decision_log::log(
                            crate::ops::decision_log::DecisionRecord::entered(
                                symbol,
                                &marker.timeframe,
                                "fsm",
                                if marker.direction == Direction::Long { "long" } else { "short" },
                                marker.score,
                                Some(cv.value),
                                marker.bar_time,
                                marker.price,
                                crate::smarttrend::memory_db::regime_label(regime),
                            )
                            .with_components(&cv.norm),
                        );
                    }
                    Some(crate::smarttrend::prime::Ruling::Veto { conviction, reason }) => {
                        tracing::debug!(
                            component = LOOP_NAME,
                            symbol,
                            tf = %marker.timeframe,
                            conviction = format!("{:.3}", conviction.value),
                            reason,
                            "PRIME veto — FSM ENTRY below conviction threshold"
                        );
                        crate::ops::decision_log::log(
                            crate::ops::decision_log::DecisionRecord::vetoed(
                                symbol,
                                &marker.timeframe,
                                "fsm",
                                if marker.direction == Direction::Long { "long" } else { "short" },
                                marker.score,
                                "prime",
                                reason,
                                marker.bar_time,
                                marker.price,
                                crate::smarttrend::memory_db::regime_label(regime),
                            )
                            .with_conviction(conviction.value)
                            .with_components(&conviction.norm),
                        );
                        continue;
                    }
                    // PRIME disabled / dplus not initialised — legacy
                    // unconditional dispatch (pre-F2 behaviour).
                    None => {}
                }
            }

            let action = BotAction {
                symbol: marker.symbol.clone(),
                kind: bot_kind,
                direction: marker.direction,
                score: marker.score,
                bar_time: marker.bar_time,
                price: marker.price,
                timeframe: marker.timeframe.clone(),
                // Legacy SentinelTrend dispatch path — strategy framework
                // hooks attach `strategy_id` only for tournament-routed
                // signals (Phase 1.6 wiring).
                strategy_id: None,
                stop_loss: None,
                take_profit: None,
                max_hold_minutes: None,
                setup_features: None,
                conviction: fsm_conviction,
            };
            if bot_tx.try_send(action).is_err() {
                market.bot_actions_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    component = LOOP_NAME,
                    symbol,
                    "Bot channel full or closed — action dropped"
                );
            }
        }
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let reconnected_at = market.stream_reconnected_at.load(std::sync::atomic::Ordering::Relaxed);
    let is_in_warmup = reconnected_at > 0 && now_secs.saturating_sub(reconnected_at) < 30;

    if result.new_fsm.phase == FsmPhase::Active
        && stream_stale
        && !is_in_warmup
        // Same phantom-exit guard as the SignalExit arm above.
        && holds_local_position(state, symbol)
    {
        let action = BotAction {
            symbol: symbol.to_string(),
            kind: BotActionKind::Exit(ExitReason::StaleData),
            direction: result
                .marker
                .as_ref()
                .map(|m| m.direction)
                .unwrap_or(Direction::Long),
            score: result.diagnostics.combined_score,
            bar_time: msg.bar_time,
            price: msg.close_price,
            timeframe: msg.timeframe.clone(),
            strategy_id: None,
            stop_loss: None,
            take_profit: None,
            max_hold_minutes: None,
            setup_features: None,
            conviction: None,
        };
        let _ = bot_tx.try_send(action);
    }

    phase_changed
}

/// Phantom-exit guard — `true` when the symbol has an open local
/// position on ANY side (side + freshness re-checked in `handle_exit`).
/// Tier-A `trading` guard, scoped and dropped immediately.
fn holds_local_position(state: &TitanState, symbol: &str) -> bool {
    let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
    trading
        .positions
        .iter()
        .any(|p| p.symbol == symbol && p.qty.abs() > rust_decimal::Decimal::ZERO)
}

/// P0 Asymmetry Protocol — returns `true` when the FSM signal exit was
/// deferred (position is a winner above `ExitPolicy::defer_min_r`).
///
/// Side effects on deferral: arms the ladder's immediate-breakeven
/// floor (`prime:be:<symbol>` flag in `trading.peak_pnl_pcts`,
/// consumed by `evaluate_prime_ladder` and evicted with the position)
/// and writes an `exit_deferred` row to the decision journal.
///
/// Lock discipline: `prime` (standalone) is taken and dropped first,
/// then `trading` (Tier A) — never overlapped. Returns `false` (= let
/// the exit through, pre-P0 behaviour) whenever PRIME is disabled or
/// the position cannot produce an R unit.
fn defer_signal_exit_if_winner(
    state: &TitanState,
    symbol: &str,
    marker: &crate::state::SmartTrendMarker,
    regime: Option<crate::smarttrend::strategies::MarketRegime>,
) -> bool {
    use crate::smarttrend::prime::exits::{arbitrate_signal_exit, SignalExitVerdict};
    use rust_decimal::prelude::ToPrimitive;

    let Some(dp) = state.dplus() else { return false };
    let policy = {
        let Ok(engine) = dp.prime.lock() else { return false };
        if !engine.enabled {
            return false;
        }
        engine.exit_policy
    };

    let verdict = {
        let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pos) = trading.positions.iter().find(|p| p.symbol == symbol) else {
            return false;
        };
        if pos.qty.abs() <= rust_decimal::Decimal::ZERO {
            return false;
        }
        let is_long = pos.side == Direction::Long;
        let entry = pos.avg_entry.to_f64().unwrap_or(0.0);
        let verdict =
            arbitrate_signal_exit(&policy, is_long, entry, pos.stop_loss, marker.price);
        if matches!(verdict, SignalExitVerdict::Defer { .. }) {
            // Arm the immediate breakeven floor while still inside the
            // guard (single lock window; value is a flag, not a peak).
            trading
                .peak_pnl_pcts
                .insert(format!("prime:be:{symbol}"), 0.0);
        }
        verdict
    };

    match verdict {
        SignalExitVerdict::Execute => false,
        SignalExitVerdict::Defer { r } => {
            tracing::info!(
                component = LOOP_NAME,
                symbol,
                tf = %marker.timeframe,
                r_multiple = format!("{r:.2}"),
                "P0 — signal exit DEFERRED on winner; PRIME ladder takes over (breakeven armed)"
            );
            crate::ops::decision_log::log(
                crate::ops::decision_log::DecisionRecord::exit_deferred(
                    symbol,
                    &marker.timeframe,
                    "fsm",
                    if marker.direction == Direction::Long { "long" } else { "short" },
                    r,
                    marker.bar_time,
                    marker.price,
                    crate::smarttrend::memory_db::regime_label(regime),
                ),
            );
            // F3b — HOLD marker (winner protected; do not sell manually).
            let mut hold = marker.clone();
            hold.kind = MarkerKind::Exit;
            hold.display_only = true;
            hold.source = Some("prime".into());
            hold.verdict = Some("HOLD".into());
            hold.conviction = Some(r);
            {
                let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                st.markers.push(hold);
            }
            true
        }
    }
}

pub(super) fn apply_eval_result(
    state: &TitanState,
    symbol: &str,
    tf: &str,
    result: &EvalResult,
    prev_phase: FsmPhase,
) -> bool {
    let mut st = state.smarttrend.lock().unwrap();
    // S36 (2026-05-28 18:46 ET) — RE-INSTATED per-TF FSM keying.
    // Each `(symbol, tf)` carries its own FSM and evolves on its own
    // bar cadence. See `sentinel/inputs.rs` for the full rationale
    // and the live-data evidence that motivated the re-flip from
    // S35.6 → S36.
    st.fsm
        .insert(fsm_key(symbol, tf), result.new_fsm.clone());

    if let Some(marker) = &result.marker {
        let is_dup = st.markers.iter().any(|m| {
            m.symbol == marker.symbol
                && m.timeframe == marker.timeframe
                && m.bar_time == marker.bar_time
                && m.kind == marker.kind
        });
        if !is_dup {
            // F3b — honest FSM provenance (never labelled GO on live path).
            let mut m = marker.clone();
            if m.source.is_none() {
                m.source = Some("fsm".into());
            }
            if m.verdict.is_none() {
                m.verdict = Some(if m.display_only { "VETO".into() } else { "FSM".into() });
            }
            st.markers.push(m);
        }
    }
    // S37 (2026-05-28 22:55 ET) — PER-(symbol, TF) marker cap.
    //
    // History:
    //   S35.3: global cap 500 → 5000 (single-symbol QBTS reference)
    //   S37  : replace global with per-(symbol, TF) so the watchlist
    //          scaling out to 40 slots (max_total_slots, AutoUniverse)
    //          cannot starve any individual chart of recent markers.
    //
    // Failure mode being closed (Opus 4.8 §E#9):
    //   With 40 watchlist slots × 8 TFs ≈ 320 chart contexts. A global
    //   5000 cap could in pathological cases drain *all* markers for a
    //   slow-evolving symbol (1D, 4h) when a fast-moving catalyst symbol
    //   churns through hundreds of new markers in a single session.
    //   Per-(symbol, TF) keeps every chart context bounded
    //   independently, so QBTS:1m's marker density cannot wipe NVDA:1D's
    //   history.
    //
    // Cap rationale:
    //   100 markers per (symbol, TF) is comfortably more than a single
    //   trading session generates on any TF (typical 1m session ≈ 40–60
    //   Entry/Exit markers, 1D ≈ 1 marker). Total upper bound:
    //   40 symbols × 8 TFs × 100 markers = 32 000 markers ≈ 4.8 MB on
    //   disk — acceptable, well below the prior 5000-cap footprint per
    //   symbol when the watchlist was a single ticker.
    //
    // Drain strategy: group by (symbol, tf), sort each group by
    // bar_time, retain the most recent N markers per group.
    const MAX_MARKERS_PER_SYMBOL_TF: usize = 100;
    {
        use std::collections::HashMap;
        let mut groups: HashMap<(String, String), Vec<crate::state::SmartTrendMarker>> =
            HashMap::new();
        // Drain markers into per-(symbol, tf) buckets.
        for m in st.markers.drain(..) {
            groups
                .entry((m.symbol.clone(), m.timeframe.clone()))
                .or_default()
                .push(m);
        }
        // Per group: sort oldest → newest by bar_time, retain the tail.
        let mut kept: Vec<crate::state::SmartTrendMarker> =
            Vec::with_capacity(groups.values().map(|v| v.len().min(MAX_MARKERS_PER_SYMBOL_TF)).sum());
        for (_key, mut bucket) in groups.drain() {
            if bucket.len() > MAX_MARKERS_PER_SYMBOL_TF {
                bucket.sort_by_key(|m| m.bar_time);
                let excess = bucket.len() - MAX_MARKERS_PER_SYMBOL_TF;
                bucket.drain(0..excess);
            }
            kept.extend(bucket);
        }
        // Restore in bar_time order so subsequent appends naturally
        // continue the chronological sequence.
        kept.sort_by_key(|m| m.bar_time);
        st.markers = kept;
    }

    st.last_diagnostics = Some(result.diagnostics.clone());

    prev_phase != result.new_fsm.phase
}

#[cfg(test)]
mod tests {
    use crate::config::RuntimeConfig;
    use crate::state::{
        fsm_key, Direction, FsmPhase, MarkerKind, SmartTrendMarker, SymbolFsm, TitanState,
    };

    use crate::smarttrend::engine::EvalResult;

    use super::apply_eval_result;

    #[test]
    fn apply_result_updates_fsm() {
        let state = TitanState::new(RuntimeConfig::default());
        let result = EvalResult {
            new_fsm: SymbolFsm {
                phase: FsmPhase::Confirming,
                score: 58.0,
                confirm_count: 1,
                entry_price: None,
                entry_time: None,
                prev_close: 100.0,
                ..Default::default()
            },
            marker: Some(SmartTrendMarker {
                kind: MarkerKind::Entry,
                direction: Direction::Long,
                bar_time: 1700000000,
                price: 185.0,
                score: 58.0,
                timeframe: "5m".to_string(),
                symbol: "AAPL".to_string(),
                display_only: false,
                source: None,
                setup: None,
                conviction: None,
                verdict: None,
            }),
            diagnostics: default_diagnostics(),
        };

        let changed = apply_eval_result(&state, "AAPL", "5m", &result, FsmPhase::Idle);
        assert!(changed);

        let st = state.smarttrend.lock().unwrap();
        assert_eq!(
            st.fsm.get(&fsm_key("AAPL", "5m")).unwrap().phase,
            FsmPhase::Confirming
        );
        assert_eq!(st.markers.len(), 1);
    }

    #[test]
    fn apply_result_no_change_returns_false() {
        let state = TitanState::new(RuntimeConfig::default());
        let result = EvalResult {
            new_fsm: SymbolFsm::default(),
            marker: None,
            diagnostics: default_diagnostics(),
        };

        let changed = apply_eval_result(&state, "AAPL", "5m", &result, FsmPhase::Idle);
        assert!(!changed);
    }

    #[test]
    fn multi_symbol_fsm_independent() {
        let state = TitanState::new(RuntimeConfig::default());
        let result_a = EvalResult {
            new_fsm: SymbolFsm {
                phase: FsmPhase::Confirming,
                score: 58.0,
                confirm_count: 1,
                ..Default::default()
            },
            marker: None,
            diagnostics: default_diagnostics(),
        };
        let result_b = EvalResult {
            new_fsm: SymbolFsm {
                phase: FsmPhase::Active,
                score: 72.0,
                entry_price: Some(350.0),
                entry_time: Some(1700000000),
                ..Default::default()
            },
            marker: None,
            diagnostics: default_diagnostics(),
        };

        apply_eval_result(&state, "AAPL", "5m", &result_a, FsmPhase::Idle);
        apply_eval_result(&state, "MSFT", "5m", &result_b, FsmPhase::Confirming);

        let st = state.smarttrend.lock().unwrap();
        assert_eq!(
            st.fsm.get(&fsm_key("AAPL", "5m")).unwrap().phase,
            FsmPhase::Confirming
        );
        assert_eq!(
            st.fsm.get(&fsm_key("MSFT", "5m")).unwrap().phase,
            FsmPhase::Active
        );
    }

    fn default_diagnostics() -> crate::smarttrend::engine::PillarDiagnostics {
        use crate::smarttrend::engine::{PillarDirection, PillarScore, SmartTrendStrength};
        crate::smarttrend::engine::PillarDiagnostics {
            momentum_score: PillarScore {
                score: 50.0,
                direction: PillarDirection::Neutral,
                confidence: 0.5,
                stale: false,
            },
            combined_score: 50.0,
            combined_direction: PillarDirection::Neutral,
            strength: SmartTrendStrength::Weak,
            hawkes_intensity: 0.0,
        }
    }
}
