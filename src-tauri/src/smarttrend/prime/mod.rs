//! PRIME — the unified conviction engine (F2).
//!
//! One brain in front of the order path. Tournament strategies and the
//! legacy FSM stop being independent order sources: they become signal
//! *candidates*, fused by [`conviction`] into a single conviction
//! number, arbitrated per symbol per bar by [`PrimeEngine::decide`],
//! sized by [`capital`], and managed by the asymmetric [`exits`]
//! ladder.
//!
//! Sub-modules (all pure, R4-compliant):
//! * [`conviction`] — fusion + reliability + regime fit.
//! * [`capital`]   — conviction-scaled sizing with exposure caps.
//! * [`exits`]     — hard-stop / breakeven / ATR-trail / time-stop.
//!
//! The engine itself owns only serializable policy (weights,
//! reliability, capital + exit policies) persisted as
//! `prime_engine.json` in the app data dir. It holds no market state
//! and takes no locks — callers extract plain numbers first (R5).

pub mod calibration;
pub mod capital;
pub mod conviction;
pub mod counterfactual;
pub mod exits;
pub mod hospital;
pub mod shadow;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use capital::CapitalPolicy;
pub use conviction::{Conviction, ConvictionWeights, FusionInput, ReliabilityTable};
pub use exits::ExitPolicy;
pub use hospital::Hospital;

use crate::smarttrend::strategies::MarketRegime;

/// One entry candidate awaiting arbitration. `source` is the playbook
/// label (`"gap_fade"`, ..., `"fsm"`), `side_long` avoids importing
/// `state::Direction` here (keeps this module state-free).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub source: String,
    pub side_long: bool,
    pub raw_score: f64,
}

/// Arbitration outcome for one candidate.
#[derive(Debug, Clone)]
pub enum Ruling {
    /// Winner — dispatch with this conviction.
    Enter(Conviction),
    /// Vetoed with a stable reason label for the decision journal.
    Veto {
        conviction: Conviction,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PrimeEngine {
    pub enabled: bool,
    pub weights: ConvictionWeights,
    pub reliability: ReliabilityTable,
    pub capital: CapitalPolicy,
    pub exit_policy: ExitPolicy,
    /// P1 — Strategy Hospital (playbook quarantine + policy).
    pub hospital: Hospital,
    /// P3 — latest Counterfactual Lab suggestion awaiting operator
    /// review. `None` = no suggestion (insufficient sample or the
    /// current weights already win). Cleared on apply.
    pub suggested_weights: Option<counterfactual::SuggestedWeights>,
    /// Unix ts of the last nightly calibration (F5b), 0 = never.
    pub last_calibration_ts: i64,
}

impl Default for PrimeEngine {
    fn default() -> Self {
        Self {
            enabled: true,
            weights: ConvictionWeights::default(),
            reliability: ReliabilityTable::default(),
            capital: CapitalPolicy::default(),
            exit_policy: ExitPolicy::default(),
            hospital: Hospital::default(),
            suggested_weights: None,
            last_calibration_ts: 0,
        }
    }
}

/// Canonical persistence location: `<data_dir>/prime_engine.json`.
pub fn default_engine_path() -> PathBuf {
    crate::paths::data_dir().join("prime_engine.json")
}

impl PrimeEngine {
    /// Load from disk, falling back to defaults on any error (missing
    /// file at first boot is the normal path).
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<Self>(&raw) {
                Ok(engine) => engine,
                Err(e) => {
                    tracing::warn!(
                        component = "prime",
                        error = %e,
                        "prime_engine.json unparsable — using defaults"
                    );
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Atomic-ish save (write temp, rename) mirroring the persistence
    /// conventions elsewhere in `ops/`.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Fuse one candidate. Thin wrapper so callers only need the
    /// engine handle.
    pub fn fuse(&self, input: &FusionInput) -> Conviction {
        conviction::fuse(&self.weights, &self.reliability, input)
    }

    /// Per-symbol per-bar arbitration.
    ///
    /// 1. Fuse every candidate into a conviction.
    /// 2. Direction consensus: sum conviction per side; the dominant
    ///    side wins, the losing side is vetoed (`conviction_conflict`).
    ///    This replaces the old "first strategy in registry order wins"
    ///    C2 tie-break with an evidence-weighted vote.
    /// 3. Threshold: dominant-side candidates below
    ///    `weights.threshold` are vetoed (`below_threshold`).
    /// 4. Single order per symbol per bar: only the highest-conviction
    ///    survivor enters; the rest are vetoed (`outranked`) — this is
    ///    what stops three playbooks from stacking triple size into
    ///    the same ticker on the same signal.
    ///
    /// Returns rulings in the same order as `candidates`.
    ///
    /// `causal_alignment` is the signed lead-lag confluence in
    /// `[-1, 1]` (F5d): positive = leaders moved bullishly, negative =
    /// bearishly. Long candidates earn the positive part as bonus,
    /// short candidates the negative part — confluence against the
    /// candidate's side simply contributes 0 (never a penalty; the
    /// conviction threshold already filters weak setups).
    /// P1 — quarantined playbooks lose arbitration eligibility before
    /// fusion outcomes matter: they are excluded from the direction
    /// vote and from the winner search, and always come back as
    /// `Veto { reason: "quarantined" }` (the caller's shadow-book hook
    /// keeps trading them counterfactually, which is exactly the
    /// evidence the hospital needs for discharge).
    /// `session` is the ET session bucket
    /// ([`conviction::session_label`]) — P2 routes the reliability
    /// lookup through `playbook|regime|session` context keys.
    #[allow(clippy::too_many_arguments)]
    pub fn decide(
        &self,
        candidates: &[Candidate],
        regime: Option<MarketRegime>,
        catalyst_score: Option<f64>,
        rvol: f64,
        spread_bps: f64,
        causal_alignment: f64,
        session: &str,
    ) -> Vec<Ruling> {
        let convictions: Vec<Conviction> = candidates
            .iter()
            .map(|c| {
                let causal_signal = if c.side_long {
                    causal_alignment.max(0.0)
                } else {
                    (-causal_alignment).max(0.0)
                };
                self.fuse(&FusionInput {
                    source: &c.source,
                    raw_score: c.raw_score,
                    regime,
                    catalyst_score,
                    rvol,
                    spread_bps,
                    causal_signal,
                    session,
                })
            })
            .collect();

        // P1 — quarantine mask, applied before the vote.
        let eligible: Vec<bool> = candidates
            .iter()
            .map(|c| !self.hospital.is_quarantined(&c.source))
            .collect();

        // 2) Direction consensus (eligible candidates only).
        let long_sum: f64 = candidates
            .iter()
            .zip(&convictions)
            .zip(&eligible)
            .filter(|((c, _), e)| c.side_long && **e)
            .map(|((_, cv), _)| cv.value)
            .sum();
        let short_sum: f64 = candidates
            .iter()
            .zip(&convictions)
            .zip(&eligible)
            .filter(|((c, _), e)| !c.side_long && **e)
            .map(|((_, cv), _)| cv.value)
            .sum();
        let dominant_long = long_sum >= short_sum;

        // 4) Highest-conviction eligible survivor on the dominant side.
        let winner_idx = candidates
            .iter()
            .zip(&convictions)
            .zip(&eligible)
            .enumerate()
            .filter(|(_, ((c, cv), e))| {
                **e && c.side_long == dominant_long && cv.value >= self.weights.threshold
            })
            .max_by(|(_, ((_, a), _)), (_, ((_, b), _))| {
                a.value
                    .partial_cmp(&b.value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i);

        candidates
            .iter()
            .zip(convictions)
            .zip(eligible)
            .enumerate()
            .map(|(i, ((c, cv), e))| {
                if !e {
                    Ruling::Veto {
                        conviction: cv,
                        reason: "quarantined",
                    }
                } else if Some(i) == winner_idx {
                    Ruling::Enter(cv)
                } else if c.side_long != dominant_long {
                    Ruling::Veto {
                        conviction: cv,
                        reason: "conviction_conflict",
                    }
                } else if cv.value < self.weights.threshold {
                    Ruling::Veto {
                        conviction: cv,
                        reason: "below_threshold",
                    }
                } else {
                    Ruling::Veto {
                        conviction: cv,
                        reason: "outranked",
                    }
                }
            })
            .collect()
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(source: &str, long: bool, score: f64) -> Candidate {
        Candidate {
            source: source.to_string(),
            side_long: long,
            raw_score: score,
        }
    }

    fn decide_default(cands: &[Candidate]) -> Vec<Ruling> {
        PrimeEngine::default().decide(
            cands,
            Some(MarketRegime::Calm),
            Some(60.0),
            2.5,
            8.0,
            0.0,
            "open",
        )
    }

    fn winners(rulings: &[Ruling]) -> Vec<usize> {
        rulings
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Ruling::Enter(_)))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn single_strong_candidate_enters() {
        let rulings = decide_default(&[cand("halt_resume", true, 85.0)]);
        assert_eq!(winners(&rulings), vec![0]);
    }

    #[test]
    fn at_most_one_winner_per_symbol() {
        let rulings = decide_default(&[
            cand("halt_resume", true, 85.0),
            cand("gap_fade", true, 80.0),
            cand("fsm", true, 90.0),
        ]);
        assert_eq!(winners(&rulings).len(), 1);
        // Losers on the same side are 'outranked' or 'below_threshold'.
        for (i, r) in rulings.iter().enumerate() {
            if let Ruling::Veto { reason, .. } = r {
                assert!(
                    *reason == "outranked" || *reason == "below_threshold",
                    "idx {i}: {reason}"
                );
            }
        }
    }

    #[test]
    fn direction_conflict_vetoes_minority_side() {
        let rulings = decide_default(&[
            cand("halt_resume", true, 85.0),
            cand("gap_fade", true, 80.0),
            cand("catalyst_frontrun", false, 88.0),
        ]);
        // Two longs outvote one short despite its higher raw score.
        match &rulings[2] {
            Ruling::Veto { reason, .. } => assert_eq!(*reason, "conviction_conflict"),
            other => panic!("expected conflict veto, got {other:?}"),
        }
        assert_eq!(winners(&rulings).len(), 1);
    }

    #[test]
    fn weak_lone_candidate_below_threshold() {
        let engine = PrimeEngine::default();
        // catalyst_frontrun prior 0.35 + weak score + poor regime fit
        // in Panic + no catalyst → under 0.55.
        let rulings = engine.decide(
            &[cand("catalyst_frontrun", true, 20.0)],
            Some(MarketRegime::Panic),
            None,
            0.5,
            40.0,
            0.0,
            "open",
        );
        match &rulings[0] {
            Ruling::Veto { reason, conviction } => {
                assert_eq!(*reason, "below_threshold");
                assert!(conviction.value < engine.weights.threshold);
            }
            other => panic!("expected threshold veto, got {other:?}"),
        }
    }

    #[test]
    fn empty_candidates_empty_rulings() {
        assert!(decide_default(&[]).is_empty());
    }

    #[test]
    fn quarantined_playbook_always_vetoed_and_never_wins() {
        let mut engine = PrimeEngine::default();
        engine.hospital.quarantined.insert(
            "halt_resume".into(),
            hospital::QuarantineRecord {
                since_ts: 0,
                reliability_at_admit: 0.2,
            },
        );
        // halt_resume has the strongest signal but is quarantined —
        // the weaker gap_fade must win instead.
        let rulings = engine.decide(
            &[
                cand("halt_resume", true, 95.0),
                cand("gap_fade", true, 80.0),
            ],
            Some(MarketRegime::Calm),
            Some(60.0),
            2.5,
            8.0,
            0.0,
            "open",
        );
        match &rulings[0] {
            Ruling::Veto { reason, .. } => assert_eq!(*reason, "quarantined"),
            other => panic!("expected quarantine veto, got {other:?}"),
        }
        assert!(matches!(rulings[1], Ruling::Enter(_)));
    }

    #[test]
    fn causal_alignment_boosts_matching_side_only() {
        let engine = PrimeEngine::default();
        let base = |alignment: f64, long: bool| {
            let rulings = engine.decide(
                &[cand("gap_fade", long, 70.0)],
                Some(MarketRegime::Calm),
                Some(60.0),
                2.0,
                8.0,
                alignment,
                "open",
            );
            match &rulings[0] {
                Ruling::Enter(cv) => cv.value,
                Ruling::Veto { conviction, .. } => conviction.value,
            }
        };
        // Bullish confluence boosts the long, not the short.
        assert!(base(0.8, true) > base(0.0, true));
        assert!((base(0.8, false) - base(0.0, false)).abs() < 1e-12);
        // Bearish confluence mirrors.
        assert!(base(-0.8, false) > base(0.0, false));
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "prime_engine_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prime_engine.json");

        let mut engine = PrimeEngine::default();
        engine.weights.threshold = 0.61;
        engine
            .reliability
            .map
            .insert("gap_fade".into(), 0.66);
        engine.save(&path).unwrap();

        let loaded = PrimeEngine::load_or_default(&path);
        assert_eq!(loaded, engine);

        // Corrupt file falls back to defaults.
        std::fs::write(&path, "{not json").unwrap();
        let fallback = PrimeEngine::load_or_default(&path);
        assert_eq!(fallback, PrimeEngine::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_defaults() {
        let loaded =
            PrimeEngine::load_or_default(Path::new("/nonexistent/prime_engine.json"));
        assert_eq!(loaded, PrimeEngine::default());
    }
}
