//! P6 — Daily Debrief.
//!
//! One EOD report that answers "what did the system do today and
//! why?" without grep archaeology across four data stores. Pulls the
//! ET-day slice of the trade journal, the decision journal, the
//! shadow book, the hospital and the slippage ledger, and assembles
//! both the numbers and a short human narrative.
//!
//! Read-only aggregation: builds on demand (PRIME tab DEBRIEF panel),
//! no background loop, no persistence — the underlying stores are the
//! system of record.
//!
//! Lock discipline: `journal` (Tier A) is taken alone and dropped;
//! then the standalone `prime` and `shadow` mutexes, one at a time.
//! Never overlapped.

use serde::Serialize;

use crate::state::TitanState;

#[derive(Debug, Clone, Serialize)]
pub struct PlaybookDay {
    pub playbook: String,
    pub trades: u32,
    pub wins: u32,
    pub pnl: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Debrief {
    /// ET date the report covers (YYYY-MM-DD).
    pub date: String,
    pub trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub total_pnl: f64,
    pub best_symbol: Option<(String, f64)>,
    pub worst_symbol: Option<(String, f64)>,
    pub playbooks: Vec<PlaybookDay>,
    /// Veto counts by stage, today.
    pub vetoes: Vec<(String, i64)>,
    /// P0 — signal exits deferred on winners today.
    pub exits_deferred: i64,
    /// Shadow book today: resolved counterfactuals and their net R
    /// (positive = the vetoes refused edge, negative = they saved).
    pub shadows_resolved: u32,
    pub shadow_net_r: f64,
    /// P1 — playbooks currently in quarantine.
    pub quarantined: Vec<String>,
    /// P5 — average adverse slippage across today's fills (bps).
    pub avg_slippage_bps: Option<f64>,
    /// Assembled narrative, one line per observation.
    pub narrative: Vec<String>,
}

/// ET midnight of the current day (S04 — session boundaries resolve
/// in `US::Eastern`, never naive UTC).
fn et_day_start(now_et: &chrono::DateTime<chrono_tz::Tz>) -> i64 {
    now_et
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_local_timezone(chrono_tz::US::Eastern).single())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

pub fn build_debrief(state: &TitanState) -> Debrief {
    use std::collections::HashMap;

    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let day_start = et_day_start(&now_et);
    let date = now_et.format("%Y-%m-%d").to_string();

    // ── Trade journal slice (Tier A, taken alone) ──
    let closes: Vec<(String, Option<String>, f64)> = {
        let journal = state.journal.lock().unwrap_or_else(|p| p.into_inner());
        journal
            .entries
            .iter()
            .filter(|e| e.exit_time.map(|t| t >= day_start).unwrap_or(false))
            .filter_map(|e| {
                e.pnl
                    .map(|p| (e.symbol.to_uppercase(), e.strategy_id.clone(), p))
            })
            .collect()
    };

    let trades = closes.len() as u32;
    let wins = closes.iter().filter(|(_, _, p)| *p > 0.0).count() as u32;
    let losses = trades - wins;
    let total_pnl: f64 = closes.iter().map(|(_, _, p)| p).sum();

    let mut per_symbol: HashMap<String, f64> = HashMap::new();
    let mut per_playbook: HashMap<String, (u32, u32, f64)> = HashMap::new();
    for (sym, playbook, pnl) in &closes {
        *per_symbol.entry(sym.clone()).or_default() += pnl;
        let key = playbook
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "fsm".to_string());
        let row = per_playbook.entry(key).or_default();
        row.0 += 1;
        if *pnl > 0.0 {
            row.1 += 1;
        }
        row.2 += pnl;
    }
    let best_symbol = per_symbol
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(s, p)| (s.clone(), *p));
    let worst_symbol = per_symbol
        .iter()
        .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(s, p)| (s.clone(), *p));
    let mut playbooks: Vec<PlaybookDay> = per_playbook
        .into_iter()
        .map(|(playbook, (t, w, pnl))| PlaybookDay {
            playbook,
            trades: t,
            wins: w,
            pnl,
        })
        .collect();
    playbooks.sort_by(|a, b| b.pnl.partial_cmp(&a.pnl).unwrap_or(std::cmp::Ordering::Equal));

    // ── Decision journal (short-lived read connection, no locks) ──
    let (vetoes, exits_deferred) = crate::ops::decision_log::global()
        .map(|log| {
            let mut by_stage: HashMap<String, i64> = HashMap::new();
            let mut deferred = 0i64;
            if let Ok(rows) = log.query("", day_start, now_et.timestamp(), 5_000) {
                for r in rows {
                    match r.outcome.as_str() {
                        "vetoed" => {
                            *by_stage
                                .entry(r.veto_stage.unwrap_or_default())
                                .or_default() += 1;
                        }
                        "exit_deferred" => deferred += 1,
                        _ => {}
                    }
                }
            }
            let mut v: Vec<(String, i64)> = by_stage.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            (v, deferred)
        })
        .unwrap_or_default();

    // ── Shadow book + hospital (standalone mutexes, one at a time) ──
    let (shadows_resolved, shadow_net_r) = state
        .dplus()
        .and_then(|dp| dp.shadow.lock().ok().map(|b| b.recent_resolved(2_048)))
        .map(|resolved| {
            let today: Vec<_> = resolved
                .into_iter()
                .filter(|r| r.closed_ts >= day_start)
                .collect();
            (
                today.len() as u32,
                today.iter().map(|r| r.pnl_r).sum::<f64>(),
            )
        })
        .unwrap_or((0, 0.0));
    let quarantined: Vec<String> = state
        .dplus()
        .and_then(|dp| dp.prime.lock().ok().map(|e| {
            let mut q: Vec<String> = e.hospital.quarantined.keys().cloned().collect();
            q.sort();
            q
        }))
        .unwrap_or_default();

    // ── Slippage (module-level ledger) ──
    let todays_slips: Vec<f64> = crate::ops::slippage::recent(crate::ops::slippage::MAX_FILLS)
        .into_iter()
        .filter(|f| f.ts >= day_start)
        .map(|f| f.bps)
        .collect();
    let avg_slippage_bps = if todays_slips.is_empty() {
        None
    } else {
        Some(todays_slips.iter().sum::<f64>() / todays_slips.len() as f64)
    };

    let narrative = build_narrative(
        trades,
        wins,
        total_pnl,
        &playbooks,
        &best_symbol,
        &worst_symbol,
        exits_deferred,
        shadows_resolved,
        shadow_net_r,
        &quarantined,
        avg_slippage_bps,
    );

    Debrief {
        date,
        trades,
        wins,
        losses,
        total_pnl,
        best_symbol,
        worst_symbol,
        playbooks,
        vetoes,
        exits_deferred,
        shadows_resolved,
        shadow_net_r,
        quarantined,
        avg_slippage_bps,
        narrative,
    }
}

/// Assemble the plain-language observations. Pure — unit-testable
/// without state.
#[allow(clippy::too_many_arguments)]
fn build_narrative(
    trades: u32,
    wins: u32,
    total_pnl: f64,
    playbooks: &[PlaybookDay],
    best: &Option<(String, f64)>,
    worst: &Option<(String, f64)>,
    exits_deferred: i64,
    shadows_resolved: u32,
    shadow_net_r: f64,
    quarantined: &[String],
    avg_slippage_bps: Option<f64>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if trades == 0 {
        lines.push("No trades closed today.".to_string());
    } else {
        lines.push(format!(
            "Closed {trades} trade(s), {wins} winner(s), net ${total_pnl:.2}."
        ));
        if let Some(pb) = playbooks.first() {
            if pb.pnl > 0.0 {
                lines.push(format!(
                    "Best playbook: {} (+${:.2} over {} trade(s)).",
                    pb.playbook, pb.pnl, pb.trades
                ));
            }
        }
        if let Some(pb) = playbooks.last() {
            if pb.pnl < 0.0 {
                lines.push(format!(
                    "Worst playbook: {} (−${:.2} over {} trade(s)).",
                    pb.playbook,
                    pb.pnl.abs(),
                    pb.trades
                ));
            }
        }
        if let (Some((bs, bp)), Some((ws, wp))) = (best, worst) {
            if bs != ws {
                lines.push(format!(
                    "Symbols: {bs} best (${bp:.2}), {ws} worst (${wp:.2})."
                ));
            }
        }
    }
    if exits_deferred > 0 {
        lines.push(format!(
            "P0 asymmetry: {exits_deferred} signal exit(s) deferred on winners — ladder managed them."
        ));
    }
    if shadows_resolved > 0 {
        if shadow_net_r >= 0.0 {
            lines.push(format!(
                "Shadows: {shadows_resolved} vetoed trade(s) resolved at +{shadow_net_r:.1}R — the vetoes refused edge; check the WEIGHTS LAB."
            ));
        } else {
            lines.push(format!(
                "Shadows: {shadows_resolved} vetoed trade(s) resolved at {shadow_net_r:.1}R — the vetoes saved money."
            ));
        }
    }
    if !quarantined.is_empty() {
        lines.push(format!("In hospital: {}.", quarantined.join(", ")));
    }
    if let Some(bps) = avg_slippage_bps {
        if bps > 10.0 {
            lines.push(format!(
                "Execution: average slippage {bps:.1} bps adverse — size down thin tickers."
            ));
        }
    }
    lines
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrative_no_trades() {
        let lines = build_narrative(0, 0, 0.0, &[], &None, &None, 0, 0, 0.0, &[], None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("No trades"));
    }

    #[test]
    fn narrative_full_day() {
        let playbooks = vec![
            PlaybookDay {
                playbook: "halt_resume".into(),
                trades: 4,
                wins: 3,
                pnl: 18.0,
            },
            PlaybookDay {
                playbook: "gap_fade".into(),
                trades: 2,
                wins: 0,
                pnl: -7.0,
            },
        ];
        let lines = build_narrative(
            6,
            3,
            11.0,
            &playbooks,
            &Some(("STTK".into(), 12.0)),
            &Some(("PURR".into(), -6.0)),
            2,
            5,
            3.5,
            &["catalyst_frontrun".to_string()],
            Some(14.0),
        );
        let text = lines.join(" ");
        assert!(text.contains("Closed 6 trade(s)"));
        assert!(text.contains("halt_resume"));
        assert!(text.contains("gap_fade"));
        assert!(text.contains("STTK"));
        assert!(text.contains("deferred"));
        assert!(text.contains("refused edge"));
        assert!(text.contains("catalyst_frontrun"));
        assert!(text.contains("slippage"));
    }

    #[test]
    fn narrative_saved_shadows_phrase() {
        let lines =
            build_narrative(0, 0, 0.0, &[], &None, &None, 0, 3, -2.5, &[], None);
        assert!(lines.iter().any(|l| l.contains("saved money")));
    }

    #[test]
    fn debrief_builds_on_empty_state() {
        let state = TitanState::new(crate::config::RuntimeConfig::default());
        let d = build_debrief(&state);
        assert_eq!(d.trades, 0);
        assert!(!d.narrative.is_empty());
    }
}
