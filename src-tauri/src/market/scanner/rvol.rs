//! S12 — Intraday RVOL with time-of-day volume modeling.
//!
//! Replaces the naive `volume_today / prev_volume` with a session-progress-
//! aware projection so that early-session metrics (e.g. premarket TSLA at
//! 8:00 AM with vol 1.2M, prev_vol 50M) yield a realistic RVOL ~0.4
//! instead of a misleading 0.024 (which would suppress every early-day
//! mover).
//!
//! ## Public surface
//!
//! - [`RvolMethod`] — enum exposed via [`crate::market::scanner::ScanParams`].
//!   `TimeAdjusted` is the default; `Naive` keeps the legacy formula for
//!   opt-in / regression studies; `Historical` is reserved for S12.5
//!   (MinuteVolumeCache) and currently maps to `TimeAdjusted` internally
//!   so the public enum is forward-compatible.
//!
//! ## Module-private surface
//!
//! - [`compute_session_progress`] — 5-segment piecewise-linear projection
//!   `[0.0, 1.0]`. Pre-04:00 / weekends → 0.0; 04:00→09:30 linear 0→0.08;
//!   09:30→16:00 linear 0.08→0.92; 16:00→20:00 linear 0.92→1.0; ≥20:00
//!   → 1.0.
//! - [`time_adjusted_rvol`] — `vol_today / (prev_volume * progress)` with
//!   neutral `1.0` fallback for IPO / weekend / NaN / progress≈0 inputs.
//!
//! ## DST
//!
//! Boundaries are wallclock ET via `chrono_tz::US::Eastern`; DST handling
//! is delegated to chrono_tz. NYSE never transitions during a trading
//! session (DST switches at 02:00 Sunday) so no special-case is needed.

use chrono::{DateTime, Datelike, Timelike, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// RVOL calculation method, persisted on `ScanParams`.
///
/// `#[serde(default)]` on `ScanParams` makes legacy `settings.json`
/// without this field deserialise to [`RvolMethod::TimeAdjusted`] —
/// no migration required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RvolMethod {
    /// Legacy formula: `volume_today / prev_volume`. Suppresses
    /// early-session movers; retained for regression study only.
    Naive,
    /// S12 default: `volume_today / (prev_volume * session_progress)`.
    /// Projects partial-day volume against fraction-of-day expected.
    #[default]
    TimeAdjusted,
    /// API-defined for forward compatibility. S12 falls through to
    /// `TimeAdjusted`; a future session (S12.5) will route this variant
    /// to a `MinuteVolumeCache` lookup with `TimeAdjusted` as fallback
    /// on cache miss.
    Historical,
}

const PRE_OPEN_MIN: f64 = 4.0 * 60.0;
const REG_OPEN_MIN: f64 = 9.0 * 60.0 + 30.0;
const REG_CLOSE_MIN: f64 = 16.0 * 60.0;
const EXT_CLOSE_MIN: f64 = 20.0 * 60.0;
const PROGRESS_AT_REG_OPEN: f64 = 0.08;
const PROGRESS_AT_REG_CLOSE: f64 = 0.92;

/// Fraction of the trading day that has elapsed in `now_et`'s wallclock,
/// returned in `[0.0, 1.0]`. Piecewise-linear over premarket, RTH and
/// extended hours; weekends and pre-04:00 ET clamp to 0.0; post-20:00
/// ET clamps to 1.0.
pub(super) fn compute_session_progress(now_et: DateTime<Tz>) -> f64 {
    let wd = now_et.weekday();
    if wd == Weekday::Sat || wd == Weekday::Sun {
        return 0.0;
    }

    let t = now_et.time();
    let mins = t.hour() as f64 * 60.0 + t.minute() as f64 + t.second() as f64 / 60.0;

    if mins < PRE_OPEN_MIN {
        return 0.0;
    }
    if mins < REG_OPEN_MIN {
        let frac = (mins - PRE_OPEN_MIN) / (REG_OPEN_MIN - PRE_OPEN_MIN);
        return frac * PROGRESS_AT_REG_OPEN;
    }
    if mins < REG_CLOSE_MIN {
        let frac = (mins - REG_OPEN_MIN) / (REG_CLOSE_MIN - REG_OPEN_MIN);
        return PROGRESS_AT_REG_OPEN + frac * (PROGRESS_AT_REG_CLOSE - PROGRESS_AT_REG_OPEN);
    }
    if mins < EXT_CLOSE_MIN {
        let frac = (mins - REG_CLOSE_MIN) / (EXT_CLOSE_MIN - REG_CLOSE_MIN);
        return PROGRESS_AT_REG_CLOSE + frac * (1.0 - PROGRESS_AT_REG_CLOSE);
    }
    1.0
}

/// Time-adjusted relative volume.
///
/// Returns `vol_today / (prev_volume * compute_session_progress(now_et))`
/// when the inputs are well-defined, otherwise the neutral value `1.0`.
/// Defensive guards collapse to `1.0` for: non-finite or non-positive
/// `prev_volume` (IPO, brand-new listing), weekend / pre-premarket
/// `progress ≈ 0`, and any divide-by-zero / NaN edge case.
pub(crate) fn time_adjusted_rvol(
    vol_today: f64,
    prev_volume: f64,
    now_et: DateTime<Tz>,
) -> f64 {
    if !prev_volume.is_finite() || prev_volume <= 0.0 {
        return 1.0;
    }
    let progress = compute_session_progress(now_et);
    if progress <= 0.001 {
        return 1.0;
    }
    let expected = prev_volume * progress;
    if !expected.is_finite() || expected <= 0.0 {
        return 1.0;
    }
    vol_today / expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn et(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(y, m, d, h, mi, 0)
            .unwrap()
    }

    // ─── compute_session_progress ──────────────────────────────────

    #[test]
    fn session_progress_premarket_8am() {
        let p = compute_session_progress(et(2026, 4, 29, 8, 0));
        assert!(
            (p - 0.0582).abs() < 0.005,
            "8:00 AM ET premarket progress = {p}, want ~0.058"
        );
    }

    #[test]
    fn session_progress_market_open() {
        let p = compute_session_progress(et(2026, 4, 29, 9, 30));
        assert!(
            (p - 0.08).abs() < 1e-9,
            "9:30 ET regular open boundary = {p}, want exactly 0.08"
        );
    }

    /// Lunch test target was a typo (0.270) in the original prompt; the
    /// linear spec yields 0.08 + (150/390)·0.84 = 0.4031 at 12:00 noon.
    #[test]
    fn session_progress_lunch() {
        let p = compute_session_progress(et(2026, 4, 29, 12, 0));
        assert!(
            (p - 0.4031).abs() < 0.005,
            "12:00 PM ET lunch progress = {p}, want ~0.403 (linear spec)"
        );
    }

    #[test]
    fn session_progress_close() {
        let p = compute_session_progress(et(2026, 4, 29, 16, 0));
        assert!(
            (p - 0.92).abs() < 1e-9,
            "16:00 ET close boundary = {p}, want exactly 0.92"
        );
    }

    #[test]
    fn session_progress_extended_end() {
        let p = compute_session_progress(et(2026, 4, 29, 20, 0));
        assert!(
            (p - 1.0).abs() < 1e-9,
            "20:00 ET extended close = {p}, want exactly 1.0"
        );
    }

    #[test]
    fn session_progress_weekend() {
        // Saturday 2026-05-02 12:00 ET → 0.0 regardless of clock.
        let p = compute_session_progress(et(2026, 5, 2, 12, 0));
        assert!(p.abs() < 1e-9, "Saturday progress = {p}, want exactly 0.0");
    }

    #[test]
    fn session_progress_pre_premarket() {
        let p = compute_session_progress(et(2026, 4, 29, 3, 0));
        assert!(p.abs() < 1e-9, "03:00 AM ET progress = {p}, want exactly 0.0");
    }

    // ─── time_adjusted_rvol ────────────────────────────────────────

    #[test]
    fn rvol_premarket_hot_momentum() {
        // 20M intraday at 8:00 AM vs 50M previous-day total.
        // expected = 50M * 0.0582 = 2.91M  →  rvol = 20M / 2.91M = 6.87
        // (Naive would yield 20/50 = 0.4 and fail typical rvol_min=2.0.)
        let r = time_adjusted_rvol(20_000_000.0, 50_000_000.0, et(2026, 4, 29, 8, 0));
        assert!(r > 5.0, "premarket hot momentum rvol = {r}, want > 5.0");
    }

    #[test]
    fn rvol_premarket_quiet() {
        // 1.2M intraday at 8:00 AM vs 50M previous-day total.
        // expected = 2.91M  →  rvol = 0.412  (correctly low; quiet AM).
        let r = time_adjusted_rvol(1_200_000.0, 50_000_000.0, et(2026, 4, 29, 8, 0));
        assert!(r < 1.0, "premarket quiet rvol = {r}, want < 1.0");
    }

    #[test]
    fn rvol_close_parity() {
        // At 16:00 ET (progress = 0.92), TimeAdjusted converges to ≈ Naive.
        // vol=10M, prev=10M  →  rvol = 10 / (10 * 0.92) = 1.087  (within ±15%).
        let r = time_adjusted_rvol(10_000_000.0, 10_000_000.0, et(2026, 4, 29, 16, 0));
        assert!(
            (0.95..=1.15).contains(&r),
            "close parity rvol = {r}, want in [0.95, 1.15]"
        );
    }

    #[test]
    fn rvol_zero_prev_volume_ipo() {
        // Brand-new IPO: prev_volume = 0  →  neutral 1.0, no panic, no NaN.
        let r = time_adjusted_rvol(5_000_000.0, 0.0, et(2026, 4, 29, 10, 0));
        assert!(
            (r - 1.0).abs() < 1e-9 && r.is_finite(),
            "IPO (prev=0) rvol = {r}, want exactly 1.0"
        );
    }

    #[test]
    fn rvol_weekend_returns_one() {
        // Saturday: progress = 0.0  →  guard returns neutral 1.0.
        let r = time_adjusted_rvol(1_000_000.0, 10_000_000.0, et(2026, 5, 2, 12, 0));
        assert!(
            (r - 1.0).abs() < 1e-9,
            "weekend rvol = {r}, want exactly 1.0"
        );
    }

    // ─── enum sanity ───────────────────────────────────────────────

    #[test]
    fn rvol_method_default_is_time_adjusted() {
        assert_eq!(RvolMethod::default(), RvolMethod::TimeAdjusted);
    }

    #[test]
    fn rvol_method_serde_roundtrip() {
        for m in [RvolMethod::Naive, RvolMethod::TimeAdjusted, RvolMethod::Historical] {
            let json = serde_json::to_string(&m).unwrap();
            let back: RvolMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }
}
