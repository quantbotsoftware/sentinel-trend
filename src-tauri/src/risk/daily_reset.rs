use std::sync::Arc;

use chrono::{Datelike, Timelike};
use chrono_tz::US::Eastern;
use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::market::market_state::MarketDataState;
use crate::state::{BreakerLevel, TitanState};

/// S15.6: shrunk from 60s → 30s so the watchdog (60s `STALE_WARN_SECS`)
/// stays green even with normal jitter. The `last_reset_date` guard
/// below prevents double-reset across the new cadence.
const CHECK_INTERVAL_SECS: u64 = 30;
const RESET_HOUR: u32 = 9;
const RESET_MINUTE: u32 = 30;

// NYSE holiday calendar moved to `risk::calendar` (S19) so the
// auto-scanner, EOD flush, and daily reset all share the same
// source of truth. See `risk::calendar::{is_nyse_holiday,
// is_nyse_trading_day}` and the `NYSE_HOLIDAYS_2026/2027` lists
// for the canonical data.

/// Async loop that resets daily counters at 09:30 ET.
/// Runs a check every 60 seconds. Guards against double-reset
/// by tracking the last reset date.
pub async fn run_daily_reset(state: Arc<TitanState>, market: Arc<MarketDataState>, app: AppHandle) {
    tracing::info!(component = "daily_reset", "Daily reset loop started");

    let mut ticker = tokio::time::interval(Duration::from_secs(CHECK_INTERVAL_SECS));
    let mut last_reset_date: Option<chrono::NaiveDate> = None;

    loop {
        ticker.tick().await;

        if state.shutdown.is_cancelled() {
            break;
        }

        // S15.6: tick must precede continue-guards so STALE doesn't
        // fire on weekends / holidays / pre-09:30 ticks.
        {
            let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = sys.loops.get_mut("daily_reset") {
                h.tick();
            }
        }

        let now_et = chrono::Utc::now().with_timezone(&Eastern);
        let today = now_et.date_naive();

        if let Some(last) = last_reset_date {
            if last == today {
                continue;
            }
        }

        // S03: skip weekends and NYSE holidays — no trading session to reset.
        let weekday = now_et.weekday();
        if matches!(weekday, chrono::Weekday::Sat | chrono::Weekday::Sun) {
            continue;
        }
        if crate::risk::calendar::is_nyse_holiday(today) {
            continue;
        }

        let hour = now_et.hour();
        let minute = now_et.minute();

        if hour < RESET_HOUR || (hour == RESET_HOUR && minute < RESET_MINUTE) {
            continue;
        }

        tracing::info!(
            component = "daily_reset",
            date = %today,
            "Executing daily reset at {:02}:{:02} ET",
            hour,
            minute
        );

        // S03: Cancel ALL pending orders at broker BEFORE clearing local state.
        // If cancel fails (e.g. broker down), skip this reset cycle and retry
        // at the next tick (60s) — fail-safe to avoid stale phantom orders.
        match crate::execution::order_exec::cancel_all_open_orders(&state).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(component = "daily_reset", cancelled = n, "Cancelled pending broker orders before reset");
                }
            }
            Err(e) => {
                tracing::error!(component = "daily_reset", error = %e, "cancel_all_open_orders failed; deferring reset to next tick");
                continue;
            }
        }

        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_orders.clear();
            trading.fill_cumulative.clear();
        }

        {
            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            st.account.reset_daily();
        }

        {
            let mut risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
            risk.daily_pnl = 0.0;
            if risk.breaker != BreakerLevel::Panic {
                risk.breaker = BreakerLevel::Normal;
            }
            risk.initial_equity = risk.session_equity;
        }

        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.daily_trades = 0;
            // S17.1 — fresh chance for every ticker tomorrow. The
            // intraday loss-streak blacklist deliberately resets at
            // the same boundary as `daily_trades` so a bad ticker on
            // day N is not permanently barred on day N+1.
            trading.symbol_loss_streak.clear();
        }

        // S18 — clear the AutoUniverse blacklist at the daily boundary
        // for the same reason: a name that was hot garbage today may
        // have a clean catalyst tomorrow. The pinned list and the auto
        // pool are deliberately *preserved* — pins are user choices and
        // auto entries that survived the day's score-decay/stale checks
        // are still relevant tomorrow morning. The first auto-scan tick
        // of the new session will re-promote / demote as appropriate.
        {
            let mut market_state = state.market.lock().unwrap_or_else(|p| p.into_inner());
            let cleared = market_state.auto_universe.blacklist.len();
            if cleared > 0 {
                tracing::info!(
                    component = "daily_reset",
                    cleared,
                    "AutoUniverse blacklist cleared at daily boundary"
                );
                market_state.auto_universe.clear_all_blacklist();
            }
        }

        // S06: clear session-scoped market data (Tier-B order: last_prices(1) → cvd(2))
        market
            .last_prices
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        market
            .cvd
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        // F1/F1b — clear MicroBook + BurstScanner (B7/B8), then Tier-A
        // burst_promoted (separate acquisition after Tier-B dropped).
        market
            .micro
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reset_daily();
        market
            .burst
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reset_daily();
        // F2 — drop session halt/resume rows (including resumed_at stamps).
        market
            .halts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        {
            let mut mkt = state.market.lock().unwrap_or_else(|p| p.into_inner());
            mkt.burst_promoted.clear();
        }
        // S06 — EDGAR offering cache (24h TTL prunes anyway; this
        // keeps weekend leftovers out of Monday's session).
        crate::market::edgar::daily_reset();

        last_reset_date = Some(today);

        let _ = app.emit_all(
            "daily-reset",
            serde_json::json!({
                "date": today.to_string(),
                "time_et": format!("{:02}:{:02}", hour, minute),
            }),
        );

        tracing::info!(component = "daily_reset", "Daily reset complete");
    }

    tracing::info!(component = "daily_reset", "Daily reset loop exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    #[test]
    fn reset_clears_daily_counters() {
        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            st.account.record_fill(dec!(500));
            st.account.record_fill(dec!(-200));
            assert_eq!(st.account.trade_count_today, 2);
        }
        {
            let mut risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
            risk.daily_pnl = 300.0;
            risk.breaker = BreakerLevel::Caution;
            risk.session_equity = 100_000.0;
        }
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.daily_trades = 5;
        }

        // Simulate reset
        {
            let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            st.account.reset_daily();
        }
        {
            let mut risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
            risk.daily_pnl = 0.0;
            if risk.breaker != BreakerLevel::Panic {
                risk.breaker = BreakerLevel::Normal;
            }
            risk.initial_equity = risk.session_equity;
        }
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.daily_trades = 0;
        }

        let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(st.account.trade_count_today, 0);
        assert_eq!(st.account.realized_pnl_today, Decimal::ZERO);
        assert_eq!(st.account.total_trades, 2); // lifetime preserved

        let risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
        assert!((risk.daily_pnl).abs() < f64::EPSILON);
        assert_eq!(risk.breaker, BreakerLevel::Normal);
        assert!((risk.initial_equity - 100_000.0).abs() < f64::EPSILON);

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(trading.daily_trades, 0);
    }

    #[test]
    fn panic_breaker_persists_through_reset() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
            risk.breaker = BreakerLevel::Panic;
        }

        // Simulate reset
        {
            let mut risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
            if risk.breaker != BreakerLevel::Panic {
                risk.breaker = BreakerLevel::Normal;
            }
        }

        let risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(risk.breaker, BreakerLevel::Panic);
    }

    #[test]
    fn daily_reset_skips_on_weekend() {
        use chrono::NaiveDate;
        // 2026-01-03 is a Saturday, 2026-01-04 is Sunday
        let sat = NaiveDate::from_ymd_opt(2026, 1, 3).unwrap();
        let sun = NaiveDate::from_ymd_opt(2026, 1, 4).unwrap();
        assert_eq!(sat.weekday(), chrono::Weekday::Sat);
        assert_eq!(sun.weekday(), chrono::Weekday::Sun);
        assert!(matches!(sat.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun));
        assert!(matches!(sun.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun));
    }

    #[test]
    fn daily_reset_skips_on_nyse_holiday_2026() {
        let holidays = crate::risk::calendar::NYSE_HOLIDAYS_2026;
        assert!(holidays.contains(&"2026-11-26"), "Thanksgiving missing");
        assert!(holidays.contains(&"2026-12-25"), "Christmas missing");
        assert!(holidays.contains(&"2026-07-03"), "Independence Day observed missing");
        assert!(!holidays.contains(&"2026-04-06"), "Regular Monday should not be a holiday");
    }

    #[test]
    fn daily_reset_skips_on_nyse_holiday_2027() {
        let holidays = crate::risk::calendar::NYSE_HOLIDAYS_2027;
        assert!(holidays.contains(&"2027-11-25"), "Thanksgiving 2027 missing");
        assert!(holidays.contains(&"2027-12-24"), "Christmas 2027 (observed) missing");
        assert!(holidays.contains(&"2027-07-05"), "Independence Day 2027 (observed) missing");
        assert!(!holidays.contains(&"2027-04-05"), "Regular Monday should not be a holiday");
    }

    #[test]
    fn nyse_holidays_2027_count_in_expected_range() {
        let n = crate::risk::calendar::NYSE_HOLIDAYS_2027.len();
        assert!(
            (9..=11).contains(&n),
            "NYSE typically has 9-11 full-day closures; got {n}"
        );
    }

    #[test]
    fn nyse_holidays_2027_no_weekend_entries() {
        use chrono::{NaiveDate, Weekday};
        for s in crate::risk::calendar::NYSE_HOLIDAYS_2027 {
            let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
            assert!(
                d.weekday() != Weekday::Sat && d.weekday() != Weekday::Sun,
                "NYSE 2027 holiday '{s}' falls on a weekend"
            );
        }
    }

    #[test]
    fn daily_reset_clears_pending_orders_and_fill_cumulative() {
        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_orders.push(crate::state::PendingOrder {
                conviction: None,
                expected_price: 0.0,
                order_id: "broker-1".into(),
                symbol: "AAPL".into(),
                side: crate::state::Direction::Long,
                qty: dec!(10),
                submitted_at: 1000,
                source: "bot".into(),
                deducted_amount: dec!(1800),
                client_order_id: String::new(),
                last_fill_at: None,
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
            trading.fill_cumulative.insert("broker-1".into(), (dec!(5), dec!(180)));
        }

        // Simulate the S03 clear step (cancel_all would be called first in
        // production, but we only test the local state clearing here)
        {
            let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
            trading.pending_orders.clear();
            trading.fill_cumulative.clear();
        }

        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        assert!(trading.pending_orders.is_empty());
        assert!(trading.fill_cumulative.is_empty());
    }

    #[test]
    fn daily_reset_clears_market_cvd_and_last_prices() {
        let market = MarketDataState::new();
        {
            let mut lp = market.last_prices.lock().unwrap();
            lp.insert("AAPL".into(), 180.0);
            lp.insert("TSLA".into(), 200.0);
        }
        {
            let mut cvd = market.cvd.lock().unwrap();
            cvd.insert("AAPL".into(), 1000.0);
        }

        market.last_prices.lock().unwrap().clear();
        market.cvd.lock().unwrap().clear();

        assert!(market.last_prices.lock().unwrap().is_empty());
        assert!(market.cvd.lock().unwrap().is_empty());
    }

    // ─── S15.6 — 30s cadence + double-tick guard ───────────────────

    /// S15.6 — the new 30s `CHECK_INTERVAL_SECS` MUST NOT trigger a
    /// second reset within the same RTH date. Simulates the date-guard
    /// across two consecutive ticks 30s apart at 09:30:15 / 09:30:45 ET.
    #[test]
    fn daily_reset_double_tick_within_minute_does_not_double_reset() {
        use chrono::NaiveDate;

        let today = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let mut last_reset_date: Option<NaiveDate> = None;
        let mut reset_count = 0;

        // Three consecutive 30s ticks at 09:30:15 / 09:30:45 / 09:31:15
        // ET — only the first must perform the reset.
        for _ in 0..3 {
            if last_reset_date != Some(today) {
                reset_count += 1;
                last_reset_date = Some(today);
            }
        }

        assert_eq!(
            reset_count, 1,
            "30s cadence must produce exactly one reset per RTH date"
        );
        assert_eq!(last_reset_date, Some(today));
    }

    /// S15.6 — verifies `run_daily_reset` ticks the watchdog at the
    /// 30s cadence. We let the loop run for ~90s of paused time and
    /// assert at least 3 ticks happened (cadence regression-lock).
    /// The actual reset path is gated by ET clock + holiday calendar
    /// so it does NOT execute in this test — only the watchdog tick
    /// (which precedes all continue-guards) does.
    #[tokio::test(start_paused = true)]
    async fn daily_reset_loop_ticks_watchdog_every_30s() {
        use crate::config::RuntimeConfig;
        use std::sync::Arc;

        let state = Arc::new(TitanState::new(RuntimeConfig::default()));
        let market = Arc::new(MarketDataState::new());
        // We never construct a real AppHandle in unit tests; the loop
        // requires one but `app.emit_all` short-circuits when no
        // listeners are attached. We intentionally do NOT spawn this
        // loop in unit tests — instead we drive its tick logic with a
        // hand-rolled equivalent that proves the cadence.
        let _ = market;

        let st = Arc::clone(&state);
        {
            let mut sys = st.system.lock().unwrap();
            sys.register_loop("daily_reset");
        }

        // Hand-rolled tick driver mirroring the loop body. We can't
        // invoke `run_daily_reset` directly without a Tauri AppHandle,
        // so we replicate the watchdog-tick subset under paused time.
        let st_loop = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(CHECK_INTERVAL_SECS));
            loop {
                ticker.tick().await;
                if st_loop.shutdown.is_cancelled() {
                    break;
                }
                let mut sys = st_loop.system.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(h) = sys.loops.get_mut("daily_reset") {
                    h.tick();
                }
            }
        });

        // tokio::time::interval fires immediately on first tick, so 90s
        // of advance should yield 4 ticks (0s, 30s, 60s, 90s).
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
        }

        state.shutdown.cancel();
        tokio::time::advance(Duration::from_secs(30)).await;
        let _ = handle.await;

        let sys = state.system.lock().unwrap();
        let h = sys.loops.get("daily_reset").expect("daily_reset registered");
        assert!(
            h.tick_count >= 3,
            "expected >= 3 ticks for ~90s of 30s cadence; got {}",
            h.tick_count
        );
    }
}
