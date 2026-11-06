//! P1 — Strategy Hospital (pure, R4-compliant).
//!
//! Two quarantine mechanisms, both empirically motivated by the
//! 2026-07-03 audit (26.5% win rate driven by a minority of playbooks
//! and symbols bleeding repeatedly):
//!
//! 1. **Playbook quarantine** — a playbook whose calibrated reliability
//!    falls below `admit_below` is admitted to the hospital: PRIME
//!    vetoes its live candidates (`quarantined`) while the shadow book
//!    keeps trading them counterfactually. Discharge is *earned*, not
//!    timed: the playbook must show `discharge_min_resolved` resolved
//!    shadows with a win rate ≥ `discharge_min_winrate` AND positive
//!    total R. This replaces the blunt 3-loss circuit breaker (which
//!    resets on a single lucky win) with an evidence-gated readmission.
//!
//! 2. **Per-symbol daily breaker** — a symbol that produced
//!    `symbol_max_consecutive_losses` consecutive losing closes today
//!    (ET) is banned for the rest of the day, for every playbook. The
//!    audit's top-of-loss book (MASK −$350, SOBR −$316, ...) is exactly
//!    the "keep re-entering the same bleeding ticker" pattern. This is
//!    a pure function of today's journal closes — no state to persist,
//!    it resets naturally at the ET day boundary.
//!
//! The [`Hospital`] struct is serialized inside `PrimeEngine`
//! (`prime_engine.json`) so quarantine survives restarts.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::conviction::ReliabilityTable;
use super::shadow::ShadowSummaryRow;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HospitalPolicy {
    /// Reliability below which a playbook is admitted.
    pub admit_below: f64,
    /// Resolved shadow count required before discharge is considered.
    pub discharge_min_resolved: u32,
    /// Shadow win rate required for discharge.
    pub discharge_min_winrate: f64,
    /// Consecutive losing closes on one symbol (same ET day) that ban
    /// the symbol for the rest of the day.
    pub symbol_max_consecutive_losses: u32,
}

impl Default for HospitalPolicy {
    fn default() -> Self {
        Self {
            admit_below: 0.30,
            discharge_min_resolved: 10,
            discharge_min_winrate: 0.45,
            symbol_max_consecutive_losses: 2,
        }
    }
}

/// One quarantined playbook.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct QuarantineRecord {
    pub since_ts: i64,
    /// Reliability at admission (for the UI and the discharge audit).
    pub reliability_at_admit: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Hospital {
    pub policy: HospitalPolicy,
    /// Quarantined playbooks keyed by source label (`"gap_fade"`, ...).
    pub quarantined: HashMap<String, QuarantineRecord>,
}

/// Transition produced by one hospital evaluation pass (for logging).
#[derive(Debug, Clone, PartialEq)]
pub enum HospitalTransition {
    Admitted { source: String, reliability: f64 },
    Discharged {
        source: String,
        resolved: u32,
        winrate: f64,
        total_r: f64,
    },
}

impl Hospital {
    pub fn is_quarantined(&self, source: &str) -> bool {
        self.quarantined.contains_key(source)
    }

    /// Run one evaluation pass (called from the nightly calibration,
    /// after reliabilities are updated):
    ///
    /// * Active playbooks with reliability < `admit_below` → admitted.
    /// * Quarantined playbooks whose *shadow* record (aggregated across
    ///   veto stages) clears the discharge bar → discharged. Their
    ///   reliability is reset to the neutral 0.50 prior so the fusion
    ///   doesn't instantly re-admit them on the stale sub-floor value.
    pub fn evaluate(
        &mut self,
        reliability: &mut ReliabilityTable,
        shadow_rows: &[ShadowSummaryRow],
        now_ts: i64,
    ) -> Vec<HospitalTransition> {
        let mut transitions = Vec::new();

        // Admissions — every playbook with a calibrated reliability
        // entry below the bar. (Unknown playbooks default to 0.50 via
        // `get` and are never admitted without evidence.)
        let admit_candidates: Vec<(String, f64)> = reliability
            .map
            .iter()
            .filter(|(source, r)| {
                // P2 — contextual keys (`playbook|regime|session`) are
                // reliability refinements, not playbooks; only global
                // playbook keys are admissible.
                !source.contains('|')
                    && **r < self.policy.admit_below
                    && !self.quarantined.contains_key(*source)
            })
            .map(|(s, r)| (s.clone(), *r))
            .collect();
        for (source, r) in admit_candidates {
            self.quarantined.insert(
                source.clone(),
                QuarantineRecord {
                    since_ts: now_ts,
                    reliability_at_admit: r,
                },
            );
            transitions.push(HospitalTransition::Admitted {
                source,
                reliability: r,
            });
        }

        // Discharges — aggregate the shadow record per source across
        // veto stages (a quarantined playbook's shadows may carry
        // stage "prime" or "quarantine" depending on where the veto
        // landed; the counterfactual evidence is the same).
        let mut per_source: HashMap<&str, (u32, u32, f64)> = HashMap::new();
        for row in shadow_rows {
            let e = per_source.entry(row.source.as_str()).or_default();
            e.0 += row.resolved;
            e.1 += row.wins;
            e.2 += row.total_r;
        }
        let discharge: Vec<String> = self
            .quarantined
            .keys()
            .filter(|source| {
                per_source.get(source.as_str()).is_some_and(|(n, w, r)| {
                    *n >= self.policy.discharge_min_resolved
                        && (*w as f64 / (*n).max(1) as f64) >= self.policy.discharge_min_winrate
                        && *r > 0.0
                })
            })
            .cloned()
            .collect();
        for source in discharge {
            self.quarantined.remove(&source);
            let (n, w, r) = per_source
                .get(source.as_str())
                .copied()
                .unwrap_or_default();
            // Neutral prior on release — the sub-floor value that got
            // the playbook admitted must not survive discharge.
            reliability.map.insert(source.clone(), 0.50);
            transitions.push(HospitalTransition::Discharged {
                source,
                resolved: n,
                winrate: w as f64 / n.max(1) as f64,
                total_r: r,
            });
        }

        transitions
    }
}

/// Per-symbol daily breaker — pure. `pnls_today` are today's (ET)
/// closed P/Ls for one symbol in chronological close order. Returns
/// `true` when the trailing run of consecutive non-positive closes has
/// reached `max_consecutive` (the symbol is banned until tomorrow; a
/// win naturally resets the streak).
pub fn symbol_banned_today(pnls_today: &[f64], max_consecutive: u32) -> bool {
    if max_consecutive == 0 {
        return false;
    }
    let trailing_losses = pnls_today
        .iter()
        .rev()
        .take_while(|p| **p <= 0.0)
        .count();
    trailing_losses >= max_consecutive as usize
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn row(source: &str, resolved: u32, wins: u32, total_r: f64) -> ShadowSummaryRow {
        ShadowSummaryRow {
            veto_stage: "prime".to_string(),
            source: source.to_string(),
            resolved,
            wins,
            total_r,
        }
    }

    #[test]
    fn admits_below_bar_only() {
        let mut hosp = Hospital::default();
        let mut rel = ReliabilityTable::default();
        rel.map.insert("catalyst_frontrun".into(), 0.22);
        rel.map.insert("gap_fade".into(), 0.55);

        let transitions = hosp.evaluate(&mut rel, &[], 1_000);
        assert!(hosp.is_quarantined("catalyst_frontrun"));
        assert!(!hosp.is_quarantined("gap_fade"));
        assert_eq!(transitions.len(), 1);
        match &transitions[0] {
            HospitalTransition::Admitted { source, reliability } => {
                assert_eq!(source, "catalyst_frontrun");
                assert!((reliability - 0.22).abs() < 1e-9);
            }
            other => panic!("expected admission, got {other:?}"),
        }
    }

    #[test]
    fn discharge_requires_shadow_evidence() {
        let mut hosp = Hospital::default();
        let mut rel = ReliabilityTable::default();
        rel.map.insert("catalyst_frontrun".into(), 0.20);
        hosp.evaluate(&mut rel, &[], 1_000);
        assert!(hosp.is_quarantined("catalyst_frontrun"));

        // Not enough resolved shadows → stays admitted.
        let t = hosp.evaluate(&mut rel, &[row("catalyst_frontrun", 4, 3, 2.0)], 2_000);
        assert!(hosp.is_quarantined("catalyst_frontrun"));
        assert!(t.is_empty());

        // Enough shadows but negative total R → stays admitted.
        let t = hosp.evaluate(&mut rel, &[row("catalyst_frontrun", 12, 6, -1.5)], 3_000);
        assert!(hosp.is_quarantined("catalyst_frontrun"));
        assert!(t.is_empty());

        // Full bar cleared → discharged, reliability reset to neutral.
        let t = hosp.evaluate(&mut rel, &[row("catalyst_frontrun", 12, 7, 4.0)], 4_000);
        assert!(!hosp.is_quarantined("catalyst_frontrun"));
        assert_eq!(t.len(), 1);
        assert!((rel.get("catalyst_frontrun") - 0.50).abs() < 1e-9);
        match &t[0] {
            HospitalTransition::Discharged { resolved, total_r, .. } => {
                assert_eq!(*resolved, 12);
                assert!(*total_r > 0.0);
            }
            other => panic!("expected discharge, got {other:?}"),
        }
    }

    #[test]
    fn discharge_aggregates_across_stages() {
        let mut hosp = Hospital::default();
        let mut rel = ReliabilityTable::default();
        rel.map.insert("etf_pair".into(), 0.18);
        hosp.evaluate(&mut rel, &[], 1_000);

        let mut r1 = row("etf_pair", 6, 4, 1.5);
        r1.veto_stage = "prime".to_string();
        let mut r2 = row("etf_pair", 6, 3, 1.0);
        r2.veto_stage = "quarantine".to_string();
        let t = hosp.evaluate(&mut rel, &[r1, r2], 2_000);
        // 12 resolved, 7 wins (58%), +2.5R → discharged.
        assert!(!hosp.is_quarantined("etf_pair"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn symbol_breaker_trips_on_consecutive_losses() {
        // Two trailing losses → banned.
        assert!(symbol_banned_today(&[-3.0, -1.5], 2));
        // Win in between resets the streak.
        assert!(!symbol_banned_today(&[-3.0, 2.0, -1.5], 2));
        // Trailing streak after a win still counts.
        assert!(symbol_banned_today(&[2.0, -1.0, -0.5], 2));
        // Zero P/L counts as a loss (scratch consumed the slot).
        assert!(symbol_banned_today(&[-1.0, 0.0], 2));
        // Not enough closes.
        assert!(!symbol_banned_today(&[-1.0], 2));
        assert!(!symbol_banned_today(&[], 2));
        // Disabled breaker.
        assert!(!symbol_banned_today(&[-1.0, -2.0, -3.0], 0));
    }

    #[test]
    fn persistence_roundtrip_via_engine() {
        let mut hosp = Hospital::default();
        hosp.quarantined.insert(
            "catalyst_frontrun".into(),
            QuarantineRecord {
                since_ts: 123,
                reliability_at_admit: 0.2,
            },
        );
        let json = serde_json::to_string(&hosp).unwrap();
        let back: Hospital = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hosp);
        // Old JSON without the hospital field → defaults.
        let legacy: Hospital = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy, Hospital::default());
    }
}
