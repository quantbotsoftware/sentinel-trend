//! Sleeve T daily simulator — pre-registration §Regula T.
//!
//! Weekly rebalance at the close of the last session of the week (MOC
//! proxy), 10 % relative band, whole shares on a fixed $ base, costs
//! $0.01/share each way + SEC §31 and FINRA TAF on sells. P/L is marked
//! close-to-close in $, non-compounded (the same unit as the A+B curve).

use chrono::NaiveDate;

use crate::daily::Panel;
use crate::signal::{target_weights, SignalParams};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimParams {
    pub signal: SignalParams,
    /// fixed sizing base (A+B use $100 000)
    pub capital: f64,
    /// relative rebalance band: trade when |w − h| > band × w
    pub band: f64,
    pub slip_per_share: f64,
    /// SEC §31 fee per $ sold ($27.80 per $1M ⇒ 27.8e-6)
    pub sec_fee_per_dollar: f64,
    /// FINRA TAF per share sold, capped per order
    pub taf_per_share: f64,
    pub taf_max_per_order: f64,
}

impl Default for SimParams {
    fn default() -> Self {
        Self {
            signal: SignalParams::default(),
            capital: 100_000.0,
            band: 0.10,
            slip_per_share: 0.01,
            sec_fee_per_dollar: 27.8e-6,
            taf_per_share: 0.000_166,
            taf_max_per_order: 8.30,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub date: NaiveDate,
    pub symbol: usize,
    pub delta_shares: i64,
    pub price: f64,
    pub cost: f64,
}

/// Daily series from `start` onwards (all aligned with `dates`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TResult {
    pub dates: Vec<NaiveDate>,
    /// net $ P/L per session (mark-to-market − costs of the day)
    pub pnl: Vec<f64>,
    pub gross_pnl: Vec<f64>,
    pub costs: Vec<f64>,
    /// Σ |shares × close| / capital after the session's trades
    pub gross_exposure: Vec<f64>,
    pub n_active: Vec<usize>,
    pub trades: Vec<Trade>,
    /// weights held after the last session (diagnostic)
    pub final_weights: Vec<f64>,
}

/// Cost of one fill: slippage both ways, SEC fee and TAF on sells only.
pub fn fill_cost(delta_shares: i64, price: f64, p: &SimParams) -> f64 {
    let n = delta_shares.unsigned_abs() as f64;
    let mut c = n * p.slip_per_share;
    if delta_shares < 0 {
        c += n * price * p.sec_fee_per_dollar;
        c += (n * p.taf_per_share).min(p.taf_max_per_order);
    }
    c
}

/// Whole-share delta to move from held weight `h` to target `w` (truncating).
pub fn share_delta(w: f64, h: f64, capital: f64, price: f64) -> i64 {
    if price <= 0.0 {
        return 0;
    }
    ((w - h) * capital / price).trunc() as i64
}

/// Run the sleeve from session index `start` (first rebalance eligible there).
/// Sessions before `start` carry no position and are not reported.
pub fn run(panel: &Panel, p: &SimParams, start: usize) -> TResult {
    let n = panel.n_days();
    let k = panel.n_symbols();
    let week_end = panel.week_ends();
    let mut shares = vec![0i64; k];
    let mut out = TResult::default();
    for (t, &is_week_end) in week_end.iter().enumerate().take(n).skip(start) {
        // mark-to-market on the position carried into the session
        let mut mtm = 0.0;
        for (i, sh) in shares.iter().enumerate() {
            if *sh != 0 {
                if let (Some(prev), Some(now)) = (t.checked_sub(1).and_then(|u| panel.close(u, i)), panel.close(t, i)) {
                    mtm += *sh as f64 * (now - prev);
                }
            }
        }
        let mut cost = 0.0;
        if is_week_end {
            let w = target_weights(panel, t, &p.signal);
            for (i, sh) in shares.iter_mut().enumerate() {
                let Some(px) = panel.close(t, i) else { continue };
                let h = *sh as f64 * px / p.capital;
                let target = w.get(i).copied().unwrap_or(0.0);
                let delta = if target <= 0.0 {
                    -*sh
                } else if (target - h).abs() > p.band * target {
                    share_delta(target, h, p.capital, px)
                } else {
                    0
                };
                if delta != 0 {
                    let c = fill_cost(delta, px, p);
                    cost += c;
                    *sh += delta;
                    out.trades.push(Trade { date: panel.dates[t], symbol: i, delta_shares: delta, price: px, cost: c });
                }
            }
        }
        let gross: f64 = (0..k).map(|i| panel.close(t, i).map(|px| (shares[i] as f64 * px).abs()).unwrap_or(0.0)).sum::<f64>() / p.capital;
        out.dates.push(panel.dates[t]);
        out.gross_pnl.push(mtm);
        out.costs.push(cost);
        out.pnl.push(mtm - cost);
        out.gross_exposure.push(gross);
        out.n_active.push(shares.iter().filter(|s| **s != 0).count());
    }
    out.final_weights = (0..k).map(|i| panel.close(n - 1, i).map(|px| shares[i] as f64 * px / p.capital).unwrap_or(0.0)).collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daily::Series;
    use chrono::Datelike;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// Weekday calendar (Mon–Fri) of `n` sessions from 2020-01-06.
    fn calendar(n: usize) -> Vec<NaiveDate> {
        let mut out = Vec::new();
        let mut day = d("2020-01-06");
        while out.len() < n {
            if day.weekday().number_from_monday() <= 5 {
                out.push(day);
            }
            day += chrono::Duration::days(1);
        }
        out
    }

    fn panel_from(cols: &[(&str, Vec<f64>)]) -> Panel {
        let cal = calendar(cols[0].1.len());
        let series: Vec<Series> = cols.iter().map(|(s, c)| Series { symbol: s.to_string(), rows: cal.iter().copied().zip(c.iter().copied()).collect() }).collect();
        Panel::build(&cal, &series)
    }

    #[test]
    fn costs_apply_slippage_both_ways_and_fees_on_sells_only() {
        let p = SimParams::default();
        assert!((fill_cost(100, 50.0, &p) - 1.0).abs() < 1e-12);
        let sell = fill_cost(-100, 50.0, &p);
        // $1 slip + 5 000 $ × 27.8e-6 = $0.139 + 100 × 0.000166 = $0.0166
        assert!((sell - (1.0 + 0.139 + 0.0166)).abs() < 1e-9, "{sell}");
        // TAF cap: 100 000 shares sold ⇒ $16.6 uncapped, $8.30 capped
        let big = fill_cost(-100_000, 1.0, &p);
        assert!((big - (1000.0 + 100_000.0 * 27.8e-6 + 8.30)).abs() < 1e-9, "{big}");
    }

    #[test]
    fn share_delta_truncates_toward_zero() {
        assert_eq!(share_delta(0.25, 0.0, 100_000.0, 33.0), 757); // 25 000 / 33 = 757.57
        assert_eq!(share_delta(0.0, 0.25, 100_000.0, 33.0), -757);
        assert_eq!(share_delta(0.25, 0.0, 100_000.0, 0.0), 0);
    }

    /// Price path with `drift` per session and alternating ±`noise` (so the
    /// sample vol is positive — a zero-vol instrument is excluded by rule).
    fn trend(n: usize, start: f64, drift: f64, noise: f64) -> Vec<f64> {
        let mut px = vec![start];
        for k in 1..n {
            let z = if k % 2 == 0 { noise } else { -noise };
            px.push(px[k - 1] * (1.0 + drift + z));
        }
        px
    }

    #[test]
    fn one_instrument_up_trend_is_bought_on_friday_and_marked_daily() {
        let px = trend(300, 100.0, 0.0005, 0.005);
        let panel = panel_from(&[("UP", px.clone())]);
        let p = SimParams { signal: SignalParams { lookback: 252, ..SignalParams::default() }, ..SimParams::default() };
        let start = panel.first_full_history(252).unwrap();
        let r = run(&panel, &p, start);
        assert_eq!(r.dates.len(), 300 - start);
        // first trade happens on the first week-end at/after start, buys the 25 % cap
        let first = &r.trades[0];
        assert!(first.delta_shares > 0);
        let w = first.delta_shares as f64 * first.price / 100_000.0;
        assert!((w - 0.25).abs() < 0.01, "{w}");
        // the weight drifts with the price between Fridays; the 10 % band bounds it at 27.5 %
        assert!(r.gross_exposure.iter().all(|g| *g <= 0.25 * 1.10 + 1e-9), "{:?}", r.gross_exposure.iter().copied().fold(0.0, f64::max));
        // P/L: shares × Δclose − costs; sum of gross P/L equals the position's price change
        let bought_t = r.dates.iter().position(|x| *x == first.date).unwrap();
        let expected: f64 = first.delta_shares as f64 * (px[299] - px[start + bought_t]);
        assert!((r.gross_pnl.iter().sum::<f64>() - expected).abs() < 1e-6, "{} vs {expected}", r.gross_pnl.iter().sum::<f64>());
        assert!(r.costs.iter().sum::<f64>() > 0.0);
        // a single vol-stable trend inside the band ⇒ no further trades
        assert_eq!(r.trades.len(), 1, "{:?}", r.trades);
    }

    #[test]
    fn down_trend_stays_flat_and_reversal_exits_completely() {
        // rises for 300 sessions, then falls hard for 300: the position is closed on the
        // first Friday whose 252-session return is negative
        let mut px = trend(300, 100.0, 0.001, 0.004);
        let fall = trend(301, px[299], -0.005, 0.004);
        px.extend_from_slice(&fall[1..]);
        let panel = panel_from(&[("X", px)]);
        let p = SimParams::default();
        let r = run(&panel, &p, panel.first_full_history(252).unwrap());
        let sells: Vec<&Trade> = r.trades.iter().filter(|t| t.delta_shares < 0).collect();
        assert!(!sells.is_empty());
        assert_eq!(*r.n_active.last().unwrap(), 0, "flat at the end");
        assert!(r.final_weights.iter().all(|w| *w == 0.0));
        // exits are full: the sum of all deltas is zero
        assert_eq!(r.trades.iter().map(|t| t.delta_shares).sum::<i64>(), 0);
    }

    #[test]
    fn band_prevents_small_rebalances() {
        // two instruments, identical paths ⇒ identical weights; a 1 % drift in one
        // between Fridays stays inside the 10 % band
        let a = trend(280, 100.0, 0.0005, 0.004);
        let b: Vec<f64> = trend(280, 50.0, 0.0005, 0.004).iter().enumerate().map(|(k, p)| p * if k % 7 == 3 { 1.01 } else { 1.0 }).collect();
        let panel = panel_from(&[("A", a), ("B", b)]);
        let start = panel.first_full_history(252).unwrap();
        let r = run(&panel, &SimParams::default(), start);
        let per_symbol_b = r.trades.iter().filter(|t| t.symbol == 1).count();
        assert!(per_symbol_b <= 2, "band should suppress weekly churn: {per_symbol_b} trades on B");
    }
}
