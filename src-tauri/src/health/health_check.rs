//! `health_check` — self-observability snapshot used by the watchdog
//! script and the UI's status bar.
//!
//! ## Output shape
//!
//! ```json
//! {
//!   "uptime_secs": 12345,
//!   "session_id": "...",
//!   "paper_mode": true,
//!   "last_trade_close_ts": 1234567890,
//!   "last_account_sync_ts": 1234567890,
//!   "open_positions": 3,
//!   "pending_orders": 0,
//!   "ws_stream_stale": false,
//!   "breaker_level": "Normal",
//!   "daily_pnl": 12.34,
//!   "buying_power": 90000.0,
//!   "wallet_floor_breach": false,
//!   "loops_running": 5,
//!   "error_rate_5m": 0
//! }
//! ```
//!
//! Watchdog thresholds (matched in `monitoring/watchdog.sh`):
//!   - `uptime_secs == 0` ⇒ critical (bot just crashed/restarted)
//!   - `ws_stream_stale == true` for >2 polls ⇒ critical
//!   - `breaker_level == "Halt"|"Panic"` ⇒ critical
//!   - `wallet_floor_breach == true` ⇒ critical
//!   - `error_rate_5m > 10` ⇒ critical
//!
//! The watchdog only acts on critical fields; warnings are logged
//! locally for retro analysis but don't trigger pager.

use std::sync::Arc;

use crate::state::TitanState;

/// Build the health snapshot. Returns a `serde_json::Value` so we
/// can ship it as Tauri command output without a hand-written serde
/// type — the schema is documented at the top of this module and
/// pinned by the unit tests below.
pub fn build_snapshot(state: &Arc<TitanState>) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp();
    let session_id = state.session_id.clone();

    // System (uptime + loops)
    let (uptime_secs, loops_running) = {
        let sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
        (sys.uptime_secs(), sys.loops.len() as u32)
    };

    // Config (paper_mode)
    let paper_mode = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        cfg.market.paper_mode
    };

    // Trading (positions + pending)
    let (open_positions, pending_orders) = {
        let t = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        (t.positions.len() as u32, t.pending_orders.len() as u32)
    };

    // Risk (breaker + daily PnL + buying_power + sync)
    let (breaker_level, daily_pnl, buying_power, last_account_sync_ts) = {
        let r = state.risk.lock().unwrap_or_else(|p| p.into_inner());
        let breaker = format!("{:?}", r.breaker);
        (breaker, r.daily_pnl, r.buying_power, r.last_account_sync_at)
    };

    // Journal — last trade close timestamp
    let last_trade_close_ts = {
        let j = state.journal.lock().unwrap_or_else(|p| p.into_inner());
        j.entries
            .iter()
            .filter_map(|e| e.exit_time)
            .max()
            .unwrap_or(0)
    };

    // Wallet floor breach (bot wallet ≤ $10 = depleted)
    let wallet_floor_breach = {
        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        use rust_decimal::prelude::ToPrimitive;
        st.bot_wallet.balance.to_f64().unwrap_or(0.0) < 10.0
    };

    // Error rate — placeholder (Sesiunea 1 emits 0; real metric in Phase 1.5
    // when we attach a tracing-Subscriber that counts ERROR-level lines).
    let error_rate_5m = 0u32;

    // Stream stale flag — read from MarketDataState if available.
    // We don't have an Arc<MarketDataState> on TitanState, so we
    // expose a boolean via a separate hook: the streamer marks
    // `system.loops["streamer"]` stale by NOT ticking it. For S1 we
    // approximate this by checking whether the streamer loop has
    // ticked in the last 60s.
    let ws_stream_stale = {
        let sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
        match sys.loops.get("streamer") {
            None => false, // not yet registered
            Some(h) => (chrono::Utc::now() - h.last_tick).num_seconds() > 60,
        }
    };

    serde_json::json!({
        "uptime_secs": uptime_secs,
        "session_id": session_id,
        "paper_mode": paper_mode,
        "last_trade_close_ts": last_trade_close_ts,
        "last_account_sync_ts": last_account_sync_ts,
        "open_positions": open_positions,
        "pending_orders": pending_orders,
        "ws_stream_stale": ws_stream_stale,
        "breaker_level": breaker_level,
        "daily_pnl": daily_pnl,
        "buying_power": buying_power,
        "wallet_floor_breach": wallet_floor_breach,
        "loops_running": loops_running,
        "error_rate_5m": error_rate_5m,
        "now": now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use rust_decimal_macros::dec;
    use std::sync::Arc;

    #[test]
    fn snapshot_contains_required_fields() {
        let state = Arc::new(TitanState::new(RuntimeConfig::default()));
        let snap = build_snapshot(&state);
        for key in [
            "uptime_secs",
            "session_id",
            "paper_mode",
            "last_trade_close_ts",
            "last_account_sync_ts",
            "open_positions",
            "pending_orders",
            "ws_stream_stale",
            "breaker_level",
            "daily_pnl",
            "buying_power",
            "wallet_floor_breach",
            "loops_running",
            "error_rate_5m",
            "now",
        ] {
            assert!(snap.get(key).is_some(), "missing key: {key}");
        }
    }

    #[test]
    fn snapshot_paper_mode_reflects_config() {
        let state = Arc::new(TitanState::new(RuntimeConfig::default()));
        // Default config has paper_mode=true.
        let snap = build_snapshot(&state);
        assert_eq!(snap["paper_mode"], serde_json::Value::Bool(true));
    }

    #[test]
    fn snapshot_wallet_floor_breach_when_low() {
        let state = Arc::new(TitanState::new(RuntimeConfig::default()));
        // default bot_wallet balance = 0 → breach (below $10)
        let snap = build_snapshot(&state);
        assert_eq!(snap["wallet_floor_breach"], serde_json::Value::Bool(true));
        // Top up bot wallet
        {
            let mut st = state.smarttrend.lock().unwrap();
            st.bot_wallet.topup(dec!(1000));
        }
        let snap = build_snapshot(&state);
        assert_eq!(snap["wallet_floor_breach"], serde_json::Value::Bool(false));
    }

    #[test]
    fn snapshot_breaker_default_normal() {
        let state = Arc::new(TitanState::new(RuntimeConfig::default()));
        let snap = build_snapshot(&state);
        assert_eq!(snap["breaker_level"], serde_json::Value::String("Normal".into()));
    }
}
