//! F3/F4/F5 — Frontier eval loop (1Hz + ResumeEvent wake).
//!
//! Modes via `evolution_protocol.frontier_mode`:
//!   * `off`    — loop idles (heartbeat only)
//!   * `shadow` — log condition vectors + ShadowBook outcomes (no orders)
//!   * `live`   — dispatch BotAction through rails-only bot path
//!
//! Never synthesises missing micro/halt data (fail-closed).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Timelike, Utc};
use rust_decimal::Decimal;
use tauri::AppHandle;

use crate::market::market_state::{MarketDataState, ResumeEvent};
use crate::ops::decision_log::{self, DecisionRecord};
use crate::smarttrend::bot::{BotAction, BotActionKind};
use crate::smarttrend::frontier_support::{
    build_frontier_context, emit_frontier_marker, resolve_shadows,
};
use crate::smarttrend::prime::shadow::ShadowTrade;
use crate::smarttrend::strategies::frontier::{
    evaluate_frontier_verbose, FrontierThresholds, SetupEval,
};
use crate::smarttrend::strategies::StrategyId;
use crate::state::{Direction, TitanState};

const LOOP_NAME: &str = "frontier";
/// Rails: max concurrent Frontier positions / entries per day.
const MAX_FRONTIER_POSITIONS: usize = 2;
const MAX_FRONTIER_ENTRIES_DAY: u32 = 4;

pub async fn run_frontier_loop(
    state: Arc<TitanState>,
    market: Arc<MarketDataState>,
    bot_tx: tokio::sync::mpsc::Sender<BotAction>,
    app: AppHandle,
) {
    tracing::info!(component = LOOP_NAME, "Frontier loop started");
    {
        let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
        sys.register_loop(LOOP_NAME);
        if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
            h.running = true;
            h.tick();
        }
    }

    let (resume_tx, mut resume_rx) = tokio::sync::mpsc::channel::<ResumeEvent>(64);
    *market.resume_tx.lock().unwrap_or_else(|p| p.into_inner()) = Some(resume_tx);

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Throttle: last logged state key per symbol → avoid 1Hz spam.
    let mut last_log_key: HashMap<String, String> = HashMap::new();
    let mut entered_today: HashMap<String, HashSet<&'static str>> = HashMap::new();
    let mut entries_today: u32 = 0;
    let mut day_et: Option<chrono::NaiveDate> = None;

    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            ev = resume_rx.recv() => {
                if let Some(ev) = ev {
                    let thresholds = current_thresholds(&state);
                    evaluate_symbol(EvalArgs {
                        state: &state,
                        market: &market,
                        bot_tx: &bot_tx,
                        app: &app,
                        symbol: &ev.symbol,
                        resume_ev: Some(&ev),
                        th: &thresholds,
                        last_log_key: &mut last_log_key,
                        entered_today: &mut entered_today,
                        entries_today: &mut entries_today,
                    }).await;
                    // Re-evaluate open Frontier positions on resume wake.
                    reeval_open_positions(&state, &market, &bot_tx, &ev.symbol).await;
                }
            }
            _ = tick.tick() => {
                {
                    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
                        h.tick();
                    }
                }
                let mode = frontier_mode(&state);
                if mode == "off" {
                    continue;
                }
                let now_et = Utc::now().with_timezone(&chrono_tz::US::Eastern);
                let today = now_et.date_naive();
                if day_et != Some(today) {
                    day_et = Some(today);
                    entered_today.clear();
                    entries_today = 0;
                    last_log_key.clear();
                }
                // Flat rail — stop new entries after 15:55 ET.
                let et_min = now_et.hour() * 60 + now_et.minute();
                if et_min >= 15 * 60 + 55 {
                    continue;
                }
                // F4 — thresholds re-read every tick so an operator
                // APPLY (calibration overrides) takes effect live.
                let thresholds = current_thresholds(&state);
                let symbols = watched_symbols(&state);
                for sym in symbols {
                    evaluate_symbol(EvalArgs {
                        state: &state,
                        market: &market,
                        bot_tx: &bot_tx,
                        app: &app,
                        symbol: &sym,
                        resume_ev: None,
                        th: &thresholds,
                        last_log_key: &mut last_log_key,
                        entered_today: &mut entered_today,
                        entries_today: &mut entries_today,
                    }).await;
                }
                // Resolve shadow outcomes on last prices.
                resolve_shadows(&state, &market);
            }
        }
    }

    tracing::info!(component = LOOP_NAME, "Frontier loop exiting");
}

fn frontier_mode(state: &TitanState) -> String {
    state
        .config
        .lock()
        .map(|c| c.evolution_protocol.frontier_mode.clone())
        .unwrap_or_else(|_| "off".into())
}

/// Defaults + persisted operator overrides (F4 calibration).
fn current_thresholds(state: &TitanState) -> FrontierThresholds {
    let overrides = state
        .config
        .lock()
        .map(|c| c.evolution_protocol.frontier_overrides.clone())
        .unwrap_or_default();
    FrontierThresholds::with_overrides(&overrides)
}

fn watched_symbols(state: &TitanState) -> Vec<String> {
    let (au_enabled, favorites) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (cfg.auto_universe.enabled, cfg.ui.favorites.clone())
    };
    let mut out: Vec<String> = {
        let mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
        let mut set = if au_enabled {
            mkt.auto_universe.active_set().into_iter().collect()
        } else {
            favorites
                .into_iter()
                .map(|s| s.to_uppercase())
                .collect::<Vec<_>>()
        };
        for s in &mkt.burst_promoted {
            if !set.iter().any(|x| x == s) {
                set.push(s.clone());
            }
        }
        set
    };
    if out.len() > 80 {
        out.truncate(80);
    }
    out
}

struct EvalArgs<'a> {
    state: &'a TitanState,
    market: &'a MarketDataState,
    bot_tx: &'a tokio::sync::mpsc::Sender<BotAction>,
    app: &'a AppHandle,
    symbol: &'a str,
    resume_ev: Option<&'a ResumeEvent>,
    th: &'a FrontierThresholds,
    last_log_key: &'a mut HashMap<String, String>,
    entered_today: &'a mut HashMap<String, HashSet<&'static str>>,
    entries_today: &'a mut u32,
}

async fn evaluate_symbol(a: EvalArgs<'_>) {
    let EvalArgs {
        state,
        market,
        bot_tx,
        app,
        symbol,
        resume_ev,
        th,
        last_log_key,
        entered_today,
        entries_today,
    } = a;
    let mode = frontier_mode(state);
    if mode == "off" {
        return;
    }
    let Some(ctx) = build_frontier_context(state, market, symbol, resume_ev, entered_today) else {
        return;
    };
    let evals = evaluate_frontier_verbose(&ctx, th);
    let cand = evals
        .iter()
        .filter_map(|e| e.candidate.clone())
        .max_by(|a, b| {
            a.conviction
                .partial_cmp(&b.conviction)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

    // F4 kill-rate — near-miss: closest failing setup (1-2 failed
    // conditions). Logged on transition only, so 1Hz stays quiet.
    let near_miss: Option<&SetupEval> = if cand.is_none() {
        evals
            .iter()
            .filter(|e| {
                let failed = e.conditions.iter().filter(|c| !c.passed).count();
                !e.conditions.is_empty() && (1..=2).contains(&failed)
            })
            .min_by_key(|e| e.conditions.iter().filter(|c| !c.passed).count())
    } else {
        None
    };

    // Throttled condition logging (transition only). A candidate's
    // conviction moves continuously with price/tape; it is evidence
    // strength, not a new setup state. Including it here used to turn
    // one persistent Tape Ignition into hundreds of "candidates" and
    // corrupted the 3–8/day calibration rate.
    let key = match (&cand, near_miss) {
        (Some(c), _) => format!("go:{}", c.setup.as_str()),
        (None, Some(m)) => {
            let failed: Vec<&str> = m
                .conditions
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.name)
                .collect();
            format!("miss:{}:{}", m.setup.as_str(), failed.join("+"))
        }
        (None, None) => "idle".into(),
    };
    let prev = last_log_key.get(symbol).cloned();
    if prev.as_deref() != Some(key.as_str()) {
        last_log_key.insert(symbol.to_string(), key.clone());
        if let Some(m) = near_miss {
            let cond_json = serde_json::to_string(&m.conditions).unwrap_or_else(|_| "[]".into());
            let failed: Vec<&str> = m
                .conditions
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.name)
                .collect();
            // direction "none" keeps near-misses out of the shadow
            // claimer (`recent_vetoed` filters direction != 'none').
            let mut rec = DecisionRecord::vetoed(
                symbol,
                "1m",
                &format!("frontier:{}", m.setup.as_str()),
                "none",
                0.0,
                "frontier",
                &format!("cond:{}", failed.join("+")),
                Utc::now().timestamp(),
                ctx.price,
                "unknown",
            );
            rec.components = Some(cond_json);
            decision_log::log(rec);
        }
        if let Some(c) = &cand {
            if mode == "shadow" {
                let cond_json =
                    serde_json::to_string(&c.conditions).unwrap_or_else(|_| "[]".into());
                let source = format!("frontier:{}", c.setup.as_str());
                let dir = if c.signal.side == Direction::Long {
                    "long"
                } else {
                    "short"
                };
                let mut rec = DecisionRecord::vetoed(
                    symbol,
                    "1m",
                    &source,
                    dir,
                    c.signal.score,
                    "shadow",
                    "frontier_shadow_candidate",
                    Utc::now().timestamp(),
                    ctx.price,
                    "unknown",
                )
                .with_conviction(c.conviction);
                // Stuff condition vector into components JSON (F4 kill-rate).
                rec.components = Some(cond_json);
                decision_log::log(rec);
            }

            emit_frontier_marker(state, app, symbol, ctx.price, c, mode == "shadow");
        }
    }

    let Some(c) = cand else { return };

    if mode == "shadow" {
        if let Some(dp) = state.dplus() {
            if let Ok(mut book) = dp.shadow.lock() {
                book.open(ShadowTrade {
                    symbol: symbol.to_string(),
                    source: format!("frontier:{}", c.setup.as_str()),
                    veto_stage: "shadow".into(),
                    veto_reason: "frontier_shadow".into(),
                    side_long: c.signal.side == Direction::Long,
                    entry_price: ctx.price,
                    stop: c.signal.stop_loss,
                    target: c.signal.take_profit,
                    opened_ts: Utc::now().timestamp(),
                    max_hold_minutes: c.signal.max_hold_minutes,
                });
            }
        }
        return;
    }

    // live
    if *entries_today >= MAX_FRONTIER_ENTRIES_DAY {
        return;
    }
    let open_frontier = count_open_frontier(state);
    if open_frontier >= MAX_FRONTIER_POSITIONS {
        return;
    }
    // Hospital / already in position — bot rails also check; cheap prefilter.
    {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        if trading
            .positions
            .iter()
            .any(|p| p.symbol.eq_ignore_ascii_case(symbol) && p.qty.abs() > Decimal::ZERO)
        {
            return;
        }
    }

    let action = BotAction {
        symbol: symbol.to_string(),
        kind: BotActionKind::Entry,
        direction: c.signal.side,
        score: c.signal.score,
        bar_time: Utc::now().timestamp(),
        price: ctx.price,
        timeframe: "1m".into(),
        strategy_id: Some(StrategyId::Frontier),
        stop_loss: Decimal::from_f64_retain(c.signal.stop_loss),
        take_profit: Decimal::from_f64_retain(c.signal.take_profit),
        max_hold_minutes: Some(c.signal.max_hold_minutes),
        setup_features: None,
        conviction: Some(c.conviction),
    };
    if bot_tx.try_send(action).is_ok() {
        *entries_today += 1;
        entered_today
            .entry(symbol.to_string())
            .or_default()
            .insert(c.setup.as_str());
        // S34 — this is queue acceptance, not a filled trade. The bot
        // writes the `entered` lifecycle row after broker-confirmed
        // quantity; matching the Tournament dispatcher semantics.
        let cond_json = serde_json::to_string(&c.conditions).unwrap_or_else(|_| "[]".into());
        let source = format!("frontier:{}", c.setup.as_str());
        let dir = if c.signal.side == Direction::Long {
            "long"
        } else {
            "short"
        };
        let mut rec = DecisionRecord::dispatched(
            symbol,
            "1m",
            &source,
            dir,
            c.signal.score,
            Some(c.conviction),
            Utc::now().timestamp(),
            ctx.price,
            "unknown",
        );
        rec.components = Some(cond_json);
        decision_log::log(rec);
        tracing::info!(
            component = LOOP_NAME,
            symbol,
            setup = c.setup.as_str(),
            conviction = format!("{:.3}", c.conviction),
            "Frontier LIVE entry dispatched"
        );
    }
}

fn count_open_frontier(state: &TitanState) -> usize {
    let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
    trading
        .positions
        .iter()
        .filter(|p| {
            p.qty.abs() > Decimal::ZERO
                && (p.source.contains("frontier") || p.source.ends_with(":frontier"))
        })
        .count()
}

async fn reeval_open_positions(
    state: &TitanState,
    _market: &MarketDataState,
    bot_tx: &tokio::sync::mpsc::Sender<BotAction>,
    symbol: &str,
) {
    // Resume wake: PRIME ladder resumes on next price tick; no extra order.
    let _ = (state, bot_tx, symbol);
}
