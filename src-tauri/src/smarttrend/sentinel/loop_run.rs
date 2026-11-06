use std::sync::Arc;
use std::time::Instant;

use tauri::AppHandle;

use crate::market::market_state::MarketDataState;
use crate::state::{BarClosedMsg, FsmPhase, TitanState};

use crate::smarttrend::bot::BotAction;

use super::alerts::{emit_status_event, SentinelMetrics};
use super::dispatch::apply_eval_side_effects;
use super::inputs::{build_symbol_eval_inputs, evaluate_symbol_data_only};
use super::tournament_dispatch::dispatch_tournament_signals;

pub(super) const LOOP_NAME: &str = "sentinel";
const FSM_PERSIST_INTERVAL_SECS: u64 = 30;
const STATUS_EMIT_INTERVAL: u64 = 10;

fn is_market_hours() -> bool {
    use chrono::{Datelike, Timelike, Utc};
    let now_et = Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let weekday = now_et.weekday();
    if weekday == chrono::Weekday::Sat || weekday == chrono::Weekday::Sun {
        return false;
    }
    let hour = now_et.hour();
    let minute = now_et.minute();
    let time_mins = hour * 60 + minute;
    (570..960).contains(&time_mins)
}

pub async fn run_sentinel_loop(
    state: Arc<TitanState>,
    market: Arc<MarketDataState>,
    mut bar_rx: tokio::sync::mpsc::Receiver<BarClosedMsg>,
    bot_tx: tokio::sync::mpsc::Sender<BotAction>,
    app: AppHandle,
) {
    tracing::info!(component = LOOP_NAME, "Sentinel loop started");

    {
        let mut sys = state.system.lock().unwrap();
        if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
            h.running = true;
            h.tick();
        }
    }

    let mut metrics = SentinelMetrics::default();
    let mut last_persist = Instant::now();
    let mut last_status_emit: u64 = 0;
    let mut fsm_dirty = false;
    let mut idle_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));

    loop {
        let msg = tokio::select! {
            _ = state.shutdown.cancelled() => {
                tracing::info!(component = LOOP_NAME, "Shutdown signal received");
                break;
            }
            _ = idle_heartbeat.tick() => {
                let mut sys = state.system.lock().unwrap();
                if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
                    h.tick();
                }
                continue;
            }
            msg = bar_rx.recv() => {
                match msg {
                    Some(m) => m,
                    None => {
                        tracing::warn!(component = LOOP_NAME, "Bar channel closed");
                        break;
                    }
                }
            }
        };

        let eval_always = state.config.lock().unwrap().smarttrend.eval_outside_hours;
        let in_hours = is_market_hours();
        if !eval_always && !in_hours {
            tracing::debug!(
                component = LOOP_NAME,
                symbol = %msg.symbol,
                eval_outside_hours = eval_always,
                "Outside market hours, skipping eval"
            );
            continue;
        }
        if !in_hours {
            tracing::info!(
                component = LOOP_NAME,
                symbol = %msg.symbol,
                "Evaluating outside market hours (eval_outside_hours=true)"
            );
        }

        let eval_start = Instant::now();

        let (symbols, st_config, stream_stale) = {
            let cfg = state.config.lock().unwrap();
            (
                vec![msg.symbol.clone()],
                cfg.smarttrend.clone(),
                market.is_stream_stale(),
            )
        };

        metrics.symbols_tracked = symbols.len() as u32;
        let mut idle_count: u32 = 0;
        let mut hot_count: u32 = 0;

        for symbol in &symbols {
            let eval_inputs = build_symbol_eval_inputs(
                &market,
                &state,
                symbol,
                &msg,
                &st_config,
            );

            let result = evaluate_symbol_data_only(
                &eval_inputs.input,
                &eval_inputs.current_fsm,
                &st_config,
                eval_inputs.entry_diagnostic_reason,
                eval_inputs.entry_diagnostic_reason_strict,
                eval_inputs.short_efficiency,
                eval_inputs.trend_metrics.has_sufficient_history,
                eval_inputs.regime,
                eval_inputs.spread_bps,
            );

            tracing::info!(
                component = LOOP_NAME,
                symbol,
                score = format!("{:.1}", result.diagnostics.combined_score),
                mom_s = format!("{:.1}", result.diagnostics.momentum_score.score),
                phase = ?result.new_fsm.phase,
                markers = if result.marker.is_some() { 1 } else { 0 },
                tf = %msg.timeframe,
                trend_bull = eval_inputs.trend_metrics.is_bullish_trend,
                trend_sideways = eval_inputs.trend_metrics.is_sideways,
                pullback_ok = eval_inputs.trend_metrics.pullback_valid,
                slope_bps = format!("{:.2}", eval_inputs.trend_metrics.slope_bps),
                efficiency = format!("{:.2}", eval_inputs.trend_metrics.efficiency_ratio),
                pullback_atr = format!("{:.2}", eval_inputs.trend_metrics.pullback_depth_atr),
                entry_diag = ?eval_inputs.entry_diagnostic_reason,
                "Eval complete"
            );

            let phase_changed = apply_eval_side_effects(
                &state,
                &market,
                &app,
                &bot_tx,
                symbol,
                &msg,
                &result,
                eval_inputs.prev_phase,
                stream_stale,
                eval_inputs.input.atr,
            );
            if phase_changed {
                fsm_dirty = true;
            }

            // v2.5.0 — Tournament dispatch. Runs in parallel with the
            // legacy FSM dispatch above so a single closed bar can fan
            // out a legacy `BotAction` AND any of the 8 tournament
            // strategies' signals on the same symbol. handle_entry's
            // dedup gate (one position per symbol) keeps us from
            // double-firing on the broker side.
            let _ = dispatch_tournament_signals(
                &state,
                &market,
                &app,
                &bot_tx,
                symbol,
                &msg,
                eval_inputs.input.atr,
            )
            .await;

            match result.new_fsm.phase {
                FsmPhase::Idle => idle_count += 1,
                _ => hot_count += 1,
            }

            metrics.evals_total += 1;
        }

        let eval_duration = eval_start.elapsed();
        metrics.last_eval_duration_us = eval_duration.as_micros() as u64;
        metrics.last_eval_ts = Some(msg.bar_time);
        metrics.symbols_idle = idle_count;
        metrics.symbols_hot = hot_count;

        {
            let mut st = state.smarttrend.lock().unwrap();
            st.sentinel_status.symbols_tracked = metrics.symbols_tracked;
            st.sentinel_status.evals_total = metrics.evals_total;
            st.sentinel_status.last_eval_ts = metrics.last_eval_ts;
            st.sentinel_status.running = true;
        }

        {
            let mut sys = state.system.lock().unwrap();
            if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
                h.tick();
            }
        }

        if metrics.evals_total - last_status_emit >= STATUS_EMIT_INTERVAL {
            emit_status_event(&app, &metrics);
            last_status_emit = metrics.evals_total;
        }

        if fsm_dirty || last_persist.elapsed().as_secs() >= FSM_PERSIST_INTERVAL_SECS {
            if let Err(e) = crate::ops::persistence::save_fsm_states(&state) {
                tracing::error!(component = LOOP_NAME, "FSM persist failed: {e}");
            }
            fsm_dirty = false;
            last_persist = Instant::now();
        }

        tracing::debug!(
            component = LOOP_NAME,
            symbols = symbols.len(),
            duration_us = metrics.last_eval_duration_us,
            hot = hot_count,
            idle = idle_count,
            "Eval cycle complete"
        );
    }

    if let Err(e) = crate::ops::persistence::save_fsm_states(&state) {
        tracing::error!(component = LOOP_NAME, "Final FSM persist failed: {e}");
    }
    tracing::info!(component = LOOP_NAME, "Sentinel loop exiting");
}

#[cfg(test)]
mod tests {
    use super::is_market_hours;

    #[test]
    fn market_hours_returns_bool() {
        let _ = is_market_hours();
    }
}
