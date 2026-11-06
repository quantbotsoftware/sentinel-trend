use std::fs;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;
use crate::state::{TitanState, Direction, PendingOrder, OrderHistoryEntry};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub client_order_id: String,
    pub symbol: String,
    pub qty: Decimal,
    pub side: Direction,
    pub strategy_id: Option<String>,
    pub timestamp: i64,
    pub deducted_amount: Decimal,
    pub order_type: String,
    pub limit_price: Option<Decimal>,
    pub stop_loss: Option<Decimal>,
    pub take_profit: Option<Decimal>,
    pub is_exit: bool,
    pub is_swing_intent: bool,
    pub entry_timeframe: String,
    pub entry_score: Option<f64>,
}

pub fn wal_dir() -> PathBuf {
    let mut p = if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h)
    } else {
        PathBuf::from(".")
    };
    p.push(".local/share/sentinel-trend/wal");
    p
}

pub fn wal_path(client_order_id: &str) -> PathBuf {
    wal_dir().join(format!("st-intent-{}.json", client_order_id))
}

pub fn write_intent(intent: &OrderIntent) -> std::io::Result<()> {
    let dir = wal_dir();
    fs::create_dir_all(&dir)?;
    let path = wal_path(&intent.client_order_id);
    let tmp = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(intent).map_err(std::io::Error::other)?;
    fs::write(&tmp, content)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn clear_intent(client_order_id: &str) -> std::io::Result<()> {
    let path = wal_path(client_order_id);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn list_intents() -> std::io::Result<Vec<OrderIntent>> {
    let dir = wal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut intents = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(intent) = serde_json::from_str::<OrderIntent>(&content) {
                    intents.push(intent);
                }
            }
        }
    }
    Ok(intents)
}

/// Reconciles WAL intents on boot by querying the Alpaca API using the client_order_id.
pub async fn reconcile_wal_on_boot(state: &std::sync::Arc<TitanState>) -> Result<(), String> {
    let intents = list_intents().map_err(|e| e.to_string())?;
    if intents.is_empty() {
        return Ok(());
    }

    tracing::info!(
        component = "order_wal",
        count = intents.len(),
        "Found residual order intents on boot, starting auto-healing reconciliation"
    );

    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    if key.is_empty() || secret.is_empty() {
        tracing::warn!(
            component = "order_wal",
            "API keys missing, skipping recovery"
        );
        return Ok(());
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    for intent in intents {
        let cid = &intent.client_order_id;
        let url = format!("{}/v2/orders/by_client_order_id?client_order_id={}", base_url, cid);

        tracing::info!(
            component = "order_wal",
            client_order_id = %cid,
            symbol = %intent.symbol,
            "Reconciling intent against broker"
        );

        let res = state
            .http_client
            .get(&url)
            .header("APCA-API-KEY-ID", &key)
            .header("APCA-API-SECRET-KEY", &secret)
            .send()
            .await;

        match res {
            Ok(resp) => {
                let status_code = resp.status();
                if status_code.is_success() {
                    // Order exists at the broker
                    if let Ok(order_json) = resp.json::<serde_json::Value>().await {
                        let broker_id = order_json["id"].as_str().unwrap_or("").to_string();
                        let order_status = order_json["status"].as_str().unwrap_or("unknown").to_string();
                        tracing::info!(
                            component = "order_wal",
                            client_order_id = %cid,
                            broker_id = %broker_id,
                            status = %order_status,
                            "Order found at broker, syncing local state"
                        );

                        if order_status == "rejected" || order_status == "canceled" {
                            // Order failed, refund the wallet
                            refund_wallet(state, intent.deducted_amount, &intent.symbol);
                        } else {
                            // Order is active or filled at broker, ensure it is in pending_orders
                            ensure_tracked_pending_order(state, &intent, broker_id, order_status);
                        }
                    }
                } else if status_code == reqwest::StatusCode::NOT_FOUND {
                    // Order never reached the broker, rollback wallet debit
                    tracing::info!(
                        component = "order_wal",
                        client_order_id = %cid,
                        symbol = %intent.symbol,
                        "Order not found at broker, performing wallet rollback (refund)"
                    );
                    refund_wallet(state, intent.deducted_amount, &intent.symbol);
                } else {
                    tracing::warn!(
                        component = "order_wal",
                        client_order_id = %cid,
                        status = %status_code,
                        "Unexpected broker response during recovery"
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    component = "order_wal",
                    client_order_id = %cid,
                    error = %e,
                    "Failed to send broker inquiry during WAL recovery"
                );
                // Keep the WAL file so we can try again on the next boot
                continue;
            }
        }

        // Successfully reconciled (committed or rolled back), clear the WAL entry
        let _ = clear_intent(cid);
    }

    Ok(())
}

fn refund_wallet(state: &TitanState, amount: Decimal, symbol: &str) {
    if amount <= Decimal::ZERO {
        return;
    }
    let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
    st.bot_wallet.balance += amount;
    tracing::info!(
        component = "order_wal",
        symbol = %symbol,
        amount = %amount,
        new_balance = %st.bot_wallet.balance,
        "Wallet successfully refunded"
    );
}

fn ensure_tracked_pending_order(
    state: &TitanState,
    intent: &OrderIntent,
    broker_id: String,
    order_status: String,
) {
    let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
    
    // Check if already in pending_orders
    if trading.pending_orders.iter().any(|po| po.client_order_id == intent.client_order_id) {
        return;
    }

    let po = PendingOrder {
        order_id: broker_id.clone(),
        symbol: intent.symbol.clone(),
        side: intent.side,
        qty: intent.qty,
        submitted_at: intent.timestamp,
        source: intent.strategy_id.as_ref().map_or("bot".to_string(), |s| format!("s2:{}", s)),
        deducted_amount: intent.deducted_amount,
        client_order_id: intent.client_order_id.clone(),
        last_fill_at: None,
        is_swing_intent: intent.is_swing_intent,
        entry_timeframe: intent.entry_timeframe.clone(),
        stop_loss: intent.stop_loss.map(|d| rust_decimal::prelude::ToPrimitive::to_f64(&d).unwrap_or(0.0)).unwrap_or(0.0),
        take_profit: intent.take_profit.map(|d| rust_decimal::prelude::ToPrimitive::to_f64(&d).unwrap_or(0.0)).unwrap_or(0.0),
        // WAL replay has no conviction trail (intent predates PRIME F3).
        conviction: None,
        expected_price: 0.0,
    };

    trading.pending_orders.push(po);

    // Also push to order history if not present
    if !trading.order_history.iter().any(|oh| oh.order_id == broker_id || oh.order_id == intent.client_order_id) {
        trading.order_history.push(OrderHistoryEntry {
            order_id: broker_id,
            symbol: intent.symbol.clone(),
            side: match intent.side {
                Direction::Long => "buy".to_string(),
                Direction::Short => "sell".to_string(),
            },
            qty: intent.qty,
            order_type: intent.order_type.clone(),
            limit_price: intent.limit_price,
            status: order_status,
            source: intent.strategy_id.as_ref().map_or("bot".to_string(), |s| format!("s2:{}", s)),
            submitted_at: intent.timestamp,
            filled_price: None,
            filled_at: None,
            reject_reason: None,
            entry_score: intent.entry_score,
            entry_timeframe: intent.entry_timeframe.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_clear_list_intents() {
        let original_home = std::env::var_os("HOME");
        let mut tmp_dir = std::env::temp_dir();
        tmp_dir.push(format!("sentinel-wal-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).unwrap();

        std::env::set_var("HOME", &tmp_dir);

        let intent = OrderIntent {
            client_order_id: "test-cid-123".to_string(),
            symbol: "AAPL".to_string(),
            qty: Decimal::new(10, 0),
            side: Direction::Long,
            strategy_id: Some("gap_fade".to_string()),
            timestamp: 123456789,
            deducted_amount: Decimal::new(500, 0),
            order_type: "market".to_string(),
            limit_price: None,
            stop_loss: None,
            take_profit: None,
            is_exit: false,
            is_swing_intent: false,
            entry_timeframe: "1m".to_string(),
            entry_score: Some(85.0),
        };

        // Write
        write_intent(&intent).unwrap();

        // List
        let list = list_intents().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].client_order_id, "test-cid-123");
        assert_eq!(list[0].symbol, "AAPL");

        // Clear
        clear_intent("test-cid-123").unwrap();
        let list_empty = list_intents().unwrap();
        assert!(list_empty.is_empty());

        // Clean up
        let _ = std::fs::remove_dir_all(&tmp_dir);
        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
    }
}
