use std::sync::Arc;

use rust_decimal::prelude::FromPrimitive;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::risk::risk_state::{push_equity_history, update_risk_from_equity};
use crate::state::{BreakerLevel, TitanState};

const EQUITY_SYNC_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Deserialize)]
struct AlpacaAccount {
    equity: Option<String>,
    cash: Option<String>,
    buying_power: Option<String>,
    #[allow(dead_code)]
    portfolio_value: Option<String>,
    /// S02: rolling 5-day day-trade count (PDT rule).
    daytrade_count: Option<u32>,
    /// S02: when `true`, account is permanently PDT-flagged.
    pattern_day_trader: Option<bool>,
    /// S40 (audit 2026-05-29): account leverage multiplier as a string
    /// ("1" = cash, "2"/"4" = margin). PDT applies only to margin.
    multiplier: Option<String>,
    /// S40: whether the account can open short positions. Cash accounts
    /// return `false`; short signals are dropped to avoid broker rejects.
    shorting_enabled: Option<bool>,
    /// S19 — equity at the previous session close. Alpaca uses
    /// `equity - last_equity` as the daily change shown in the
    /// account dashboard. We mirror it here so the Journal tab
    /// stays in lockstep with the broker UI even after a mid-day
    /// restart (which used to clobber `initial_equity` to the
    /// current value and zero out the daily figure).
    last_equity: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct EquitySyncPayload {
    equity: f64,
    cash: f64,
    buying_power: f64,
    daily_pnl: f64,
    drawdown_pct: f64,
    breaker_level: String,
    /// S02: PDT day-trade count (UI status bar).
    daytrade_count: u32,
    /// S02: PDT flag from broker (UI warning).
    pattern_day_trader: bool,
    /// S02: convenience flag — `equity < $25_000`. UI uses to colour DT counter
    /// gold (count == 2) / red (count >= 3) and show PDT warn row.
    equity_below_25k: bool,
}

async fn fetch_account(state: &TitanState) -> Option<AlpacaAccount> {
    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    if key.is_empty() || secret.is_empty() {
        return None;
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    let url = format!("{base_url}/v2/account");

    match state
        .http_client
        .get(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                tracing::warn!(
                    component = "equity_sync",
                    status = %resp.status(),
                    "Failed to fetch account"
                );
                return None;
            }
            resp.json::<AlpacaAccount>().await.ok()
        }
        Err(e) => {
            tracing::warn!(component = "equity_sync", "HTTP error: {e}");
            None
        }
    }
}

fn parse_f64(s: &Option<String>) -> f64 {
    s.as_deref()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

pub async fn run_equity_sync(state: Arc<TitanState>, app: AppHandle) {
    tracing::info!(component = "equity_sync", "Equity sync loop started");

    let mut ticker = tokio::time::interval(Duration::from_secs(EQUITY_SYNC_INTERVAL_SECS));

    loop {
        ticker.tick().await;

        if state.shutdown.is_cancelled() {
            break;
        }

        {
            let mut sys = state.system.lock().unwrap();
            if let Some(h) = sys.loops.get_mut("equity_sync") {
                h.tick();
            }
        }

        let account = match fetch_account(&state).await {
            Some(a) => a,
            None => continue,
        };

        let equity = parse_f64(&account.equity);
        let cash = parse_f64(&account.cash);
        let buying_power = parse_f64(&account.buying_power);
        let last_equity = parse_f64(&account.last_equity);
        let daytrade_count = account.daytrade_count.unwrap_or(0);
        let pattern_day_trader = account.pattern_day_trader.unwrap_or(false);
        // S40: parse account_multiplier (default 1.0 = cash when the
        // field is present-but-unparseable; Alpaca always returns it).
        let account_multiplier = account
            .multiplier
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(1.0);
        let shorting_enabled = account.shorting_enabled.unwrap_or(true);

        if equity <= 0.0 {
            continue;
        }

        let initial_equity = {
            let mut risk = state.risk.lock().unwrap();
            if risk.initial_equity <= 0.0 {
                risk.initial_equity = equity;
                risk.peak_equity = equity;
            }
            risk.cash = cash;
            risk.buying_power = buying_power;
            risk.daytrade_count = daytrade_count;
            risk.pattern_day_trader = pattern_day_trader;
            risk.account_multiplier = account_multiplier;
            risk.shorting_enabled = shorting_enabled;
            risk.last_account_sync_at = chrono::Utc::now().timestamp();
            // S19 — broker-side daily anchor. Updated every tick; the
            // value is stable across the session (Alpaca only changes
            // it at the daily close-of-business rollover) so this is a
            // cheap, idempotent write.
            if last_equity > 0.0 {
                risk.alpaca_last_equity = last_equity;
            }
            risk.initial_equity
        };

        // Policy: allocated_capital = broker_equity at each sync.
        // After this, allocated_capital updates ONLY through record_fill;
        // broker_equity refreshes at 30s. Divergence is an audit signal, not a reset.
        {
            let equity_dec = rust_decimal::Decimal::from_f64(equity).unwrap_or(rust_decimal::Decimal::ZERO);
            let mut st = state.smarttrend.lock().unwrap();
            st.account.allocated_capital = equity_dec;
            if equity_dec > st.account.high_water_mark {
                st.account.high_water_mark = equity_dec;
            }
        }

        push_equity_history(&state, equity);

        if let Some((prev, new)) = update_risk_from_equity(&state, equity, initial_equity) {
            tracing::warn!(
                component = "equity_sync",
                prev = ?prev,
                new = ?new,
                equity = format!("{equity:.2}"),
                "Breaker level changed"
            );
            let _ = app.emit_all(
                "breaker-changed",
                serde_json::json!({
                    "prev": format!("{prev:?}"),
                    "new": format!("{new:?}"),
                    "equity": equity,
                }),
            );

            if new == BreakerLevel::Panic {
                tracing::error!(component = "equity_sync", "PANIC breaker triggered!");
            }
        }

        let (daily_pnl, drawdown_pct, breaker_level) = {
            let risk = state.risk.lock().unwrap();
            (
                risk.daily_pnl,
                risk.current_drawdown_pct,
                format!("{:?}", risk.breaker),
            )
        };

        let _ = app.emit_all(
            "equity-sync",
            EquitySyncPayload {
                equity,
                cash,
                buying_power,
                daily_pnl,
                drawdown_pct,
                breaker_level,
                daytrade_count,
                pattern_day_trader,
                equity_below_25k: equity < 25_000.0,
            },
        );
    }

    tracing::info!(component = "equity_sync", "Equity sync loop exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_f64_basic() {
        assert!((parse_f64(&Some("100.5".into())) - 100.5).abs() < f64::EPSILON);
        assert!((parse_f64(&None)).abs() < f64::EPSILON);
        assert!((parse_f64(&Some("garbage".into()))).abs() < f64::EPSILON);
    }

    #[test]
    fn alpaca_account_deserialize() {
        let json = r#"{"equity":"100000.50","cash":"50000","buying_power":"200000","portfolio_value":"100000.50"}"#;
        let acct: AlpacaAccount = serde_json::from_str(json).unwrap();
        assert!((parse_f64(&acct.equity) - 100_000.5).abs() < 0.01);
        assert_eq!(acct.daytrade_count, None);
        assert_eq!(acct.pattern_day_trader, None);
    }

    #[test]
    fn alpaca_account_deserialize_with_pdt() {
        let json = r#"{"equity":"24000.00","cash":"5000","buying_power":"10000","portfolio_value":"24000.00","daytrade_count":3,"pattern_day_trader":true}"#;
        let acct: AlpacaAccount = serde_json::from_str(json).unwrap();
        assert_eq!(acct.daytrade_count, Some(3));
        assert_eq!(acct.pattern_day_trader, Some(true));
    }

    #[test]
    fn equity_sync_payload_serializes() {
        let p = EquitySyncPayload {
            equity: 100_000.0,
            cash: 50_000.0,
            buying_power: 200_000.0,
            daily_pnl: -500.0,
            drawdown_pct: 1.5,
            breaker_level: "Normal".into(),
            daytrade_count: 1,
            pattern_day_trader: false,
            equity_below_25k: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("100000"));
        assert!(json.contains("daytrade_count"));
        assert!(json.contains("equity_below_25k"));
    }
}
