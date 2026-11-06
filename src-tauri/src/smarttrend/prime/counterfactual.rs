//! P3 — Counterfactual Lab.
//!
//! Nightly offline grid search over the conviction fusion weights,
//! driven by *real recorded decisions* instead of synthetic backtests.
//! Every PRIME-fused decision (entered or vetoed) persists its
//! normalized component vector in the decision journal; entered
//! decisions get their outcome from the trade journal, vetoed ones
//! from the shadow book. Re-fusing those vectors under a candidate
//! weight set answers, per candidate: *"with these weights, which of
//! the last N decisions would have fired, and what would that
//! selection have earned in R?"*
//!
//! The lab never applies anything by itself. It writes
//! [`SuggestedWeights`] onto the engine (persisted in
//! `prime_engine.json`); the operator reviews the evidence in the
//! PRIME tab and applies with one click (`apply_suggested_weights`).
//! Human-in-the-loop by design: an automated weight flip driven by a
//! 30-day sample would be curve-fitting with extra steps.
//!
//! Pure module (R4): no locks, no I/O — callers assemble
//! [`LabeledDecision`]s and persist the result.

use serde::{Deserialize, Serialize};

use super::conviction::{refuse, ConvictionWeights};

/// One historical decision the lab can re-fuse: the recorded
/// normalized component vector plus the realized (or counterfactual)
/// outcome in R-multiples.
#[derive(Debug, Clone)]
pub struct LabeledDecision {
    /// `[signal, reliability, regime_fit, catalyst, rvol, spread,
    /// causal]` as recorded at fusion time.
    pub norm: [f64; 7],
    /// Signed R outcome: real trades report `pnl / risk_unit`; shadow
    /// resolutions report their counterfactual `pnl_r`.
    pub outcome_r: f64,
}

/// Minimum labeled sample before the lab will emit a suggestion.
/// Below this the grid search is dominated by noise.
pub const MIN_SAMPLE: usize = 20;
/// Minimum decisions a candidate weight set must select — a weight
/// set that "wins" by trading twice is degenerate.
pub const MIN_SELECTED: usize = 5;
/// Required improvement over baseline, in R, before suggesting.
pub const MIN_IMPROVEMENT_R: f64 = 1.0;

/// Multiplicative perturbation grid per weight dimension.
const FACTORS: [f64; 3] = [0.7, 1.0, 1.3];
/// Candidate thresholds swept alongside the weight grid.
const THRESHOLDS: [f64; 4] = [0.50, 0.55, 0.60, 0.65];

/// Outcome of one nightly lab run, persisted on the engine for the
/// operator to review + apply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuggestedWeights {
    pub weights: ConvictionWeights,
    /// Total R the CURRENT weights would have selected on the sample.
    pub baseline_r: f64,
    /// Total R the suggested weights would have selected.
    pub suggested_r: f64,
    /// Decisions selected by baseline / suggested weights.
    pub baseline_selected: usize,
    pub suggested_selected: usize,
    /// Labeled sample size the search ran on.
    pub sample: usize,
    pub generated_ts: i64,
}

/// One PRIME-fused decision extracted from the decision journal
/// (plain tuple form — keeps this module free of `ops` imports, R5).
#[derive(Debug, Clone)]
pub struct RecordedDecision {
    pub norm: [f64; 7],
    pub symbol: String,
    pub source: String,
    pub ts: i64,
    /// true = "entered" (outcome in the trade journal), false =
    /// PRIME-vetoed (outcome in the shadow book).
    pub entered: bool,
}

/// One real closed trade, for matching against entered decisions.
#[derive(Debug, Clone)]
pub struct RealClose {
    pub symbol: String,
    /// Playbook (empty / legacy folds to "fsm" by the caller).
    pub source: String,
    pub entry_time: i64,
    pub pnl: f64,
}

/// One resolved shadow, for matching against vetoed decisions.
#[derive(Debug, Clone)]
pub struct ShadowOutcome {
    pub symbol: String,
    pub source: String,
    pub opened_ts: i64,
    pub pnl_r: f64,
}

/// Match window between a decision row and its outcome row. Entered
/// decisions can lag their fill by the order round-trip; shadows are
/// opened in the same dispatch tick as the veto.
const REAL_MATCH_SECS: i64 = 600;
const SHADOW_MATCH_SECS: i64 = 120;

/// Join recorded decisions with their outcomes into the labeled
/// sample the grid search consumes.
///
/// Real trades have dollar P/L but no recorded risk unit
/// (`JournalEntry` carries no stop), so R is estimated empirically:
/// **1R = the mean |P/L| of the losing trades in the window** (the
/// classic realized-risk estimator). Falls back to mean |P/L| of all
/// trades when the window has no losers; skips real outcomes entirely
/// when it has no trades. Shadow outcomes already carry an exact
/// `pnl_r` (stop distance was recorded at veto time).
pub fn build_labeled_sample(
    decisions: &[RecordedDecision],
    real: &[RealClose],
    shadows: &[ShadowOutcome],
) -> Vec<LabeledDecision> {
    let losses: Vec<f64> = real
        .iter()
        .filter(|c| c.pnl < 0.0)
        .map(|c| c.pnl.abs())
        .collect();
    let r_unit = if !losses.is_empty() {
        losses.iter().sum::<f64>() / losses.len() as f64
    } else if !real.is_empty() {
        real.iter().map(|c| c.pnl.abs()).sum::<f64>() / real.len() as f64
    } else {
        0.0
    };

    let mut sample = Vec::new();
    for d in decisions {
        let outcome_r = if d.entered {
            if r_unit <= 1e-9 {
                continue;
            }
            match real.iter().find(|c| {
                c.symbol == d.symbol
                    && c.source == d.source
                    && (c.entry_time - d.ts).abs() <= REAL_MATCH_SECS
            }) {
                Some(c) => c.pnl / r_unit,
                None => continue,
            }
        } else {
            match shadows.iter().find(|s| {
                s.symbol == d.symbol
                    && s.source == d.source
                    && (s.opened_ts - d.ts).abs() <= SHADOW_MATCH_SECS
            }) {
                Some(s) => s.pnl_r,
                None => continue,
            }
        };
        sample.push(LabeledDecision {
            norm: d.norm,
            outcome_r,
        });
    }
    sample
}

/// Score one weight set: total R over the decisions it would select
/// (re-fused conviction ≥ threshold), plus how many it selects.
fn score(w: &ConvictionWeights, sample: &[LabeledDecision]) -> (f64, usize) {
    let mut total_r = 0.0;
    let mut selected = 0usize;
    for d in sample {
        if refuse(w, &d.norm) >= w.threshold {
            total_r += d.outcome_r;
            selected += 1;
        }
    }
    (total_r, selected)
}

/// Grid search around `current`. Returns `None` when the sample is too
/// small or no candidate clears the improvement bar — "no suggestion"
/// is a first-class result, not a failure.
pub fn grid_search(
    current: &ConvictionWeights,
    sample: &[LabeledDecision],
    now_ts: i64,
) -> Option<SuggestedWeights> {
    if sample.len() < MIN_SAMPLE {
        return None;
    }
    let (baseline_r, baseline_selected) = score(current, sample);

    let mut best: Option<(f64, usize, ConvictionWeights)> = None;
    // 3^6 weight combos × 4 thresholds ≈ 2916 evaluations — trivial
    // offline. `causal_bonus_cap` is calibrated by its own loop (F5d)
    // and stays fixed here.
    for f_sig in FACTORS {
        for f_rel in FACTORS {
            for f_reg in FACTORS {
                for f_cat in FACTORS {
                    for f_rvol in FACTORS {
                        for f_spr in FACTORS {
                            for thr in THRESHOLDS {
                                let w = ConvictionWeights {
                                    w_signal: current.w_signal * f_sig,
                                    w_reliability: current.w_reliability * f_rel,
                                    w_regime: current.w_regime * f_reg,
                                    w_catalyst: current.w_catalyst * f_cat,
                                    w_rvol: current.w_rvol * f_rvol,
                                    w_spread: current.w_spread * f_spr,
                                    causal_bonus_cap: current.causal_bonus_cap,
                                    threshold: thr,
                                };
                                let (r, sel) = score(&w, sample);
                                if sel < MIN_SELECTED {
                                    continue;
                                }
                                let better = match &best {
                                    None => true,
                                    Some((best_r, _, _)) => r > *best_r,
                                };
                                if better {
                                    best = Some((r, sel, w));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let (best_r, best_selected, weights) = best?;
    if best_r < baseline_r + MIN_IMPROVEMENT_R {
        return None;
    }
    Some(SuggestedWeights {
        weights,
        baseline_r,
        suggested_r: best_r,
        baseline_selected,
        suggested_selected: best_selected,
        sample: sample.len(),
        generated_ts: now_ts,
    })
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// Sample engineered so the DEFAULT weights get it exactly wrong:
    /// winners (+2R) ride a strong catalyst but a weak raw signal, so
    /// they fuse *below* the default threshold; losers (−1R) flash a
    /// strong raw signal + rvol and fuse *above* it. Baseline therefore
    /// selects only the losers. A grid search must find weights (e.g.
    /// signal down / catalyst up / threshold shift) that flip the
    /// selection.
    fn catalyst_split_sample(n_each: usize) -> Vec<LabeledDecision> {
        let mut v = Vec::new();
        for _ in 0..n_each {
            v.push(LabeledDecision {
                //     sig   rel  fit  cat   rvol spr  causal
                norm: [0.30, 0.5, 0.5, 0.95, 0.4, 0.1, 0.0],
                outcome_r: 2.0,
            });
            v.push(LabeledDecision {
                norm: [0.95, 0.5, 0.5, 0.05, 0.9, 0.1, 0.0],
                outcome_r: -1.0,
            });
        }
        v
    }

    #[test]
    fn small_sample_yields_no_suggestion() {
        let current = ConvictionWeights::default();
        let sample = catalyst_split_sample(3); // 6 < MIN_SAMPLE
        assert!(grid_search(&current, &sample, 0).is_none());
    }

    #[test]
    fn finds_separating_weights_on_catalyst_split() {
        let current = ConvictionWeights::default();
        let sample = catalyst_split_sample(20);
        let s = grid_search(&current, &sample, 1_700_000_000)
            .expect("separable sample must produce a suggestion");
        assert!(s.suggested_r > s.baseline_r);
        // The winning selection should skew toward the +2R half.
        assert!(s.suggested_r > 0.0);
        assert_eq!(s.sample, 40);
        assert!(s.suggested_selected >= MIN_SELECTED);
    }

    #[test]
    fn no_suggestion_when_baseline_already_optimal() {
        // All decisions profitable — selecting everything is optimal,
        // which the default weights already roughly do; requiring
        // +1R improvement over a baseline that captures all of it
        // should usually return None. Build the degenerate case where
        // baseline literally selects everything.
        let current = ConvictionWeights {
            threshold: 0.0,
            ..ConvictionWeights::default()
        };
        let sample: Vec<LabeledDecision> = (0..30)
            .map(|_| LabeledDecision {
                norm: [0.8, 0.6, 0.7, 0.5, 0.5, 0.1, 0.0],
                outcome_r: 1.0,
            })
            .collect();
        assert!(grid_search(&current, &sample, 0).is_none());
    }

    #[test]
    fn labeled_sample_joins_real_and_shadow_outcomes() {
        let decisions = vec![
            RecordedDecision {
                norm: [0.8; 7],
                symbol: "AAPL".into(),
                source: "gap_fade".into(),
                ts: 1000,
                entered: true,
            },
            RecordedDecision {
                norm: [0.4; 7],
                symbol: "TSLA".into(),
                source: "halt_resume".into(),
                ts: 2000,
                entered: false,
            },
            // Entered but no matching close (order rejected) — dropped.
            RecordedDecision {
                norm: [0.5; 7],
                symbol: "NVDA".into(),
                source: "fsm".into(),
                ts: 3000,
                entered: true,
            },
        ];
        let real = vec![
            RealClose {
                symbol: "AAPL".into(),
                source: "gap_fade".into(),
                entry_time: 1090,
                pnl: 20.0,
            },
            // Loser establishing the R unit (mean |loss| = 10).
            RealClose {
                symbol: "XYZ".into(),
                source: "gap_fade".into(),
                entry_time: 500,
                pnl: -10.0,
            },
        ];
        let shadows = vec![ShadowOutcome {
            symbol: "TSLA".into(),
            source: "halt_resume".into(),
            opened_ts: 2010,
            pnl_r: -0.8,
        }];
        let sample = build_labeled_sample(&decisions, &real, &shadows);
        assert_eq!(sample.len(), 2);
        // AAPL: +20 / 10 = +2R.
        assert!((sample[0].outcome_r - 2.0).abs() < 1e-9);
        assert!((sample[1].outcome_r + 0.8).abs() < 1e-9);
    }

    #[test]
    fn labeled_sample_no_real_trades_skips_entered() {
        let decisions = vec![RecordedDecision {
            norm: [0.8; 7],
            symbol: "AAPL".into(),
            source: "gap_fade".into(),
            ts: 1000,
            entered: true,
        }];
        assert!(build_labeled_sample(&decisions, &[], &[]).is_empty());
    }

    #[test]
    fn degenerate_selection_rejected() {
        // Only 2 decisions would ever be selected by any weight set
        // above threshold 0.5 — under MIN_SELECTED, so no suggestion
        // even though those 2 are hugely profitable.
        let current = ConvictionWeights::default();
        let mut sample: Vec<LabeledDecision> = (0..30)
            .map(|_| LabeledDecision {
                norm: [0.01, 0.01, 0.01, 0.01, 0.01, 0.9, 0.0],
                outcome_r: 5.0,
            })
            .collect();
        sample.push(LabeledDecision {
            norm: [0.9, 0.9, 0.9, 0.9, 0.9, 0.0, 0.0],
            outcome_r: 10.0,
        });
        sample.push(LabeledDecision {
            norm: [0.9, 0.9, 0.9, 0.9, 0.9, 0.0, 0.0],
            outcome_r: 10.0,
        });
        // Low-component rows fuse ≈ 0 under any positive weights and
        // FACTORS≤1.3 with thresholds ≥ 0.5, so max selectable = 2 <
        // MIN_SELECTED.
        assert!(grid_search(&current, &sample, 0).is_none());
    }
}
