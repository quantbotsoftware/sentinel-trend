//! OPEN_SWEEP — RTH open liquidity sweep / reclaim (max 1 entry/symbol/day).

use crate::state::Direction;

use super::{
    bracket, clamp_stop_dist, ConditionHit, FrontierCandidate, FrontierContext, FrontierSetup,
    FrontierThresholds, SetupEval,
};
use crate::smarttrend::strategies::{EntrySignal, StrategyId};

/// Full condition vector even on failure (F4 kill-rate).
pub fn eval_open_sweep_full(ctx: &FrontierContext, th: &FrontierThresholds) -> SetupEval {
    let mut conditions = Vec::new();

    if ctx
        .entered_today
        .contains(FrontierSetup::OpenSweep.as_str())
    {
        conditions.push(ConditionHit {
            name: "once_per_day",
            passed: false,
            value: 1.0,
            threshold: 0.0,
        });
        return SetupEval {
            setup: FrontierSetup::OpenSweep,
            conditions,
            candidate: None,
        };
    }
    conditions.push(ConditionHit {
        name: "once_per_day",
        passed: true,
        value: 0.0,
        threshold: 0.0,
    });

    let in_window = (th.sweep_window_start_et..th.sweep_window_end_et).contains(&ctx.now_et_min);
    conditions.push(ConditionHit {
        name: "et_window",
        passed: in_window,
        value: ctx.now_et_min as f64,
        threshold: th.sweep_window_start_et as f64,
    });

    let gap_pct = if ctx.prev_close > 0.0 {
        ((ctx.session_open - ctx.prev_close) / ctx.prev_close) * 100.0
    } else {
        0.0
    };
    let gap_ok = gap_pct.abs() >= th.sweep_min_gap_pct;
    conditions.push(ConditionHit {
        name: "gap_pct",
        passed: gap_ok,
        value: gap_pct,
        threshold: th.sweep_min_gap_pct,
    });

    let spread_ok = ctx.micro.spread_bps > 0.0 && ctx.micro.spread_bps <= th.sweep_max_spread_bps;
    conditions.push(ConditionHit {
        name: "spread_bps",
        passed: spread_ok,
        value: ctx.micro.spread_bps,
        threshold: th.sweep_max_spread_bps,
    });

    // Reclaim: price back through session open in the gap direction's fade.
    // Gap-up: look for sweep below open then reclaim (long). Gap-down: opposite.
    let reclaim = if gap_pct > 0.0 {
        ctx.price >= ctx.session_open
            && ctx
                .candles_1m
                .iter()
                .rev()
                .take(10)
                .any(|c| c.low < ctx.session_open)
    } else {
        ctx.price <= ctx.session_open
            && ctx
                .candles_1m
                .iter()
                .rev()
                .take(10)
                .any(|c| c.high > ctx.session_open)
    };
    conditions.push(ConditionHit {
        name: "reclaim",
        passed: reclaim,
        value: if reclaim { 1.0 } else { 0.0 },
        threshold: 1.0,
    });

    if conditions.iter().any(|c| !c.passed) {
        return SetupEval {
            setup: FrontierSetup::OpenSweep,
            conditions,
            candidate: None,
        };
    }

    let side = if gap_pct > 0.0 {
        Direction::Long
    } else {
        Direction::Short
    };
    let atr = ctx.atr.max(ctx.price * 0.005);
    let stop_dist = clamp_stop_dist(atr * 1.25, atr, th.stop_atr_mult_lo, th.stop_atr_mult_hi);
    let (stop, target) = bracket(side, ctx.price, stop_dist, th.target_r);
    let conviction = ((gap_pct.abs() / th.sweep_min_gap_pct).min(3.0) / 3.0 * 0.6
        + (1.0 - ctx.micro.spread_bps / th.sweep_max_spread_bps).clamp(0.0, 1.0) * 0.4)
        .clamp(0.0, 1.0);

    let candidate = FrontierCandidate {
        setup: FrontierSetup::OpenSweep,
        signal: EntrySignal {
            strategy: StrategyId::Frontier,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes: 45,
            score: conviction * 100.0,
            reason: "open_sweep",
        },
        conditions: conditions.clone(),
        conviction,
    };
    SetupEval {
        setup: FrontierSetup::OpenSweep,
        conditions,
        candidate: Some(candidate),
    }
}
