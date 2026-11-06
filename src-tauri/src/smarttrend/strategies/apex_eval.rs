//! F4-APEX — full condition-vector evaluation (Frontier protocol).
//!
//! Sibling of `apex.rs` (split to respect the R2 line cap): this file
//! holds the single-source-of-truth gate chain as a second inherent
//! `impl Apex` block. `evaluate_entry_explained` in `apex.rs` simply
//! returns the `Result` half of this evaluation.

use crate::state::Direction;

use super::apex::{rolling_vwap, Apex};
use super::frontier::ConditionHit;
use super::{ctx_is_sane, pure_atr, EntrySignal, MarketContext, MarketRegime, StrategyId};

impl Apex {
    /// F4-APEX — full condition-vector evaluation (Frontier protocol).
    ///
    /// Evaluates EVERY gate (no early return past the structural sanity
    /// checks) so near-misses carry the same `[{name, passed, value,
    /// threshold}]` vector Frontier logs to `decisions.sqlite`, feeding
    /// the calibration engine. The `Result` half preserves the exact
    /// first-failure semantics (and reason strings) of the legacy
    /// early-return chain — gate order below IS the failure priority.
    ///
    /// An empty vector means "not applicable" (insufficient data /
    /// halted / no ATR), mirroring Frontier's `SetupEval` convention.
    pub fn evaluate_entry_full(
        &self,
        ctx: &MarketContext,
    ) -> (Vec<ConditionHit>, Result<EntrySignal, &'static str>) {
        if !ctx_is_sane(ctx, 21) {
            return (vec![], Err("insufficient data (need 21 closed candles)"));
        }
        if ctx.is_halted {
            return (vec![], Err("symbol halted"));
        }
        let min_rvol = ctx.get_param(StrategyId::Apex, "min_rvol", self.min_rvol);
        let evidence_rvol = ctx.get_param(StrategyId::Apex, "evidence_rvol", self.evidence_rvol);
        let min_catalyst = ctx.get_param(
            StrategyId::Apex,
            "min_catalyst_score",
            self.min_catalyst_score,
        );
        let ignition_range_atr = ctx.get_param(
            StrategyId::Apex,
            "ignition_range_atr",
            self.ignition_range_atr,
        );
        let ignition_close_pos = ctx.get_param(
            StrategyId::Apex,
            "ignition_close_pos",
            self.ignition_close_pos,
        );
        let ignition_vol_mult = ctx.get_param(
            StrategyId::Apex,
            "ignition_vol_mult",
            self.ignition_vol_mult,
        );
        let max_extension_atr = ctx.get_param(
            StrategyId::Apex,
            "max_extension_atr",
            self.max_extension_atr,
        );
        let target_r = ctx.get_param(
            StrategyId::Apex,
            "target_r_multiple",
            self.target_r_multiple,
        );
        let max_hold = ctx.get_param(
            StrategyId::Apex,
            "max_hold_minutes",
            self.max_hold_minutes as f64,
        ) as u32;

        let mut conds: Vec<ConditionHit> = Vec::with_capacity(10);
        let mut first_fail: Option<&'static str> = None;
        let mut check = |name: &'static str,
                         passed: bool,
                         value: f64,
                         threshold: f64,
                         reason: &'static str,
                         conds: &mut Vec<ConditionHit>| {
            conds.push(ConditionHit {
                name,
                passed,
                value,
                threshold,
            });
            if !passed && first_fail.is_none() {
                first_fail = Some(reason);
            }
        };

        // 1. Morning window only — hard empirical boundary.
        let entry_start = ctx.get_param(
            StrategyId::Apex,
            "entry_start_et_min",
            self.entry_start_et_min as f64,
        ) as u32;
        let entry_end = ctx.get_param(
            StrategyId::Apex,
            "entry_end_et_min",
            self.entry_end_et_min as f64,
        ) as u32;
        check(
            "et_window",
            ctx.now_et_min >= entry_start && ctx.now_et_min <= entry_end,
            f64::from(ctx.now_et_min),
            f64::from(entry_start),
            "outside entry window 09:00-16:00 ET",
            &mut conds,
        );
        // 2. Liquidity floor.
        check(
            "price_floor",
            ctx.price >= self.min_price,
            ctx.price,
            self.min_price,
            "price below $2 floor",
            &mut conds,
        );
        // S29.3 — Regulatorul Dinamic de Spread (Adaptive Spread Matrix)
        let base_max_spread =
            ctx.get_param(StrategyId::Apex, "max_spread_bps", self.max_spread_bps);
        let effective_max_spread = if ctx.price < 5.0 && ctx.rvol >= 5.0 {
            60.0
        } else {
            base_max_spread
        };
        check(
            "spread_bps",
            ctx.spread_bps.is_finite() && ctx.spread_bps <= effective_max_spread,
            ctx.spread_bps,
            effective_max_spread,
            "spread too wide",
            &mut conds,
        );

        // S29.3 — RVOL Adaptiv corelat cu regimul de piață. Condition
        // name carries the active regime so F4 calibrates each regime
        // threshold independently.
        let (rvol_cond, regime_adjusted_min_rvol) = match ctx.regime {
            Some(MarketRegime::Trending) => (
                "rvol_trending",
                ctx.get_param(StrategyId::Apex, "rvol_trending", 1.8),
            ),
            Some(MarketRegime::Calm) => (
                "rvol_calm",
                ctx.get_param(StrategyId::Apex, "rvol_calm", 2.2),
            ),
            Some(MarketRegime::Volatile) | Some(MarketRegime::Panic) => (
                "rvol_volatile",
                ctx.get_param(StrategyId::Apex, "rvol_volatile", 3.0),
            ),
            None => ("rvol", min_rvol),
        };
        check(
            rvol_cond,
            ctx.rvol.is_finite() && ctx.rvol >= regime_adjusted_min_rvol,
            ctx.rvol,
            regime_adjusted_min_rvol,
            "RVOL below regime-adjusted threshold",
            &mut conds,
        );
        // 3. Event evidence: a real catalyst OR extreme relative volume.
        let catalyst = ctx.catalyst_score.unwrap_or(0.0);
        check(
            "evidence",
            catalyst >= min_catalyst || ctx.rvol >= evidence_rvol,
            catalyst,
            min_catalyst,
            "no evidence: catalyst < 40 and RVOL < 4.0",
            &mut conds,
        );
        let atr = if ctx.atr > 0.0 {
            ctx.atr
        } else {
            pure_atr(&ctx.candles, 14)
        };
        if atr <= 0.0 {
            return (vec![], Err(first_fail.unwrap_or("ATR unavailable")));
        }
        // 4. Ignition bar anatomy on the last closed candle
        // (`ctx_is_sane` guarantees the candle exists).
        let Some(last) = ctx.candles.last() else {
            return (vec![], Err("no closed candle"));
        };
        let range = last.high - last.low;
        let range_ratio = if range.is_finite() && atr > 0.0 {
            range / atr
        } else {
            f64::NAN
        };
        check(
            "ignition_range",
            range_ratio.is_finite() && range_ratio >= ignition_range_atr,
            range_ratio,
            ignition_range_atr,
            Self::R_NO_IGNITION,
            &mut conds,
        );
        // Close position: 0 = at the low, 1 = at the high. Decisiveness
        // is the distance to the NEAREST extreme, so one threshold
        // covers both directions. Side falls out of which half the
        // close sits in (only used further when the check passes —
        // identical to the legacy long/short/indecisive branch).
        let close_pos = if range > 0.0 {
            (last.close - last.low) / range
        } else {
            0.5
        };
        let decisive = close_pos.max(1.0 - close_pos);
        check(
            "ignition_close_pos",
            decisive >= ignition_close_pos,
            decisive,
            ignition_close_pos,
            Self::R_INDECISIVE,
            &mut conds,
        );
        let side = if close_pos >= 0.5 {
            Direction::Long
        } else {
            Direction::Short
        };
        // Shorts only when the tape is actually moving (audit: thin
        // short sample, all in volatile conditions; Calm shorts are
        // fighting the market's long drift).
        let short_ok =
            !(side == Direction::Short && matches!(ctx.regime, Some(MarketRegime::Calm) | None));
        check(
            "short_regime",
            short_ok,
            if short_ok { 1.0 } else { 0.0 },
            1.0,
            "short setup but regime is Calm",
            &mut conds,
        );
        // Volume confirmation: ignition bar prints ≥ mult× prior-20 avg.
        let n = ctx.candles.len();
        let prior = &ctx.candles[n.saturating_sub(21)..n - 1];
        let avg_vol = prior.iter().map(|c| c.volume).sum::<f64>() / prior.len().max(1) as f64;
        let vol_ratio_raw = if avg_vol > 0.0 {
            last.volume / avg_vol
        } else {
            f64::NAN
        };
        check(
            "ignition_vol",
            vol_ratio_raw.is_finite() && vol_ratio_raw >= ignition_vol_mult,
            vol_ratio_raw,
            ignition_vol_mult,
            Self::R_LOW_VOLUME,
            &mut conds,
        );
        // 5. Alignment with VWAP and session open.
        let vwap = rolling_vwap(&ctx.candles, 30);
        if vwap <= 0.0 {
            return (vec![], Err(first_fail.unwrap_or("VWAP unavailable")));
        }
        let aligned = match side {
            Direction::Long => last.close > vwap && last.close > ctx.session_open,
            Direction::Short => last.close < vwap && last.close < ctx.session_open,
        };
        check(
            "aligned",
            aligned,
            last.close - vwap,
            0.0,
            Self::R_NOT_ALIGNED,
            &mut conds,
        );
        // 6. Not already extended past the move's useful life.
        let extension_ratio = (last.close - vwap).abs() / atr;
        check(
            "extension_atr",
            extension_ratio <= max_extension_atr,
            extension_ratio,
            max_extension_atr,
            Self::R_EXTENDED,
            &mut conds,
        );

        if let Some(reason) = first_fail {
            return (conds, Err(reason));
        }

        let stop_dist = self.stop_distance(ctx, atr, side);
        let (stop, target) = match side {
            Direction::Long => (ctx.price - stop_dist, ctx.price + stop_dist * target_r),
            Direction::Short => (ctx.price + stop_dist, ctx.price - stop_dist * target_r),
        };

        // Score from the physical strength of the ignition, NOT from a
        // model forecast (audit: model scores were anti-predictive).
        let vol_ratio = vol_ratio_raw.min(6.0); // 2..6
        let range_ratio_capped = range_ratio.min(4.0); // 1.2..4
        let score = (30.0
            + vol_ratio * 6.0                  // ≤ 36
            + range_ratio_capped * 5.0         // ≤ 20
            + (catalyst / 100.0) * 14.0) // ≤ 14
            .clamp(0.0, 100.0);

        let sig = EntrySignal {
            strategy: StrategyId::Apex,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes: max_hold,
            score,
            reason: "apex_ignition",
        };
        (conds, Ok(sig))
    }
}
