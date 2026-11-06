use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use super::sessions::{compute_is_stale, is_current_session_trade};
use super::types::{ScanParams, ScoreInput, ScoreWeights, SnapshotData};

const GAP_SCANNER_PRESET: &str = "gap_scanner";
const GAP_SCANNER_MAX_GAP_PCT: f64 = 8.0;
const GAP_SCANNER_MAX_CHANGE_PCT: f64 = 16.0;
const GAP_SCANNER_MAX_FADE_PCT: f64 = 2.2;
const GAP_SCANNER_MAX_BODY_SPIKE_PCT: f64 = 12.0;
const GAP_SCANNER_MIN_CONTINUATION_BODY_PCT: f64 = 0.20;
const GAP_SCANNER_MIN_PULLBACK_BODY_PCT: f64 = 0.90;
const GAP_SCANNER_PULLBACK_GAP_CEILING_PCT: f64 = 1.8;
const GAP_SCANNER_NOISE_BODY_BAND_PCT: f64 = 0.15;
const GAP_SCANNER_RVOL_NOISE_BUFFER: f64 = 0.75;

pub fn score_weights_for_preset(preset: &str) -> (f64, f64, f64, f64) {
    match preset {
        "momentum" => (50.0, 20.0, 10.0, 20.0),
        "volume_hunter" => (15.0, 20.0, 10.0, 55.0),
        "gap_scanner" => (10.0, 15.0, 55.0, 20.0),
        _ => (25.0, 25.0, 25.0, 25.0), // balanced
    }
}

pub(super) fn is_gap_scanner_profile(params: &ScanParams) -> bool {
    params.score_preset == GAP_SCANNER_PRESET
}

pub(super) fn gap_scanner_alignment_multiplier(
    params: &ScanParams,
    close: f64,
    open_price: f64,
    prev_close: f64,
    change_pct: f64,
    gap_pct: f64,
    rvol: f64,
) -> Option<f64> {
    if !close.is_finite() || !open_price.is_finite() || !prev_close.is_finite() {
        return None;
    }
    if close <= 0.0 || open_price <= 0.0 || prev_close <= 0.0 {
        return None;
    }

    let body_pct = ((close - open_price) / open_price) * 100.0;
    let fade_pct = (gap_pct - change_pct).max(0.0);
    if gap_pct > GAP_SCANNER_MAX_GAP_PCT
        || change_pct > GAP_SCANNER_MAX_CHANGE_PCT
        || body_pct > GAP_SCANNER_MAX_BODY_SPIKE_PCT
        || fade_pct > GAP_SCANNER_MAX_FADE_PCT
    {
        return None;
    }

    let doji_noise = body_pct.abs() < GAP_SCANNER_NOISE_BODY_BAND_PCT
        && rvol < (params.rvol_min + GAP_SCANNER_RVOL_NOISE_BUFFER);
    if doji_noise {
        return None;
    }

    let min_gap = params.gap_min.max(0.0);
    let continuation_setup = gap_pct >= min_gap
        && gap_pct <= 6.0
        && body_pct >= GAP_SCANNER_MIN_CONTINUATION_BODY_PCT
        && close >= prev_close
        && change_pct >= params.change_min;
    let pullback_setup = gap_pct >= min_gap
        && gap_pct <= GAP_SCANNER_PULLBACK_GAP_CEILING_PCT
        && body_pct >= GAP_SCANNER_MIN_PULLBACK_BODY_PCT
        && change_pct >= params.change_min;
    if !(continuation_setup || pullback_setup) {
        return None;
    }

    let trend_component = (change_pct.min(10.0) / 10.0).clamp(0.0, 1.0);
    let reclaim_component = (body_pct.min(5.0) / 5.0).clamp(0.0, 1.0);
    let sweet_spot_component = ((2.5 - (gap_pct - 1.8).abs()) / 2.5).clamp(0.0, 1.0);
    let quality = (0.7 + (trend_component * 0.35 + reclaim_component * 0.45 + sweet_spot_component * 0.20) * 0.5)
        .clamp(0.6, 1.2);
    Some(quality)
}

/// Weighted composite score — EXACT port from Titan HFT scanner.rs
pub fn compute_candidate_score(input: &ScoreInput, weights: &ScoreWeights) -> f64 {
    let ScoreInput { change_pct, rvol, gap_pct, volume, prev_volume } = *input;
    let ScoreWeights { w_change, w_rvol, w_gap, w_volume } = *weights;
    let change_score = (change_pct.abs().min(20.0) / 20.0) * w_change;
    let rvol_score = if rvol > 1.0 {
        ((rvol - 1.0).min(10.0) / 10.0) * w_rvol
    } else {
        0.0
    };
    let gap_score = (gap_pct.abs().min(10.0) / 10.0) * w_gap;
    let vol_score = if volume > 5_000_000.0 {
        w_volume
    } else if volume > 2_000_000.0 {
        w_volume * 0.72
    } else if volume > 2.0 * prev_volume && prev_volume > 100_000.0 {
        w_volume * 0.48
    } else {
        (volume / 5_000_000.0).min(1.0) * w_volume * 0.4
    };
    change_score + rvol_score + gap_score + vol_score
}

/// S11: structured output of the snapshot fallback chain.
///
/// `current_price` priority — `latestTrade.p` → `minuteBar.c` → `dailyBar.c` → 0.
/// `volume_today` priority  — `dailyBar.v` → `minuteBar.v` → 0.
/// `is_stale` is session-aware (S15.5): RTH > 60s, premarket/extended
/// > 300s, dead-time > 3600s. See [`super::sessions::compute_is_stale`].
pub(crate) struct BarMetrics {
    pub current_price: f64,
    pub open_price: f64,
    pub prev_close: f64,
    pub volume_today: f64,
    pub prev_volume: f64,
    pub change_pct: f64,
    pub bid: f64,
    pub ask: f64,
    pub spread_bps: f64,
    pub last_trade_age_seconds: u32,
    pub is_stale: bool,
    /// SC11: `latestTrade.t` is present and belongs to today's ET date.
    pub has_today_trade: bool,
}

/// Resolve a SnapshotData into the metrics consumed by the prefilter.
///
/// `now_et` is injected (not derived from `Utc::now()`) so the function
/// stays pure / testable and so the caller can compute a single
/// timestamp per scan batch.
pub(crate) fn extract_bar_metrics(snap: &SnapshotData, now_et: DateTime<Tz>) -> BarMetrics {
    let daily_c = snap.daily_bar.as_ref().map(|d| d.c);
    let daily_o = snap.daily_bar.as_ref().map(|d| d.o);
    let daily_v = snap.daily_bar.as_ref().map(|d| d.v);
    let minute_c = snap.minute_bar.as_ref().map(|m| m.c);
    let minute_o = snap.minute_bar.as_ref().map(|m| m.o);
    let minute_v = snap.minute_bar.as_ref().map(|m| m.v);
    let trade_p = snap.latest_trade.as_ref().map(|t| t.p);

    let current_price = trade_p.or(minute_c).or(daily_c).unwrap_or(0.0);
    let open_price = daily_o.or(minute_o).unwrap_or(current_price);
    let volume_today = daily_v.or(minute_v).unwrap_or(0.0);
    let prev_close = snap
        .prev_daily_bar
        .as_ref()
        .map(|p| p.c)
        .unwrap_or(open_price);
    let prev_volume = snap
        .prev_daily_bar
        .as_ref()
        .map(|p| p.v)
        .unwrap_or(volume_today);

    let change_pct = if prev_close > 0.0 {
        ((current_price - prev_close) / prev_close) * 100.0
    } else {
        0.0
    };

    let (bid, ask) = snap
        .latest_quote
        .as_ref()
        .map(|q| (q.bp, q.ap))
        .unwrap_or((0.0, 0.0));
    let spread_bps = if bid > 0.0 && ask > 0.0 && ask >= bid {
        ((ask - bid) / ((ask + bid) / 2.0)) * 10_000.0
    } else {
        0.0
    };

    let now_utc = now_et.with_timezone(&Utc);
    let last_trade_age_seconds = snap
        .latest_trade
        .as_ref()
        .and_then(|t| DateTime::parse_from_rfc3339(&t.t).ok())
        .map(|dt| {
            (now_utc - dt.with_timezone(&Utc))
                .num_seconds()
                .max(0)
                .min(u32::MAX as i64) as u32
        })
        .unwrap_or(u32::MAX);

    let is_stale = compute_is_stale(now_et, last_trade_age_seconds);
    let has_today_trade = snap
        .latest_trade
        .as_ref()
        .map(|t| is_current_session_trade(now_et, &t.t))
        .unwrap_or(false);

    BarMetrics {
        current_price,
        open_price,
        prev_close,
        volume_today,
        prev_volume,
        change_pct,
        bid,
        ask,
        spread_bps,
        last_trade_age_seconds,
        is_stale,
        has_today_trade,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::scanner::types::{LatestQuote, LatestTrade, MinuteBar};
    use crate::market::scanner::SnapshotDailyBar;
    use chrono::TimeZone;

    fn make_snapshot(close: f64, open: f64, vol: f64, prev_close: f64, prev_vol: f64) -> SnapshotData {
        SnapshotData {
            latest_trade: None,
            latest_quote: None,
            minute_bar: None,
            daily_bar: Some(SnapshotDailyBar { c: close, o: open, v: vol }),
            prev_daily_bar: Some(SnapshotDailyBar { c: prev_close, o: prev_close, v: prev_vol }),
        }
    }

    /// Wednesday 2026-04-29 10:30 ET — solidly inside RTH.
    fn rth_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 10, 30, 0)
            .unwrap()
    }

    /// Wednesday 2026-04-29 07:30 ET — premarket (outside RTH).
    fn premarket_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 7, 30, 0)
            .unwrap()
    }

    #[test]
    fn test_score_weights_balanced() {
        let (c, r, g, v) = score_weights_for_preset("balanced");
        assert!((c - 25.0).abs() < 0.01);
        assert!((r - 25.0).abs() < 0.01);
        assert!((g - 25.0).abs() < 0.01);
        assert!((v - 25.0).abs() < 0.01);
    }

    #[test]
    fn test_score_weights_momentum() {
        let (c, _r, _g, v) = score_weights_for_preset("momentum");
        assert!((c - 50.0).abs() < 0.01);
        assert!((v - 20.0).abs() < 0.01);
    }

    #[test]
    fn test_score_weights_volume_hunter() {
        let (_, _, _, v) = score_weights_for_preset("volume_hunter");
        assert!((v - 55.0).abs() < 0.01);
    }

    #[test]
    fn test_score_weights_gap_scanner() {
        let (_, _, g, _) = score_weights_for_preset("gap_scanner");
        assert!((g - 55.0).abs() < 0.01);
    }

    #[test]
    fn test_score_weights_unknown_defaults_balanced() {
        let w = score_weights_for_preset("nonexistent");
        assert_eq!(w, (25.0, 25.0, 25.0, 25.0));
    }

    fn balanced() -> ScoreWeights {
        ScoreWeights { w_change: 25.0, w_rvol: 25.0, w_gap: 25.0, w_volume: 25.0 }
    }

    #[test]
    fn test_compute_score_high_volume() {
        let inp = ScoreInput { change_pct: 10.0, rvol: 5.0, gap_pct: 3.0, volume: 6_000_000.0, prev_volume: 1_000_000.0 };
        let score = compute_candidate_score(&inp, &balanced());
        assert!(score > 0.0);
        assert!(score <= 100.0);
    }

    #[test]
    fn test_compute_score_low_rvol_zero() {
        let inp = ScoreInput { change_pct: 5.0, rvol: 0.5, gap_pct: 0.0, volume: 1_000_000.0, prev_volume: 1_000_000.0 };
        let score = compute_candidate_score(&inp, &balanced());
        assert!(score > 0.0);
    }

    /// PARITY: a snapshot containing only `dailyBar` + `prevDailyBar`
    /// MUST yield the same numerics as the pre-S11 tuple version.
    /// S15.5: with no `latest_trade`, age = u32::MAX, so premarket
    /// `is_stale` is now true under the 300s threshold (DGSN-style row
    /// dimming is the desired behaviour). Numerics are still parity.
    #[test]
    fn extract_falls_back_to_daily_bar_when_only_daily_present() {
        let snap = make_snapshot(110.0, 105.0, 1_000_000.0, 100.0, 800_000.0);
        let m = extract_bar_metrics(&snap, premarket_now());
        assert!((m.current_price - 110.0).abs() < 0.01);
        assert!((m.open_price - 105.0).abs() < 0.01);
        assert!((m.volume_today - 1_000_000.0).abs() < 0.01);
        assert!((m.prev_close - 100.0).abs() < 0.01);
        assert!((m.prev_volume - 800_000.0).abs() < 0.01);
        assert!((m.change_pct - 10.0).abs() < 0.01);
        assert!((m.bid - 0.0).abs() < f64::EPSILON);
        assert!((m.ask - 0.0).abs() < f64::EPSILON);
        assert_eq!(m.last_trade_age_seconds, u32::MAX);
        assert!(m.is_stale, "S15.5: premarket with MAX age is now stale (>300s)");
    }

    #[test]
    fn extract_bar_metrics_no_prev_uses_open_as_prev_close() {
        let snap = SnapshotData {
            latest_trade: None,
            latest_quote: None,
            minute_bar: None,
            daily_bar: Some(SnapshotDailyBar { c: 50.0, o: 48.0, v: 500_000.0 }),
            prev_daily_bar: None,
        };
        let m = extract_bar_metrics(&snap, premarket_now());
        assert!((m.prev_close - m.open_price).abs() < 0.01);
        assert!((m.prev_volume - m.volume_today).abs() < 0.01);
    }

    #[test]
    fn extract_uses_latest_trade_when_present() {
        let mut snap = make_snapshot(110.0, 105.0, 1_000_000.0, 100.0, 800_000.0);
        snap.latest_trade = Some(LatestTrade {
            p: 115.0,
            t: "2026-04-29T14:29:30Z".to_string(),
            s: Some(100),
        });
        let m = extract_bar_metrics(&snap, rth_now());
        assert!(
            (m.current_price - 115.0).abs() < 0.01,
            "current_price must come from latestTrade.p, not dailyBar.c"
        );
        // change_pct relative to prev_close=100 → +15%
        assert!((m.change_pct - 15.0).abs() < 0.01);
    }

    #[test]
    fn extract_falls_back_to_minute_bar_when_no_daily() {
        let snap = SnapshotData {
            latest_trade: None,
            latest_quote: None,
            minute_bar: Some(MinuteBar {
                o: 99.0, h: 101.0, l: 98.5, c: 100.0, v: 5_000.0,
                t: "2026-04-29T14:30:00Z".to_string(),
            }),
            daily_bar: None,
            prev_daily_bar: Some(SnapshotDailyBar { c: 95.0, o: 90.0, v: 1_000_000.0 }),
        };
        let m = extract_bar_metrics(&snap, rth_now());
        assert!((m.current_price - 100.0).abs() < 0.01);
        assert!((m.open_price - 99.0).abs() < 0.01);
        assert!((m.volume_today - 5_000.0).abs() < 0.01);
    }

    #[test]
    fn extract_is_stale_during_rth_when_trade_old() {
        let mut snap = make_snapshot(110.0, 105.0, 1_000_000.0, 100.0, 800_000.0);
        // Trade 5 min before now (RTH 10:30 ET = 14:30 UTC)
        snap.latest_trade = Some(LatestTrade {
            p: 110.0,
            t: "2026-04-29T14:25:00Z".to_string(),
            s: None,
        });
        let m = extract_bar_metrics(&snap, rth_now());
        assert!(m.last_trade_age_seconds >= 290 && m.last_trade_age_seconds <= 310);
        assert!(m.is_stale, "5-min-old trade in RTH must be flagged stale");
    }

    /// S15.5 — premarket trades fresher than 300s (5 min) are NOT stale.
    /// Replaces the pre-S15.5 `extract_not_stale_outside_rth_even_when_trade_old`
    /// test which asserted "premarket never stale" — that contract is
    /// dropped by SC3-extended (see `sessions.rs` for thresholds).
    #[test]
    fn extract_premarket_fresh_under_300s_not_stale() {
        let mut snap = make_snapshot(110.0, 105.0, 1_000_000.0, 100.0, 800_000.0);
        // 1 min before premarket_now (07:30 ET = 11:30 UTC) → 60s age.
        snap.latest_trade = Some(LatestTrade {
            p: 110.0,
            t: "2026-04-29T11:29:00Z".to_string(),
            s: None,
        });
        let m = extract_bar_metrics(&snap, premarket_now());
        assert!(m.last_trade_age_seconds <= 70);
        assert!(!m.is_stale, "1-min-old premarket trade is NOT stale (300s threshold)");
    }

    /// S15.5 — premarket trades older than 300s (5 min) ARE stale.
    /// DGSN-style: a row with a 13-hour-old quote should be dimmed.
    #[test]
    fn extract_premarket_old_over_300s_is_stale() {
        let mut snap = make_snapshot(110.0, 105.0, 1_000_000.0, 100.0, 800_000.0);
        // 30 min before premarket_now → 1800s age.
        snap.latest_trade = Some(LatestTrade {
            p: 110.0,
            t: "2026-04-29T11:00:00Z".to_string(),
            s: None,
        });
        let m = extract_bar_metrics(&snap, premarket_now());
        assert!(m.last_trade_age_seconds > 300);
        assert!(m.is_stale, "30-min-old premarket trade IS stale (S15.5)");
    }

    #[test]
    fn extract_spread_bps_around_10_for_99_95_100_05() {
        let mut snap = make_snapshot(100.0, 99.0, 1_000_000.0, 99.0, 800_000.0);
        snap.latest_quote = Some(LatestQuote {
            bp: 99.95,
            ap: 100.05,
            bs: 100,
            as_: 100,
            t: "2026-04-29T14:30:00Z".to_string(),
        });
        let m = extract_bar_metrics(&snap, rth_now());
        assert!((m.bid - 99.95).abs() < 0.001);
        assert!((m.ask - 100.05).abs() < 0.001);
        assert!(
            (m.spread_bps - 10.0).abs() < 0.5,
            "spread_bps={} should be ~10",
            m.spread_bps
        );
    }

    #[test]
    fn extract_spread_bps_zero_when_quote_missing() {
        let snap = make_snapshot(100.0, 99.0, 1_000_000.0, 99.0, 800_000.0);
        let m = extract_bar_metrics(&snap, rth_now());
        assert!((m.spread_bps - 0.0).abs() < f64::EPSILON);
    }

}
