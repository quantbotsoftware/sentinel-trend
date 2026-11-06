use std::sync::Arc;

use tauri::{AppHandle, Manager, State};

use crate::error::TitanError;
use crate::state::{BreakerLevel, TitanState};

#[tauri::command]
pub async fn liquidate_all(
    confirm_token: String,
    state: State<'_, Arc<TitanState>>,
    app: AppHandle,
) -> Result<String, TitanError> {
    if confirm_token != "CONFIRM_LIQUIDATE" {
        return Err(TitanError::Validation(
            "Missing confirmation token. Pass 'CONFIRM_LIQUIDATE' to confirm.".into(),
        ));
    }

    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    let url = format!("{base_url}/v2/positions");

    tracing::warn!(component = "trading", "LIQUIDATE ALL — sending DELETE {url}");

    let resp = state
        .http_client
        .delete(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(format!("Liquidate request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() && status.as_u16() != 207 {
        let req_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("?")
            .to_string();
        let txt = resp.text().await.unwrap_or_default();
        tracing::error!(status = %status, request_id = %req_id, body = %txt, "Alpaca API error (liquidate)");
        return Err(TitanError::Broker(format!(
            "Liquidate failed {status}: request_id={req_id}"
        )));
    }

    let count = {
        let mut trading = state.trading.lock().unwrap();
        let c = trading.positions.len();
        trading.positions.clear();
        trading.pending_orders.clear();
        c
    };

    tracing::warn!(component = "trading", count, "Positions liquidated at broker");
    let _ = app.emit_all(
        "positions-liquidated",
        serde_json::json!({ "count": count }),
    );

    Ok(format!("Liquidated {count} positions"))
}

#[tauri::command]
pub async fn emergency_liquidate(
    confirm_token: String,
    state: State<'_, Arc<TitanState>>,
    app: AppHandle,
) -> Result<String, TitanError> {
    if confirm_token != "CONFIRM_EMERGENCY" {
        return Err(TitanError::Validation(
            "Missing confirmation token. Pass 'CONFIRM_EMERGENCY' to confirm.".into(),
        ));
    }

    tracing::error!(component = "trading", "EMERGENCY LIQUIDATION triggered!");

    {
        // S05: config(1) — dropped before trading(4) and risk(5)
        let (key, secret, paper) = {
            let cfg = state.config.lock().unwrap();
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
                cfg.market.paper_mode,
            )
        };
        let base_url = if paper {
            "https://paper-api.alpaca.markets"
        } else {
            "https://api.alpaca.markets"
        };
        let cancel_url = format!("{base_url}/v2/orders");
        let _ = state
            .http_client
            .delete(&cancel_url)
            .header("APCA-API-KEY-ID", &key)
            .header("APCA-API-SECRET-KEY", &secret)
            .send()
            .await;
        tracing::warn!(component = "trading", "All pending orders cancelled");
    }

    // S05: trading(4) before risk(5) — ascending order
    state.trading.lock().unwrap().pending_orders.clear();

    {
        let mut risk = state.risk.lock().unwrap();
        risk.breaker = BreakerLevel::Panic;
    }

    let _ = app.emit_all("breaker-triggered", serde_json::json!({ "level": "Panic", "reason": "emergency_liquidate" }));

    let result = liquidate_all(
        "CONFIRM_LIQUIDATE".into(),
        state,
        app.clone(),
    )
    .await?;

    let _ = app.emit_all(
        "log-message",
        "EMERGENCY LIQUIDATION COMPLETE — breaker set to Panic",
    );

    Ok(format!("EMERGENCY LIQUIDATION COMPLETE. {result}"))
}
