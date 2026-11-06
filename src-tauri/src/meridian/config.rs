//! MERIDIAN runtime configuration (plan §4.5). Persisted in
//! `settings.json` under `"meridian"`. Defaults reproduce the validated
//! A+B configuration (`docs/meridian/validation/2026-09-03_x0_spy.md`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MeridianConfig {
    /// Run the shadow driver (history sync + replay + intents). Alerts and
    /// the twin log only — no orders unless `execute` is also true.
    pub enabled: bool,
    /// Phase 4b: send orders for intents. Requires the paper-trading gate
    /// in `docs/MERIDIAN_PLAN.md` §4.9 to have passed. Default off.
    pub execute: bool,
    /// Sizing base for the engine (fixed; equity compounding is a phase-5
    /// decision).
    pub capital: f64,
    pub symbols: Vec<String>,
    /// Calendar days of 1-min history to keep synced (SMA200 needs ≥ 200
    /// sessions ≈ 290 calendar days; extra margin for holidays).
    pub history_days: u32,
    /// Sleeve A: noise-envelope lookback (sessions) and the base band
    /// multiplier the envelope is fitted with (1.0; members scale it).
    pub envelope_lookback: usize,
    pub envelope_mult: f64,
    /// Sleeve A ensemble: one member per band multiplier k, equal share of
    /// the A vol budget (plan §4.4 X0 decision of 2026-09-03 — PBO 64 %,
    /// re-measured 90 % on 2026-09-18: the k ranking is noise, so no k is
    /// *chosen*). Empty = single member at `envelope_mult`.
    pub band_mults: Vec<f64>,
    /// Sleeve A daily vol target (0.01 = 1 %/day — plan §3.3/§3.7; the paper's
    /// 2 %/4× ran 2026-09-16→18 and, under the time-synchronous overlay,
    /// tripped the −1.5 % daily budget ~21×/year, see ANALIZA_2026-09-18 §6).
    pub sigma_target_daily: f64,
    /// Sleeve B overnight vol target (0.006 = 0.6 %/night).
    pub sigma_target_night: f64,
    /// Sleeve B IBS cut-off above which no carry is taken.
    pub ibs_cut: f64,
    pub max_leverage: f64,
    pub allow_short: bool,
    /// Risk overlay (daily/weekly/monthly loss budgets, gross caps) —
    /// part of the validated configuration.
    pub risk_overlay: bool,
    /// Sleeve B decides this many bars before the close (11 = 15:48 ET) so
    /// the entry can be a market-on-close order (Alpaca cutoff 15:50).
    pub b_decide_offset: usize,
    /// While MERIDIAN executes, the legacy SmartTrend bot is pinned to
    /// MANUAL: the snapshot restore and the UI toggle both refuse AUTO.
    /// 2026-09-16 evidence: mode drifted back to Auto and the legacy bot
    /// bought NOK premarket and queued IONQ/AGQ entries next to MERIDIAN.
    pub lock_legacy_manual: bool,
    /// Hard sanity cap on any single order (shares). Larger plans are
    /// refused and logged — protects against a sizing bug, not a market view.
    pub max_order_shares: f64,
    /// Replay cadence during the session, seconds.
    pub poll_secs: u64,
    /// Assumed per-share slippage in the shadow book (twin vs. sim).
    pub slippage_per_share: f64,
    /// Send auction orders (MOC for the close, OPG for B's exit). `None` =
    /// live only: paper Alpaca fills `cls`/`opg` at the NBBO with random
    /// partial fills and expires the rest (3/3 MOC, 2/2 OPG failed
    /// 2026-09-21→24 — ANALIZA chat 7), so paper emulates the auctions with
    /// day market orders (`exec.rs`). `Some(false)` on live if Alpaca does
    /// not enable MOC/OPG on the account (forum: "elite smart router").
    pub auction_orders: Option<bool>,
}

impl MeridianConfig {
    /// Whether the planner may use auction orders for this broker mode.
    pub fn use_auction(&self, paper: bool) -> bool {
        self.auction_orders.unwrap_or(!paper)
    }
}

impl Default for MeridianConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            execute: false,
            capital: 100_000.0,
            symbols: vec!["SPY".into(), "QQQ".into()],
            history_days: 330,
            envelope_lookback: 14,
            envelope_mult: 1.0,
            band_mults: vec![1.0, 1.25, 1.5],
            sigma_target_daily: 0.01,
            sigma_target_night: 0.006,
            ibs_cut: 0.7,
            max_leverage: 2.0,
            allow_short: true,
            risk_overlay: true,
            b_decide_offset: 11,
            lock_legacy_manual: true,
            max_order_shares: 3000.0,
            poll_secs: 60,
            slippage_per_share: 0.01,
            auction_orders: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_validated_run_and_execution_is_off() {
        let c = MeridianConfig::default();
        assert!(c.enabled && !c.execute);
        assert_eq!(c.symbols, vec!["SPY", "QQQ"]);
        assert_eq!((c.envelope_lookback, c.envelope_mult), (14, 1.0));
        assert_eq!(c.band_mults, vec![1.0, 1.25, 1.5], "X0: ensemble over k, none chosen");
        assert_eq!((c.sigma_target_daily, c.sigma_target_night, c.ibs_cut, c.max_leverage), (0.01, 0.006, 0.7, 2.0), "plan §3.3/§3.7 sizing");
        assert!(c.risk_overlay && c.b_decide_offset == 11, "operational config: overlay on, B decided at 15:48 for MOC");
        assert!(c.use_auction(false) && !c.use_auction(true), "auction orders on live only unless overridden");
        let forced = MeridianConfig { auction_orders: Some(false), ..c };
        assert!(!forced.use_auction(false), "live without MOC/OPG support falls back to the market emulation");
    }

    #[test]
    fn legacy_settings_without_section_deserialize_to_defaults() {
        let c: MeridianConfig = serde_json::from_str("{}").unwrap_or_default();
        assert_eq!(c, MeridianConfig::default());
        let c: MeridianConfig = serde_json::from_str(r#"{"execute": true, "capital": 50000}"#).unwrap_or_default();
        assert!(c.execute);
        assert_eq!(c.capital, 50_000.0);
    }
}
