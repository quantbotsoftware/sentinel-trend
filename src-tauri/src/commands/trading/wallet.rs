use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::*;
use serde::Serialize;
use tauri::State;

use crate::error::TitanError;
use crate::state::{OrderHistoryEntry, TitanState};

#[derive(Debug, Clone, Serialize)]
pub struct WalletStatusResponse {
    pub user_balance: f64,
    pub user_total_deposited: f64,
    pub user_total_withdrawn: f64,
    pub bot_balance: f64,
    pub bot_total_deposited: f64,
    pub bot_total_withdrawn: f64,
    pub max_usd_per_asset: f64,
    pub main_cash: f64,
    /// Sum of `qty × current_price` for positions tagged `source == "bot"`.
    /// Combined with `bot_balance` to compute true bot equity (cash + holdings).
    pub bot_positions_value: f64,
    /// Sum of `qty × current_price` for positions tagged `source == "user"`.
    pub user_positions_value: f64,
    /// Sum of `qty × current_price` for positions with empty `source`
    /// (legacy / reconciled from broker without entry attribution).
    pub unattributed_positions_value: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DualBalanceResponse {
    pub user_balance: f64,
    pub bot_balance: f64,
}

#[tauri::command]
pub fn get_wallet_status(
    state: State<'_, Arc<TitanState>>,
) -> WalletStatusResponse {
    // S05: non-overlapping — config(1), smarttrend(3), trading(4), risk(5) each dropped before next
    let max_usd_per_asset = {
        let cfg = state.config.lock().unwrap();
        cfg.execution.max_usd_per_asset
    };
    let (user_balance, user_total_deposited, user_total_withdrawn,
         bot_balance, bot_total_deposited, bot_total_withdrawn) = {
        let st = state.smarttrend.lock().unwrap();
        (
            st.user_wallet.balance.to_f64().unwrap_or(0.0),
            st.user_wallet.total_deposited.to_f64().unwrap_or(0.0),
            st.user_wallet.total_withdrawn.to_f64().unwrap_or(0.0),
            st.bot_wallet.balance.to_f64().unwrap_or(0.0),
            st.bot_wallet.total_deposited.to_f64().unwrap_or(0.0),
            st.bot_wallet.total_withdrawn.to_f64().unwrap_or(0.0),
        )
    };
    let (bot_positions_value, user_positions_value, unattributed_positions_value) = {
        let trading = state.trading.lock().unwrap();
        let mut bot = 0.0_f64;
        let mut user = 0.0_f64;
        let mut other = 0.0_f64;
        for p in &trading.positions {
            let qty_f = p.qty.to_f64().unwrap_or(0.0);
            let val = qty_f * p.current_price;
            match p.source.as_str() {
                "bot" => bot += val,
                "user" => user += val,
                _ => other += val,
            }
        }
        (bot, user, other)
    };
    let main_cash = {
        let risk = state.risk.lock().unwrap();
        risk.cash
    };

    WalletStatusResponse {
        user_balance,
        user_total_deposited,
        user_total_withdrawn,
        bot_balance,
        bot_total_deposited,
        bot_total_withdrawn,
        max_usd_per_asset,
        main_cash,
        bot_positions_value,
        user_positions_value,
        unattributed_positions_value,
    }
}

#[tauri::command]
pub fn get_order_history(
    state: State<'_, Arc<TitanState>>,
) -> Vec<OrderHistoryEntry> {
    let trading = state.trading.lock().unwrap();
    trading.order_history.clone()
}

#[tauri::command]
pub fn topup_user(
    amount: f64,
    state: State<'_, Arc<TitanState>>,
) -> Result<f64, TitanError> {
    if amount <= 0.0 {
        return Err(TitanError::Validation("Amount must be positive".into()));
    }

    let amount_dec = Decimal::from_f64(amount).unwrap_or(Decimal::ZERO);

    // S05: smarttrend(3) → risk(5) — ascending order, never invert
    let mut st = state.smarttrend.lock().unwrap();
    let risk = state.risk.lock().unwrap();

    if amount > risk.cash {
        return Err(TitanError::Validation(format!(
            "Amount {amount:.2} exceeds available Main cash {:.2}",
            risk.cash
        )));
    }

    st.user_wallet.topup(amount_dec);
    let bal = st.user_wallet.balance.to_f64().unwrap_or(0.0);
    tracing::info!(component = "wallet", "User topup +{amount:.2} → balance {bal:.2}");
    Ok(bal)
}

#[tauri::command]
pub fn withdraw_user(
    amount: f64,
    state: State<'_, Arc<TitanState>>,
) -> Result<f64, TitanError> {
    if amount <= 0.0 {
        return Err(TitanError::Validation("Amount must be positive".into()));
    }

    let amount_dec = Decimal::from_f64(amount).unwrap_or(Decimal::ZERO);
    let mut st = state.smarttrend.lock().unwrap();

    st.user_wallet.withdraw(amount_dec).map_err(TitanError::Validation)?;
    let bal = st.user_wallet.balance.to_f64().unwrap_or(0.0);
    tracing::info!(component = "wallet", "User withdraw -{amount:.2} → balance {bal:.2}");
    Ok(bal)
}

#[tauri::command]
pub fn topup_bot(
    amount: f64,
    state: State<'_, Arc<TitanState>>,
) -> Result<DualBalanceResponse, TitanError> {
    if amount <= 0.0 {
        return Err(TitanError::Validation("Amount must be positive".into()));
    }

    let amount_dec = Decimal::from_f64(amount).unwrap_or(Decimal::ZERO);
    let mut st = state.smarttrend.lock().unwrap();

    if amount_dec > st.user_wallet.balance {
        return Err(TitanError::Validation(format!(
            "Amount {amount:.2} exceeds User wallet balance {:.2}",
            st.user_wallet.balance
        )));
    }

    st.user_wallet.withdraw(amount_dec).map_err(TitanError::Validation)?;
    st.bot_wallet.topup(amount_dec);
    let user_bal = st.user_wallet.balance.to_f64().unwrap_or(0.0);
    let bot_bal = st.bot_wallet.balance.to_f64().unwrap_or(0.0);
    tracing::info!(
        component = "wallet",
        "Bot topup +{amount:.2} (user→bot) → user={user_bal:.2}, bot={bot_bal:.2}",
    );

    Ok(DualBalanceResponse {
        user_balance: user_bal,
        bot_balance: bot_bal,
    })
}

#[tauri::command]
pub fn withdraw_bot(
    amount: f64,
    state: State<'_, Arc<TitanState>>,
) -> Result<DualBalanceResponse, TitanError> {
    if amount <= 0.0 {
        return Err(TitanError::Validation("Amount must be positive".into()));
    }

    let amount_dec = Decimal::from_f64(amount).unwrap_or(Decimal::ZERO);
    let mut st = state.smarttrend.lock().unwrap();

    st.bot_wallet.withdraw(amount_dec).map_err(TitanError::Validation)?;
    st.user_wallet.topup(amount_dec);
    let user_bal = st.user_wallet.balance.to_f64().unwrap_or(0.0);
    let bot_bal = st.bot_wallet.balance.to_f64().unwrap_or(0.0);
    tracing::info!(
        component = "wallet",
        "Bot withdraw -{amount:.2} (bot→user) → user={user_bal:.2}, bot={bot_bal:.2}",
    );

    Ok(DualBalanceResponse {
        user_balance: user_bal,
        bot_balance: bot_bal,
    })
}

/// v2.5.0 — Sync bot_wallet.balance to the broker's actual cash position.
///
/// Background: the internal `bot_wallet` accounting is debit-on-entry /
/// credit-on-close. Over long runs (especially after bracket-SL/TP-driven
/// closes that race with the recon loop), small accounting drifts add up
/// and bot_wallet falls below broker cash. Position sizing then under-
/// shoots actual buying power dramatically — live evidence 2026-05-14:
/// bot_wallet=$29k while broker cash=$85k, capping every entry at the
/// internal "max" instead of the broker's real headroom.
///
/// This command does an unconditional one-way sync: hit Alpaca
/// `/v2/account`, read `cash`, set `bot_wallet.balance = cash`. The
/// delta is recorded via `topup` or `withdraw` semantics so the
/// `total_deposited` / `total_withdrawn` cumulative figures stay
/// auditable.
///
/// Note: this DOES rewrite the bot wallet authoritatively to broker
/// truth. Any in-flight `pending_orders` will continue using the new
/// balance for sizing on next signal. Existing open positions are
/// unaffected — they're tracked separately in `state.trading.positions`.
#[tauri::command]
pub async fn sync_bot_wallet_to_broker(
    state: State<'_, Arc<TitanState>>,
) -> Result<f64, TitanError> {
    let (key, secret, paper) = {
        let cfg = state.config.lock().map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };
    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation("Alpaca credentials not set".into()));
    }
    let host = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };
    let url = format!("{host}/v2/account");
    let resp = state
        .http_client
        .get(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .map_err(|e| TitanError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(TitanError::Network(format!(
            "Alpaca account fetch failed: HTTP {}",
            resp.status()
        )));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| TitanError::Network(e.to_string()))?;
    let cash: f64 = body
        .get("cash")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| TitanError::Validation("Missing cash field in account response".into()))?;
    let cash_dec = Decimal::from_f64(cash).unwrap_or(Decimal::ZERO);

    let mut st = state.smarttrend.lock().unwrap();
    let prev = st.bot_wallet.balance;
    if cash_dec > prev {
        let delta = cash_dec - prev;
        st.bot_wallet.topup(delta);
        tracing::info!(
            component = "wallet",
            "Bot wallet SYNC +${delta} (was ${prev}, now ${cash_dec}) — matched to Alpaca cash",
        );
    } else if cash_dec < prev {
        let delta = prev - cash_dec;
        st.bot_wallet.withdraw(delta).map_err(TitanError::Validation)?;
        tracing::info!(
            component = "wallet",
            "Bot wallet SYNC -${delta} (was ${prev}, now ${cash_dec}) — matched to Alpaca cash",
        );
    } else {
        tracing::info!(
            component = "wallet",
            "Bot wallet SYNC no-op (${prev} already matches Alpaca cash)",
        );
    }
    let bal = st.bot_wallet.balance.to_f64().unwrap_or(0.0);
    Ok(bal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_status_serializes() {
        let resp = WalletStatusResponse {
            user_balance: 2500.0,
            user_total_deposited: 3000.0,
            user_total_withdrawn: 500.0,
            bot_balance: 1000.0,
            bot_total_deposited: 1200.0,
            bot_total_withdrawn: 200.0,
            max_usd_per_asset: 500.0,
            main_cash: 50_000.0,
            bot_positions_value: 800.0,
            user_positions_value: 0.0,
            unattributed_positions_value: 0.0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("2500"));
        assert!(json.contains("1000"));
        assert!(json.contains("max_usd_per_asset"));
        assert!(json.contains("bot_positions_value"));
    }

    /// S16.1 — Bot equity must include open-position market value so the UI's
    /// `pnl = balance + positions_value - total_deposited + total_withdrawn`
    /// formula reflects deployed capital, not phantom losses.
    #[test]
    fn s16_1_bot_pnl_formula_with_open_positions() {
        // Scenario: $5,000 deposit, $1,500 deployed into 3 positions worth
        // $1,495 today. Pure-cash formula would show -$1,500 fake loss.
        let resp = WalletStatusResponse {
            user_balance: 0.0,
            user_total_deposited: 0.0,
            user_total_withdrawn: 0.0,
            bot_balance: 3500.0,
            bot_total_deposited: 5000.0,
            bot_total_withdrawn: 0.0,
            max_usd_per_asset: 500.0,
            main_cash: 0.0,
            bot_positions_value: 1495.0,
            user_positions_value: 0.0,
            unattributed_positions_value: 0.0,
        };
        let bot_equity = resp.bot_balance + resp.bot_positions_value;
        let bot_pnl = bot_equity - resp.bot_total_deposited + resp.bot_total_withdrawn;
        // True P/L is -$5 (cash $3500 + holdings $1495 = $4995 vs $5000 deposit)
        assert!((bot_pnl - (-5.0)).abs() < 0.01,
                "bot_pnl should be -$5, got ${bot_pnl}");
        // Without positions_value the formula would lie:
        let phantom_pnl = resp.bot_balance - resp.bot_total_deposited + resp.bot_total_withdrawn;
        assert!((phantom_pnl - (-1500.0)).abs() < 0.01,
                "phantom_pnl proves the old formula was -$1500 fake loss");
    }

    #[test]
    fn dual_balance_response_serializes() {
        let resp = DualBalanceResponse {
            user_balance: 1500.0,
            bot_balance: 800.0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("1500"));
        assert!(json.contains("800"));
    }
}
