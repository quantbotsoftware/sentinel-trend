//! MERIDIAN shadow driver (plan §4.5 / §4.9 phase 4 "dry-run + twin").
//!
//! Every `poll_secs` during the session (and once after the close):
//!   1. Tier-A `config` alone → clone `MeridianConfig` + API keys.
//!   2. Network, no locks: incremental history sync per symbol.
//!   3. Pure: load caches → replay the validated engine → snapshot.
//!   4. Diff against the previous snapshot → new intents → tracing +
//!      `<data_dir>/meridian/live/intents.jsonl` (twin log) +
//!      `emit_all("meridian-state")` after all guards are dropped.
//!
//! `execute = false` (default): intents are never sent to the broker.
//! Phase 4b plugs an execution adapter in at the single call site marked
//! below, behind the paper gate.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use chrono_tz::America::New_York;
use serde::Serialize;
use tauri::Manager;

use super::history;
use super::broker::Broker;
use super::clock::{parse_close_minutes, SessionClock, FULL_CLOSE_MIN};
use super::exec::{plan, residual, Action, Phase, PlanInput};
use super::exec_apply::{apply, Outcome};
use super::live_risk::{self, LiveRisk};
use super::shadow::{load_data, new_intents, replay, replay_close_preview, Intent, ReplaySnapshot};
use super::twin_log::{append_twin_log, config_fingerprint, seed_from_twin_log};
use crate::state::TitanState;

const LOOP_NAME: &str = "meridian";

/// Last emitted `meridian-state` payload, so a UI that loads after the
/// driver's first replay (or after the poll window closed) can fetch it.
fn last_state() -> &'static std::sync::Mutex<Option<serde_json::Value>> {
    static LAST: std::sync::OnceLock<std::sync::Mutex<Option<serde_json::Value>>> = std::sync::OnceLock::new();
    LAST.get_or_init(|| std::sync::Mutex::new(None))
}

pub fn last_state_json() -> Option<serde_json::Value> {
    last_state().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

pub fn remember_state(v: serde_json::Value) {
    *last_state().lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
}

#[derive(Debug, Serialize)]
struct StatePayload<'a> {
    /// Which process produced this state: "app" (GUI drives) or "daemon".
    host: &'static str,
    ts: String,
    pid: u32,
    snapshot: &'a ReplaySnapshot,
    new_intents: &'a [Intent],
    execute: bool,
    errors: &'a [String],
    /// Orders the planner wants right now (sent when `execute`, else dry-run).
    planned: &'a [(String, Vec<Action>)],
    applied: &'a [String],
    /// Live risk rail on broker equity (`live_risk.rs`).
    live_risk: &'a LiveRisk,
}

/// The moment `reconcile` plans for: phase, ET date, live rail, wall clock,
/// broker mode (auction orders or the paper market emulation) and the
/// seconds left to the scheduled close.
struct Tick<'a> {
    phase: Phase,
    date: &'a str,
    rail: &'a LiveRisk,
    now: chrono::DateTime<Utc>,
    auction: bool,
    secs_to_close: i64,
    /// Seconds left in the current phase (R6 E2 deadline for `apply`).
    phase_secs_left: i64,
}

/// Plan (and, when `execute`, send) orders for every symbol from broker truth.
/// `now` drives the per-symbol stale-bar guard: in Session / ClosePreview a
/// symbol whose own newest bar is older than `MAX_BAR_AGE_MIN` is planned
/// but not sent (its desired position is a replay of the past), while a
/// symbol with fresh bars still executes.
async fn reconcile(broker: &Broker<'_>, cfg: &super::MeridianConfig, snap: &ReplaySnapshot, t: &Tick<'_>) -> (Vec<(String, Vec<Action>)>, Vec<String>, Vec<String>) {
    let Tick { phase, date, rail, now, auction, secs_to_close, phase_secs_left } = *t;
    let mut planned = Vec::new();
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    for sym in &cfg.symbols {
        let sym = sym.to_uppercase();
        let bars_today = snap.bars_today.get(&sym).copied().unwrap_or(0);
        let needs_fresh = matches!(phase, Phase::Session | Phase::ClosePreview);
        let age = snap.date.and_then(|d| bar_age_min(d, bars_today, now));
        let fresh = !needs_fresh || age.is_some_and(|a| a <= MAX_BAR_AGE_MIN);
        if !fresh {
            tracing::warn!(component = LOOP_NAME, symbol = %sym, ?age, "bars stale — orders suppressed for this symbol");
            errors.push(format!("{sym}: bars stale ({age:?} min) — no orders sent"));
        }
        // R6 E3 (chat 9 P1.6): position and open orders are two HTTP calls; a
        // fill between them yields a state the broker never had (position
        // stale, order gone ⇒ duplicate buy). Read orders → position →
        // orders and require the two order snapshots to agree; otherwise
        // skip the symbol this poll (the next one re-reads).
        let (qty, open) = match (broker.open_orders(&sym).await, broker.position(&sym).await, broker.open_orders(&sym).await) {
            (Ok(o1), Ok(q), Ok(o2)) => {
                let same = o1.len() == o2.len() && o1.iter().zip(o2.iter()).all(|(a, b)| a.id == b.id && a.qty == b.qty);
                if !same {
                    tracing::warn!(component = LOOP_NAME, symbol = %sym, "open orders changed while reading the position — skipping this poll (E3)");
                    errors.push(format!("{sym}: broker snapshot inconsistent — re-read next poll"));
                    continue;
                }
                (q, o2)
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                errors.push(format!("{sym}: broker read failed: {e}"));
                continue;
            }
        };
        let shares = snap.sleeve_shares.get(&sym);
        let a = shares.and_then(|m| m.get("A")).copied().unwrap_or(0.0);
        let b = shares.and_then(|m| m.get("B")).copied().unwrap_or(0.0);
        // the live rail (real equity) overrides the engine's desired book
        let (desired_net, a, b) = live_risk::apply(rail, snap.net_shares.get(&sym).copied().unwrap_or(0.0), a, b);
        let bar_idx = bars_today.saturating_sub(1);
        let input = PlanInput {
            symbol: &sym,
            date,
            bar_idx,
            bars_today,
            phase,
            desired_net,
            a_shares: a,
            b_shares: b,
            broker_qty: qty,
            open: &open,
            auction,
            secs_to_close,
        };
        if phase == Phase::AfterClose {
            let r = residual(&input);
            if r != 0.0 {
                tracing::error!(
                    component = LOOP_NAME,
                    symbol = %sym,
                    residual = r,
                    broker_qty = qty,
                    desired = desired_net,
                    "position differs from the close book after the close — not sending (would queue to the open); the OPG window will flatten"
                );
                errors.push(format!("{sym}: position differs from the close book after the close by {r:+.0} sh — nothing sent"));
            }
        }
        let actions = plan(&input);
        if actions.is_empty() {
            continue;
        }
        // Sanity caps: shares (config) and notional (3 × capital — a sizing
        // bug, never a market view; the largest legitimate order is A flip +
        // B carry ≈ 2.5 × capital). Refused orders are logged and shown.
        let last_px = snap.views.iter().find(|v| v.symbol == sym).and_then(|v| v.last).unwrap_or(0.0);
        if let Some(reason) = actions.iter().find_map(|a| match a {
            Action::Submit(s) if s.qty.abs() > cfg.max_order_shares => Some(format!("order {:+.0} above share cap {}", s.qty, cfg.max_order_shares)),
            Action::Submit(s) if last_px > 0.0 && s.qty.abs() * last_px > MAX_NOTIONAL_X_CAPITAL * cfg.capital => {
                Some(format!("order {:+.0} × {last_px:.2} = ${:.0} above {MAX_NOTIONAL_X_CAPITAL}× capital", s.qty, s.qty.abs() * last_px))
            }
            _ => None,
        }) {
            tracing::error!(component = LOOP_NAME, symbol = %sym, %reason, "MERIDIAN order refused by sanity cap");
            errors.push(format!("{sym}: {reason}"));
            continue;
        }
        let execute = cfg.execute && fresh;
        tracing::info!(component = LOOP_NAME, symbol = %sym, ?phase, broker_qty = qty, desired = input.desired_net, a, b, execute, actions = ?actions, "MERIDIAN plan");
        if execute {
            // R6 E2: every submit is bounded by the end of the *phase*, not
            // by the close; the deadline is fixed per tick and shared by both
            // symbols (a wait on SPY must not extend QQQ's budget).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(phase_secs_left.max(0) as u64);
            match apply(broker, &actions, deadline).await {
                Ok((done, Outcome::Complete)) => applied.extend(done),
                Ok((done, Outcome::Replan)) => {
                    applied.extend(done);
                    tracing::info!(component = LOOP_NAME, symbol = %sym, "cancel confirmed — batch stopped, re-planning from broker truth next poll (E1)");
                    applied.push(format!("{sym}: re-plan next poll"));
                }
                Err(e) => {
                    tracing::error!(component = LOOP_NAME, symbol = %sym, "MERIDIAN order failed: {e}");
                    errors.push(format!("{sym}: {e}"));
                }
            }
        }
        planned.push((sym, actions));
    }
    (planned, applied, errors)
}

fn close_bar_seen(snap: &ReplaySnapshot, session_bars: usize) -> bool {
    !snap.bars_today.is_empty() && snap.bars_today.values().all(|n| *n >= session_bars)
}

/// Fewest bars of `today` cached across the symbols (0 before the first bar
/// or when a symbol has none) — decides whether the close preview applies.
fn today_bars_min(data: &[titan_meridian::sim::SymbolData], today: Option<chrono::NaiveDate>) -> usize {
    data.iter().map(|s| s.days.last().filter(|d| Some(d.date) == today).map(|d| d.len()).unwrap_or(0)).min().unwrap_or(0)
}

/// Which replay the phase needs. ClosePreview, LateClose and AfterClose plan against
/// the *close book* (A flat, B as decided `offset` bars before the close),
/// so both use the close preview until the closing bar itself is cached
/// (`bars_today`). In ClosePreview the preview is only meaningful once the
/// decision bar (index `session_bars − offset − 1`, closing at 15:49 ET)
/// has closed on the clock (`bars_closed`): at 15:48:02 it has not, and a
/// preview on one bar less sizes B on different inputs than the 16:00
/// replay (ANALIZA chat 5 L25). Once it has closed, the usual
/// `bars_missing` retry waits for it to reach the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayKind {
    Regular,
    ClosePreview,
    /// ClosePreview phase, decision bar not closed yet: nothing this minute.
    WaitDecisionBar,
}

fn replay_kind(phase: Phase, bars_closed: Option<usize>, bars_today: usize, session_bars: usize, b_decide_offset: usize) -> ReplayKind {
    match phase {
        // offset 0 = research semantics (decide on the closing bar itself): no
        // exact decision bar exists before the close, preview right away
        Phase::ClosePreview if b_decide_offset > 0 && bars_closed.unwrap_or(0) < session_bars.saturating_sub(b_decide_offset) => ReplayKind::WaitDecisionBar,
        Phase::ClosePreview => ReplayKind::ClosePreview,
        Phase::LateClose | Phase::AfterClose if bars_today < session_bars => ReplayKind::ClosePreview,
        _ => ReplayKind::Regular,
    }
}

/// Age (minutes) of the newest of `bars` cached bars of `date` relative to
/// `now`; `None` before the first bar. A decision computed on stale bars
/// must not be executed (remediation plan P1 "stale bar" rule).
pub fn bar_age_min(date: chrono::NaiveDate, bars: usize, now: chrono::DateTime<Utc>) -> Option<i64> {
    if bars == 0 {
        return None;
    }
    let last_start = super::shadow::bar_start_utc(date, bars - 1)?;
    Some((now.timestamp() - (last_start + 60)).div_euclid(60))
}

/// Age of the *oldest* symbol's newest bar — the snapshot-level view for the
/// UI. Per-symbol execution uses `bar_age_min` directly (one symbol's failed
/// sync must not be hidden by another's fresh bars, nor block it).
pub fn newest_bar_age_min(snap: &ReplaySnapshot, now: chrono::DateTime<Utc>) -> Option<i64> {
    let date = snap.date?;
    let bars = *snap.bars_today.values().min()?;
    bar_age_min(date, bars, now)
}

const MAX_BAR_AGE_MIN: i64 = 3;
const MAX_BAR_RETRIES: u32 = 8;
const MAX_NOTIONAL_X_CAPITAL: f64 = 3.0;

/// Is any symbol's cache still missing the bar that closed at the current
/// minute boundary (RTH only)?
fn bars_missing(snap: &ReplaySnapshot, clock: &SessionClock, now: chrono::DateTime<Utc>) -> bool {
    let Some(closed) = clock.bars_closed(now) else { return false };
    if snap.date != Some(titan_meridian::session::et_date(now)) || closed == 0 {
        return false;
    }
    snap.bars_today.values().any(|n| *n < closed)
}

/// `app`: emit `meridian-state` to a window (None headless). `lease`: the
/// single-executor lock (`daemon.rs`), held for the driver's lifetime.
/// `host`: "app" | "daemon", stamped on every payload.
pub async fn run_meridian_driver(
    state: Arc<TitanState>,
    app: Option<tauri::AppHandle>,
    lease: Option<super::daemon::ExecutorLease>,
    host: &'static str,
) {
    let _lease = lease;
    tracing::info!(component = LOOP_NAME, host, strategy = env!("ST_MERIDIAN_HASH"), "MERIDIAN shadow driver started");
    let mut prev = seed_from_twin_log(Utc::now().with_timezone(&New_York).date_naive());
    if !prev.intents_today.is_empty() {
        tracing::info!(component = LOOP_NAME, already_logged = prev.intents_today.len(), "seeded from today's twin log");
    }
    let mut last_synced_minute: Option<i64> = None;
    let mut bar_retries: u32 = 0;
    let mut close_done_for: Option<chrono::NaiveDate> = None;
    let mut halted_for: Option<chrono::NaiveDate> = None;
    // Today's session clock (early closes) — refreshed from the broker
    // calendar once per date; until then the regular 16:00 close is assumed.
    let mut clock_for: Option<chrono::NaiveDate> = None;
    let mut clock = SessionClock::default();
    let mut trading_day = true;
    loop {
        if state.shutdown.is_cancelled() {
            break;
        }
        let (cfg, key, secret, paper) = {
            let c = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (c.meridian.clone(), c.market.alpaca_key_id.clone(), c.market.alpaca_secret.clone(), c.market.paper_mode)
        };
        let now = Utc::now();
        let today_et = now.with_timezone(&New_York).date_naive();
        if clock_for != Some(today_et) && !key.is_empty() {
            let broker = Broker::new(&state.http_client, paper, &key, &secret);
            match broker.calendar(today_et).await {
                Ok(v) => {
                    let close = parse_close_minutes(&v, today_et);
                    clock = SessionClock::new(close.unwrap_or(FULL_CLOSE_MIN));
                    trading_day = close.is_some();
                    clock_for = Some(today_et);
                    tracing::info!(component = LOOP_NAME, date = %today_et, trading_day = close.is_some(), early_close = clock.is_early_close(), session_bars = clock.session_bars(), "session clock set from broker calendar");
                }
                Err(e) => {
                    // fall back to the crate's early-close list (2016–2027) so a
                    // 13:00 close is honoured even while the broker is silent
                    let fallback = if titan_meridian::session::is_early_close(today_et) { super::clock::EARLY_CLOSE_MIN } else { FULL_CLOSE_MIN };
                    clock = SessionClock::new(fallback);
                    tracing::warn!(component = LOOP_NAME, early_close = clock.is_early_close(), "calendar unavailable — using the built-in early-close list until it answers: {e}");
                }
            }
        }
        let minute = now.timestamp().div_euclid(60);
        // Holiday (calendar answered, no session): observe only — the OPG for a
        // carried B position is placed in the next trading day's 09:00 window.
        let phase = if clock_for == Some(today_et) && !trading_day { Phase::Idle } else { clock.phase_at(now) };
        let close_watch = clock.is_close_watch(now) && close_done_for != Some(today_et);
        let due = cfg.enabled
            && !key.is_empty()
            && (last_synced_minute.is_none()
                || close_watch
                || ((clock.in_poll_window(now) || phase == Phase::OpgWindow) && last_synced_minute != Some(minute)));
        if !due {
            // idle: keep the watchdog informed (the loop is alive, just outside the session)
            {
                let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
                    h.tick();
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(clock.next_poll_secs(now, cfg.poll_secs))) => {}
                _ = state.shutdown.cancelled() => break,
            }
            continue;
        }
        if close_watch {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }

        // ── 2. network, no locks ──
        let mut errors = Vec::new();
        for sym in &cfg.symbols {
            match history::sync(&state.http_client, &key, &secret, sym, cfg.history_days).await {
                Ok(n) if n > 0 => tracing::debug!(component = LOOP_NAME, symbol = %sym, appended = n, "history synced"),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(component = LOOP_NAME, symbol = %sym, close_watch, "history sync failed: {e}");
                    errors.push(format!("{sym}: {e}"));
                }
            }
        }

        // ── 3. pure ──
        let today = Some(now.with_timezone(&New_York).date_naive());
        let (data, mut load_errors) = load_data(&cfg, today);
        errors.append(&mut load_errors);
        if data.is_empty() {
            tracing::warn!(component = LOOP_NAME, errors = ?errors, "no usable history yet");
            last_synced_minute = Some(minute);
            continue;
        }
        let session_bars = clock.session_bars();
        let kind = replay_kind(phase, clock.bars_closed(now), today_bars_min(&data, today), session_bars, cfg.b_decide_offset);
        if kind == ReplayKind::WaitDecisionBar {
            tracing::info!(component = LOOP_NAME, closed = ?clock.bars_closed(now), need = session_bars.saturating_sub(cfg.b_decide_offset), "close preview: decision bar not closed yet — waiting for the next minute");
            last_synced_minute = Some(minute);
            continue;
        }
        let snap = match (kind, today) {
            (ReplayKind::ClosePreview, Some(d)) => replay_close_preview(&cfg, &data, d, session_bars),
            _ => replay(&cfg, &data, today, session_bars),
        };
        // Latency: the bar that closed at this minute boundary is usually
        // published 1–3 s later. Until it is in the cache the replay is one
        // decision behind — retry a few times instead of waiting a minute.
        if !close_watch && bars_missing(&snap, &clock, now) {
            if bar_retries < MAX_BAR_RETRIES {
                bar_retries += 1;
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            tracing::warn!(component = LOOP_NAME, ?phase, bars = ?snap.bars_today, "closed bar still missing after {MAX_BAR_RETRIES} retries — proceeding one bar behind");
        }
        bar_retries = 0;
        last_synced_minute = Some(minute);
        let fresh = new_intents(&prev, &snap);
        for i in &fresh {
            tracing::info!(
                component = LOOP_NAME,
                symbol = %i.symbol,
                bar_idx = i.bar_idx,
                delta_shares = i.delta_shares,
                fill_price = i.fill_price,
                sleeves = ?i.sleeves,
                execute = cfg.execute,
                "MERIDIAN intent"
            );
        }
        let cfg_fp = config_fingerprint(&cfg);
        append_twin_log(&fresh, &snap, &cfg_fp);

        // ── 3b. execution (phase 4b): plan from broker truth; send only when `execute` ──
        // Stale-data guard is per symbol inside `reconcile` (a symbol whose
        // newest bar is older than MAX_BAR_AGE_MIN is planned, not sent).
        let exec_now = Utc::now();
        let broker = Broker::new(&state.http_client, paper, &key, &secret);
        let date_str = today_et.to_string();
        // Live risk rail on broker equity (part of the risk overlay). A halt is
        // sticky for the session even if equity recovers.
        let rail = if phase == Phase::Idle || !cfg.risk_overlay {
            LiveRisk::Normal
        } else if halted_for == Some(today_et) {
            LiveRisk::Halted { reason: "daily loss budget hit earlier today".into() }
        } else {
            match live_risk::fetch_marks(&broker, today_et).await {
                Ok(m) => live_risk::evaluate(&m, &titan_meridian::risk::RiskParams::default()),
                Err(e) => {
                    tracing::warn!(component = LOOP_NAME, "live risk marks unavailable (rail inactive this poll): {e}");
                    errors.push(format!("live risk marks unavailable: {e}"));
                    LiveRisk::Normal
                }
            }
        };
        match &rail {
            LiveRisk::Halted { reason } if halted_for != Some(today_et) => {
                halted_for = Some(today_et);
                tracing::error!(component = LOOP_NAME, reason = %reason, "MERIDIAN live risk HALT — flat now, nothing new today");
            }
            LiveRisk::Half { reason } => tracing::warn!(component = LOOP_NAME, reason = %reason, "MERIDIAN live risk HALF — desired shares halved today"),
            _ => {}
        }
        let (planned, applied, mut exec_errors) = if phase == Phase::Idle {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            let tick = Tick { phase, date: &date_str, rail: &rail, now: exec_now, auction: cfg.use_auction(paper), secs_to_close: clock.secs_to_close(exec_now), phase_secs_left: clock.secs_left_in_phase(exec_now) };
            reconcile(&broker, &cfg, &snap, &tick).await
        };
        errors.append(&mut exec_errors);

        // ── 4. emit after every guard is dropped ──
        let payload = StatePayload {
            host,
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            pid: std::process::id(),
            snapshot: &snap,
            new_intents: &fresh,
            execute: cfg.execute,
            errors: &errors,
            planned: &planned,
            applied: &applied,
            live_risk: &rail,
        };
        if let Ok(v) = serde_json::to_value(&payload) {
            super::daemon::write_state(&v);
            remember_state(v);
        }
        if let Some(app) = &app {
            let _ = app.emit_all("meridian-state", &payload);
        }
        {
            let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
                h.tick();
            }
        }
        if close_watch && close_bar_seen(&snap, session_bars) {
            close_done_for = Some(today_et);
            tracing::info!(component = LOOP_NAME, "close bar replayed for every symbol");
        }
        prev = snap;
    }
    tracing::info!(component = LOOP_NAME, "MERIDIAN shadow driver exiting");
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
