use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::error::TitanError;
use crate::execution::order_exec::{send_order, ExecutionMetrics, OrderRequest};
use crate::state::{Direction, TitanState};

#[derive(Debug, Clone, Deserialize)]
pub struct ManualOrderParams {
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
    pub notional_usd: Option<f64>,
    pub wallet_source: Option<String>,
    #[serde(default)]
    pub order_type: Option<String>,
    #[serde(default)]
    pub limit_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManualOrderResponse {
    pub broker_id: String,
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub status: String,
    pub est_cost: f64,
}

#[tauri::command]
pub async fn submit_manual_order(
    params: ManualOrderParams,
    state: State<'_, Arc<TitanState>>,
    app: AppHandle,
) -> Result<ManualOrderResponse, TitanError> {
    let side = match params.side.to_lowercase().as_str() {
        "buy" | "long" => Direction::Long,
        "sell" | "short" => Direction::Short,
        other => {
            return Err(TitanError::Validation(format!(
                "Invalid side '{other}'. Use 'buy'/'long' or 'sell'/'short'."
            )));
        }
    };

    let est_cost = params.notional_usd.unwrap_or(params.qty * 0.0);
    let est_cost_dec = Decimal::from_f64(est_cost).unwrap_or(Decimal::ZERO);
    let wallet_src = params.wallet_source.as_deref().unwrap_or("");

    let deducted = match wallet_src {
        "user" | "bot" => {
            if est_cost_dec <= Decimal::ZERO {
                return Err(TitanError::Validation(
                    "Estimated cost must be positive for wallet orders".into(),
                ));
            }
            let mut st = state.smarttrend.lock().unwrap();
            let wallet = if wallet_src == "user" {
                &mut st.user_wallet
            } else {
                &mut st.bot_wallet
            };
            if est_cost_dec > wallet.balance {
                return Err(TitanError::Validation(format!(
                    "Insufficient {} balance: need ${:.2}, have ${:.2}",
                    wallet_src, est_cost_dec, wallet.balance
                )));
            }
            // S16.0: use `debit()` for trade entries; only operator-
            // initiated cash withdrawals should bump `total_withdrawn`.
            wallet.debit(est_cost_dec).map_err(TitanError::Validation)?;
            tracing::info!(
                component = "wallet",
                source = wallet_src,
                "Deducted ${:.2} for manual order → balance ${:.2}",
                est_cost_dec,
                wallet.balance,
            );
            est_cost_dec
        }
        _ => Decimal::ZERO,
    };

    let order = OrderRequest {
        symbol: params.symbol.trim().to_uppercase(),
        side,
        qty: Decimal::from_f64(params.qty).unwrap_or(Decimal::ZERO),
        stop_loss: params.stop_loss.and_then(Decimal::from_f64),
        take_profit: params.take_profit.and_then(Decimal::from_f64),
        source: wallet_src.to_string(),
        deducted_amount: deducted,
        order_type: params.order_type.as_deref().unwrap_or("market").to_string(),
        limit_price: params.limit_price.and_then(Decimal::from_f64),
        bar_time: None,
        notional: None,
        is_exit: false,
        is_swing_intent: false,
        entry_timeframe: String::new(),
        entry_score: None,
        conviction: None,
        expected_price: 0.0,
    };

    tracing::info!(
        component = "trading",
        symbol = %order.symbol,
        side = ?order.side,
        qty = %order.qty,
        sl = ?order.stop_loss,
        wallet = wallet_src,
        "Manual order submitted"
    );

    let mut metrics = ExecutionMetrics::default();
    let send_result = send_order(state.inner().clone(), order, app.clone(), &mut metrics).await;

    match send_result {
        Ok(result) => {
            let result_qty_f64 = result.qty.to_f64().unwrap_or(0.0);
            let deducted_f64 = deducted.to_f64().unwrap_or(0.0);
            let resp = ManualOrderResponse {
                broker_id: result.broker_id.clone(),
                symbol: result.symbol.clone(),
                side: result.side.clone(),
                qty: result_qty_f64,
                status: result.status.clone(),
                est_cost: if deducted > Decimal::ZERO { deducted_f64 } else { 0.0 },
            };

            let _ = app.emit_all("manual-order-result", &resp);
            Ok(resp)
        }
        Err(e) => {
            if deducted > Decimal::ZERO {
                let mut st = state.smarttrend.lock().unwrap();
                let wallet = if wallet_src == "user" {
                    &mut st.user_wallet
                } else {
                    &mut st.bot_wallet
                };
                wallet.credit(deducted);
                tracing::warn!(
                    component = "wallet",
                    source = wallet_src,
                    "Refunded ${:.2} after order failure → balance ${:.2}",
                    deducted,
                    wallet.balance,
                );
            }
            Err(e)
        }
    }
}

#[tauri::command]
pub async fn close_position(
    symbol: String,
    state: State<'_, Arc<TitanState>>,
    app: AppHandle,
) -> Result<String, TitanError> {
    let symbol = symbol.trim().to_uppercase();

    let (qty, close_side, pos_source) = {
        let trading = state.trading.lock().unwrap();
        let pos = trading
            .positions
            .iter()
            .find(|p| p.symbol == symbol)
            .ok_or_else(|| {
                TitanError::Validation(format!("No open position for {symbol}"))
            })?;
        let close_side = match pos.side {
            Direction::Long => Direction::Short,
            Direction::Short => Direction::Long,
        };
        (pos.qty.abs(), close_side, pos.source.clone())
    };

    let order = OrderRequest {
        symbol: symbol.clone(),
        side: close_side,
        qty,
        stop_loss: None,
        take_profit: None,
        source: pos_source,
        deducted_amount: Decimal::ZERO,
        order_type: "market".to_string(),
        limit_price: None,
        bar_time: None,
        notional: None,
        is_exit: true,
        is_swing_intent: false,
        entry_timeframe: String::new(),
        entry_score: None,
        conviction: None,
        expected_price: 0.0,
    };

    tracing::info!(
        component = "trading",
        symbol = %symbol,
        qty = %qty,
        "Closing position"
    );

    let mut metrics = ExecutionMetrics::default();
    let result = send_order(state.inner().clone(), order, app.clone(), &mut metrics).await?;

    let _ = app.emit_all(
        "position-closed",
        serde_json::json!({
            "symbol": symbol,
            "broker_id": result.broker_id,
        }),
    );

    Ok(format!("CLOSE {symbol} x{qty:.4} [{}]", &result.broker_id[..result.broker_id.len().min(8)]))
}

#[tauri::command]
pub async fn close_position_partial(
    symbol: String,
    percent: f64,
    state: State<'_, Arc<TitanState>>,
    app: AppHandle,
) -> Result<String, TitanError> {
    if !percent.is_finite() || !(1.0..=100.0).contains(&percent) {
        return Err(TitanError::Validation(format!(
            "Percent must be between 1 and 100, got {percent}"
        )));
    }

    let symbol = symbol.trim().to_uppercase();

    let (full_qty, close_side, pos_source) = {
        let trading = state.trading.lock().unwrap();
        let pos = trading
            .positions
            .iter()
            .find(|p| p.symbol == symbol)
            .ok_or_else(|| {
                TitanError::Validation(format!("No open position for {symbol}"))
            })?;
        let close_side = match pos.side {
            Direction::Long => Direction::Short,
            Direction::Short => Direction::Long,
        };
        (pos.qty.abs(), close_side, pos.source.clone())
    };

    let pct_dec = Decimal::from_f64(percent).unwrap_or(Decimal::ZERO);
    let min_qty = Decimal::from_f64(0.01).unwrap_or(Decimal::ZERO);
    let partial_qty = (full_qty * pct_dec / Decimal::ONE_HUNDRED).max(min_qty);

    if partial_qty >= full_qty {
        return close_position(symbol, state, app).await;
    }

    let order = OrderRequest {
        symbol: symbol.clone(),
        side: close_side,
        qty: partial_qty,
        stop_loss: None,
        take_profit: None,
        source: pos_source,
        deducted_amount: Decimal::ZERO,
        order_type: "market".to_string(),
        limit_price: None,
        bar_time: None,
        notional: None,
        is_exit: true,
        is_swing_intent: false,
        entry_timeframe: String::new(),
        entry_score: None,
        conviction: None,
        expected_price: 0.0,
    };

    let partial_qty_f64 = partial_qty.to_f64().unwrap_or(0.0);
    tracing::info!(
        component = "trading",
        symbol = %symbol,
        partial_qty = partial_qty_f64,
        percent,
        "Partial close position"
    );

    let mut metrics = ExecutionMetrics::default();
    let result = send_order(state.inner().clone(), order, app.clone(), &mut metrics).await?;

    let _ = app.emit_all(
        "position-partial-close",
        serde_json::json!({
            "symbol": symbol,
            "percent": percent,
            "qty": partial_qty_f64,
            "broker_id": result.broker_id,
        }),
    );

    Ok(format!(
        "PARTIAL CLOSE {symbol} {percent:.0}% x{partial_qty_f64:.4} [{}]",
        &result.broker_id[..result.broker_id.len().min(8)]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_order_params_deserializes() {
        let json = r#"{"symbol":"AAPL","side":"buy","qty":10.0,"stop_loss":170.0}"#;
        let params: ManualOrderParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.symbol, "AAPL");
        assert_eq!(params.side, "buy");
        assert!((params.qty - 10.0).abs() < f64::EPSILON);
        assert!((params.stop_loss.unwrap() - 170.0).abs() < f64::EPSILON);
        assert!(params.notional_usd.is_none());
        assert!(params.wallet_source.is_none());
    }

    #[test]
    fn manual_order_params_no_stop_loss() {
        let json = r#"{"symbol":"NVDA","side":"sell","qty":5.0}"#;
        let params: ManualOrderParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.symbol, "NVDA");
        assert!(params.stop_loss.is_none());
        assert!(params.notional_usd.is_none());
        assert!(params.wallet_source.is_none());
    }

    #[test]
    fn manual_order_params_with_wallet() {
        let json = r#"{"symbol":"QBTS","side":"buy","qty":35.0,"notional_usd":495.25,"wallet_source":"user"}"#;
        let params: ManualOrderParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.symbol, "QBTS");
        assert!((params.notional_usd.unwrap() - 495.25).abs() < f64::EPSILON);
        assert_eq!(params.wallet_source.as_deref(), Some("user"));
        assert!(params.order_type.is_none());
        assert!(params.limit_price.is_none());
    }

    #[test]
    fn manual_order_params_limit_order() {
        let json = r#"{"symbol":"AAPL","side":"buy","qty":10.0,"order_type":"limit","limit_price":150.50}"#;
        let params: ManualOrderParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.order_type.as_deref(), Some("limit"));
        assert!((params.limit_price.unwrap() - 150.50).abs() < f64::EPSILON);
    }

    #[test]
    fn manual_order_params_backward_compat_no_order_type() {
        let json = r#"{"symbol":"NVDA","side":"sell","qty":5.0}"#;
        let params: ManualOrderParams = serde_json::from_str(json).unwrap();
        assert!(params.order_type.is_none());
        assert!(params.limit_price.is_none());
    }

    #[test]
    fn manual_order_response_serializes() {
        let resp = ManualOrderResponse {
            broker_id: "abc12345".into(),
            symbol: "QBTS".into(),
            side: "buy".into(),
            qty: 35.0,
            status: "accepted".into(),
            est_cost: 495.25,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("QBTS"));
        assert!(json.contains("495.25"));
        assert!(json.contains("abc12345"));
    }
}
