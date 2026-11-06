//! TAPE_IGNITION — burst + microprice lead continuation (calibratable).

use crate::state::Direction;

use super::{
    bracket, clamp_stop_dist, ConditionHit, FrontierCandidate, FrontierContext, FrontierSetup,
    FrontierThresholds, SetupEval,
};
use crate::smarttrend::strategies::{EntrySignal, StrategyId};

/// F4 kill-rate needs the condition vector of the FAILED evaluations
/// too, so this always returns the full vector.
pub fn eval_tape_ignition_full(ctx: &FrontierContext, th: &FrontierThresholds) -> SetupEval {
    let mut conditions = Vec::new();

    let age_ok = ctx.micro_age_secs >= th.tape_min_micro_age_secs;
    conditions.push(ConditionHit {
        name: "micro_warmup",
        passed: age_ok,
        value: ctx.micro_age_secs as f64,
        threshold: th.tape_min_micro_age_secs as f64,
    });

    let pps = ctx.micro.prints_per_sec_15s;
    let pps_ok = pps >= th.tape_min_pps_15s;
    conditions.push(ConditionHit {
        name: "pps_15s",
        passed: pps_ok,
        value: pps,
        threshold: th.tape_min_pps_15s,
    });

    let signed = ctx.micro.signed_notional_15s;
    let signed_ok = signed.abs() >= th.tape_min_signed_notional_15s;
    conditions.push(ConditionHit {
        name: "signed_notional_15s",
        passed: signed_ok,
        value: signed,
        threshold: th.tape_min_signed_notional_15s,
    });

    let spread_ok = ctx.micro.spread_bps > 0.0 && ctx.micro.spread_bps <= th.tape_max_spread_bps;
    conditions.push(ConditionHit {
        name: "spread_bps",
        passed: spread_ok,
        value: ctx.micro.spread_bps,
        threshold: th.tape_max_spread_bps,
    });

    let imb = ctx.micro.imbalance;
    let imb_ok = imb.abs() >= th.tape_min_imbalance;
    conditions.push(ConditionHit {
        name: "imbalance",
        passed: imb_ok,
        value: imb,
        threshold: th.tape_min_imbalance,
    });

    let baseline_ok = if ctx.baseline_pps_5m > 0.0 {
        ctx.micro.prints_per_sec_60s >= ctx.baseline_pps_5m * th.tape_baseline_mult
    } else {
        // No baseline yet (cold promote) — require absolute pps only.
        true
    };
    conditions.push(ConditionHit {
        name: "baseline_burst",
        passed: baseline_ok,
        value: ctx.micro.prints_per_sec_60s,
        threshold: ctx.baseline_pps_5m * th.tape_baseline_mult,
    });

    // Microprice lead: microprice on the aggressive side of last trade.
    let lead = ctx.micro.microprice - ctx.micro.last_trade_price;
    let lead_ok = if signed >= 0.0 {
        lead >= 0.0
    } else {
        lead <= 0.0
    };
    conditions.push(ConditionHit {
        name: "microprice_lead",
        passed: lead_ok,
        value: lead,
        threshold: 0.0,
    });

    if conditions.iter().any(|c| !c.passed) {
        return SetupEval {
            setup: FrontierSetup::TapeIgnition,
            conditions,
            candidate: None,
        };
    }

    let side = if signed >= 0.0 {
        Direction::Long
    } else {
        Direction::Short
    };
    let atr = ctx.atr.max(ctx.price * 0.005);
    let stop_dist = clamp_stop_dist(atr, atr, th.stop_atr_mult_lo, th.stop_atr_mult_hi);
    let (stop, target) = bracket(side, ctx.price, stop_dist, th.target_r);
    let conviction = ((pps / th.tape_min_pps_15s).min(3.0) / 3.0 * 0.4
        + (imb.abs() / 1.0).min(1.0) * 0.3
        + (1.0 - ctx.micro.spread_bps / th.tape_max_spread_bps).clamp(0.0, 1.0) * 0.3)
        .clamp(0.0, 1.0);
    let score = conviction * 100.0;

    let candidate = FrontierCandidate {
        setup: FrontierSetup::TapeIgnition,
        signal: EntrySignal {
            strategy: StrategyId::Frontier,
            side,
            limit_price: None,
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes: 45,
            score,
            reason: "tape_ignition",
        },
        conditions: conditions.clone(),
        conviction,
    };
    SetupEval {
        setup: FrontierSetup::TapeIgnition,
        conditions,
        candidate: Some(candidate),
    }
}
