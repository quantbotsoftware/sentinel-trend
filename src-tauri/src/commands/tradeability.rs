//! Tradeability verdict for the active symbol (2026-07-04).
//!
//! Manual-trading confidence layer: before an operator follows any
//! arrow on the chart, this command answers "are the *conditions* for
//! a clean fill even present right now?" — the same physical gates
//! APEX itself trades through (session, price floor, spread, RVOL,
//! halt, data freshness), evaluated independently of any signal.
//!
//! Data sources are Tier-B reads (`last_prices` B1 → `halts` B5 →
//! `watchlist_metrics` B6, acquired ascending, clone-and-drop). No
//! Tier-A guard is ever held simultaneously.

use std::sync::Arc;

use serde::Serialize;

use crate::market::market_state::MarketDataState;

/// APEX physical gates, mirrored (keep in sync with
/// `smarttrend::strategies::apex::Apex::default()`).
const MIN_PRICE_USD: f64 = 1.0;
const MAX_SPREAD_BPS: f64 = 100.0;
const MIN_RVOL: f64 = 1.0;
const APEX_WINDOW_START_MIN: u32 = 9 * 60;
const APEX_WINDOW_END_MIN: u32 = 16 * 60;

#[derive(Debug, Clone, Serialize)]
pub struct TradeabilityCheck {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TradeabilityPayload {
    pub symbol: String,
    /// Master verdict: all liquidity/safety checks pass AND RTH open.
    pub tradeable: bool,
    /// Stricter flag: we are inside APEX's own entry window — the
    /// conditions under which the house strategy itself would act.
    pub apex_window: bool,
    /// "rth" / "pre" / "ext" / "closed"
    pub session: &'static str,
    pub checks: Vec<TradeabilityCheck>,
}

fn session_now(et_min: u32, weekday_ok: bool) -> &'static str {
    if !weekday_ok {
        return "closed";
    }
    match et_min {
        m if (4 * 60..9 * 60 + 30).contains(&m) => "pre",
        m if (9 * 60 + 30..16 * 60).contains(&m) => "rth",
        m if (16 * 60..20 * 60).contains(&m) => "ext",
        _ => "closed",
    }
}

#[tauri::command]
pub fn get_tradeability(
    market: tauri::State<'_, Arc<MarketDataState>>,
    symbol: String,
) -> Result<TradeabilityPayload, String> {
    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err("Symbol cannot be empty".to_string());
    }

    use chrono::{Datelike, Timelike};
    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let et_min = now_et.hour() * 60 + now_et.minute();
    let weekday_ok = !matches!(now_et.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
    let session = session_now(et_min, weekday_ok);

    // Tier B reads, ascending, each guard dropped before the next.
    let live_price: Option<f64> = {
        let lp = market.last_prices.lock().unwrap_or_else(|p| p.into_inner());
        lp.get(&symbol).copied()
    };
    let halted = {
        let halts = market.halts.lock().unwrap_or_else(|p| p.into_inner());
        // F2: resumed rows stay in the map — only active halts count.
        halts.get(&symbol).map(|h| h.resumed_at.is_none()).unwrap_or(false)
    };
    let wm = {
        let wms = market
            .watchlist_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        wms.get(&symbol).cloned()
    };

    let price = live_price.or(wm.as_ref().map(|m| m.price)).unwrap_or(0.0);
    let spread_bps = wm.as_ref().map(|m| m.spread_bps);
    let rvol = wm.as_ref().map(|m| m.rvol);
    let is_stale = wm.as_ref().map(|m| m.is_stale).unwrap_or(true);

    let mut checks = Vec::with_capacity(6);
    let session_ok = session == "rth";
    checks.push(TradeabilityCheck {
        name: "session",
        passed: session_ok,
        detail: format!("{} (ET {:02}:{:02})", session, now_et.hour(), now_et.minute()),
    });
    let price_ok = price >= MIN_PRICE_USD;
    checks.push(TradeabilityCheck {
        name: "price",
        passed: price_ok,
        detail: if price > 0.0 {
            format!("${:.2} (min ${:.0})", price, MIN_PRICE_USD)
        } else {
            "no live price".to_string()
        },
    });
    let spread_ok = spread_bps.map(|s| s <= MAX_SPREAD_BPS).unwrap_or(false);
    checks.push(TradeabilityCheck {
        name: "spread",
        passed: spread_ok,
        detail: match spread_bps {
            Some(s) => format!("{:.0} bps (max {:.0})", s, MAX_SPREAD_BPS),
            None => "no quote data".to_string(),
        },
    });
    let rvol_ok = rvol.map(|r| r >= MIN_RVOL).unwrap_or(false);
    checks.push(TradeabilityCheck {
        name: "rvol",
        passed: rvol_ok,
        detail: match rvol {
            Some(r) => format!("{:.1}x (min {:.1}x)", r, MIN_RVOL),
            None => "no volume data".to_string(),
        },
    });
    checks.push(TradeabilityCheck {
        name: "halt",
        passed: !halted,
        detail: if halted { "HALTED".to_string() } else { "trading".to_string() },
    });
    checks.push(TradeabilityCheck {
        name: "data",
        passed: !is_stale,
        detail: if is_stale { "stale / no metrics".to_string() } else { "fresh".to_string() },
    });

    // RVOL is an APEX-quality gate, not a hard "you cannot click buy"
    // gate — a liquid mega-cap at rvol 1.0 is still executable. The
    // master verdict therefore excludes rvol; the APEX flag includes it.
    let tradeable = session_ok && price_ok && spread_ok && !halted && !is_stale;
    let apex_window = tradeable
        && rvol_ok
        && (APEX_WINDOW_START_MIN..=APEX_WINDOW_END_MIN).contains(&et_min);

    Ok(TradeabilityPayload {
        symbol,
        tradeable,
        apex_window,
        session,
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_labels() {
        assert_eq!(session_now(8 * 60, true), "pre");
        assert_eq!(session_now(10 * 60, true), "rth");
        assert_eq!(session_now(17 * 60, true), "ext");
        assert_eq!(session_now(2 * 60, true), "closed");
        assert_eq!(session_now(10 * 60, false), "closed");
    }

    #[test]
    fn apex_window_bounds_match_strategy_defaults() {
        let apex = crate::smarttrend::strategies::apex::Apex::default();
        assert_eq!(APEX_WINDOW_START_MIN, apex.entry_start_et_min);
        assert_eq!(APEX_WINDOW_END_MIN, apex.entry_end_et_min);
        assert_eq!(MIN_PRICE_USD, apex.min_price);
        assert_eq!(MAX_SPREAD_BPS, apex.max_spread_bps);
        assert_eq!(MIN_RVOL, apex.min_rvol);
    }
}
