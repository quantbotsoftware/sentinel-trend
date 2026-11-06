//! PRIME F5b — nightly reliability auto-calibration.
//!
//! Once per day (21:30 ET, after the OMEGA retrain window) the loop
//! reads the trade journal, aggregates closed trades of the trailing
//! [`CALIBRATION_WINDOW_DAYS`] per playbook, and re-derives each
//! playbook's reliability via
//! [`ReliabilityTable::update_from_outcomes`] (Laplace-smoothed win
//! rate blended with a bounded profit-factor term, clamped to
//! `[0.15, 0.90]`). The updated engine is persisted to
//! `prime_engine.json` so convictions survive restarts.
//!
//! The aggregation itself is a pure function (R4) — the loop only
//! shuttles data between the journal guard (Tier A, dropped before
//! the standalone `prime` mutex is taken) and the engine.

use std::collections::HashMap;
use std::sync::Arc;

use crate::state::TitanState;

use super::PrimeEngine;

pub const CALIBRATION_WINDOW_DAYS: i64 = 14;
/// ET minute-of-day at which the daily calibration fires (21:30).
const FIRE_AT_ET_MIN: u32 = 21 * 60 + 30;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct OutcomeAgg {
    pub wins: u32,
    pub losses: u32,
    pub gross_profit: f64,
    pub gross_loss: f64,
}

/// Aggregate `(playbook, pnl)` pairs per playbook. `None` playbooks
/// (legacy momentum trades) attribute to `"fsm"`. Zero-P/L scratches
/// count as losses (they consumed a slot without paying for it).
pub fn aggregate_outcomes(closed: &[(Option<String>, f64)]) -> HashMap<String, OutcomeAgg> {
    let mut agg: HashMap<String, OutcomeAgg> = HashMap::new();
    for (source, pnl) in closed {
        let key = source
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("fsm")
            .to_string();
        let row = agg.entry(key).or_default();
        if *pnl > 0.0 {
            row.wins += 1;
            row.gross_profit += pnl;
        } else {
            row.losses += 1;
            row.gross_loss += pnl.abs();
        }
    }
    agg
}

/// P2 — minimum closed trades required in one `playbook|regime|session`
/// context before a contextual reliability key is written. Below this
/// the fusion falls back to the playbook-global value (avoids letting
/// two random trades define a context).
pub const CONTEXT_MIN_TRADES: u32 = 3;

/// One closed trade with its context: `(playbook, pnl, regime_label,
/// entry_time)`. `regime_label` is `None` for legacy rows.
pub type ContextualClose = (Option<String>, f64, Option<String>, i64);

/// P2 — aggregate per `playbook|regime|session` context key. Trades
/// without a regime label are skipped (they still feed the global
/// aggregation). Session derives from `entry_time` in ET.
pub fn aggregate_contextual(closed: &[ContextualClose]) -> HashMap<String, OutcomeAgg> {
    use chrono::Timelike;
    let mut agg: HashMap<String, OutcomeAgg> = HashMap::new();
    for (source, pnl, regime, entry_time) in closed {
        let Some(regime) = regime.as_deref().filter(|r| !r.is_empty()) else {
            continue;
        };
        let playbook = source
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("fsm");
        let session = chrono::DateTime::<chrono::Utc>::from_timestamp(*entry_time, 0)
            .map(|d| d.with_timezone(&chrono_tz::US::Eastern))
            .map(|d| super::conviction::session_label(d.hour() * 60 + d.minute()))
            .unwrap_or("off");
        let key = format!("{playbook}|{regime}|{session}");
        let row = agg.entry(key).or_default();
        if *pnl > 0.0 {
            row.wins += 1;
            row.gross_profit += pnl;
        } else {
            row.losses += 1;
            row.gross_loss += pnl.abs();
        }
    }
    agg
}

/// P2 — apply contextual aggregations onto the reliability table.
/// Contexts under `CONTEXT_MIN_TRADES` closed trades are dropped.
pub fn apply_context_calibration(
    engine: &mut PrimeEngine,
    ctx_agg: &HashMap<String, OutcomeAgg>,
) {
    for (key, o) in ctx_agg {
        if o.wins + o.losses < CONTEXT_MIN_TRADES {
            continue;
        }
        engine.reliability.update_from_outcomes(
            key,
            o.wins,
            o.losses,
            o.gross_profit,
            o.gross_loss,
        );
    }
}

/// Apply an aggregation onto the engine's reliability table. Playbooks
/// absent from `agg` keep their current value (priors persist until
/// evidence arrives — same contract as `update_from_outcomes`).
pub fn apply_calibration(
    engine: &mut PrimeEngine,
    agg: &HashMap<String, OutcomeAgg>,
    now_ts: i64,
) {
    for (source, o) in agg {
        engine.reliability.update_from_outcomes(
            source,
            o.wins,
            o.losses,
            o.gross_profit,
            o.gross_loss,
        );
    }
    engine.last_calibration_ts = now_ts;
}

/// Background loop — wakes every 60 s, fires once per ET day when the
/// clock crosses 21:30 ET. Spawned from `main.rs` alongside the other
/// D-PLUS loops; exits on shutdown.
pub async fn run_calibration_loop(state: Arc<TitanState>) {
    use chrono::{Datelike, Timelike};
    let mut last_fired_day: i64 = -1;
    loop {
        if state.shutdown.is_cancelled() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;

        let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        let et_min = now_et.hour() * 60 + now_et.minute();
        let et_day = i64::from(now_et.num_days_from_ce());
        if et_min < FIRE_AT_ET_MIN || et_day == last_fired_day {
            continue;
        }
        last_fired_day = et_day;

        calibrate_once(&state);
    }
    tracing::info!(component = "prime", "calibration loop exiting");
}

/// One calibration pass. Split out of the loop so the settings UI /
/// tests can trigger it on demand.
pub fn calibrate_once(state: &TitanState) {
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - CALIBRATION_WINDOW_DAYS * 86_400;

    // Tier-A journal guard — extract plain tuples, then drop.
    let (contextual, real_closes): (
        Vec<ContextualClose>,
        Vec<super::counterfactual::RealClose>,
    ) = {
        let journal = state.journal.lock().unwrap_or_else(|p| p.into_inner());
        let closed: Vec<&crate::state::JournalEntry> = journal
            .entries
            .iter()
            .filter(|e| e.exit_time.map(|t| t >= cutoff).unwrap_or(false) && e.pnl.is_some())
            .collect();
        let contextual = closed
            .iter()
            .map(|e| {
                (
                    e.strategy_id.clone(),
                    e.pnl.unwrap_or(0.0),
                    e.regime.clone(),
                    e.entry_time,
                )
            })
            .collect();
        // P3 — same closes reshaped for the Counterfactual Lab join.
        let real_closes = closed
            .iter()
            .map(|e| super::counterfactual::RealClose {
                symbol: e.symbol.to_uppercase(),
                source: e
                    .strategy_id
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "fsm".to_string()),
                entry_time: e.entry_time,
                pnl: e.pnl.unwrap_or(0.0),
            })
            .collect();
        (contextual, real_closes)
    };
    let closed: Vec<(Option<String>, f64)> = contextual
        .iter()
        .map(|(s, p, _, _)| (s.clone(), *p))
        .collect();
    if closed.is_empty() {
        tracing::info!(
            component = "prime",
            window_days = CALIBRATION_WINDOW_DAYS,
            "calibration skipped — no closed trades in window"
        );
        return;
    }

    let agg = aggregate_outcomes(&closed);
    // P2 — contextual aggregation (playbook|regime|session).
    let ctx_agg = aggregate_contextual(&contextual);

    let Some(dp) = state.dplus() else { return };
    // P1 — shadow evidence snapshot for hospital discharge decisions.
    // The shadow mutex is standalone; take + drop it BEFORE the prime
    // guard (never overlapped). P3 reuses the same window for the
    // Counterfactual Lab's vetoed-outcome join.
    let (shadow_rows, shadow_outcomes) = dp
        .shadow
        .lock()
        .map(|book| {
            let outcomes: Vec<super::counterfactual::ShadowOutcome> = book
                .recent_resolved(super::shadow::MAX_RESOLVED)
                .into_iter()
                .map(|r| super::counterfactual::ShadowOutcome {
                    symbol: r.trade.symbol,
                    source: r.trade.source,
                    opened_ts: r.trade.opened_ts,
                    pnl_r: r.pnl_r,
                })
                .collect();
            (book.summary(), outcomes)
        })
        .unwrap_or_default();

    // P3 — recorded fusion components from the decision journal
    // (no locks involved; short-lived read connection).
    let recorded: Vec<super::counterfactual::RecordedDecision> =
        crate::ops::decision_log::global()
            .and_then(|log| log.rows_with_components(cutoff, 5_000).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|rec| {
                rec.parsed_components()
                    .map(|norm| super::counterfactual::RecordedDecision {
                        norm,
                        symbol: rec.symbol.clone(),
                        source: rec.source.clone(),
                        ts: rec.ts,
                        entered: rec.outcome == "entered",
                    })
            })
            .collect();
    let labeled = super::counterfactual::build_labeled_sample(
        &recorded,
        &real_closes,
        &shadow_outcomes,
    );

    let (snapshot, transitions) = {
        let Ok(mut engine) = dp.prime.lock() else {
            return;
        };
        apply_calibration(&mut engine, &agg, now);
        // P2 — contextual keys land after the globals so a context
        // with enough evidence refines (not merely repeats) them.
        apply_context_calibration(&mut engine, &ctx_agg);
        // P1 — hospital pass runs on the freshly-calibrated table.
        let engine_ref = &mut *engine;
        let transitions =
            engine_ref
                .hospital
                .evaluate(&mut engine_ref.reliability, &shadow_rows, now);
        // P3 — Counterfactual Lab grid search. Never auto-applies:
        // the suggestion waits for the operator in the PRIME tab.
        let suggestion =
            super::counterfactual::grid_search(&engine.weights, &labeled, now);
        match &suggestion {
            Some(s) => tracing::info!(
                component = "prime",
                sample = s.sample,
                baseline_r = format!("{:.2}", s.baseline_r),
                suggested_r = format!("{:.2}", s.suggested_r),
                "P3 Counterfactual Lab — weight suggestion ready (review in PRIME tab)"
            ),
            None => tracing::info!(
                component = "prime",
                sample = labeled.len(),
                "P3 Counterfactual Lab — no suggestion (small sample or baseline optimal)"
            ),
        }
        // Keep a previous suggestion only if the new run produced one
        // (a None run means "insufficient evidence", not "revoke").
        if suggestion.is_some() {
            engine.suggested_weights = suggestion;
        }
        (engine.clone(), transitions)
    };
    // Persist outside the guard.
    if let Err(e) = snapshot.save(&super::default_engine_path()) {
        tracing::warn!(component = "prime", error = %e, "prime engine save failed");
    }
    for (source, o) in &agg {
        tracing::info!(
            component = "prime",
            source = %source,
            wins = o.wins,
            losses = o.losses,
            gross_profit = format!("{:.2}", o.gross_profit),
            gross_loss = format!("{:.2}", o.gross_loss),
            reliability = format!("{:.3}", snapshot.reliability.get(source)),
            "nightly reliability calibration applied"
        );
    }
    for t in &transitions {
        match t {
            super::hospital::HospitalTransition::Admitted { source, reliability } => {
                tracing::warn!(
                    component = "prime",
                    source = %source,
                    reliability = format!("{:.3}", reliability),
                    "P1 hospital ADMISSION — playbook quarantined (live entries vetoed, shadows continue)"
                );
            }
            super::hospital::HospitalTransition::Discharged {
                source,
                resolved,
                winrate,
                total_r,
            } => {
                tracing::warn!(
                    component = "prime",
                    source = %source,
                    resolved,
                    winrate = format!("{:.2}", winrate),
                    total_r = format!("{:.2}", total_r),
                    "P1 hospital DISCHARGE — playbook re-activated on shadow evidence"
                );
            }
        }
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_groups_and_defaults_fsm() {
        let closed = vec![
            (Some("gap_fade".to_string()), 12.0),
            (Some("gap_fade".to_string()), -4.0),
            (None, 7.5),
            (Some(String::new()), -2.5),
        ];
        let agg = aggregate_outcomes(&closed);
        let gf = &agg["gap_fade"];
        assert_eq!((gf.wins, gf.losses), (1, 1));
        assert!((gf.gross_profit - 12.0).abs() < 1e-9);
        assert!((gf.gross_loss - 4.0).abs() < 1e-9);
        // None and empty-string both fold into "fsm".
        let fsm = &agg["fsm"];
        assert_eq!((fsm.wins, fsm.losses), (1, 1));
    }

    #[test]
    fn zero_pnl_counts_as_loss() {
        let agg = aggregate_outcomes(&[(Some("halt_resume".to_string()), 0.0)]);
        assert_eq!(agg["halt_resume"].losses, 1);
    }

    #[test]
    fn contextual_aggregation_keys_and_min_sample() {
        // 2026-06-26 19:12 UTC = 15:12 ET (EDT) → session "close".
        let entry_ts = chrono::DateTime::parse_from_rfc3339("2026-06-26T19:12:00Z")
            .unwrap()
            .timestamp();
        let closes: Vec<ContextualClose> = vec![
            (Some("afternoon_fade".into()), -4.0, Some("calm".into()), entry_ts),
            (Some("afternoon_fade".into()), -5.0, Some("calm".into()), entry_ts),
            (Some("afternoon_fade".into()), 2.0, Some("calm".into()), entry_ts),
            // No regime → skipped from contextual aggregation.
            (Some("gap_fade".into()), 3.0, None, entry_ts),
        ];
        let ctx = aggregate_contextual(&closes);
        let row = &ctx["afternoon_fade|calm|close"];
        assert_eq!((row.wins, row.losses), (1, 2));
        assert!(!ctx.keys().any(|k| k.starts_with("gap_fade")));

        // Application respects the min-sample gate.
        let mut engine = PrimeEngine::default();
        apply_context_calibration(&mut engine, &ctx);
        assert!(engine
            .reliability
            .map
            .contains_key("afternoon_fade|calm|close"));
        let mut sparse = HashMap::new();
        sparse.insert(
            "gap_fade|calm|open".to_string(),
            OutcomeAgg {
                wins: 1,
                losses: 1,
                gross_profit: 5.0,
                gross_loss: 3.0,
            },
        );
        apply_context_calibration(&mut engine, &sparse);
        assert!(!engine.reliability.map.contains_key("gap_fade|calm|open"));
    }

    #[test]
    fn apply_moves_reliability_and_stamps_ts() {
        let mut engine = PrimeEngine::default();
        let before = engine.reliability.get("catalyst_frontrun");
        let mut agg = HashMap::new();
        agg.insert(
            "catalyst_frontrun".to_string(),
            OutcomeAgg {
                wins: 14,
                losses: 4,
                gross_profit: 620.0,
                gross_loss: 180.0,
            },
        );
        apply_calibration(&mut engine, &agg, 1_750_000_000);
        assert!(engine.reliability.get("catalyst_frontrun") > before);
        assert_eq!(engine.last_calibration_ts, 1_750_000_000);
        // Untouched playbooks keep their priors.
        assert_eq!(
            engine.reliability.get("halt_resume"),
            PrimeEngine::default().reliability.get("halt_resume")
        );
    }
}
