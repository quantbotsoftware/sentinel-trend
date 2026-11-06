//! Tauri commands surface for the tournament tracker.
//!
//! UI flow:
//!   - On boot, the front-end calls `get_tournament_state` once to
//!     populate the leaderboard.
//!   - The bot loop emits `tournament-update` events whenever a trade
//!     closes or a status transition happens; the front-end refreshes
//!     by listening on that event (no polling).
//!   - Manual overrides (`drain_strategy`, `kill_strategy`,
//!     `resurrect_strategy`) are wired to admin buttons in the
//!     leaderboard panel — protected behind a confirm dialog in the
//!     front-end.
//!
//! All commands are read-mostly; the only mutating ones operate on
//! the in-memory `TournamentState` and persist via
//! `tournament::Tournament::save` immediately after the mutation.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri::Manager;

use crate::smarttrend::strategies::StrategyId;
use crate::smarttrend::tournament::{Tournament, default_base_dir};

use rust_decimal::Decimal;

/// Process-wide tournament state. The bot loop owns a clone of this
/// `Arc` and pushes updates into it; commands read snapshots without
/// holding the mutex across `await`.
pub type TournamentHandle = Arc<Mutex<Tournament>>;

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    pub strategy: String,
    pub status: String,
    pub trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub win_rate_pct: f64,
    #[serde(with = "crate::serde_decimal")]
    pub cumulative_pnl: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub expectancy: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub avg_win: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub avg_loss: Decimal,
    pub max_drawdown_pct: f64,
    #[serde(with = "crate::serde_decimal")]
    pub current_sub_wallet: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub initial_sub_wallet: Decimal,
    pub last_trade_close_ts: i64,
    pub current_streak_len: i32,
    pub pending_sandbox: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TournamentStateOut {
    #[serde(with = "crate::serde_decimal")]
    pub paper_capital: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub free_pool: Decimal,
    pub last_saved_at: i64,
    pub leaderboard: Vec<LeaderboardRow>,
}

#[tauri::command]
pub fn get_tournament_state(
    state: tauri::State<'_, TournamentHandle>,
) -> Result<TournamentStateOut, String> {
    let t = state.lock().map_err(|e| format!("tournament lock: {e}"))?;
    let leaderboard = t
        .leaderboard()
        .into_iter()
        .map(|(id, s)| LeaderboardRow {
            strategy: id.to_string(),
            status: format!("{:?}", s.status).to_lowercase(),
            trades: s.total_trades,
            wins: s.wins,
            losses: s.losses,
            win_rate_pct: s.win_rate_pct(),
            cumulative_pnl: s.cumulative_pnl,
            expectancy: s.expectancy(),
            avg_win: s.avg_win,
            avg_loss: s.avg_loss,
            max_drawdown_pct: s.max_drawdown_pct,
            current_sub_wallet: s.current_sub_wallet,
            initial_sub_wallet: s.initial_sub_wallet,
            last_trade_close_ts: s.last_trade_close_ts,
            current_streak_len: s.current_streak_len,
            pending_sandbox: s.pending_sandbox,
        })
        .collect();
    Ok(TournamentStateOut {
        paper_capital: t.paper_capital,
        free_pool: t.free_pool,
        last_saved_at: t.last_saved_at,
        leaderboard,
    })
}

#[derive(Debug, Deserialize)]
pub struct StrategyMutation {
    pub strategy: String,
}

fn parse_strategy_id(s: &str) -> Result<StrategyId, String> {
    for id in StrategyId::ALL {
        if id.as_str().eq_ignore_ascii_case(s) {
            return Ok(*id);
        }
    }
    Err(format!("unknown strategy id: {s}"))
}

#[tauri::command]
pub fn drain_strategy(
    state: tauri::State<'_, TournamentHandle>,
    app: AppHandle,
    payload: StrategyMutation,
) -> Result<bool, String> {
    let id = parse_strategy_id(&payload.strategy)?;
    let mut t = state.lock().map_err(|e| format!("tournament lock: {e}"))?;
    let now = chrono::Utc::now().timestamp();
    let changed = t.drain(id, now);
    if changed {
        let _ = t.save(&default_base_dir());
        drop(t);
        let _ = app.emit_all("tournament-update", serde_json::json!({"reason": "drain", "strategy": payload.strategy}));
    }
    Ok(changed)
}

#[tauri::command]
pub fn kill_strategy(
    state: tauri::State<'_, TournamentHandle>,
    app: AppHandle,
    payload: StrategyMutation,
) -> Result<bool, String> {
    let id = parse_strategy_id(&payload.strategy)?;
    let mut t = state.lock().map_err(|e| format!("tournament lock: {e}"))?;
    let changed = t.kill(id);
    if changed {
        let _ = t.save(&default_base_dir());
        drop(t);
        let _ = app.emit_all("tournament-update", serde_json::json!({"reason": "kill", "strategy": payload.strategy}));
    }
    Ok(changed)
}

#[tauri::command]
pub fn resurrect_strategy(
    state: tauri::State<'_, TournamentHandle>,
    app: AppHandle,
    payload: StrategyMutation,
) -> Result<bool, String> {
    let id = parse_strategy_id(&payload.strategy)?;
    let mut t = state.lock().map_err(|e| format!("tournament lock: {e}"))?;
    let changed = t.resurrect(id);
    if changed {
        let _ = t.save(&default_base_dir());
        drop(t);
        let _ = app.emit_all("tournament-update", serde_json::json!({"reason": "resurrect", "strategy": payload.strategy}));
    }
    Ok(changed)
}

#[tauri::command]
pub fn get_health_snapshot(
    state: tauri::State<'_, Arc<crate::state::TitanState>>,
) -> Result<serde_json::Value, String> {
    Ok(crate::health::health_check::build_snapshot(&state))
}
