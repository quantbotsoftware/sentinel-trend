//! PRIME F2a — Conviction fusion (pure, no I/O, no state).
//!
//! Fuses a raw strategy/FSM signal with playbook reliability, regime
//! fit, catalyst confluence, relative volume, and a spread penalty into
//! a single conviction number in `[0, 1]`. The breakdown is retained so
//! markers / the decision journal can explain *why* an entry fired.
//!
//! Design constraints (R4): every function here is a pure function of
//! its inputs — testable without Tauri, locks, or the network.
//!
//! ## Empirical priors (audit 2026-07-02, 1066 journaled trades)
//!
//! Default reliabilities encode the live P/L audit: `halt_resume`
//! (+$233, PF > 1) starts high; `catalyst_frontrun` (worst book,
//! direction bug fixed in F0.5) starts low and must *earn* its way up
//! via nightly calibration (F5b). Playbooks with zero trades (ETF
//! family) start neutral at 0.50.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::smarttrend::strategies::MarketRegime;

/// Weights of the conviction fusion. Persisted in `prime_weights.json`
/// and adjusted by the nightly auto-calibration (F5b). All weights are
/// non-negative; `spread` acts as a penalty (subtracted).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConvictionWeights {
    pub w_signal: f64,
    pub w_reliability: f64,
    pub w_regime: f64,
    pub w_catalyst: f64,
    pub w_rvol: f64,
    pub w_spread: f64,
    /// Additive cap for the causal-confluence bonus (F5d).
    pub causal_bonus_cap: f64,
    /// Minimum conviction required to dispatch an entry.
    pub threshold: f64,
}

impl Default for ConvictionWeights {
    fn default() -> Self {
        Self {
            w_signal: 0.30,
            w_reliability: 0.25,
            w_regime: 0.15,
            w_catalyst: 0.15,
            w_rvol: 0.10,
            w_spread: 0.05,
            causal_bonus_cap: 0.10,
            threshold: 0.55,
        }
    }
}

/// Per-playbook reliability in `[0.15, 0.90]`, keyed by the playbook
/// source string (`"gap_fade"`, ..., `"fsm"`). Updated nightly by the
/// auto-calibration from the decision journal + trade journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReliabilityTable {
    pub map: HashMap<String, f64>,
}

pub const RELIABILITY_FLOOR: f64 = 0.15;
pub const RELIABILITY_CEIL: f64 = 0.90;

impl Default for ReliabilityTable {
    fn default() -> Self {
        let mut map = HashMap::new();
        // Empirical priors from the 2026-07-02 audit (see module doc).
        // APEX (2026-07-04): the house strategy is engineered directly
        // from the audit's winning patterns (event evidence + morning
        // window + asymmetric holds) — starts trusted just under the
        // proven halt_resume, above every unproven playbook.
        map.insert("apex".into(), 0.65);
        map.insert("halt_resume".into(), 0.72);
        map.insert("gap_fade".into(), 0.55);
        map.insert("opening_range_breakout".into(), 0.50);
        map.insert("fsm".into(), 0.50);
        map.insert("etf_pair".into(), 0.50);
        map.insert("stat_arb_50".into(), 0.50);
        map.insert("leveraged_etf".into(), 0.50);
        map.insert("afternoon_fade".into(), 0.42);
        map.insert("extended_news".into(), 0.40);
        map.insert("catalyst_frontrun".into(), 0.35);
        Self { map }
    }
}

/// P2 — stable lowercase regime key for context-conditional
/// reliability. Self-contained (no `memory_db` dependency).
pub fn regime_key(regime: Option<MarketRegime>) -> &'static str {
    match regime {
        Some(MarketRegime::Calm) => "calm",
        Some(MarketRegime::Trending) => "trending",
        Some(MarketRegime::Volatile) => "volatile",
        Some(MarketRegime::Panic) => "panic",
        None => "none",
    }
}

/// P2 — coarse ET session bucket. Coarse on purpose: with a small
/// trade sample, finer buckets would never accumulate the minimum
/// evidence (`CONTEXT_MIN_TRADES`) needed to activate a context key.
pub fn session_label(et_min: u32) -> &'static str {
    match et_min {
        m if m < 4 * 60 => "off",
        m if m < 9 * 60 + 30 => "pre",
        m if m < 11 * 60 => "open",
        m if m < 14 * 60 + 30 => "mid",
        m if m < 16 * 60 => "close",
        m if m < 20 * 60 => "ext",
        _ => "off",
    }
}

/// P2 — composite reliability key `playbook|regime|session`.
pub fn context_key(source: &str, regime: Option<MarketRegime>, session: &str) -> String {
    format!("{source}|{}|{session}", regime_key(regime))
}

impl ReliabilityTable {
    /// Reliability for `source`, neutral 0.50 for unknown playbooks.
    pub fn get(&self, source: &str) -> f64 {
        self.map.get(source).copied().unwrap_or(0.50)
    }

    /// P2 — context-conditional lookup with hierarchical fallback:
    /// exact `playbook|regime|session` key when the calibration has
    /// produced one (it only does so with enough evidence), otherwise
    /// the playbook-global value. A playbook can therefore be trusted
    /// at the open in a trending tape and distrusted midday in chop —
    /// the empirically-observed failure mode a single global number
    /// cannot express.
    pub fn get_ctx(&self, source: &str, regime: Option<MarketRegime>, session: &str) -> f64 {
        self.map
            .get(&context_key(source, regime, session))
            .copied()
            .unwrap_or_else(|| self.get(source))
    }

    /// F5b calibration update: Laplace-smoothed win rate blended with a
    /// bounded profit-factor term. `wins`/`losses` are closed-trade
    /// counts over the calibration window; `gross_profit`/`gross_loss`
    /// are absolute dollar sums (`gross_loss >= 0`).
    ///
    /// reliability = 0.6 · winrate_smoothed + 0.4 · pf_term, clamped to
    /// `[FLOOR, CEIL]`. With no trades the entry is left untouched so
    /// priors persist until evidence arrives.
    pub fn update_from_outcomes(
        &mut self,
        source: &str,
        wins: u32,
        losses: u32,
        gross_profit: f64,
        gross_loss: f64,
    ) {
        let n = wins + losses;
        if n == 0 {
            return;
        }
        // Laplace smoothing (+1/+2) pulls small samples toward 0.5.
        let winrate = (wins as f64 + 1.0) / (n as f64 + 2.0);
        let pf_term = if gross_loss > 1e-9 {
            let pf = gross_profit / gross_loss;
            // Map PF ∈ [0, ∞) → [0, 1) with PF=1 → 0.5.
            pf / (pf + 1.0)
        } else if gross_profit > 0.0 {
            1.0
        } else {
            0.5
        };
        let r = (0.6 * winrate + 0.4 * pf_term).clamp(RELIABILITY_FLOOR, RELIABILITY_CEIL);
        self.map.insert(source.to_string(), r);
    }
}

/// Playbook × regime fit in `[0, 1]`. Pure lookup with sane defaults:
/// mean-reversion playbooks prefer Calm, momentum playbooks prefer
/// Trending, everything is throttled in Panic.
pub fn regime_fit(source: &str, regime: Option<MarketRegime>) -> f64 {
    use MarketRegime::*;
    let r = match regime {
        Some(r) => r,
        None => return 0.5,
    };
    match source {
        // Mean reversion — best when the tape is quiet.
        "gap_fade" => match r {
            Calm => 0.7,
            Trending => 0.4,
            Volatile => 0.6,
            Panic => 0.3,
        },
        "afternoon_fade" => match r {
            Calm => 0.6,
            Trending => 0.3,
            Volatile => 0.5,
            Panic => 0.3,
        },
        // Momentum / breakout — best when the tape trends.
        "catalyst_frontrun" | "extended_news" => match r {
            Calm => 0.4,
            Trending => 0.7,
            Volatile => 0.6,
            Panic => 0.3,
        },
        "opening_range_breakout" | "fsm" => match r {
            Calm => 0.5,
            Trending => 0.8,
            Volatile => 0.4,
            Panic => 0.2,
        },
        // APEX — volume-event momentum: strongest in a trending tape,
        // still viable in volatile (ignitions are volatility events),
        // throttled in panic like everything else.
        "apex" => match r {
            Calm => 0.6,
            Trending => 0.8,
            Volatile => 0.7,
            Panic => 0.3,
        },
        // Halt resume — volatility event by definition.
        "halt_resume" => match r {
            Calm => 0.6,
            Trending => 0.6,
            Volatile => 0.7,
            Panic => 0.4,
        },
        // Pairs / stat-arb — need stable correlation structure.
        "etf_pair" | "stat_arb_50" => match r {
            Calm => 0.7,
            Trending => 0.5,
            Volatile => 0.4,
            Panic => 0.2,
        },
        // Leveraged ETF momentum — its own gate already refuses
        // Volatile/Panic; the fit mirrors that.
        "leveraged_etf" => match r {
            Calm => 0.7,
            Trending => 0.7,
            Volatile => 0.1,
            Panic => 0.0,
        },
        _ => 0.5,
    }
}

/// Explainable conviction breakdown. Every term is the *weighted*
/// contribution, so `value = Σ(terms) - spread_penalty + causal_bonus`
/// before the final clamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conviction {
    /// Final fused conviction, clamped to `[0, 1]`.
    pub value: f64,
    pub signal: f64,
    pub reliability: f64,
    pub regime_fit: f64,
    pub catalyst: f64,
    pub rvol: f64,
    pub spread_penalty: f64,
    pub causal_bonus: f64,
    /// P3 — the *normalized* (pre-weight) fusion inputs, in the fixed
    /// order `[signal, reliability, regime_fit, catalyst, rvol,
    /// spread, causal]`, all in `[0, 1]`. Recorded in the decision
    /// journal so the Counterfactual Lab can re-fuse historical
    /// decisions under candidate weight vectors offline.
    #[serde(default)]
    pub norm: [f64; 7],
}

/// Raw inputs of one fusion. All fields are plain numbers extracted
/// from `MarketContext` + the signal by the caller — keeps this module
/// dependency-free (R5).
#[derive(Debug, Clone, Copy)]
pub struct FusionInput<'a> {
    /// Playbook source string (`"gap_fade"`, ..., `"fsm"`).
    pub source: &'a str,
    /// Raw signal score 0–100 as emitted by the strategy / FSM.
    pub raw_score: f64,
    pub regime: Option<MarketRegime>,
    /// Scanner catalyst score 0–100 (None = no catalyst data).
    pub catalyst_score: Option<f64>,
    pub rvol: f64,
    pub spread_bps: f64,
    /// Causal-confluence bonus in `[0, 1]` (F5d; 0 until wired).
    pub causal_signal: f64,
    /// P2 — ET session bucket (`session_label`); enables the
    /// context-conditional reliability lookup.
    pub session: &'a str,
}

/// Fuse one candidate into a conviction. Pure.
pub fn fuse(w: &ConvictionWeights, rel: &ReliabilityTable, input: &FusionInput) -> Conviction {
    let signal_norm = (input.raw_score / 100.0).clamp(0.0, 1.0);
    let reliability = rel.get_ctx(input.source, input.regime, input.session);
    let fit = regime_fit(input.source, input.regime);
    // Neutral 0.3 when the scanner has no catalyst row — a missing
    // catalyst must not zero out playbooks that don't need one.
    let catalyst_norm = input
        .catalyst_score
        .map(|s| (s / 100.0).clamp(0.0, 1.0))
        .unwrap_or(0.3);
    // RVOL 4x+ = full marks; <1x decays toward 0.
    let rvol_norm = (input.rvol / 4.0).clamp(0.0, 1.0);
    // 50 bps+ = full penalty.
    let spread_norm = (input.spread_bps / 50.0).clamp(0.0, 1.0);
    let causal = (input.causal_signal.clamp(0.0, 1.0)) * w.causal_bonus_cap;

    let signal_c = w.w_signal * signal_norm;
    let rel_c = w.w_reliability * reliability;
    let regime_c = w.w_regime * fit;
    let catalyst_c = w.w_catalyst * catalyst_norm;
    let rvol_c = w.w_rvol * rvol_norm;
    let spread_c = w.w_spread * spread_norm;

    let value = (signal_c + rel_c + regime_c + catalyst_c + rvol_c - spread_c + causal)
        .clamp(0.0, 1.0);

    Conviction {
        value,
        signal: signal_c,
        reliability: rel_c,
        regime_fit: regime_c,
        catalyst: catalyst_c,
        rvol: rvol_c,
        spread_penalty: spread_c,
        causal_bonus: causal,
        norm: [
            signal_norm,
            reliability,
            fit,
            catalyst_norm,
            rvol_norm,
            spread_norm,
            input.causal_signal.clamp(0.0, 1.0),
        ],
    }
}

/// P3 — re-fuse recorded normalized components under an arbitrary
/// weight vector. Must mirror [`fuse`]'s arithmetic exactly (the
/// Counterfactual Lab's validity depends on this equivalence, locked
/// by `refuse_matches_fuse` below).
pub fn refuse(w: &ConvictionWeights, norm: &[f64; 7]) -> f64 {
    (w.w_signal * norm[0]
        + w.w_reliability * norm[1]
        + w.w_regime * norm[2]
        + w.w_catalyst * norm[3]
        + w.w_rvol * norm[4]
        - w.w_spread * norm[5]
        + w.causal_bonus_cap * norm[6])
        .clamp(0.0, 1.0)
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input(source: &str) -> FusionInput<'_> {
        FusionInput {
            source,
            raw_score: 70.0,
            regime: Some(MarketRegime::Calm),
            catalyst_score: Some(60.0),
            rvol: 2.0,
            spread_bps: 10.0,
            causal_signal: 0.0,
            session: "open",
        }
    }

    #[test]
    fn refuse_matches_fuse() {
        let w = ConvictionWeights::default();
        let rel = ReliabilityTable::default();
        for source in ["gap_fade", "halt_resume", "fsm"] {
            let mut input = base_input(source);
            input.causal_signal = 0.4;
            let c = fuse(&w, &rel, &input);
            assert!(
                (refuse(&w, &c.norm) - c.value).abs() < 1e-12,
                "{source}: refuse {} vs fuse {}",
                refuse(&w, &c.norm),
                c.value
            );
        }
    }

    #[test]
    fn session_label_buckets() {
        assert_eq!(session_label(3 * 60), "off");
        assert_eq!(session_label(5 * 60), "pre");
        assert_eq!(session_label(9 * 60 + 30), "open");
        assert_eq!(session_label(12 * 60), "mid");
        assert_eq!(session_label(15 * 60), "close");
        assert_eq!(session_label(17 * 60), "ext");
        assert_eq!(session_label(21 * 60), "off");
    }

    #[test]
    fn contextual_reliability_overrides_global() {
        let mut rel = ReliabilityTable::default();
        // Global gap_fade prior is 0.55; the context key says gap_fade
        // is terrible midday in a trending tape.
        rel.map.insert(
            context_key("gap_fade", Some(MarketRegime::Trending), "mid"),
            0.20,
        );
        assert!(
            (rel.get_ctx("gap_fade", Some(MarketRegime::Trending), "mid") - 0.20).abs() < 1e-9
        );
        // Other contexts fall back to the global prior.
        assert!(
            (rel.get_ctx("gap_fade", Some(MarketRegime::Calm), "open") - 0.55).abs() < 1e-9
        );
        // Fusion consumes the contextual value.
        let w = ConvictionWeights::default();
        let mut input = base_input("gap_fade");
        input.regime = Some(MarketRegime::Trending);
        input.session = "mid";
        let ctx_conv = fuse(&w, &rel, &input);
        input.session = "open";
        let global_conv = fuse(&w, &rel, &input);
        assert!(ctx_conv.value < global_conv.value);
    }

    #[test]
    fn conviction_in_unit_interval() {
        let w = ConvictionWeights::default();
        let rel = ReliabilityTable::default();
        for source in [
            "gap_fade",
            "halt_resume",
            "catalyst_frontrun",
            "fsm",
            "unknown_playbook",
        ] {
            let c = fuse(&w, &rel, &base_input(source));
            assert!((0.0..=1.0).contains(&c.value), "{source}: {}", c.value);
        }
    }

    #[test]
    fn higher_reliability_yields_higher_conviction() {
        let w = ConvictionWeights::default();
        let rel = ReliabilityTable::default();
        let strong = fuse(&w, &rel, &base_input("halt_resume"));
        let weak = fuse(&w, &rel, &base_input("catalyst_frontrun"));
        assert!(
            strong.value > weak.value,
            "halt_resume {} vs catalyst_frontrun {}",
            strong.value,
            weak.value
        );
    }

    #[test]
    fn spread_penalty_reduces_conviction() {
        let w = ConvictionWeights::default();
        let rel = ReliabilityTable::default();
        let tight = fuse(&w, &rel, &base_input("gap_fade"));
        let mut wide_in = base_input("gap_fade");
        wide_in.spread_bps = 100.0;
        let wide = fuse(&w, &rel, &wide_in);
        assert!(tight.value > wide.value);
    }

    #[test]
    fn missing_catalyst_uses_neutral_not_zero() {
        let w = ConvictionWeights::default();
        let rel = ReliabilityTable::default();
        let mut input = base_input("gap_fade");
        input.catalyst_score = None;
        let none = fuse(&w, &rel, &input);
        input.catalyst_score = Some(0.0);
        let zero = fuse(&w, &rel, &input);
        assert!(none.value > zero.value);
    }

    #[test]
    fn causal_bonus_is_capped() {
        let w = ConvictionWeights::default();
        let rel = ReliabilityTable::default();
        let mut input = base_input("gap_fade");
        input.causal_signal = 5.0; // out of range on purpose
        let c = fuse(&w, &rel, &input);
        assert!(c.causal_bonus <= w.causal_bonus_cap + 1e-12);
    }

    #[test]
    fn leveraged_etf_panic_fit_zero() {
        assert_eq!(regime_fit("leveraged_etf", Some(MarketRegime::Panic)), 0.0);
        assert_eq!(regime_fit("leveraged_etf", None), 0.5);
    }

    #[test]
    fn breakdown_sums_to_value_within_clamp() {
        let w = ConvictionWeights::default();
        let rel = ReliabilityTable::default();
        let c = fuse(&w, &rel, &base_input("gap_fade"));
        let sum = c.signal + c.reliability + c.regime_fit + c.catalyst + c.rvol
            - c.spread_penalty
            + c.causal_bonus;
        assert!((sum.clamp(0.0, 1.0) - c.value).abs() < 1e-12);
    }

    #[test]
    fn calibration_moves_reliability_with_evidence() {
        let mut rel = ReliabilityTable::default();
        let before = rel.get("catalyst_frontrun");
        // 12 wins / 4 losses, PF 2.5 — should lift reliability.
        rel.update_from_outcomes("catalyst_frontrun", 12, 4, 500.0, 200.0);
        let after = rel.get("catalyst_frontrun");
        assert!(after > before, "{before} -> {after}");
        assert!(after <= RELIABILITY_CEIL);

        // Disaster window drags it toward the floor.
        rel.update_from_outcomes("catalyst_frontrun", 1, 19, 20.0, 900.0);
        let floor_ward = rel.get("catalyst_frontrun");
        assert!(floor_ward < after);
        assert!(floor_ward >= RELIABILITY_FLOOR);
    }

    #[test]
    fn calibration_no_trades_is_noop() {
        let mut rel = ReliabilityTable::default();
        let before = rel.get("gap_fade");
        rel.update_from_outcomes("gap_fade", 0, 0, 0.0, 0.0);
        assert_eq!(rel.get("gap_fade"), before);
    }
}
