//! D4 — causality mutation test for the live sleeves
//! (`docs/meridian/validation/2026-09-27_diagnostice_astra_preregistration.md`).
//!
//! The `Sleeve` contract says a sleeve MUST only read `today.bars[..=idx]`.
//! This module checks it the adversarial way: run a sleeve on a synthetic
//! day, then re-run it on a copy whose bars *after* a cut `K` are replaced
//! by unrelated random bars. Every target emitted at or before `K` must be
//! identical. For Sleeve B, whose decision is frozen `offset` bars before
//! the close, the bars inside the frozen window are mutated as well and
//! the closing-bar target must not move.
//!
//! Pure functions here; the `#[cfg(test)]` module drives them across many
//! days and seeds. Nothing in `titan-meridian` is touched.

use titan_meridian::bars::{Bar, DaySeries};
use titan_meridian::envelope::NoiseEnvelope;
use titan_meridian::sleeve::{DayCtx, Sleeve, Target};
use titan_meridian::vol::VolStats;

/// xorshift64* — deterministic, dependency-free.
#[derive(Debug, Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0.max(1);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in (−1, 1).
    pub fn sym(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// A random-walk session of `n` bars around `start`, 1-minute σ ≈ `vol`.
pub fn synthetic_day(rng: &mut Rng, date: chrono::NaiveDate, n: usize, start: f64, vol: f64) -> DaySeries {
    let mut p = start;
    let mut bars = Vec::with_capacity(n);
    for _ in 0..n {
        let o = p;
        let c = (p * (1.0 + vol * rng.sym())).max(1.0);
        let h = o.max(c) * (1.0 + vol.abs() * 0.3 * rng.sym().abs());
        let l = o.min(c) * (1.0 - vol.abs() * 0.3 * rng.sym().abs());
        bars.push(Bar { o, h, l, c, v: 1_000.0 + 500.0 * rng.sym().abs() });
        p = c;
    }
    DaySeries { date, bars, real_bars: n }
}

/// Copy of `day` with bars in `from..` replaced by fresh random bars that
/// continue from the last untouched close.
pub fn mutate_after(day: &DaySeries, from: usize, rng: &mut Rng, vol: f64) -> DaySeries {
    let mut out = day.clone();
    let n = out.bars.len();
    if from >= n {
        return out;
    }
    let start = if from == 0 { out.bars[0].o } else { out.bars[from - 1].c };
    let tail = synthetic_day(rng, day.date, n - from, start, vol * 3.0);
    out.bars.truncate(from);
    out.bars.extend(tail.bars);
    out
}

/// Run `sleeve` over `day` with the given context pieces; returns the
/// (idx, exposure) sequence.
pub fn run_targets(sleeve: &mut dyn Sleeve, day: &DaySeries, env: Option<&NoiseEnvelope>, vol: VolStats) -> Vec<(usize, f64)> {
    let ctx = DayCtx {
        date: day.date,
        symbol: "SPY",
        today: day,
        prev_close: day.open() * 0.999,
        envelope: env,
        vol,
        equity: 100_000.0,
        session_bars: day.len(),
        force_close: false,
    };
    sleeve.on_day_start(&ctx);
    let vw = titan_meridian::vwap::Vwap::series(&day.bars);
    (0..day.len()).filter_map(|i| sleeve.on_bar(&ctx, i, vw[i]).map(|t: Target| (i, t.exposure))).collect()
}

/// Targets emitted at or before `k`.
pub fn upto(seq: &[(usize, f64)], k: usize) -> Vec<(usize, f64)> {
    seq.iter().copied().filter(|(i, _)| *i <= k).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use titan_meridian::session::is_decision_bar;
    use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

    fn a_params() -> IntradayParams {
        IntradayParams { lookback_days: 14, band_mult: 1.0, sigma_target_daily: 0.01, max_leverage: 2.0, allow_short: true }
    }
    fn b_params() -> OvernightParams {
        OvernightParams { sigma_target_night: 0.006, decide_bars_before_close: 11, ..Default::default() }
    }
    fn vol() -> VolStats {
        VolStats { sigma_cc: Some(0.009), sigma_on: Some(0.004), atr: None, sma: Some(99.0), ibs_last: 0.5 }
    }

    /// A: for every decision bar K, mutating bars > K must not change any
    /// target emitted at or before K. 3 members × 200 days × 3 seeds.
    #[test]
    fn sleeve_a_targets_do_not_depend_on_future_bars() {
        let env = NoiseEnvelope { sigma: (0..390).map(|i| 0.002 + 0.00001 * i as f64).collect() };
        let mut checked = 0usize;
        for seed in [1u64, 2, 3] {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
            for d in 0..200u32 {
                let date = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(d as i64);
                // a tighter band than the walk, so entries actually happen
                let day = synthetic_day(&mut rng, date, 390, 100.0, 0.0015);
                for (m, member) in IntradayMomentum::ensemble(a_params(), &[1.0, 1.25, 1.5]).into_iter().enumerate() {
                    let mut base = member.clone();
                    let full = run_targets(&mut base, &day, Some(&env), vol());
                    for k in (0..390).filter(|&i| is_decision_bar(i, 390)) {
                        let mutated = mutate_after(&day, k + 1, &mut rng, 0.0015);
                        let mut s = member.clone();
                        let seq = run_targets(&mut s, &mutated, Some(&env), vol());
                        assert_eq!(
                            upto(&seq, k),
                            upto(&full, k),
                            "A member {m} seed {seed} day {d} cut {k}: targets up to {k} changed when future bars were mutated"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 20_000, "checked {checked}");
    }

    /// B decides at the close from bars[..390 − offset]. Mutating the last
    /// `offset` bars (and the close itself) must leave the closing target
    /// unchanged; mutating a bar *inside* the frozen window must be allowed
    /// to change it (sanity that the test can fail).
    #[test]
    fn sleeve_b_closing_target_ignores_bars_after_the_freeze() {
        let mut rng = Rng(42);
        let mut moved = 0usize;
        let mut total = 0usize;
        for d in 0..300u32 {
            let date = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(d as i64);
            let day = synthetic_day(&mut rng, date, 390, 100.0, 0.001);
            let mut b = OvernightCarry::new(b_params());
            let full = run_targets(&mut b, &day, None, vol());
            let close_target = full.iter().find(|(i, _)| *i == 389).map(|(_, e)| *e);
            // mutate strictly after the freeze (bars 379..390)
            let mutated = mutate_after(&day, 379, &mut rng, 0.02);
            let mut b2 = OvernightCarry::new(b_params());
            let seq = run_targets(&mut b2, &mutated, None, vol());
            let close2 = seq.iter().find(|(i, _)| *i == 389).map(|(_, e)| *e);
            assert_eq!(close_target, close2, "B day {d}: closing target changed when post-freeze bars were mutated");
            total += 1;
            // mutate inside the frozen window: the target is allowed to move
            let inside = mutate_after(&day, 300, &mut rng, 0.02);
            let mut b3 = OvernightCarry::new(b_params());
            let seq3 = run_targets(&mut b3, &inside, None, vol());
            if seq3.iter().find(|(i, _)| *i == 389).map(|(_, e)| *e) != close_target {
                moved += 1;
            }
        }
        assert!(moved > 0, "sanity: mutating inside the decision window never changed B on {total} days — test is vacuous");
    }
}
