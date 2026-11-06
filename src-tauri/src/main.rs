#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

mod backtest;
mod commands;
mod config;
mod error;
mod execution;
mod health;
mod logging;
mod market;
mod meridian;
mod ml;
mod ops;
mod paths;
mod risk;
mod serde_decimal;
mod smarttrend;
mod state;

use std::sync::Arc;

use serde::Serialize;
use rust_decimal::Decimal;

use market::market_state::MarketDataState;
use state::TitanState;

// ─── Build info ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct BuildInfo {
    version: &'static str,
    git_hash: &'static str,
    build_date: &'static str,
}

#[tauri::command]
fn get_build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        git_hash: env!("ST_GIT_HASH"),
        build_date: env!("ST_BUILD_DATE"),
    }
}

// ─── Health command ─────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthResponse {
    uptime_secs: i64,
    session_id: String,
    /// Two runtime modes — `paper_mode=true` routes to the Alpaca
    /// **PAPER** API (virtual money, real fills) and is the default.
    /// `paper_mode=false` routes to the Alpaca **LIVE** API (real
    /// money — operator must explicitly opt in via the UI).
    paper_mode: bool,
    loops: std::collections::HashMap<String, state::LoopHealth>,
    last_persist_ts: Option<String>,
    active_symbol: String,
    trading_mode: String,
    breaker_level: String,
    stream_connected: bool,
}

#[tauri::command]
fn get_health(state: tauri::State<'_, Arc<TitanState>>) -> Result<HealthResponse, String> {
    // Lock order: market(2), smarttrend(3), risk(5), system(7)
    let (active_symbol, stream_connected) = {
        let m = state.market.lock().map_err(|e| format!("lock: {e}"))?;
        (m.active_symbol.clone(), m.connected)
    };
    let trading_mode = {
        let st = state.smarttrend.lock().map_err(|e| format!("lock: {e}"))?;
        format!("{:?}", st.mode)
    };
    let breaker_level = {
        let r = state.risk.lock().map_err(|e| format!("lock: {e}"))?;
        format!("{:?}", r.breaker)
    };
    let paper_mode = {
        let cfg = state.config.lock().map_err(|e| format!("lock: {e}"))?;
        cfg.market.paper_mode
    };
    let (uptime_secs, loops, last_persist_ts) = {
        let sys = state.system.lock().map_err(|e| format!("lock: {e}"))?;
        (
            sys.uptime_secs(),
            sys.loops.clone(),
            sys.last_persist_ts.map(|t| t.to_rfc3339()),
        )
    };

    Ok(HealthResponse {
        uptime_secs,
        session_id: state.session_id.clone(),
        paper_mode,
        loops,
        last_persist_ts,
        active_symbol,
        trading_mode,
        breaker_level,
        stream_connected,
    })
}

// ─── App lifecycle ──────────────────────────────────────────────────

fn main() {
    // Headless CLI subcommands (backtest / orb-tf / orb-sweep) run the
    // backtest engine from the terminal and exit before any Tauri /
    // live-trading setup. See `backtest::cli`.
    let cli_args: Vec<String> = std::env::args().collect();
    if backtest::cli::dispatch(&cli_args) {
        return;
    }
    // MERIDIAN headless executor (systemd unit) — same binary, same config,
    // same twin log as the GUI; see `meridian/daemon.rs`.
    if cli_args.iter().any(|a| a == "--meridian-daemon") {
        meridian::run_headless();
        return;
    }

    logging::init();

    tracing::info!(
        "SentinelTrend v{} ({}) starting...",
        env!("CARGO_PKG_VERSION"),
        env!("ST_GIT_HASH"),
    );

    // ML accelerator banner. Logged once at boot so the operator can
    // see at a glance whether OMEGA training will hit the GPU.
    {
        let cuda_built = cfg!(feature = "cuda");
        let metal_built = cfg!(feature = "metal");
        if cuda_built {
            match candle_core::Device::cuda_if_available(0) {
                Ok(_) => tracing::info!(
                    component = "ml",
                    backend = "cuda",
                    "ML accelerator: CUDA available — OMEGA training will use the GPU"
                ),
                Err(e) => tracing::warn!(
                    component = "ml",
                    backend = "cuda",
                    error = %e,
                    "ML accelerator: built with `cuda` feature but device unavailable; falling back to CPU"
                ),
            }
        } else if metal_built {
            tracing::info!(
                component = "ml",
                backend = "metal",
                "ML accelerator: built with `metal` feature"
            );
        } else {
            tracing::info!(
                component = "ml",
                backend = "cpu",
                "ML accelerator: CPU-only build (rebuild with `bash build.sh --cuda` for GPU training)"
            );
        }
    }

    if let Err(e) = paths::ensure_data_dirs() {
        tracing::error!("Failed to create data directories: {e}");
    }

    // PRIME F1 — decision journal (persistent audit of enter/veto
    // decisions). Non-fatal on failure; trading proceeds regardless.
    ops::decision_log::init(paths::data_dir().join("memory/decisions.sqlite"));

    let loaded_config = ops::persistence::load_config();
    // v2.5 — DRY mode removed. Two runtime modes only:
    //   * paper_mode=true  → Alpaca PAPER API (virtual money, real fills)
    //   * paper_mode=false → Alpaca LIVE  API (real money — DANGER)
    // The operator opts in to LIVE explicitly via the UI; misconfigured
    // keys will hard-fail at order submission, not silently mis-route.
    if loaded_config.market.paper_mode {
        tracing::info!(
            component = "bootstrap",
            paper_mode = true,
            "PAPER mode active — Alpaca PAPER API (virtual money, real fills)."
        );
    } else {
        tracing::warn!(
            component = "bootstrap",
            paper_mode = false,
            "LIVE mode active — orders will route to Alpaca LIVE (REAL MONEY)."
        );
    }
    tracing::info!(
        "Config loaded: symbol={}, paper={}",
        loaded_config.ui.active_symbol,
        loaded_config.market.paper_mode,
    );

    let titan_state = Arc::new(TitanState::new(loaded_config));

    {
        // Lock order: config(1), then market(2)
        let sym = titan_state.config.lock().unwrap().ui.active_symbol.clone();
        titan_state.market.lock().unwrap().active_symbol = sym;
    }

    // Restore FSM states: dedicated file takes priority over snapshot
    let fsm_from_file = ops::persistence::load_fsm_states();
    let snapshot = ops::persistence::load_snapshot();
    if !fsm_from_file.is_empty() {
        tracing::info!("Restored {} FSM states from fsm_states.json", fsm_from_file.len());
        let mut st = titan_state.smarttrend.lock().unwrap();
        st.fsm = fsm_from_file;
    } else if let Some(ref snap) = snapshot {
        tracing::info!(
            "Restored snapshot from {} (session {})",
            snap.saved_at.format("%Y-%m-%d %H:%M:%S"),
            snap.session_id,
        );
        let mut st = titan_state.smarttrend.lock().unwrap();
        st.fsm = snap.fsm_states.clone();
    }
    // S17.0 — `trading_mode` MUST be restored from snapshot independent
    // of the FSM-state branch above. The original code only reached the
    // `st.mode = snap.trading_mode` line when `fsm_states.json` was
    // missing, so once the dedicated FSM file existed (every restart
    // after the first snapshot save) the mode silently reverted to
    // `TradingMode::default() == Manual`. Live evidence: editing
    // `state_snapshot.json` to set mode=Auto, restarting, then
    // observing the persistence loop write Manual back 30 s later.
    if let Some(ref snap) = snapshot {
        let lock_manual = {
            let c = titan_state.config.lock().unwrap();
            c.meridian.execute && c.meridian.lock_legacy_manual
        };
        let mut st = titan_state.smarttrend.lock().unwrap();
        st.mode = if lock_manual { state::TradingMode::Manual } else { snap.trading_mode };
        tracing::info!(lock_manual, "Restored trading_mode={:?} from snapshot", st.mode);
    }

    // Restore wallets + order history + auto-universe from snapshot
    if let Some(snap) = snapshot {
        {
            let mut st = titan_state.smarttrend.lock().unwrap();
            if snap.user_wallet.balance > rust_decimal::Decimal::ZERO || snap.user_wallet.total_deposited > rust_decimal::Decimal::ZERO
                || snap.bot_wallet.balance > rust_decimal::Decimal::ZERO || snap.bot_wallet.total_deposited > rust_decimal::Decimal::ZERO
            {
                tracing::info!(
                    "Restored wallets: user={:.2}, bot={:.2}",
                    snap.user_wallet.balance,
                    snap.bot_wallet.balance,
                );
                st.user_wallet = snap.user_wallet;
                st.bot_wallet = snap.bot_wallet;
            }
        }
        if !snap.order_history.is_empty() {
            let mut trading = titan_state.trading.lock().unwrap();
            tracing::info!("Restored {} order history entries", snap.order_history.len());
            trading.order_history = snap.order_history;
        }

        // S18 — restore AutoUniverse (pinned + auto-pool + blacklist)
        // from the snapshot. The blacklist's TTL is wall-clock based,
        // so a stale entry whose TTL expired during downtime cleans
        // itself up on the next call to `is_blacklisted` /
        // `cleanup_expired_blacklist`. The first call from auto_scan
        // loop's `process_scan_results` does this implicitly.
        if !snap.auto_universe.pinned.is_empty()
            || !snap.auto_universe.auto_pool.is_empty()
            || !snap.auto_universe.blacklist.is_empty()
        {
            let mut market = titan_state.market.lock().unwrap();
            tracing::info!(
                "Restored AutoUniverse: pinned={}, auto={}, blacklist={}",
                snap.auto_universe.pinned.len(),
                snap.auto_universe.auto_pool.len(),
                snap.auto_universe.blacklist.len(),
            );
            market.auto_universe = snap.auto_universe;
        }
    }

    // S18 — first-run migration: if AutoUniverse is empty (fresh
    // upgrade), seed the `pinned` list from `cfg.ui.favorites` so the
    // existing user sees no behaviour change. Idempotent — only fires
    // when both `pinned` and `auto_pool` are empty.
    {
        let cfg = titan_state.config.lock().unwrap();
        let favorites = cfg.ui.favorites.clone();
        let au_cfg = cfg.auto_universe.clone();
        drop(cfg);
        let mut market = titan_state.market.lock().unwrap();
        let migrated = market
            .auto_universe
            .migrate_from_favorites_if_empty(&favorites, &au_cfg);
        if migrated && !market.auto_universe.pinned.is_empty() {
            tracing::info!(
                "S18 migration: seeded {} pinned symbol(s) from cfg.ui.favorites",
                market.auto_universe.pinned.len(),
            );
        }
        // Permanent chart anchor (QBTS) — always pinned so the chart
        // never boots empty after EOD / demote / migration.
        if market.auto_universe.ensure_permanent_anchors() {
            tracing::info!(
                component = "bootstrap",
                "Ensured permanent chart anchors in pinned: {:?}",
                crate::market::auto_universe::AutoUniverse::PERMANENT_CHART_SYMBOLS
            );
        }
    }

    // Keep QBTS in favorites + chart-ready active_symbol when missing.
    {
        let mut cfg_changed = false;
        let mut cfg = titan_state.config.lock().unwrap();
        for &sym in crate::market::auto_universe::AutoUniverse::PERMANENT_CHART_SYMBOLS {
            let key = sym.to_uppercase();
            if !cfg.ui.favorites.iter().any(|s| s.eq_ignore_ascii_case(&key)) {
                cfg.ui.favorites.insert(0, key.clone());
                cfg_changed = true;
            }
            if cfg.ui.active_symbol.trim().is_empty() {
                cfg.ui.active_symbol = key;
                cfg_changed = true;
            }
        }
        let active = cfg.ui.active_symbol.clone();
        drop(cfg);
        {
            let mut market = titan_state.market.lock().unwrap();
            if market.active_symbol.trim().is_empty() {
                market.active_symbol = if active.trim().is_empty() {
                    "QBTS".to_string()
                } else {
                    active
                };
            }
        }
        if cfg_changed {
            if let Err(e) = ops::persistence::save_config(&titan_state) {
                tracing::warn!(component = "bootstrap", "Failed to persist QBTS favorites: {e}");
            }
        }
    }

    // Restore markers from markers.json
    let restored_markers = ops::persistence::load_markers();
    if !restored_markers.is_empty() {
        tracing::info!("Restored {} markers from markers.json", restored_markers.len());
        let mut st = titan_state.smarttrend.lock().unwrap();
        st.markers = restored_markers;
    }

    // Restore journal entries from trades.json
    let journal_entries = ops::journal::load_journal();
    if !journal_entries.is_empty() {
        tracing::info!("Restored {} journal entries from trades.json", journal_entries.len());
        let mut journal = titan_state.journal.lock().unwrap();
        journal.entries = journal_entries;
    }

    {
        let mut sys = titan_state.system.lock().unwrap();
        sys.register_loop("streamer");
        sys.register_loop("persistence");
        sys.register_loop("watchdog");
        sys.register_loop("trade_updates");
        sys.register_loop("position_recon");
        sys.register_loop("equity_sync");
        sys.register_loop("daily_reset");
        sys.register_loop("discovery");
        sys.register_loop("auto_scan");
        sys.register_loop("eod_flush"); // S19 — fresh-born watchlist
        sys.register_loop("catalyst_news"); // S13
        sys.register_loop("watchlist_refresh"); // S14
        sys.register_loop("premarket_live"); // MERIDIAN C-PM
        sys.register_loop("meridian"); // MERIDIAN A+B shadow driver
    }

    let market_data = Arc::new(MarketDataState::new());

    // ── D Evolution Protocol (Sesiunea 1) — tournament tracker ──
    //
    // Loads persisted state from `~/.local/share/sentinel-trend/tournament/`
    // if present; otherwise creates a fresh 8-strategy tournament with
    // even $12.5K sub-wallet allocation across 8 strategies (= $100K paper
    // capital). The handle lives behind an Arc<Mutex> and is shared with
    // the bot's trade-close path + the Tauri commands surface.
    let tournament_handle: commands::tournament::TournamentHandle = {
        let dir = smarttrend::tournament::default_base_dir();
        let loaded = smarttrend::tournament::Tournament::load(&dir).ok();
        let initial = match loaded {
            Some(t) => {
                tracing::info!(
                    paper_capital = %t.paper_capital,
                    free_pool = %t.free_pool,
                    "Tournament tracker rehydrated from disk"
                );
                t
            }
            None => {
                // S38 (2026-05-29 01:05 ET) — honour the configured
                // `paper_capital` instead of the hardcoded $100K.
                //
                // Live bug (Tony, 2026-05-29 00:50 ET): the real Alpaca
                // paper account holds $298.92 buying power, but Tournament
                // initialised with $100K split into 8 × $12.5K sub-wallets.
                // Every strategy then sized positions against $12.5K and
                // the broker rejected them for insufficient buying power —
                // Tournament was structurally non-functional on this account.
                //
                // Reading `evolution_protocol.paper_capital` (set to ~the
                // real account balance) makes sub-wallets realistic
                // (e.g. $300 / 8 = $37.50) so strategies size positions
                // the account can actually fill. Honoured on fresh boot
                // only; persisted tournament state takes precedence above.
                let paper_capital = {
                    let cfg = titan_state.config.lock().unwrap();
                    Decimal::from_f64_retain(cfg.evolution_protocol.paper_capital)
                        .unwrap_or(Decimal::ZERO)
                };
                let t = smarttrend::tournament::Tournament::new_default(paper_capital);
                let per_strategy = paper_capital / Decimal::from(smarttrend::strategies::StrategyId::ALL.len());
                tracing::info!(
                    paper_capital = %paper_capital,
                    per_strategy = %per_strategy,
                    n_strategies = smarttrend::strategies::StrategyId::ALL.len(),
                    "Tournament tracker initialised fresh (sub-wallets sized from config paper_capital)"
                );
                t
            }
        };
        std::sync::Arc::new(std::sync::Mutex::new(initial))
    };

    // ── D-PLUS (Sesiunea 2) — handles + OnceLock injection ──
    //
    // Builds the four D-PLUS subsystems (Adaptive Risk, Memory DB,
    // Causal Lead-Lag, Strategy Evolver) and stuffs them in
    // `titan_state.dplus`. The bot loop, the trade-close hooks, and the
    // Tauri commands all reach the same instance via
    // `state.dplus()` — see `smarttrend::dplus` for the facade.
    let evolution_enabled = {
        let cfg = titan_state.config.lock().unwrap();
        cfg.evolution_protocol.enabled
    };
    if evolution_enabled {
        let adaptive_risk = std::sync::Arc::new(std::sync::Mutex::new({
            let dir = smarttrend::risk_adaptive::default_base_dir();
            smarttrend::risk_adaptive::AdaptiveRisk::load(&dir)
                .unwrap_or_else(|_| smarttrend::risk_adaptive::AdaptiveRisk::new())
        }));
        let memory_db = std::sync::Arc::new({
            let path = smarttrend::memory_db::default_db_path();
            smarttrend::memory_db::MemoryDb::open(&path).unwrap_or_else(|e| {
                tracing::error!(component = "dplus", error = %e,
                    "Memory DB open failed; falling back to in-memory (data WILL be lost on restart)");
                smarttrend::memory_db::MemoryDb::open_in_memory()
                    .expect("in-memory DB always opens")
            })
        });
        let causal = smarttrend::causal_loop::new_handle();
        let (evolver, overrides) = smarttrend::evolver_loop::new_handles(0xC0FFEE_u64);
        let dplus_handles = smarttrend::dplus::DPlusHandles::new(
            adaptive_risk,
            memory_db,
            causal,
            evolver,
            overrides,
            Arc::clone(&tournament_handle),
        );
        if titan_state.dplus.set(dplus_handles).is_err() {
            tracing::warn!(component = "dplus", "DPlusHandles already initialised");
        } else {
            tracing::info!(component = "dplus", "DPlusHandles initialised (S2)");
        }
    } else {
        tracing::info!(
            component = "dplus",
            "Evolution protocol disabled — DPlus subsystems not initialised"
        );
    }

    let shutdown_state = Arc::clone(&titan_state);
    let streamer_state = Arc::clone(&titan_state);
    let streamer_market = Arc::clone(&market_data);
    let sentinel_state = Arc::clone(&titan_state);
    let sentinel_market = Arc::clone(&market_data);
    let frontier_state = Arc::clone(&titan_state);
    let frontier_market = Arc::clone(&market_data);
    let trade_updates_state = Arc::clone(&titan_state);
    let position_recon_state = Arc::clone(&titan_state);
    let bot_state = Arc::clone(&titan_state);
    // S15.13: bot needs latest price at order-submit time to recompute SL/TP
    // against the *current* market (avoids Alpaca rejection on stale prices
    // from FSM-eval-time bar closes).
    let bot_market = Arc::clone(&market_data);
    let equity_sync_state = Arc::clone(&titan_state);
    let daily_reset_state = Arc::clone(&titan_state);
    let daily_reset_market = Arc::clone(&market_data);
    let watchdog_state = Arc::clone(&titan_state);
    let persistence_state = Arc::clone(&titan_state);
    let evictor_state = Arc::clone(&titan_state);
    let evictor_market = Arc::clone(&market_data);
    let discovery_state = Arc::clone(&titan_state);
    let discovery_market = Arc::clone(&market_data);
    let fmp_state = Arc::clone(&titan_state);
    let auto_scan_state = Arc::clone(&titan_state);
    // S13: thread MarketDataState into the auto-scan loop so the
    // scanner can read the halts map (Tier B5) for catalyst computation.
    let auto_scan_market = Arc::clone(&market_data);
    // S13: catalyst news refresh loop (5-min interval, watchlist + active).
    let catalyst_state = Arc::clone(&titan_state);
    // S14: watchlist refresh loop (30s interval, favorites only).
    let watchlist_state = Arc::clone(&titan_state);
    let watchlist_market = Arc::clone(&market_data);
    // MERIDIAN C-PM: premarket live loop (headline → tape → alerts).
    let premarket_live_state = Arc::clone(&titan_state);
    let premarket_live_market = Arc::clone(&market_data);
    // MERIDIAN A+B shadow driver (history sync + replay + intents; no orders).
    let meridian_state = Arc::clone(&titan_state);
    // S02: one-shot asset-metadata bootstrap (populates the cache used by
    // Gate 0 short/fractionable checks before the first scan runs).
    let bootstrap_state = Arc::clone(&titan_state);

    tauri::Builder::default()
        .manage(Arc::clone(&titan_state))
        .manage(Arc::clone(&market_data))
        .manage(Arc::clone(&tournament_handle))
        .invoke_handler(tauri::generate_handler![
            get_build_info,
            commands::settings::get_config,
            commands::settings::set_config,
            commands::settings::reset_config,
            commands::settings::set_api_keys,
            commands::settings::validate_config,
            get_health,
            commands::market_data::fetch_alpaca_history,
            commands::market_data::set_active_symbol,
            commands::market_data::get_active_symbol,
            commands::market_data::get_markers,
            commands::trading::toggle_trading_mode,
            commands::trading::get_trading_status,
            commands::trading::submit_manual_order,
            commands::trading::close_position,
            commands::trading::close_position_partial,
            commands::trading::liquidate_all,
            commands::trading::emergency_liquidate,
            commands::trading::topup_user,
            commands::trading::withdraw_user,
            commands::trading::topup_bot,
            commands::trading::withdraw_bot,
            commands::trading::sync_bot_wallet_to_broker,
            commands::trading::get_wallet_status,
            commands::trading::get_order_history,
            commands::monitoring::get_engine_details,
            commands::discovery::get_movers,
            commands::discovery::get_watchlist,
            commands::discovery::add_to_watchlist,
            commands::discovery::remove_from_watchlist,
            commands::discovery::get_news_for_symbol,
            commands::discovery::open_external_url,
            commands::journal::get_journal,
            commands::journal::get_equity_curve,
            commands::journal::export_journal_csv,
            meridian::get_meridian_state,
            commands::scanner::run_market_scan,
            commands::scanner::get_scan_results,
            commands::scanner::set_scan_params,
            commands::scanner::toggle_auto_scan,
            commands::scanner::apply_scan_to_watchlist,
            commands::backtest::run_backtest,
            commands::backtest::run_backtest_all,
            // Strategy Lab — multi-strategy parallel backtest engine
            commands::backtest_engine::run_strategy_backtest,
            // S18 — AutoUniverse (self-curating watchlist)
            commands::auto_universe::get_auto_universe_state,
            commands::auto_universe::set_auto_universe_enabled,
            commands::auto_universe::pin_symbol,
            commands::auto_universe::unpin_symbol,
            commands::auto_universe::promote_auto_to_pinned,
            commands::auto_universe::force_remove_universe_symbol,
            commands::auto_universe::manual_blacklist_symbol,
            commands::auto_universe::clear_blacklist_entry,
            commands::auto_universe::clear_all_blacklist,
            // D Evolution Protocol (Sesiunea 1) — tournament tracker + health
            commands::tournament::get_tournament_state,
            commands::tournament::drain_strategy,
            commands::tournament::kill_strategy,
            commands::tournament::resurrect_strategy,
            commands::tournament::get_health_snapshot,
            // D-PLUS (Sesiunea 2) — adaptive risk + memory db + causal + evolver
            commands::dplus::get_adaptive_risk_state,
            commands::dplus::reset_adaptive_risk,
            commands::dplus::get_memory_db_stats,
            commands::dplus::get_lead_lag_graph,
            commands::dplus::force_recompute_causal_graph,
            commands::dplus::list_evolver_proposals,
            commands::dplus::approve_evolver_proposal,
            commands::dplus::reject_evolver_proposal,
            commands::dplus::get_param_overrides,
            commands::dplus::run_evolver_now,
            // OMEGA (Sesiunea 3) — predictor surface
            commands::omega::get_omega_state,
            commands::omega::get_recent_predictions,
            commands::omega::force_retrain,
            commands::omega::toggle_omega_enabled,
            commands::omega::get_regime_forecast,
            commands::bootstrap::bootstrap_setups,
            commands::frontier::get_bandit_state,
            commands::frontier::preview_mc_simulation,
            commands::frontier::get_frontier_status,
            commands::frontier::toggle_bandit_enabled,
            commands::frontier::get_frontier_constants,
            // PRIME F1 — decision journal (persistent enter/veto audit)
            commands::decisions::get_decisions,
            commands::decisions::get_veto_summary,
            // F4 — Frontier per-condition kill-rate + calibration
            commands::decisions::get_frontier_killrate,
            commands::frontier_calibrate::get_frontier_calibration,
            commands::frontier_calibrate::apply_frontier_calibration,
            // PRIME F5a — shadow-trading counterfactuals
            commands::decisions::get_shadow_summary,
            // PRIME P1 — strategy hospital
            commands::decisions::get_hospital_status,
            // PRIME P3 — counterfactual weight lab
            commands::decisions::get_prime_weights,
            commands::decisions::apply_suggested_weights,
            // PRIME P5 — slippage ledger
            commands::decisions::get_slippage_ledger,
            // PRIME P6 — daily debrief
            commands::decisions::get_debrief,
            // Manual-trading confidence layer (2026-07-04)
            commands::tradeability::get_tradeability,
            commands::trade_plan::get_trade_plan,
            commands::apex_radar::get_apex_radar,
        ])
        .setup(move |app| {
            let (stream_tx, stream_rx) =
                tokio::sync::mpsc::channel::<market::streamer::StreamCmd>(64);
            *streamer_state.stream_tx.lock().unwrap() = Some(stream_tx);

            let (bar_tx, bar_rx) =
                tokio::sync::mpsc::channel::<state::BarClosedMsg>(1024);
            *streamer_state.bar_tx.lock().unwrap() = Some(bar_tx.clone());
            // v2.5.0 NUCLEAR (CB2) — bot channel widened from 128 to 2048.
            //
            // Live evidence 2026-05-15: 7,323 Tournament signals were
            // dropped with "bot channel full" because handle_entry on
            // the consumer side processes orders through Alpaca's REST
            // API (~100-200 ms per submit) while the dispatcher fires
            // signals at ~1 kHz on busy bars. With cap 128, ≈90 % of
            // valid Tournament signals were dropped on the floor.
            //
            // 2048 gives 16x the previous headroom — empirically more
            // than the worst-second burst observed (~600 signals on
            // RTH open). Backpressure still applies past 2048; signals
            // dropped after that are mostly redundant duplicates from
            // multi-strategy convergence on the same bar.
            let (bot_tx, bot_rx) =
                tokio::sync::mpsc::channel::<smarttrend::bot::BotAction>(2048);

            let handle = app.handle();
            let streamer_handle = handle.clone();
            let sentinel_handle = handle.clone();
            let frontier_handle = handle.clone();
            let tu_handle = handle.clone();
            let pr_handle = handle.clone();
            let bot_handle = handle.clone();
            let eq_handle = handle.clone();
            let dr_handle = handle.clone();
            let wd_handle = handle.clone();
            let disc_handle = handle.clone();
            let fmp_handle = handle.clone();
            let auto_scan_handle = handle.clone();
            let watchlist_handle = handle.clone(); // S14
            let regime_handle = handle.clone(); // S20 — D Evolution Protocol
            let _ = handle;

            tauri::async_runtime::spawn(market::streamer::run_streamer(
                streamer_state,
                streamer_market,
                stream_rx,
                bar_tx,
                streamer_handle,
            ));
            tracing::info!("Streamer task spawned");

            // ── Phase 6 step 1 (2026-09-18): legacy strategy loops are no
            // longer started. Sentinel (FSM evaluation, ~48 % CPU), Frontier
            // and the SmartTrend bot belong to the retired system; MERIDIAN
            // owns execution. The bar channel is drained so the streamer's
            // back-pressure path stays quiet. Code removal follows in steps 3–4
            // (docs/meridian/PHASE6_DECOMMISSION.md).
            // 2026-09-18: the first drain `select!`ed on both channels and
            // broke as soon as `bot_rx` closed — which was immediately, since
            // `bot_tx` is dropped right here. `bar_rx` died with the task and
            // the streamer logged `ERROR bar_tx closed` on every closed bar
            // (25 658 lines / 6 GB on 2026-09-18). Drain the bar channel
            // alone; the bot channel has no sender left and is simply dropped.
            drop((sentinel_state, sentinel_market, sentinel_handle, frontier_state, frontier_market, frontier_handle, bot_state, bot_market, bot_handle, bot_tx, bot_rx));
            tauri::async_runtime::spawn(async move {
                let mut bar_rx = bar_rx;
                while bar_rx.recv().await.is_some() {}
            });
            tracing::info!("Legacy strategy loops retired (phase 6); bar/bot channels drained");

            tauri::async_runtime::spawn(execution::trade_updates::run_trade_updates(
                trade_updates_state,
                tu_handle,
            ));
            tracing::info!("Trade updates stream spawned");

            tauri::async_runtime::spawn(execution::position_recon::run_position_recon(
                position_recon_state,
                pr_handle,
            ));
            tracing::info!("Position reconciliation loop spawned");

            tauri::async_runtime::spawn(risk::equity_sync::run_equity_sync(
                equity_sync_state,
                eq_handle,
            ));
            tracing::info!("Equity sync loop spawned");

            tauri::async_runtime::spawn(risk::daily_reset::run_daily_reset(
                daily_reset_state,
                daily_reset_market,
                dr_handle,
            ));
            tracing::info!("Daily reset loop spawned");

            tauri::async_runtime::spawn(ops::watchdog::run_watchdog(
                watchdog_state,
                wd_handle,
            ));
            tracing::info!("Watchdog loop spawned");

            tauri::async_runtime::spawn(ops::persistence::run_persistence_loop(
                persistence_state,
            ));
            tracing::info!("Persistence loop spawned");

            tauri::async_runtime::spawn(market::indicator_cache::run_indicator_cache_evictor(
                evictor_state,
                evictor_market,
            ));
            tracing::info!("Indicator cache evictor spawned");

            tauri::async_runtime::spawn(market::discovery::run_discovery_loop(
                discovery_state,
                discovery_market,
                disc_handle,
            ));
            tracing::info!("Discovery loop spawned");

            tauri::async_runtime::spawn(market::fmp::run_fmp_schedule(
                fmp_state,
                fmp_handle,
            ));
            tracing::info!("FMP schedule spawned");

            tauri::async_runtime::spawn(market::auto_scan::run_auto_scan_loop(
                auto_scan_state,
                auto_scan_market,
                auto_scan_handle,
            ));
            tracing::info!("Auto-scan loop spawned");

            // S19 — EOD AutoUniverse flush at 16:00 ET. Wipes
            // auto_pool + pinned + blacklist so tomorrow's session
            // begins with zero carry-over from today. Surviving
            // symbols are snapshotted into `recently_traded` and
            // excluded from re-promotion for ~18h (overnight +
            // next premarket).
            let eod_flush_state = Arc::clone(&titan_state);
            let eod_flush_handle = app.handle();
            tauri::async_runtime::spawn(market::auto_scan::run_eod_universe_flush_loop(
                eod_flush_state,
                eod_flush_handle,
            ));
            tracing::info!("EOD universe flush loop spawned");

            // v2.5.0 — Bot-wallet → broker cash auto-sync on startup.
            //
            // The internal bot_wallet accounting drifts below broker
            // cash over long sessions (debit-on-entry never perfectly
            // matches credit-on-close when bracket SL/TP closes race
            // with the recon loop). Live evidence 2026-05-14: after a
            // crash + restart bot_wallet was $29k while Alpaca held
            // $85k cash → all position sizing capped at ~$600. Auto-
            // syncing on every startup re-anchors the internal ledger
            // to ground truth without operator action. Spawned as a
            // separate task so a slow Alpaca round-trip doesn't block
            // the rest of the bootstrap.
            let wallet_sync_state = Arc::clone(&titan_state);
            tauri::async_runtime::spawn(async move {
                // Small delay so config + http_client are fully wired
                // and the bot doesn't hit Alpaca with a thundering
                // herd of bootstrap requests.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let (key, secret, paper) = {
                    let cfg = wallet_sync_state.config.lock().unwrap_or_else(|p| p.into_inner());
                    (
                        cfg.market.alpaca_key_id.clone(),
                        cfg.market.alpaca_secret.clone(),
                        cfg.market.paper_mode,
                    )
                };
                if key.is_empty() || secret.is_empty() {
                    tracing::warn!(
                        component = "wallet",
                        "Startup sync skipped — Alpaca credentials not configured"
                    );
                    return;
                }
                let host = if paper {
                    "https://paper-api.alpaca.markets"
                } else {
                    "https://api.alpaca.markets"
                };
                let url = format!("{host}/v2/account");
                let resp = match wallet_sync_state
                    .http_client
                    .get(&url)
                    .header("APCA-API-KEY-ID", &key)
                    .header("APCA-API-SECRET-KEY", &secret)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => r,
                    Ok(r) => {
                        tracing::warn!(
                            component = "wallet",
                            status = %r.status(),
                            "Startup sync skipped — Alpaca returned non-2xx"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            component = "wallet",
                            error = %e,
                            "Startup sync skipped — Alpaca request failed"
                        );
                        return;
                    }
                };
                let body: serde_json::Value = match resp.json().await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            component = "wallet",
                            error = %e,
                            "Startup sync skipped — Alpaca response not JSON"
                        );
                        return;
                    }
                };
                let cash: f64 = match body
                    .get("cash")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                {
                    Some(c) => c,
                    None => {
                        tracing::warn!(
                            component = "wallet",
                            "Startup sync skipped — missing cash field"
                        );
                        return;
                    }
                };
                let cash_dec = rust_decimal::Decimal::from_f64_retain(cash)
                    .unwrap_or(rust_decimal::Decimal::ZERO);
                let mut st = wallet_sync_state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                let prev = st.bot_wallet.balance;
                if cash_dec > prev {
                    let delta = cash_dec - prev;
                    st.bot_wallet.topup(delta);
                    tracing::info!(
                        component = "wallet",
                        delta = %delta,
                        prev = %prev,
                        new_balance = %cash_dec,
                        "Startup sync: bot wallet credited to match Alpaca cash"
                    );
                } else if cash_dec < prev {
                    let delta = prev - cash_dec;
                    if st.bot_wallet.withdraw(delta).is_ok() {
                        tracing::info!(
                            component = "wallet",
                            delta = %delta,
                            prev = %prev,
                            new_balance = %cash_dec,
                            "Startup sync: bot wallet debited to match Alpaca cash"
                        );
                    } else {
                        tracing::warn!(
                            component = "wallet",
                            "Startup sync: bot wallet withdraw failed (negative balance protection)"
                        );
                    }
                } else {
                    tracing::info!(
                        component = "wallet",
                        balance = %prev,
                        "Startup sync no-op: bot wallet already matches Alpaca cash"
                    );
                }
            });
            tracing::info!("Wallet startup-sync task spawned");

            // v2.5.0 NUCLEAR (HF1) — Wallet heartbeat sync every 5 min.
            //
            // Live evidence 2026-05-14/15: the `trade_updates` websocket
            // authenticated then went silent for 17 hours. While silent,
            // every Tournament entry debited the bot wallet but the
            // matching close credit never arrived, draining $86k →
            // $0.82 in one session. The startup sync (above) fixes drift
            // at boot but does NOT cover drift accumulated during a long
            // running session.
            //
            // This periodic heartbeat re-anchors bot_wallet to broker
            // cash every 5 minutes (cheap one-shot HTTP GET). It is
            // race-safe — if the wallet credits a refund mid-cycle, the
            // next heartbeat just re-aligns. Worst case it cancels a
            // few seconds of pending in-flight debits, but those become
            // open positions anyway so total bot equity (cash + holdings)
            // stays correct.
            let wallet_hb_state = Arc::clone(&titan_state);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                loop {
                    if wallet_hb_state.shutdown.is_cancelled() {
                        break;
                    }
                    let (key, secret, paper) = {
                        let cfg = wallet_hb_state.config.lock().unwrap_or_else(|p| p.into_inner());
                        (
                            cfg.market.alpaca_key_id.clone(),
                            cfg.market.alpaca_secret.clone(),
                            cfg.market.paper_mode,
                        )
                    };
                    if !key.is_empty() && !secret.is_empty() {
                        let host = if paper {
                            "https://paper-api.alpaca.markets"
                        } else {
                            "https://api.alpaca.markets"
                        };
                        let url = format!("{host}/v2/account");
                        let req = wallet_hb_state
                            .http_client
                            .get(&url)
                            .header("APCA-API-KEY-ID", &key)
                            .header("APCA-API-SECRET-KEY", &secret)
                            .send()
                            .await;
                        if let Ok(resp) = req {
                            if resp.status().is_success() {
                                if let Ok(body) = resp.json::<serde_json::Value>().await {
                                    if let Some(cash) = body
                                        .get("cash")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| s.parse::<f64>().ok())
                                    {
                                        let cash_dec = rust_decimal::Decimal::from_f64_retain(cash)
                                            .unwrap_or(rust_decimal::Decimal::ZERO);
                                        let mut st = wallet_hb_state.smarttrend
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner());
                                        let prev = st.bot_wallet.balance;
                                        if cash_dec > prev {
                                            let delta = cash_dec - prev;
                                            st.bot_wallet.topup(delta);
                                            tracing::info!(
                                                component = "wallet",
                                                delta = %delta,
                                                prev = %prev,
                                                new_balance = %cash_dec,
                                                "Heartbeat sync: bot wallet credited to match Alpaca cash"
                                            );
                                        } else if cash_dec < prev {
                                            let delta = prev - cash_dec;
                                            if st.bot_wallet.withdraw(delta).is_ok() {
                                                tracing::info!(
                                                    component = "wallet",
                                                    delta = %delta,
                                                    prev = %prev,
                                                    new_balance = %cash_dec,
                                                    "Heartbeat sync: bot wallet debited to match Alpaca cash"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                }
                tracing::info!("Wallet heartbeat task exiting");
            });
            tracing::info!("Wallet heartbeat-sync task spawned (interval=300s)");

            // v2.5.0 — Candle buffer backfill on startup. The streamer
            // feeds bars live from trades, so a fresh bot process starts
            // with an empty buffer for every symbol and needs ~15-30 min
            // before strategies that require 14-50 candles can fire.
            // After a crash + restart that's a black-out window. Backfill
            // pulls the last 200 1-minute bars from Alpaca's historical
            // endpoint for every favorite + every symbol with an open
            // position, then `seed()`s the candle buffer so Tournament
            // strategies have ATR / volatility context from tick one.
            let backfill_state = Arc::clone(&titan_state);
            let backfill_market = Arc::clone(&market_data);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let (key, secret, paper, favorites) = {
                    let cfg = backfill_state.config.lock().unwrap_or_else(|p| p.into_inner());
                    (
                        cfg.market.alpaca_key_id.clone(),
                        cfg.market.alpaca_secret.clone(),
                        cfg.market.paper_mode,
                        cfg.ui.favorites.clone(),
                    )
                };
                if key.is_empty() || secret.is_empty() {
                    return;
                }
                let _ = paper;
                let position_syms: Vec<String> = {
                    let trading = backfill_state.trading.lock().unwrap_or_else(|p| p.into_inner());
                    trading.positions.iter().map(|p| p.symbol.clone()).collect()
                };
                let mut symbols: Vec<String> = favorites
                    .into_iter()
                    .chain(position_syms)
                    .collect();
                symbols.sort();
                symbols.dedup();
                if symbols.is_empty() {
                    return;
                }
                // Alpaca data API is on data.alpaca.markets regardless of
                // paper/live (paper has no separate data endpoint).
                let host = "https://data.alpaca.markets";
                let mut seeded = 0usize;
                for sym in &symbols {
                    // Pull last ~200 bars at 1Min timeframe. Limit caps at
                    // 10000 server-side; 200 is plenty for ATR(14), 50-bar
                    // rolling SD, gap-pct anchor, etc.
                    // feed=iex works for paper accounts without a data
                    // subscription. SIP is available on paid plans; iex
                    // is fine for warmup (it's a single venue but the
                    // OHLCV signal needed for ATR/rolling-SD is
                    // statistically identical to SIP for liquid names).
                    let url = format!(
                        "{host}/v2/stocks/{sym}/bars?timeframe=1Min&limit=200&feed=iex"
                    );
                    let resp = match backfill_state
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
                        // Reject obviously-corrupt bars (zero/non-finite).
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

                    // v2.5.0 NUCLEAR (C4) — Ghost-price filter.
                    //
                    // Live evidence 2026-05-14: AGL Tournament dispatcher
                    // saw the symbol at $27.80, $53.00, $55.47 in the same
                    // second. The cause was stale or mismatched bars in
                    // the historical backfill from earlier corporate
                    // actions (splits, IPO regimes). Strategies relying
                    // on `prev_close` and `session_open` produced
                    // nonsensical gap_pct values, leading to wrong-side
                    // entries.
                    //
                    // Filter: walk the candles chronologically with a
                    // 20-bar rolling median of closes; drop any bar whose
                    // close deviates >50% from that median. The first 20
                    // bars are kept as-is (no anchor yet). Tested against
                    // recent splits this catches the obvious phantom
                    // prints without over-filtering legitimate gap-and-go
                    // moves (those rarely exceed 50% in a single 1m bar).
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
                                component = "bootstrap",
                                symbol = %sym,
                                rejected,
                                kept = filtered.len(),
                                "Ghost-price filter rejected anomalous bars"
                            );
                        }
                        candles = filtered;
                    }
                    if candles.is_empty() {
                        continue;
                    }
                    let key_1m = format!("{sym}:1m");
                    let key_5m = format!("{sym}:5m");
                    let mut buffers = backfill_market.candle_buffers.lock().unwrap_or_else(|p| p.into_inner());
                    let buf1 = buffers
                        .entry(key_1m)
                        .or_insert_with(|| crate::market::candle_buffer::CandleRingBuffer::new(60));
                    buf1.seed(candles.clone());
                    // 5m buffer: aggregate the 1m bars into 5m buckets.
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
                    component = "bootstrap",
                    symbols = symbols.len(),
                    seeded,
                    "Candle buffer backfill complete (1m + 5m)"
                );
            });
            tracing::info!("Candle backfill task spawned");

            // v2.5.0 NUCLEAR (N1) — Float cache initialise + FMP refresh.
            // Load from disk first (preserves history across restarts),
            // then on startup walk favorites + open positions and fetch
            // any missing/stale entries from FMP. 7-day TTL inside the
            // cache prevents thrashing on the rate-limited free tier.
            let float_cache_state = Arc::clone(&titan_state);
            tauri::async_runtime::spawn(async move {
                let cache_path = crate::paths::data_dir()
                    .join("cache")
                    .join("float_cache.json");
                if let Some(parent) = cache_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let loaded = crate::market::float_cache::FloatCache::load(&cache_path);
                let _ = float_cache_state
                    .float_cache
                    .set(std::sync::Mutex::new(loaded));
                tracing::info!(component = "bootstrap", path = %cache_path.display(), "Float cache initialised");

                // Refresh missing entries for favorites + open positions.
                tokio::time::sleep(std::time::Duration::from_secs(7)).await;
                let (fmp_key, favorites) = {
                    let cfg = float_cache_state.config.lock().unwrap_or_else(|p| p.into_inner());
                    (
                        cfg.market.fmp_api_key.clone(),
                        cfg.ui.favorites.clone(),
                    )
                };
                if fmp_key.is_empty() {
                    tracing::warn!(component = "bootstrap", "Float refresh skipped — FMP key not configured");
                    return;
                }
                let position_syms: Vec<String> = {
                    let trading = float_cache_state.trading.lock().unwrap_or_else(|p| p.into_inner());
                    trading.positions.iter().map(|p| p.symbol.clone()).collect()
                };
                let mut symbols: Vec<String> = favorites
                    .into_iter()
                    .chain(position_syms)
                    .collect();
                symbols.sort();
                symbols.dedup();
                let mut fetched = 0usize;
                let mut skipped = 0usize;
                for sym in &symbols {
                    let needs = float_cache_state
                        .float_cache
                        .get()
                        .and_then(|fc| fc.lock().ok())
                        .map(|fc| fc.needs_fetch(sym))
                        .unwrap_or(false);
                    if !needs {
                        skipped += 1;
                        continue;
                    }
                    if let Some(shares) = crate::market::float_cache::fetch_float_fmp(
                        &float_cache_state.http_client,
                        &fmp_key,
                        sym,
                    )
                    .await
                    {
                        if let Some(fc) = float_cache_state.float_cache.get() {
                            if let Ok(mut g) = fc.lock() {
                                g.insert(sym.clone(), shares);
                                fetched += 1;
                            }
                        }
                    }
                    // Be polite to FMP rate-limiter (free tier ~300/min).
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                if let Some(fc) = float_cache_state.float_cache.get() {
                    if let Ok(mut g) = fc.lock() {
                        let _ = g.save(&cache_path);
                    }
                }
                tracing::info!(
                    component = "bootstrap",
                    symbols = symbols.len(),
                    fetched,
                    skipped_cached = skipped,
                    "Float cache refresh complete"
                );
            });
            tracing::info!("Float cache refresh task spawned");

            // S13: catalyst news refresh loop. Reads watchlist + active
            // symbol from `state.config` (Tier A only) → calls Alpaca
            // News API → updates the global `NewsCache` (own Tier-B
            // mutex). No Tier-A and Tier-B held simultaneously.
            tauri::async_runtime::spawn(market::alpaca_news::run_catalyst_refresh_loop(
                catalyst_state,
            ));
            tracing::info!("Catalyst news refresh loop spawned");

            // Real-time news websocket (seconds latency) — writes into
            // the same NewsCache the REST poller backfills.
            tauri::async_runtime::spawn(market::news_stream::run_news_stream(Arc::clone(
                &titan_state,
            )));
            tracing::info!("News stream (websocket) spawned");

            // MERIDIAN C-PM — premarket live loop: classified headlines arm
            // a tracker, premarket tape (B9) feeds it, entry/exit signals
            // emit `scanner-ignition` + `meridian-pm-signal` (alerts only;
            // execution is phase 4b behind the backtest gate).
            tauri::async_runtime::spawn(market::premarket_live::run_premarket_live(
                premarket_live_state,
                premarket_live_market,
                app.handle(),
            ));
            tracing::info!("Premarket live loop spawned");

            // MERIDIAN A+B driver: syncs SPY/QQQ 1-min history, replays the
            // validated engine every minute of the session, logs intents to
            // the twin log and emits `meridian-state`. When the headless
            // daemon holds the executor lease this window is only a viewer
            // (`meridian/daemon.rs`).
            tauri::async_runtime::spawn(meridian::start_in_app(
                meridian_state,
                app.handle(),
            ));
            tracing::info!("MERIDIAN driver/viewer spawned");

            // SEC EDGAR poller — 8-K headlines + offering-risk flags.
            tauri::async_runtime::spawn(market::edgar::run_edgar_loop(Arc::clone(
                &titan_state,
            )));
            tracing::info!("EDGAR filings poller spawned");

            // S14: watchlist refresh loop (30s). Reads favorites/keys from
            // `state.config` (Tier A) → fetches snapshots + news cache → reads
            // halts (Tier B5) → writes `MarketDataState.watchlist_metrics`
            // (Tier B6) → emits `watchlist-update` after every guard is
            // dropped (S05 invariant).
            tauri::async_runtime::spawn(market::watchlist_refresh::run_watchlist_refresh_loop(
                watchlist_state,
                watchlist_market,
                watchlist_handle,
            ));
            tracing::info!("Watchlist refresh loop spawned");

            // ── S20 / D Evolution Protocol — regime detector loop ──
            //
            // Polls SPY 5-min bars from MarketDataState every
            // `cfg.evolution_protocol.regime_poll_secs` seconds and
            // emits `regime-change` events when the detector flips
            // classification. No-op when `evolution_protocol.enabled`
            // is `false` (default) — the loop returns immediately so
            // legacy users see zero behaviour change.
            // Phase 6 step 1: regime detector retired (fed only the legacy
            // strategy framework).
            drop(regime_handle);

            // ── Research loops (causal / evolver / OMEGA) ──
            //
            // F0 FRONTIER Absolute: parked by default via
            // `evolution_protocol.research_loops=false`. Tournament
            // APEX/HaltResume orders already bypass OMEGA/bandit sizing;
            // these loops only burn CPU and fill logs until re-enabled.
            let research_loops = titan_state
                .config
                .lock()
                .map(|c| c.evolution_protocol.research_loops)
                .unwrap_or(false);
            if research_loops {
                if let Some(dplus) = titan_state.dplus() {
                    let causal_state = Arc::clone(&titan_state);
                    let causal_market = Arc::clone(&market_data);
                    let causal_handle_clone = Arc::clone(&dplus.causal);
                    let causal_app_handle = app.handle();
                    tauri::async_runtime::spawn(smarttrend::causal_loop::run_causal_loop(
                        causal_state,
                        causal_market,
                        causal_handle_clone,
                        causal_app_handle,
                    ));
                    tracing::info!("Causal lead-lag loop spawned");
                }

                if let Some(dplus) = titan_state.dplus() {
                    let evolver_state = Arc::clone(&titan_state);
                    let market_data_clone = Arc::clone(&market_data);
                    let evolver_handle_clone = Arc::clone(&dplus.evolver);
                    let overrides_handle_clone = Arc::clone(&dplus.overrides);
                    let evolver_app_handle = app.handle();
                    tauri::async_runtime::spawn(smarttrend::evolver_loop::run_evolver_loop(
                        evolver_state,
                        market_data_clone,
                        evolver_handle_clone,
                        overrides_handle_clone,
                        evolver_app_handle,
                    ));
                    tracing::info!("Strategy evolver loop spawned");
                }

                if let Some(dplus) = titan_state.dplus() {
                    let initial_registry = ml::predictor_loop::build_loaded_registry();
                    dplus.swap_omega(initial_registry);
                    let omega_state = Arc::clone(&titan_state);
                    let omega_app_handle = app.handle();
                    tauri::async_runtime::spawn(ml::predictor_loop::run_predictor_loop(
                        omega_state,
                        omega_app_handle,
                    ));
                    tracing::info!("OMEGA predictor loop spawned");
                }
            } else {
                tracing::info!(
                    "Research loops parked (evolution_protocol.research_loops=false)"
                );
            }

            // ── PRIME F5b — nightly reliability calibration (21:30 ET) ──
            //
            // Re-derives per-playbook reliability from the trailing
            // 14 days of journaled trades and persists the engine to
            // prime_engine.json. Fires after the OMEGA 21:00 retrain
            // so both learners see the same closed-trade set.
            // Phase 6 step 1: PRIME calibration retired.

            // S17.0: one-shot startup cleanup of stale broker orders.
            //
            // Live evidence (2026-05-05 08:31): on cold start the bot
            // found 6 open orders at the broker from a previous session
            // (5 buy markets, 1 sell market — none had filled because
            // the previous run shut down before they reached the
            // exchange or got cancelled). The stuck QBTS sell market
            // was holding 23 shares (`held_for_orders=23`), so every
            // FSM-driven exit attempt failed with HTTP 403
            // `insufficient qty available for order`. The bot ran 30+
            // identical exit submissions in 5 s before being killed.
            //
            // `cancel_open_orders_for_symbol` deliberately skips
            // market orders to avoid cancelling its own pending exit.
            // At app boot there are no pending exits yet — anything
            // open at the broker is by definition stale and should be
            // wiped before any trading code runs.
            let cleanup_state = Arc::clone(&titan_state);
            tauri::async_runtime::spawn(async move {
                // MERIDIAN (2026-09-16): `DELETE /v2/orders` also wiped the
                // `mrd-*` auction orders (MOC / OPG). Cancel per order and
                // keep MERIDIAN's — see execution::order_exec::cancel_all_open_orders.
                match execution::order_exec::cancel_all_open_orders(&cleanup_state).await {
                    Ok(n) if n > 0 => tracing::warn!(component = "bootstrap", cancelled = n, "Stale broker orders cancelled at startup"),
                    Ok(_) => tracing::info!(component = "bootstrap", "Startup broker order cleanup: nothing pending"),
                    Err(e) => tracing::warn!(component = "bootstrap", "Startup broker order cleanup failed: {e}"),
                }
            });
            tracing::info!("Startup stale-order cleanup spawned");

            // S02: bootstrap asset metadata cache (one-shot at startup).
            // Compliance gates (short shortable+ETB, fractionable) require
            // this cache populated; without bootstrap, all entries reject
            // with `AssetUnknown` until the first periodic scan runs.
            tauri::async_runtime::spawn(async move {
                let (key, secret) = {
                    let cfg = bootstrap_state.config.lock().unwrap();
                    (
                        cfg.market.alpaca_key_id.clone(),
                        cfg.market.alpaca_secret.clone(),
                    )
                };
                if key.is_empty() || secret.is_empty() {
                    tracing::warn!(
                        component = "bootstrap",
                        "Asset metadata bootstrap skipped — Alpaca keys not configured"
                    );
                    return;
                }
                match market::scanner::refresh_asset_metadata(
                    &bootstrap_state.http_client,
                    &key,
                    &secret,
                )
                .await
                {
                    Ok(n) => tracing::info!(
                        component = "bootstrap",
                        count = n,
                        "Asset metadata cache populated"
                    ),
                    Err(e) => tracing::warn!(
                        component = "bootstrap",
                        "Asset metadata bootstrap failed: {e}"
                    ),
                }
            });
            tracing::info!("Asset-metadata bootstrap task spawned");

            // Advanced Quantum Auto-Bootstrap Hook
            let auto_bootstrap_state = Arc::clone(&titan_state);
            tauri::async_runtime::spawn(async move {
                let cache_dir = crate::paths::data_dir();
                let filepath = cache_dir.join("symbols_to_bootstrap.json");
                if filepath.exists() {
                    tracing::info!(component = "bootstrap", "symbols_to_bootstrap.json file detected at startup! Running automated bootstrap...");
                    if let Ok(raw) = std::fs::read_to_string(&filepath) {
                        #[derive(serde::Deserialize)]
                        struct BootstrapReq {
                            months: u32,
                            symbols: Vec<String>,
                        }
                        if let Ok(req) = serde_json::from_str::<BootstrapReq>(&raw) {
                            tracing::info!(
                                component = "bootstrap",
                                months = req.months,
                                symbols = req.symbols.len(),
                                "Parsed auto-bootstrap request"
                            );
                            match commands::bootstrap::bootstrap_setups_internal(req.months, &req.symbols, &auto_bootstrap_state).await {
                                Ok(report) => {
                                    tracing::info!(
                                        component = "bootstrap",
                                        samples = report.samples_recorded,
                                        "Automated bootstrap completed successfully! Trashing trigger file."
                                    );
                                    let _ = std::fs::remove_file(&filepath);
                                    
                                    // Trigger OMEGA retraining if models are enabled
                                    let has_dplus = auto_bootstrap_state.dplus().is_some();
                                    if has_dplus {
                                        tracing::info!(component = "omega", "Kicking off forced retraining after automated bootstrap...");
                                        let ml_dir = crate::ml::default_ml_dir();
                                        let memdb = auto_bootstrap_state.dplus().unwrap().memory_db.clone();
                                        let omega_handle = auto_bootstrap_state.dplus().unwrap().omega.clone();
                                        tokio::task::spawn_blocking(move || {
                                            let _reports = crate::ml::predictor_loop::train_models_blocking(&ml_dir, memdb);
                                            let new_reg = crate::ml::predictor_loop::build_loaded_registry();
                                            omega_handle.store(Arc::new(new_reg));
                                            tracing::info!(component = "omega", "Forced retraining after bootstrap complete!");
                                        });
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(component = "bootstrap", "Automated bootstrap failed: {e}");
                                }
                            }
                        } else {
                            tracing::error!(component = "bootstrap", "Failed to deserialize symbols_to_bootstrap.json");
                        }
                    } else {
                        tracing::error!(component = "bootstrap", "Failed to read symbols_to_bootstrap.json");
                    }
                }
            });

            // Quantum Frontier - WAL reconciliation on boot.
            let wal_state = Arc::clone(&titan_state);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                if let Err(e) = crate::execution::order_wal::reconcile_wal_on_boot(&wal_state).await {
                    tracing::error!(
                        component = "order_wal",
                        error = %e,
                        "WAL reconciliation on boot failed"
                    );
                }
            });
            tracing::info!("WAL reconciliation task spawned");

            Ok(())
        })
        .on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event.event() {
                graceful_shutdown(&shutdown_state);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running SentinelTrend");
}

/// Graceful shutdown (Bonus 3): persist state and signal all loops.
fn graceful_shutdown(state: &TitanState) {
    let uptime = {
        let sys = state.system.lock().unwrap();
        sys.uptime_secs()
    };

    tracing::info!("Shutdown initiated (session uptime: {uptime}s)");

    state.shutdown.cancel();

    if let Err(e) = ops::persistence::save_config(state) {
        tracing::error!("Shutdown: failed to save config: {e}");
    }
    if let Err(e) = ops::persistence::save_fsm_states(state) {
        tracing::error!("Shutdown: failed to save FSM states: {e}");
    }
    if let Err(e) = ops::persistence::save_markers(state) {
        tracing::error!("Shutdown: failed to save markers: {e}");
    }
    if let Err(e) = ops::persistence::save_snapshot(state) {
        tracing::error!("Shutdown: failed to save snapshot: {e}");
    }
    if let Err(e) = ops::journal::save_journal(state) {
        tracing::error!("Shutdown: failed to save journal: {e}");
    }

    tracing::info!("SentinelTrend shutdown complete");
}
