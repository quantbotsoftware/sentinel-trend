use std::collections::HashSet;
use std::sync::Arc;

use chrono::Timelike;
use serde::Serialize;
use tauri::AppHandle;

use crate::market::auto_universe::PromotionReport;
use crate::market::market_state::MarketDataState;
use crate::market::scanner::{self, ScanResult};
use crate::state::{FsmPhase, TitanState};

// ─── AutoUniverse Application ───────────────────────────────────────
//
pub(crate) async fn backfill_symbols_history(
    state: &Arc<TitanState>,
    market_data: &Arc<MarketDataState>,
    symbols: &[String],
) {
    if symbols.is_empty() {
        return;
    }
    
    let (key, secret) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };
    if key.is_empty() || secret.is_empty() {
        return;
    }
    
    let host = "https://data.alpaca.markets";
    let mut seeded = 0usize;
    
    for sym in symbols {
        // Sleep to avoid rate limiting (300ms)
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        
        let url = format!(
            "{host}/v2/stocks/{sym}/bars?timeframe=1Min&limit=200&feed=iex"
        );
        let resp = match state
            .http_client
            .get(&url)
            .header("APCA-API-KEY-ID", &key)
            .header("APCA-API-SECRET-KEY", &secret)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let bars = match body.get("bars").and_then(|b| b.as_array()) {
            Some(arr) => arr,
            None => continue,
        };
        
        let mut candles: Vec<titan_types::Candle> = Vec::with_capacity(bars.len());
        for b in bars {
            let t_str = match b.get("t").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let ts = match chrono::DateTime::parse_from_rfc3339(t_str) {
                Ok(dt) => dt.timestamp(),
                Err(_) => continue,
            };
            let o = b.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let h = b.get("h").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let l = b.get("l").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let c = b.get("c").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let v = b.get("v").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if !c.is_finite() || c <= 0.0 || !o.is_finite() || o <= 0.0
                || !h.is_finite() || h <= 0.0 || !l.is_finite() || l <= 0.0
                || h < l
            {
                continue;
            }
            candles.push(titan_types::Candle {
                time: ts,
                open: o,
                high: h,
                low: l,
                close: c,
                volume: v,
            });
        }
        if candles.is_empty() {
            continue;
        }

        // Ghost-price filter
        if candles.len() >= 20 {
            let mut filtered: Vec<titan_types::Candle> = Vec::with_capacity(candles.len());
            let mut window: std::collections::VecDeque<f64> = std::collections::VecDeque::with_capacity(20);
            let mut rejected = 0usize;
            for c in &candles {
                if window.len() < 20 {
                    window.push_back(c.close);
                    filtered.push(c.clone());
                    continue;
                }
                let mut sorted: Vec<f64> = window.iter().copied().collect();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median = sorted[sorted.len() / 2];
                if median > 0.0 && (c.close - median).abs() / median > 0.5 {
                    rejected += 1;
                    continue;
                }
                window.pop_front();
                window.push_back(c.close);
                filtered.push(c.clone());
            }
            if rejected > 0 {
                tracing::warn!(
                    component = "bootstrap_auto",
                    symbol = %sym,
                    rejected,
                    kept = filtered.len(),
                    "Ghost-price filter rejected anomalous bars during auto-backfill"
                );
            }
            candles = filtered;
        }
        if candles.is_empty() {
            continue;
        }

        let key_1m = format!("{sym}:1m");
        let key_5m = format!("{sym}:5m");
        let mut buffers = market_data.candle_buffers.lock().unwrap_or_else(|p| p.into_inner());
        let buf1 = buffers
            .entry(key_1m)
            .or_insert_with(|| crate::market::candle_buffer::CandleRingBuffer::new(60));
        buf1.seed(candles.clone());
        
        let mut bars_5m: Vec<titan_types::Candle> = Vec::new();
        let mut bucket: Option<titan_types::Candle> = None;
        for c in &candles {
            let bucket_start = (c.time / 300) * 300;
            match bucket.as_mut() {
                Some(b) if b.time == bucket_start => {
                    b.high = b.high.max(c.high);
                    b.low = b.low.min(c.low);
                    b.close = c.close;
                    b.volume += c.volume;
                }
                _ => {
                    if let Some(b) = bucket.take() {
                        bars_5m.push(b);
                    }
                    bucket = Some(titan_types::Candle {
                        time: bucket_start,
                        open: c.open,
                        high: c.high,
                        low: c.low,
                        close: c.close,
                        volume: c.volume,
                    });
                }
            }
        }
        if let Some(b) = bucket.take() {
            bars_5m.push(b);
        }
        let buf5 = buffers
            .entry(key_5m)
            .or_insert_with(|| crate::market::candle_buffer::CandleRingBuffer::new(300));
        buf5.seed(bars_5m);
        seeded += 1;
    }
    
    tracing::info!(
        component = "bootstrap_auto",
        requested = symbols.len(),
        seeded,
        "AutoUniverse dynamic backfill complete"
    );
}

// Lock order: config(1) → market(2) → smarttrend(3) → trading(4),
// each in its own critical section. Returns `None` when the runtime
// opt-in flag is off (`auto_universe.enabled = false`).
pub(crate) fn apply_scan_to_universe(
    state: &Arc<TitanState>,
    market_data: &Arc<MarketDataState>,
    app: &AppHandle,
    results: &[ScanResult],
) -> Option<PromotionReport> {
    let (au_enabled, au_cfg) = {
        let cfg = state.config.lock().ok()?;
        (cfg.auto_universe.enabled, cfg.auto_universe.clone())
    };
    if !au_enabled {
        return None;
    }

    // S19 "Refreshing on demand": promotion is permitted across the
    // entire scan window (04:00–15:30 ET). `process_scan_results`
    // simultaneously demotes auto-pool entries that lost their edge
    // (score decay / stale catalyst), so the watchlist is hot-swapped
    // continuously: stale names drop out, fresh names take their slots.
    let report = {
        let mut market = state.market.lock().ok()?;
        let st = state.smarttrend.lock().ok()?;
        let trading = state.trading.lock().ok()?;
        let mut protected: HashSet<String> = HashSet::new();
        for p in &trading.positions {
            protected.insert(p.symbol.trim().to_uppercase());
        }
        for (sym, fsm) in &st.fsm {
            if matches!(
                fsm.phase,
                FsmPhase::Confirming | FsmPhase::Active | FsmPhase::Exit
            ) {
                protected.insert(sym.trim().to_uppercase());
            }
        }
        let active = market.active_symbol.trim().to_uppercase();
        if !active.is_empty() {
            protected.insert(active);
        }
        market.auto_universe.process_scan_results(
            results,
            &protected,
            &au_cfg,
            chrono::Utc::now(),
        )
    };

    let touched = !report.promoted.is_empty()
        || !report.demoted.is_empty()
        || !report.skipped_blacklisted.is_empty();
    if touched {
        tracing::info!(
            component = "auto_universe",
            promoted = ?report.promoted,
            demoted = ?report.demoted,
            blacklisted_skipped = ?report.skipped_blacklisted,
            "Auto-universe updated from scan results"
        );
        let _ = tauri::Manager::emit_all(app, "auto-universe-update", &report);

        if !report.promoted.is_empty() {
            let state_clone = Arc::clone(state);
            let market_clone = Arc::clone(market_data);
            let promoted_clone = report.promoted.clone();
            tauri::async_runtime::spawn(async move {
                backfill_symbols_history(&state_clone, &market_clone, &promoted_clone).await;
            });
        }
    }

    Some(report)
}

// ─── ScanStatus ─────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, serde::Deserialize)]
pub struct ScanStatus {
    pub running: bool,
    pub last_scan_ts: Option<i64>,
    pub last_scan_duration_ms: u64,
    pub results_count: u32,
    pub auto_active: bool,
}

// ─── Scan Window Guard ──────────────────────────────────────────────
//
// S19 "Fresh-born + refreshing on demand" daily policy (May 2026):
// the scanner is bound to the same intraday lifecycle as the bot's
// trade book — fresh start every premarket, hot-swap candidates on
// demand throughout the session, then wipe at the EOD boundary so
// the next weekday's session has zero carry-over.
//
//   * 04:05 ET: first scan of the day. The 5-minute delay vs the
//     04:00 ET extended-hours open lets Alpaca's snapshot stabilise
//     before we hit it.
//   * 04:05 → 09:30 ET (premarket): cadence 10 min (configurable via
//     `scan_params.scan_interval_premarket_minutes`). News/gappers
//     move fast pre-bell, the watchlist needs a tight refresh.
//   * 09:30 → 15:30 ET (RTH): cadence 30 min (configurable via
//     `scan_params.scan_interval_minutes`). Promotion still active —
//     `process_scan_results` demotes any auto-pool entry that fell
//     below `score_decay_threshold` / went stale, and the same pass
//     promotes fresh names that still meet the gate parameters.
//     This is "refreshing on demand": stale slots get reclaimed.
//   * 15:30 ET: scanner stops entirely. 30-minute "do-not-disturb"
//     buffer ahead of the 15:50 ET flat-by-close so no new
//     admissions race the EOD liquidator.
//   * 15:50 ET: bot's `flat_by_close` flattens every open position.
//   * 16:00 ET: `eod_universe_flush` wipes `auto_pool` + `pinned` +
//     `blacklist` and snapshots every name into `recently_traded`
//     so tomorrow's scanner excludes them for the TTL window
//     (default 18h covers overnight + next premarket).
//   * 16:00 → 04:05 ET next weekday: scanner sleeps.
//   * Saturday/Sunday: scanner sleeps.
//
// IMPORTANT: this gate ONLY guards the auto-scan loop. The bot's entry/
// exit gates remain RTH-only (`bot/entry.rs`, `bot/exit.rs`) so the bot
// will NOT submit orders premarket — premarket scans exist purely to
// pre-stage the watchlist for the 09:30 ET bell.
fn is_scan_active_hours() -> bool {
    let now = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    is_scan_active_at(now)
}

fn is_scan_active_at(et: chrono::DateTime<chrono_tz::Tz>) -> bool {
    if !crate::risk::calendar::is_nyse_trading_day(et.date_naive()) {
        return false;
    }
    let mins = et.hour() * 60 + et.minute();
    // 04:05 ET (245) inclusive → 15:30 ET (930) exclusive.
    (245..930).contains(&mins)
}

/// True if the current moment is in the premarket scan phase
/// (04:05–09:29 ET). Caller picks `scan_interval_premarket_minutes`
/// instead of `scan_interval_minutes` based on this flag.
fn is_premarket_scan_phase_at(et: chrono::DateTime<chrono_tz::Tz>) -> bool {
    if !crate::risk::calendar::is_nyse_trading_day(et.date_naive()) {
        return false;
    }
    let mins = et.hour() * 60 + et.minute();
    // 04:05 ET (245) inclusive → 09:30 ET (570) exclusive.
    (245..570).contains(&mins)
}

/// Picks the tick cadence in minutes for the next sleep based on
/// the current session phase. Premarket = tight refresh, RTH = lazy.
/// Falls back to the RTH interval (`scan_interval_minutes`) when
/// the premarket field is `0` so legacy configs upgrade silently.
fn next_scan_interval_minutes(
    et: chrono::DateTime<chrono_tz::Tz>,
    premarket_mins: u32,
    rth_mins: u32,
) -> u32 {
    if is_premarket_scan_phase_at(et) && premarket_mins > 0 {
        premarket_mins.max(1)
    } else {
        rth_mins.max(1)
    }
}

// ─── Auto-Scan Loop ─────────────────────────────────────────────────

pub async fn run_auto_scan_loop(
    state: Arc<TitanState>,
    market_data: Arc<MarketDataState>,
    app: AppHandle,
) {
    tracing::info!("Auto-scan loop started");

    loop {
        if state.shutdown.is_cancelled() {
            tracing::info!("Auto-scan loop shutting down");
            break;
        }

        let (auto_active, rth_interval, premarket_interval, score_preset, max_candidates, ignition_params) = {
            let cfg = state.config.lock().unwrap();
            (
                cfg.market.auto_scan_active,
                cfg.market.scan_params.scan_interval_minutes.max(1),
                cfg.market.scan_params.scan_interval_premarket_minutes,
                cfg.market.scan_params.score_preset.clone(),
                cfg.market.scan_params.max_candidates as u32,
                cfg.market.scan_params.ignition.clone(),
            )
        };
        // Ignition alerts are a premarket instrument (04:00–09:30 ET); during
        // RTH every new scan entrant would toast + chirp every 10 s (2026-09-18).
        let ignition_mode = scanner::is_premarket_ignition(&score_preset)
            && scanner::in_premarket(chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern));

        // S19 — dual-cadence: 10 min in premarket, 30 min in RTH.
        // Computed fresh every iteration so a tick that straddles
        // 09:30 ET smoothly transitions cadences without restart.
        let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        let interval_mins = next_scan_interval_minutes(
            now_et,
            premarket_interval,
            rth_interval,
        );

        if auto_active && is_scan_active_hours() {
            let phase = if is_premarket_scan_phase_at(now_et) {
                "premarket"
            } else {
                "rth"
            };
            tracing::info!(
                component = "auto_scan",
                preset = %score_preset,
                phase,
                interval_mins,
                max_candidates,
                "Auto-scan tick starting"
            );
            let _ = tauri::Manager::emit_all(
                &app,
                "scan-progress",
                &scanner::ScanProgressEvent {
                    phase: "start".into(),
                    label: format!("Auto scan ({score_preset}) starting"),
                    percent: 5,
                    current: None,
                    total: None,
                },
            );
            {
                let mut market = state.market.lock().unwrap();
                market.scan_status.running = true;
                market.scan_status.auto_active = true;
            }
            let status = { state.market.lock().unwrap().scan_status.clone() };
            let _ = tauri::Manager::emit_all(&app, "scan-status", &status);

            let start = std::time::Instant::now();
            let progress_reporter: scanner::ProgressReporter = Arc::new({
                let app = app.clone();
                move |progress| {
                    let _ = tauri::Manager::emit_all(&app, "scan-progress", &progress);
                }
            });
            match scanner::run_scan_with_progress(
                &state,
                &market_data,
                Some(progress_reporter),
            )
            .await
            {
                Ok(results) => {
                    let elapsed = start.elapsed().as_millis() as u64;
                    let count = results.len() as u32;
                    let now_ts = chrono::Utc::now().timestamp();

                    // Premarket Ignition: diff against the previous tick
                    // *before* overwriting so each entrant alerts once.
                    let ignitions = {
                        let mut market = state.market.lock().unwrap();
                        let ignitions = if ignition_mode {
                            scanner::new_ignitions(&market.scan_results, &results, &ignition_params)
                        } else {
                            Vec::new()
                        };
                        market.scan_results = results.clone();
                        market.scan_status.running = false;
                        market.scan_status.last_scan_ts = Some(now_ts);
                        market.scan_status.last_scan_duration_ms = elapsed;
                        market.scan_status.results_count = count;
                        ignitions
                    };
                    // Guard dropped above (S05 emit-after-drop).
                    if !ignitions.is_empty() {
                        for a in &ignitions {
                            tracing::info!(
                                component = "auto_scan",
                                symbol = %a.symbol,
                                score = a.score,
                                change_pct = a.change_percent,
                                rvol = a.rvol,
                                dollar_volume = a.dollar_volume,
                                headline = %a.news_headline,
                                "IGNITION: new premarket entrant"
                            );
                        }
                        let _ = tauri::Manager::emit_all(&app, "scanner-ignition", &ignitions);
                    }

                    // S18 — feed AutoUniverse (promote top candidates,
                    // demote stale/decayed entries). Shared helper so
                    // the manual `apply_scan_to_watchlist` command runs
                    // through the exact same path. Gated on the runtime
                    // opt-in flag (`auto_universe.enabled`).
                    let _ = apply_scan_to_universe(&state, &market_data, &app, &results);

                    let _ = tauri::Manager::emit_all(&app, "scan-results", &results);
                    let status = { state.market.lock().unwrap().scan_status.clone() };
                    let _ = tauri::Manager::emit_all(&app, "scan-status", &status);
                    let _ = tauri::Manager::emit_all(
                        &app,
                        "scan-progress",
                        &scanner::ScanProgressEvent {
                            phase: "complete".into(),
                            label: format!("Auto scan complete ({score_preset}): {count} results"),
                            percent: 100,
                            current: None,
                            total: None,
                        },
                    );

                    tracing::info!(
                        component = "auto_scan",
                        results = count,
                        elapsed_ms = elapsed,
                        "Auto-scan tick complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(component = "auto_scan", "Auto-scan failed: {e}");
                    {
                        let mut market = state.market.lock().unwrap();
                        market.scan_status.running = false;
                    }
                    let status = { state.market.lock().unwrap().scan_status.clone() };
                    let _ = tauri::Manager::emit_all(&app, "scan-status", &status);
                    let _ = tauri::Manager::emit_all(
                        &app,
                        "scan-progress",
                        &scanner::ScanProgressEvent {
                            phase: "error".into(),
                            label: format!("Auto scan failed ({score_preset}): {e}"),
                            percent: 100,
                            current: None,
                            total: None,
                        },
                    );
                }
            }
        } else {
            // Outside scan window / auto off — purge prior-session
            // actionable caches (SC11 TODAY-ONLY).
            let et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
            let today = scanner::session_date_et_str(et);
            let mut market = state.market.lock().unwrap();
            market.scan_status.auto_active = auto_active;
            market.scan_status.running = false;
            let stale = market
                .scan_status
                .last_scan_ts
                .map(|ts| !scanner::timestamp_is_today_et(ts, et))
                .unwrap_or(false)
                || market
                    .scan_results
                    .iter()
                    .any(|r| r.session_date_et != today && !r.session_date_et.is_empty());
            if stale || !scanner::in_actionable_scan_window(et) {
                if !market.scan_results.is_empty() || market.scan_status.last_scan_ts.is_some() {
                    market.scan_results.clear();
                    market.scan_status.last_scan_ts = None;
                    market.scan_status.results_count = 0;
                }
                if !market.movers.is_empty()
                    && market.movers.iter().any(|m| m.session_date_et != today)
                {
                    market.movers.clear();
                }
            }
        }

        {
            let mut sys = state.system.lock().unwrap();
            if let Some(h) = sys.loops.get_mut("auto_scan") {
                h.tick();
            }
        }

        let mut remaining_secs = (interval_mins as u64) * 60;
        while remaining_secs > 0 {
            let chunk_secs = remaining_secs.min(30);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(chunk_secs)) => {
                    remaining_secs = remaining_secs.saturating_sub(chunk_secs);
                    let mut sys = state.system.lock().unwrap();
                    if let Some(h) = sys.loops.get_mut("auto_scan") {
                        h.tick();
                    }
                }
                _ = state.shutdown.cancelled() => { break; }
            }
            if state.shutdown.is_cancelled() {
                break;
            }
        }
    }
}

// ─── EOD Universe Flush Loop ────────────────────────────────────────
//
// S19 "Fresh-born" — at 16:00 ET (RTH close) the watchlist is wiped
// to zero so tomorrow's session starts from a clean scan. The bot's
// `flat_by_close` should have closed every position 10 min earlier,
// so by the time this loop fires `protected_symbols` is normally
// empty. If it isn't (manual fill still settling, broker-side
// position the bot didn't see, etc.) we leave those names in
// `auto_pool` so the streamer keeps feeding bars — the next EOD
// boundary or daily_reset will sweep them.
//
// Idempotent: a date-guard prevents double-flush within the same
// trading session even if the loop's timing jitters across 16:00 ET.

fn is_eod_flush_time(et: chrono::DateTime<chrono_tz::Tz>) -> bool {
    if !crate::risk::calendar::is_nyse_trading_day(et.date_naive()) {
        return false;
    }
    let mins = et.hour() * 60 + et.minute();
    // 16:00 ET (960) inclusive → 16:05 ET (965) exclusive.
    // 5-minute window gives the loop's tick cadence (30s) plenty
    // of room without ever firing twice (date-guard handles
    // double-fire anyway).
    (960..965).contains(&mins)
}

pub async fn run_eod_universe_flush_loop(state: Arc<TitanState>, app: AppHandle) {
    tracing::info!(component = "eod_flush", "EOD universe flush loop started");

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    let mut last_flush_date: Option<chrono::NaiveDate> = None;

    loop {
        ticker.tick().await;
        if state.shutdown.is_cancelled() {
            tracing::info!(component = "eod_flush", "EOD flush loop shutting down");
            break;
        }

        {
            let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = sys.loops.get_mut("eod_flush") {
                h.tick();
            }
        }

        let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        let today = now_et.date_naive();

        if let Some(d) = last_flush_date {
            if d == today {
                continue;
            }
        }
        if !is_eod_flush_time(now_et) {
            continue;
        }

        // Snapshot AutoUniverseConfig (for the recently_traded TTL).
        let au_cfg = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.auto_universe.clone()
        };

        // Protect symbols with a real risk footprint at flush time:
        //   1) Open broker positions (flat_by_close may still be settling).
        //   2) FSM phase = Confirming or Active — bar pipeline must
        //      keep feeding these so an in-flight entry/exit ladder
        //      doesn't dangle.
        //
        // S22.1 (2026-05-20) — `FsmPhase::Exit` REMOVED from the
        // protection set. Exit is the post-trade cooldown phase; the
        // position itself was either already closed (and `trading.
        // positions` is empty) or is still settling (in which case
        // it's covered by branch 1 above). Keeping Exit in protected
        // was preserving every symbol whose FSM remembered an old
        // cooldown, defeating the S19 fresh-born intent. Live
        // evidence (2026-05-20 16:00 ET flush): BDTX had
        // `phase=Exit, cooldown=38` from a 14:00 ET Confirming round
        // that never converted to a trade; protected retained it
        // and `auto_cleared=0` despite no real position existing.
        // S29.1 (2026-05-28 18:23 ET) — REMOVED FSM-phase protection.
        //
        // Pre-S29.1, this loop also added every symbol with `FsmPhase::
        // Confirming | Active` to `protected`, on the assumption that
        // FSM Active = "the bot might still have a position settling".
        //
        // That assumption was historically valid (S22, May 20) but
        // broke after S35.4/S35.6 introduced multi-key FSM state
        // (symbol-only + per-TF). Live evidence (2026-05-28 16:00 ET):
        //   - AIRO catalyst pump 7.45 → 9.08 (+22%)
        //   - At 16:00:27 ET, AIRO/AIRO:1m/AIRO:5m all reported
        //     `phase=Active` because the 15:55-16:00 ET bar was still
        //     trending up
        //   - Bot was PDT-blocked, never entered AIRO (zero positions
        //     in `trading.positions`, zero journal entries, $0 P/L)
        //   - EOD flush saw FSM=Active and protected AIRO
        //   - AIRO survived in auto_pool indefinitely while QBTS was
        //     the only intended pinned symbol post-flush
        //
        // Resolution: protect ONLY symbols with real broker positions.
        // FSM Active without an open position is residual signal —
        // `flat_by_close` at 15:50 ET already closed every position
        // 10 minutes earlier by design. If broker reports a position
        // we didn't expect (settling ladder, manual fill mid-flush),
        // it's still in `trading.positions` and gets protected via
        // the loop above.
        let report = {
            let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
            let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            let mut protected: HashSet<String> = HashSet::new();
            for p in &trading.positions {
                protected.insert(p.symbol.trim().to_uppercase());
            }
            // Cleanup expired recently_traded so the snapshot we're
            // about to write doesn't accumulate forever.
            market.auto_universe.cleanup_recently_traded(
                chrono::Utc::now(),
                au_cfg.recently_traded_ttl_hours,
            );
            market
                .auto_universe
                .eod_flush(&protected, chrono::Utc::now())
        };

        // SC11 — wipe actionable scan/movers caches so overnight UI
        // cannot show yesterday's names as "today".
        {
            let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
            market.scan_results.clear();
            market.movers.clear();
            market.scan_status.last_scan_ts = None;
            market.scan_status.results_count = 0;
        }
        let _ = tauri::Manager::emit_all(&app, "scan-results", &Vec::<scanner::ScanResult>::new());
        let _ = tauri::Manager::emit_all(&app, "discovery-movers", &Vec::<crate::state::Mover>::new());

        tracing::info!(
            component = "eod_flush",
            auto_cleared = report.auto_cleared,
            pinned_cleared = report.pinned_cleared,
            blacklist_cleared = report.blacklist_cleared,
            recently_traded_size = report.recently_traded_size,
            "EOD AutoUniverse flush complete — tomorrow starts fresh"
        );
        let _ = tauri::Manager::emit_all(&app, "auto-universe-eod-flush", &report);

        // S22.2 (2026-05-20) — FSM zombie cleanup. The sentinel
        // evaluation loop maintains an FSM entry per symbol it has
        // ever seen (active_symbol switches, scan rotations, etc.),
        // and those entries persist across sessions because they're
        // snapshotted to `fsm_states.json`. Without cleanup the FSM
        // map drifts to dozens of stale tickers (live evidence
        // 2026-05-20 16:45 ET: 21 FSM states tracked but only 3
        // names actually in the watchlist — AAPL, BKKT, BLND, CMBT,
        // GENI, CRMG, STUB and 11 more were ghost entries from
        // earlier chart selections / yesterday's scans).
        //
        // After the EOD flush the only legitimate FSM consumers
        // are pinned ∪ auto_pool (the names we'll stream tomorrow)
        // plus any symbol that survived as `protected` (open
        // position / Confirming / Active mid-flight). Everything
        // else can be dropped — they're zombie evaluators wasting
        // CPU on every bar tick and they were the actual cause
        // of `auto_cleared=0` today (their `phase=Exit` cooldowns
        // forced them into `protected`).
        let fsm_cleaned = {
            let market = state.market.lock().unwrap_or_else(|p| p.into_inner());
            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            // S29.1: keep = (pinned ∪ auto_pool after flush) ∪ open positions.
            // FSM-phase preservation was removed for the same reason as the
            // protected set above: residual `Active` without an open position
            // is meaningless post flat_by_close. We need to handle the
            // multi-key FSM map (symbol-only + symbol:tf) carefully — strip
            // the `:tf` suffix when checking against active_set so e.g.
            // "AIRO:1m" doesn't accidentally survive when "AIRO" was wiped.
            let mut keep: HashSet<String> = market.auto_universe.active_set();
            for p in &trading.positions {
                keep.insert(p.symbol.trim().to_uppercase());
            }
            let before = st.fsm.len();
            st.fsm.retain(|sym, _| {
                // S29.1: FSM keys may be "SYMBOL" or "SYMBOL:TF" (S35.4
                // legacy + S35.6 symbol-only). Compare against the
                // `keep` set (which contains uppercase symbols only)
                // by stripping any TF suffix.
                let base = sym
                    .split(':')
                    .next()
                    .unwrap_or(sym)
                    .trim()
                    .to_uppercase();
                keep.contains(&base)
            });
            before - st.fsm.len()
        };
        if fsm_cleaned > 0 {
            tracing::info!(
                component = "eod_flush",
                fsm_cleaned,
                "FSM zombie cleanup — removed stale per-symbol state machines"
            );
        }

        last_flush_date = Some(today);
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::auto_universe::{AutoEntry, AutoUniverse};
    use chrono::TimeZone;

    fn et(y: i32, mo: u32, d: u32, h: u32, m: u32) -> chrono::DateTime<chrono_tz::Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(y, mo, d, h, m, 0)
            .single()
            .expect("valid ET timestamp")
    }

    // ── S19 scan window: 04:05 ET → 15:30 ET, weekdays ──────────────
    //
    // Scanner runs (and PROMOTES) across the entire window with a
    // dual-cadence: 10 min in premarket (04:05–09:30), 30 min in
    // RTH (09:30–15:30). "Refreshing on demand" — RTH scans can
    // both add new names and demote stale ones in the same pass.

    #[test]
    fn scan_window_opens_at_04_05_et() {
        // 2026-05-11 is a Monday. First scan of the day at 04:05 ET
        // — gives Alpaca's snapshot 5 min after the 04:00 ET
        // extended-hours open to stabilise.
        assert!(is_scan_active_at(et(2026, 5, 11, 4, 5)), "first scan at 04:05 ET");
        assert!(is_scan_active_at(et(2026, 5, 11, 4, 15)), "second scan at 04:15 ET");
    }

    #[test]
    fn scan_window_closed_before_04_05_et() {
        assert!(!is_scan_active_at(et(2026, 5, 11, 4, 4)), "04:04 too early");
        assert!(!is_scan_active_at(et(2026, 5, 11, 3, 59)));
        assert!(!is_scan_active_at(et(2026, 5, 11, 0, 0)));
    }

    #[test]
    fn scan_window_active_through_rth_until_15_29_et() {
        assert!(is_scan_active_at(et(2026, 5, 11, 9, 30)), "bell: scanner ON");
        assert!(is_scan_active_at(et(2026, 5, 11, 12, 0)), "midday: scanner ON");
        assert!(is_scan_active_at(et(2026, 5, 11, 15, 0)), "afternoon: scanner ON");
        assert!(is_scan_active_at(et(2026, 5, 11, 15, 29)), "last allowed minute");
    }

    #[test]
    fn scan_window_closes_at_15_30_et_buffer_before_flat_by_close() {
        // S19: 20-min quiet buffer ahead of 15:50 ET flat_by_close.
        assert!(!is_scan_active_at(et(2026, 5, 11, 15, 30)), "scanner OFF at 15:30");
        assert!(!is_scan_active_at(et(2026, 5, 11, 15, 50)), "flat_by_close: scanner OFF");
        assert!(!is_scan_active_at(et(2026, 5, 11, 16, 0)), "RTH close: scanner OFF");
        assert!(!is_scan_active_at(et(2026, 5, 11, 18, 0)), "afterhours: scanner OFF");
        assert!(!is_scan_active_at(et(2026, 5, 11, 23, 59)), "overnight: scanner OFF");
    }

    #[test]
    fn scan_window_closed_on_weekends() {
        // 2026-05-09 is Saturday, 2026-05-10 is Sunday.
        assert!(!is_scan_active_at(et(2026, 5, 9, 12, 0)));
        assert!(!is_scan_active_at(et(2026, 5, 10, 12, 0)));
        assert!(!is_scan_active_at(et(2026, 5, 9, 9, 30)));
    }

    #[test]
    fn scan_window_closed_on_nyse_holidays_2026() {
        // S19 — scanner respects NYSE full-day closures.
        // 2026-05-25 Memorial Day (Monday, would otherwise be active).
        assert!(!is_scan_active_at(et(2026, 5, 25, 10, 0)));
        // 2026-07-03 Independence Day observed (Friday).
        assert!(!is_scan_active_at(et(2026, 7, 3, 10, 0)));
        // 2026-11-26 Thanksgiving (Thursday).
        assert!(!is_scan_active_at(et(2026, 11, 26, 12, 0)));
        // 2026-12-25 Christmas (Friday).
        assert!(!is_scan_active_at(et(2026, 12, 25, 8, 0)));
        // Sanity: 2026-05-26 (Tuesday after Memorial Day) IS active.
        assert!(is_scan_active_at(et(2026, 5, 26, 10, 0)));
    }

    #[test]
    fn scan_window_closed_on_nyse_holidays_2027() {
        // 2027-01-01 New Year's Day (Friday).
        assert!(!is_scan_active_at(et(2027, 1, 1, 10, 0)));
        // 2027-07-05 Independence Day observed (Monday).
        assert!(!is_scan_active_at(et(2027, 7, 5, 10, 0)));
    }

    #[test]
    fn premarket_phase_closed_on_nyse_holidays() {
        // No dual-cadence on holidays — premarket gate also blocks.
        assert!(!is_premarket_scan_phase_at(et(2026, 5, 25, 7, 0)), "Memorial Day premarket");
        assert!(!is_premarket_scan_phase_at(et(2026, 11, 26, 8, 0)), "Thanksgiving premarket");
        // Normal Monday: premarket OK.
        assert!(is_premarket_scan_phase_at(et(2026, 5, 18, 7, 0)));
    }

    #[test]
    fn eod_flush_closed_on_nyse_holidays() {
        // No EOD flush on closed days — nothing to wipe, and the
        // bot was never trading those days anyway.
        assert!(!is_eod_flush_time(et(2026, 5, 25, 16, 0)), "Memorial Day");
        assert!(!is_eod_flush_time(et(2026, 11, 26, 16, 0)), "Thanksgiving");
        assert!(!is_eod_flush_time(et(2026, 12, 25, 16, 0)), "Christmas");
        // Normal Monday: flush fires.
        assert!(is_eod_flush_time(et(2026, 5, 18, 16, 0)));
    }

    // ── S19 dual cadence: 10 min premarket, 30 min RTH ──────────────

    #[test]
    fn premarket_phase_returns_true_inside_04_05_to_09_29_et() {
        assert!(is_premarket_scan_phase_at(et(2026, 5, 11, 4, 5)));
        assert!(is_premarket_scan_phase_at(et(2026, 5, 11, 7, 0)));
        assert!(is_premarket_scan_phase_at(et(2026, 5, 11, 9, 29)));
    }

    #[test]
    fn premarket_phase_returns_false_outside_window() {
        assert!(!is_premarket_scan_phase_at(et(2026, 5, 11, 4, 4)), "too early");
        assert!(!is_premarket_scan_phase_at(et(2026, 5, 11, 9, 30)), "bell rings → RTH");
        assert!(!is_premarket_scan_phase_at(et(2026, 5, 11, 12, 0)), "midday RTH");
        assert!(!is_premarket_scan_phase_at(et(2026, 5, 11, 16, 0)), "after close");
        assert!(!is_premarket_scan_phase_at(et(2026, 5, 9, 7, 0)), "weekend");
    }

    #[test]
    fn cadence_is_10_in_premarket_30_in_rth() {
        // Premarket → 10-minute cadence.
        assert_eq!(next_scan_interval_minutes(et(2026, 5, 11, 4, 5), 10, 30), 10);
        assert_eq!(next_scan_interval_minutes(et(2026, 5, 11, 8, 0), 10, 30), 10);
        // RTH → 30-minute cadence.
        assert_eq!(next_scan_interval_minutes(et(2026, 5, 11, 9, 30), 10, 30), 30);
        assert_eq!(next_scan_interval_minutes(et(2026, 5, 11, 14, 0), 10, 30), 30);
        // Outside scan window (e.g. weekend) → RTH default applies
        // to the sleep, doesn't matter since the gate also blocks.
        assert_eq!(next_scan_interval_minutes(et(2026, 5, 9, 12, 0), 10, 30), 30);
    }

    #[test]
    fn cadence_falls_back_to_rth_when_premarket_zero() {
        // Legacy config without `scan_interval_premarket_minutes`
        // deserialises with the field at zero → use the RTH value
        // so the loop still runs (no zero-sleep tight loop).
        assert_eq!(next_scan_interval_minutes(et(2026, 5, 11, 4, 5), 0, 30), 30);
    }

    #[test]
    fn cadence_clamped_to_at_least_one_minute() {
        // Guard against absurd config values causing tight loops.
        assert_eq!(next_scan_interval_minutes(et(2026, 5, 11, 4, 5), 0, 0), 1);
    }

    // ── S19 EOD flush trigger: 16:00–16:04 ET, weekdays ─────────────

    #[test]
    fn eod_flush_fires_at_16_00_et() {
        assert!(is_eod_flush_time(et(2026, 5, 11, 16, 0)));
        assert!(is_eod_flush_time(et(2026, 5, 11, 16, 4)), "30s ticker grace ends at :04");
    }

    #[test]
    fn eod_flush_inactive_outside_window() {
        assert!(!is_eod_flush_time(et(2026, 5, 11, 15, 59)));
        assert!(!is_eod_flush_time(et(2026, 5, 11, 16, 5)), ":05 closes the trigger window");
        assert!(!is_eod_flush_time(et(2026, 5, 11, 18, 0)));
        assert!(!is_eod_flush_time(et(2026, 5, 11, 4, 0)), "premarket: definitely not EOD");
    }

    #[test]
    fn eod_flush_skips_weekends() {
        assert!(!is_eod_flush_time(et(2026, 5, 9, 16, 0)), "Saturday");
        assert!(!is_eod_flush_time(et(2026, 5, 10, 16, 0)), "Sunday");
    }

    // ── S29.1 EOD flush no longer protects FSM-Active without position ──

    #[test]
    fn s29_1_eod_flush_purges_symbol_with_fsm_active_but_no_position() {
        // Live scenario reproducer (2026-05-28 16:00 ET): AIRO catalyst
        // pump pushed FSM into `Active` at the EOD boundary, but bot
        // was PDT-blocked and never opened a position. Pre-S29.1 the
        // symbol survived in `auto_pool` because FSM=Active triggered
        // the protection path. Post-S29.1, only open broker positions
        // protect — FSM phase is irrelevant.
        let mut universe = AutoUniverse::default();
        universe
            .auto_pool
            .insert("AIRO".into(), AutoEntry::new("AIRO".into(), 84.0, chrono::Utc::now()));
        universe.pinned.push("QBTS".into());

        // Simulate the post-S29.1 protected set: empty positions list,
        // no FSM-phase contribution.
        let protected: HashSet<String> = HashSet::new();

        let report = universe.eod_flush(&protected, chrono::Utc::now());

        assert_eq!(report.auto_cleared, 1, "AIRO must be cleared even with FSM=Active");
        assert!(!universe.auto_pool.contains_key("AIRO"), "AIRO removed from auto_pool");
        assert_eq!(universe.pinned, vec!["QBTS".to_string()], "pinned QBTS survives");
        assert!(
            universe.recently_traded.contains_key("AIRO"),
            "AIRO snapshotted to recently_traded for 18h cooldown"
        );
    }

    #[test]
    fn s29_1_eod_flush_still_protects_real_open_positions() {
        // Sanity: the one legitimate protection path (open broker
        // position) still works. Symbol with a real position survives
        // the flush; symbol without a position gets wiped even if its
        // FSM was hot.
        let mut universe = AutoUniverse::default();
        universe.auto_pool.insert(
            "HELD".into(),
            AutoEntry::new("HELD".into(), 80.0, chrono::Utc::now()),
        );
        universe.auto_pool.insert(
            "DROP".into(),
            AutoEntry::new("DROP".into(), 70.0, chrono::Utc::now()),
        );

        let mut protected: HashSet<String> = HashSet::new();
        protected.insert("HELD".into());

        let report = universe.eod_flush(&protected, chrono::Utc::now());

        assert_eq!(report.auto_cleared, 1, "only DROP cleared");
        assert!(universe.auto_pool.contains_key("HELD"), "open position preserved");
        assert!(!universe.auto_pool.contains_key("DROP"), "FSM-only signal does NOT preserve");
    }
}
