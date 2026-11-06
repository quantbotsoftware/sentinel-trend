use std::sync::Arc;

use rust_decimal::prelude::*;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::error::TitanError;
use crate::state::{Position, TitanState, TradingMode};

#[derive(Debug, Clone, Serialize)]
pub struct TradingStatusResponse {
    pub mode: String,
    pub positions: Vec<Position>,
    pub pending_orders_count: usize,
    pub daily_trades: u32,
    pub capital: f64,
    pub realized_pnl_today: f64,
    pub total_realized_pnl: f64,
    pub drawdown_pct: f64,
    pub win_rate: f64,
    pub trade_count_today: u32,
    pub breaker_level: String,
    pub max_positions: u32,
    pub equity: f64,
    pub cash: f64,
    pub buying_power: f64,
}

#[tauri::command]
pub async fn toggle_trading_mode(
    state: State<'_, Arc<TitanState>>,
    app: AppHandle,
) -> Result<String, TitanError> {
    // MERIDIAN owns execution: the legacy bot stays MANUAL (config(1) then smarttrend(3), never overlapping).
    let lock_manual = {
        let c = state.config.lock().unwrap();
        c.meridian.execute && c.meridian.lock_legacy_manual
    };
    let new_mode = {
        let mut st = state.smarttrend.lock().unwrap();
        st.mode = match st.mode {
            TradingMode::Manual if lock_manual => {
                tracing::warn!(component = "trading", "AUTO refused: legacy bot is locked to MANUAL while MERIDIAN executes");
                TradingMode::Manual
            }
            TradingMode::Manual => TradingMode::Auto,
            TradingMode::Auto => TradingMode::Manual,
        };
        st.mode
    };

    let label = format!("{:?}", new_mode);
    tracing::info!(component = "trading", "Mode toggled to {label}");
    let _ = app.emit_all("mode-changed", &label);
    Ok(label)
}

#[tauri::command]
pub fn get_trading_status(
    state: State<'_, Arc<TitanState>>,
) -> TradingStatusResponse {
    // S05: non-overlapping acquisitions — config(1) first, then 3, 4, 5
    // each dropped before next.
    let max_positions = {
        let cfg = state.config.lock().unwrap();
        cfg.execution.max_positions
    };
    let (mode, capital, realized_pnl_today, total_realized_pnl,
         drawdown_pct, win_rate, trade_count_today) = {
        let st = state.smarttrend.lock().unwrap();
        (
            format!("{:?}", st.mode),
            st.account.allocated_capital.to_f64().unwrap_or(0.0),
            st.account.realized_pnl_today.to_f64().unwrap_or(0.0),
            st.account.total_realized_pnl.to_f64().unwrap_or(0.0),
            st.account.drawdown_pct(),
            st.account.win_rate(),
            st.account.trade_count_today,
        )
    };
    let (positions, pending_orders_count, daily_trades) = {
        let trading = state.trading.lock().unwrap();
        (trading.positions.clone(), trading.pending_orders.len(), trading.daily_trades)
    };
    let (breaker_level, equity, cash, buying_power) = {
        let risk = state.risk.lock().unwrap();
        (format!("{:?}", risk.breaker), risk.session_equity, risk.cash, risk.buying_power)
    };

    TradingStatusResponse {
        mode,
        positions,
        pending_orders_count,
        daily_trades,
        capital,
        realized_pnl_today,
        total_realized_pnl,
        drawdown_pct,
        win_rate,
        trade_count_today,
        breaker_level,
        max_positions,
        equity,
        cash,
        buying_power,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trading_status_response_serializes() {
        let resp = TradingStatusResponse {
            mode: "Manual".into(),
            positions: vec![],
            pending_orders_count: 0,
            daily_trades: 0,
            capital: 100_000.0,
            realized_pnl_today: 0.0,
            total_realized_pnl: 0.0,
            drawdown_pct: 0.0,
            win_rate: 0.0,
            trade_count_today: 0,
            breaker_level: "Normal".into(),
            max_positions: 5,
            equity: 100_000.0,
            cash: 50_000.0,
            buying_power: 200_000.0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Manual"));
        assert!(json.contains("100000"));
    }
}
