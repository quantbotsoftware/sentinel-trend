//! Tauri command surface for the Strategy Lab backtest engine.
//!
//! The heavy lifting lives in `crate::backtest`; this file is the
//! tiny shim that:
//!   1. Pulls Alpaca credentials from `TitanState.config` (Tier-A
//!      lock — acquired, cloned, dropped *before* the simulation).
//!   2. Calls `backtest::runner::run` with those creds.
//!   3. Returns the resulting `StrategyBacktestReport` to the UI.
//!
//! S05 (emit-after-drop) invariant: every state guard is released
//! before the async work starts. There is no event emission here —
//! the command returns the report directly to the invoke caller.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::backtest::runner::{run, BacktestRequest, StrategyBacktestReport};
use crate::error::TitanError;
use crate::smarttrend::strategies::StrategyId;
use crate::state::TitanState;

/// Wire-format request from the UI. ISO-8601 timestamps are accepted
/// for `start`/`end` so the operator can paste dates from the date
/// picker without worrying about timezone conversion (chrono parses
/// UTC offset).
#[derive(Debug, Deserialize)]
pub struct StrategyBacktestArgs {
    pub symbols: Vec<String>,
    pub strategies: Vec<StrategyId>,
    pub timeframe: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default = "default_initial_equity")]
    pub initial_equity: f64,
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: f64,
    #[serde(default)]
    pub commission: f64,
    #[serde(default = "default_spread_bps")]
    pub spread_bps: f64,
    #[serde(default = "default_use_cache")]
    pub use_cache: bool,
}

fn default_initial_equity() -> f64 {
    10_000.0
}
fn default_slippage_bps() -> f64 {
    5.0
}
fn default_spread_bps() -> f64 {
    5.0
}
fn default_use_cache() -> bool {
    true
}

#[tauri::command]
pub async fn run_strategy_backtest(
    args: StrategyBacktestArgs,
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<StrategyBacktestReport, TitanError> {
    // ─── Tier-A read: copy credentials, then DROP the guard. ───
    let (key, secret) = {
        let cfg = state
            .config
            .lock()
            .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };

    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation(
            "Alpaca API keys not configured".into(),
        ));
    }
    if args.symbols.is_empty() {
        return Err(TitanError::Validation("No symbols supplied".into()));
    }
    if args.strategies.is_empty() {
        return Err(TitanError::Validation("No strategies supplied".into()));
    }

    let request = BacktestRequest {
        symbols: args.symbols,
        strategies: args.strategies,
        timeframe: args.timeframe,
        start: args.start,
        end: args.end,
        initial_equity: args.initial_equity,
        slippage_bps: args.slippage_bps,
        commission: args.commission,
        spread_bps: args.spread_bps,
        use_cache: args.use_cache,
    };

    let cache_dir: Option<PathBuf> = Some(crate::paths::data_dir().join("backtest_cache"));

    let report = run(&request, &state.http_client, &key, &secret, cache_dir).await?;

    tracing::info!(
        component = "backtest_lab",
        symbols = report.symbols_fetched,
        strategies = report.per_strategy.len(),
        simulate_ms = report.simulate_ms,
        "Strategy Lab backtest completed"
    );

    Ok(report)
}
