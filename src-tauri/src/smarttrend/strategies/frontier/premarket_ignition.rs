//! PREMARKET_IGNITION — catalyst gapper riding fast premarket tape.
//!
//! Window 07:00–09:28 ET, max 1 entry/symbol/day. Requires a real
//! catalyst (fresh news OR daily gap ≥ pm_min_gap_pct) plus a live
//! tape (pps + signed notional) and a bounded spread. Entries are
//! LIMIT-capped at the touch (extended session forbids market orders
//! and the book is thin — never cross a premarket spread blind).

use crate::state::Direction;

use super::{
    bracket, clamp_stop_dist, ConditionHit, FrontierCandidate, FrontierContext, FrontierSetup,
    FrontierThresholds, SetupEval,
};
use crate::smarttrend::strategies::{EntrySignal, StrategyId};

/// Full condition vector even on failure (F4 kill-rate).
pub fn eval_premarket_ignition_full(ctx: &FrontierContext, th: &FrontierThresholds) -> SetupEval {
    let fail = |conditions: Vec<ConditionHit>| SetupEval {
        setup: FrontierSetup::PremarketIgnition,
        conditions,
        candidate: None,
    };
    let mut conditions = Vec::new();

    if ctx
        .entered_today
        .contains(FrontierSetup::PremarketIgnition.as_str())
    {
        conditions.push(ConditionHit {
            name: "once_per_day",
            passed: false,
            value: 1.0,
            threshold: 0.0,
        });
        return fail(conditions);
    }
    conditions.push(ConditionHit {
        name: "once_per_day",
        passed: true,
        value: 0.0,
        threshold: 0.0,
    });

    let in_window = (th.pm_window_start_et..th.pm_window_end_et).contains(&ctx.now_et_min);
    conditions.push(ConditionHit {
        name: "et_window",
        passed: in_window,
        value: ctx.now_et_min as f64,
        threshold: th.pm_window_start_et as f64,
    });

    let age_ok = ctx.micro_age_secs >= th.tape_min_micro_age_secs;
    conditions.push(ConditionHit {
        name: "micro_warmup",
        passed: age_ok,
        value: ctx.micro_age_secs as f64,
        threshold: th.tape_min_micro_age_secs as f64,
    });

    // Catalyst gate: fresh news bypasses the gap requirement — a
    // headline can ignite before the gap is visible in daily bars.
    let gap_ok = ctx.catalyst_gap_pct.abs() >= th.pm_min_gap_pct;
    conditions.push(ConditionHit {
        name: "catalyst",
        passed: ctx.catalyst_news || gap_ok,
        value: ctx.catalyst_gap_pct,
        threshold: th.pm_min_gap_pct,
    });

    let pps = ctx.micro.prints_per_sec_15s;
    conditions.push(ConditionHit {
        name: "pps_15s",
        passed: pps >= th.pm_min_pps,
        value: pps,
        threshold: th.pm_min_pps,
    });

    let signed = ctx.micro.signed_notional_15s;
    conditions.push(ConditionHit {
        name: "signed_notional_15s",
        passed: signed.abs() >= th.pm_min_notional,
        value: signed,
        threshold: th.pm_min_notional,
    });

    let spread_ok = ctx.micro.spread_bps > 0.0 && ctx.micro.spread_bps <= th.pm_max_spread_bps;
    conditions.push(ConditionHit {
        name: "spread_bps",
        passed: spread_ok,
        value: ctx.micro.spread_bps,
        threshold: th.pm_max_spread_bps,
    });

    if conditions.iter().any(|c| !c.passed) {
        return fail(conditions);
    }

    let side = if signed >= 0.0 {
        Direction::Long
    } else {
        Direction::Short
    };
    // Wider structural stop than RTH tape: premarket swings are jagged.
    let atr = ctx.atr.max(ctx.price * 0.01);
    let stop_dist = clamp_stop_dist(atr * 1.5, atr, th.stop_atr_mult_lo, th.stop_atr_mult_hi);
    let (stop, target) = bracket(side, ctx.price, stop_dist, th.target_r);
    // Limit at the touch of the entry side — never cross the spread.
    let limit = if side == Direction::Long {
        ctx.micro.ask
    } else {
        ctx.micro.bid
    };
    let conviction = ((pps / th.pm_min_pps).min(3.0) / 3.0 * 0.35
        + (ctx.catalyst_gap_pct.abs() / th.pm_min_gap_pct).min(2.0) / 2.0 * 0.35
        + (1.0 - ctx.micro.spread_bps / th.pm_max_spread_bps).clamp(0.0, 1.0) * 0.3)
        .clamp(0.0, 1.0);

    let candidate = FrontierCandidate {
        setup: FrontierSetup::PremarketIgnition,
        signal: EntrySignal {
            strategy: StrategyId::Frontier,
            side,
            limit_price: Some(limit),
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes: 30,
            score: conviction * 100.0,
            reason: "premarket_ignition",
        },
        conditions: conditions.clone(),
        conviction,
    };
    SetupEval {
        setup: FrontierSetup::PremarketIgnition,
        conditions,
        candidate: Some(candidate),
    }
}
