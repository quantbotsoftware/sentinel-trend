//! TRUE_RESUME — event-driven fade of the first post-halt prints.

use crate::state::Direction;

use super::{
    bracket, clamp_stop_dist, ConditionHit, FrontierCandidate, FrontierContext, FrontierSetup,
    FrontierThresholds, SetupEval,
};
use crate::smarttrend::strategies::{EntrySignal, StrategyId};

/// Full condition vector even on failure (F4 kill-rate). A symbol with
/// no resume snapshot yields an empty vector — the setup is not
/// applicable, which is different from "killed by a condition".
pub fn eval_true_resume_full(ctx: &FrontierContext, th: &FrontierThresholds) -> SetupEval {
    let fail = |conditions: Vec<ConditionHit>| SetupEval {
        setup: FrontierSetup::TrueResume,
        conditions,
        candidate: None,
    };
    let Some(resume) = ctx.resume.as_ref() else {
        return fail(Vec::new());
    };
    let mut conditions = Vec::new();

    let age = ctx
        .now_utc
        .signed_duration_since(resume.resumed_at)
        .num_seconds()
        .max(0) as u64;
    let age_ok = age <= th.resume_max_age_secs;
    conditions.push(ConditionHit {
        name: "resume_age_secs",
        passed: age_ok,
        value: age as f64,
        threshold: th.resume_max_age_secs as f64,
    });

    let spread_ok = ctx.micro.spread_bps > 0.0 && ctx.micro.spread_bps <= th.resume_max_spread_bps;
    conditions.push(ConditionHit {
        name: "spread_bps",
        passed: spread_ok,
        value: ctx.micro.spread_bps,
        threshold: th.resume_max_spread_bps,
    });

    let n_prints = resume.first_prints.len();
    let prints_ok = n_prints >= 3;
    conditions.push(ConditionHit {
        name: "first_prints",
        passed: prints_ok,
        value: n_prints as f64,
        threshold: 3.0,
    });

    let buy_notional: f64 = resume
        .first_prints
        .iter()
        .filter(|(_, _, b)| *b)
        .map(|(p, s, _)| p * s)
        .sum();
    let sell_notional: f64 = resume
        .first_prints
        .iter()
        .filter(|(_, _, b)| !*b)
        .map(|(p, s, _)| p * s)
        .sum();
    let net = buy_notional - sell_notional;
    let notional_ok = net.abs() >= th.resume_min_print_notional;
    conditions.push(ConditionHit {
        name: "resume_print_notional",
        passed: notional_ok,
        value: net.abs(),
        threshold: th.resume_min_print_notional,
    });

    if let Some(pre) = resume.pre_halt_price {
        let move_pct = ((ctx.price - pre) / pre).abs() * 100.0;
        let move_ok = move_pct >= 2.0;
        conditions.push(ConditionHit {
            name: "pre_halt_move_pct",
            passed: move_ok,
            value: move_pct,
            threshold: 2.0,
        });
    }

    if conditions.iter().any(|c| !c.passed) {
        return fail(conditions);
    }

    // Fade the print pressure.
    let side = if net > 0.0 {
        Direction::Short
    } else {
        Direction::Long
    };
    let atr = ctx.atr.max(ctx.price * 0.008);
    let stop_dist = clamp_stop_dist(atr * 1.0, atr, th.stop_atr_mult_lo, th.stop_atr_mult_hi);
    let (stop, target) = bracket(side, ctx.price, stop_dist, th.target_r);
    let conviction = ((net.abs() / th.resume_min_print_notional).min(3.0) / 3.0 * 0.5
        + (1.0 - age as f64 / th.resume_max_age_secs as f64).clamp(0.0, 1.0) * 0.5)
        .clamp(0.0, 1.0);

    let candidate = FrontierCandidate {
        setup: FrontierSetup::TrueResume,
        signal: EntrySignal {
            strategy: StrategyId::Frontier,
            side,
            // Limit-IOC preferred at resume cross (caller may peg).
            limit_price: Some(ctx.price),
            stop_loss: stop,
            take_profit: target,
            max_hold_minutes: 8,
            score: conviction * 100.0,
            reason: "true_resume",
        },
        conditions: conditions.clone(),
        conviction,
    };
    SetupEval {
        setup: FrontierSetup::TrueResume,
        conditions,
        candidate: Some(candidate),
    }
}
