// ═══════════════════════════════════════════════════════════════════
//  S13 — Catalyst Engine (pure functions, ZERO locks, ZERO I/O)
//
//  Quantifies "is this name moving for a reason worth trading?" via
//  five binary flags and a 0-100 composite score, plus a UI-ready
//  list of pills.
//
//    - earnings     — symbol on earnings calendar (FMP feed)
//    - news_recent  — at least one Alpaca news item within lookback
//    - halted       — Alpaca SIP status sub OR RTH heuristic
//    - gap_dir      — open vs prev close > gap_threshold
//    - vol_spike    — RVOL >= rvol_threshold
//
//  HALT semantics (HFT lens): a halted symbol is NEVER a tradable
//  catalyst. `compute` short-circuits to `score=0, pills=["HALT"]`.
//  Strategy code may still surface halted candidates for visibility
//  (e.g. dimmed in the UI by S14) — exclusion is a strategy concern,
//  not a catalyst concern.
//
//  CONFIG SHAPE (config.rs):
//    RuntimeConfig.catalyst.weights        ← these struct's defaults
//    RuntimeConfig.catalyst.news_enabled
//    RuntimeConfig.catalyst.news_lookback_minutes
// ═══════════════════════════════════════════════════════════════════

use std::collections::HashMap;

use chrono::DateTime;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::market::market_state::HaltState;
use crate::market::scanner::in_rth;

// ─── Catalyst flags ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GapDir {
    #[default]
    None,
    Up,
    Down,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CatalystFlags {
    pub earnings: bool,
    pub news_recent: bool,
    pub halted: bool,
    pub gap_dir: GapDir,
    pub vol_spike: bool,
    /// Offering / dilution filing (424B*, S-1/3, F-1/3) on EDGAR in the
    /// last 24h — the classic gapper-killer. Negative catalyst: never
    /// adds to `score`, surfaced as a danger pill only.
    pub offering_risk: bool,
    pub score: f64,
}

pub struct CatalystInput<'a> {
    /// Reserved for telemetry / future symbol-aware scoring (e.g., per-
    /// industry weights). Currently unused inside `compute` — clippy's
    /// `dead_code` warning is silenced rather than removed because
    /// callers (`prefilter.rs`, `discovery.rs`) populate it consistently
    /// and the field will be read in S14+ logging.
    #[allow(dead_code)]
    pub symbol: &'a str,
    /// Reserved alongside `gap_pct`: while gap measures the open vs prev
    /// close, `change_pct` is the live intraday move. S13 only consumes
    /// `gap_pct`; S15 will add a "stretched" detector that needs both.
    #[allow(dead_code)]
    pub change_pct: f64,
    pub gap_pct: f64,
    pub rvol: f64,
    pub has_earnings: bool,
    pub has_recent_news: bool,
    pub is_halted: bool,
    pub has_offering: bool,
}

// ─── Weights (configurable) ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalystWeights {
    /// Score contribution when on earnings calendar.
    pub w_earn: f64,
    /// Score contribution when news_recent.
    pub w_news: f64,
    /// Reserved penalty marker. Compute() short-circuits on halt and
    /// returns `score=0`; this field is retained for future telemetry
    /// (e.g., observable post-halt cooldown weight). Default -50.
    pub w_halt: f64,
    /// Score contribution when gap is significant (Up or Down).
    pub w_gap: f64,
    /// Score contribution when RVOL >= rvol_threshold.
    pub w_vol: f64,
    /// Absolute gap percentage (% of prev close) considered significant.
    pub gap_threshold: f64,
    /// RVOL multiple that qualifies as a volume spike.
    pub rvol_threshold: f64,
}

impl Default for CatalystWeights {
    fn default() -> Self {
        Self {
            w_earn: 30.0,
            w_news: 25.0,
            w_halt: -50.0,
            w_gap: 20.0,
            w_vol: 25.0,
            gap_threshold: 5.0,
            rvol_threshold: 3.0,
        }
    }
}

// ─── Compute ────────────────────────────────────────────────────────

impl CatalystFlags {
    /// Pure scoring step. NEVER emits events, NEVER locks. Halt is the
    /// dominant signal — compute short-circuits to `score=0`.
    pub fn compute(input: &CatalystInput, w: &CatalystWeights) -> Self {
        if input.is_halted {
            return Self {
                halted: true,
                offering_risk: input.has_offering,
                score: 0.0,
                ..Self::default()
            };
        }

        let gap_dir = if input.gap_pct >= w.gap_threshold {
            GapDir::Up
        } else if input.gap_pct <= -w.gap_threshold {
            GapDir::Down
        } else {
            GapDir::None
        };
        let vol_spike = input.rvol >= w.rvol_threshold;

        let mut score = 0.0_f64;
        if input.has_earnings {
            score += w.w_earn;
        }
        if input.has_recent_news {
            score += w.w_news;
        }
        if !matches!(gap_dir, GapDir::None) {
            score += w.w_gap;
        }
        if vol_spike {
            score += w.w_vol;
        }
        let score = score.clamp(0.0, 100.0);

        Self {
            earnings: input.has_earnings,
            news_recent: input.has_recent_news,
            halted: false,
            gap_dir,
            vol_spike,
            offering_risk: input.has_offering,
            score,
        }
    }

    /// UI-ready pill labels. `["HALT"]` short-circuits when halted
    /// (offering still shows — dilution outlives the halt).
    pub fn pills(&self) -> Vec<&'static str> {
        if self.halted {
            return if self.offering_risk {
                vec!["HALT", "OFFER"]
            } else {
                vec!["HALT"]
            };
        }
        let mut out = Vec::with_capacity(5);
        if self.offering_risk {
            out.push("OFFER");
        }
        if self.earnings {
            out.push("EARN");
        }
        if self.news_recent {
            out.push("NEWS");
        }
        match self.gap_dir {
            GapDir::Up => out.push("GAP↑"),
            GapDir::Down => out.push("GAP↓"),
            GapDir::None => {}
        }
        if self.vol_spike {
            out.push("VOL!");
        }
        out
    }

    /// Convenience: pills as `Vec<String>` for serde-able payloads.
    pub fn pills_owned(&self) -> Vec<String> {
        self.pills().into_iter().map(String::from).collect()
    }
}

// ─── Headline sentiment (PRIME F0.6) ────────────────────────────────

/// Positive-polarity keywords for financial headlines. Lowercase;
/// matched as substrings of the lowercased headline. Multi-word
/// entries match phrase-style ("raises guidance").
const POSITIVE_KEYWORDS: &[&str] = &[
    "beat", "beats", "record", "surge", "soar", "rally", "rallies",
    "upgrade", "upgraded", "approval", "approves", "approved", "wins",
    "contract", "partnership", "raises guidance", "raised guidance",
    "strong", "profit", "buyback", "breakthrough", "exceeds", "tops",
    "jumps", "acquisition", "merger", "outperform", "expands",
];

/// Negative-polarity keywords. Same matching rules.
const NEGATIVE_KEYWORDS: &[&str] = &[
    "miss", "misses", "missed", "downgrade", "downgraded", "plunge",
    "plunges", "sinks", "falls", "drops", "lawsuit", "probe",
    "investigation", "recall", "bankruptcy", "chapter 11", "delisting",
    "dilution", "offering", "fraud", "warning", "cuts guidance",
    "cut guidance", "layoffs", "fails", "failed", "weak", "resigns",
    "underperform", "halts trial", "sec charges",
];

/// PRIME F0.6 — aggregate polarity of a set of headlines in [-1, +1].
///
/// Per headline: (+1 per positive keyword hit, -1 per negative hit),
/// clamped to [-1, 1]. The result is the mean across headlines that
/// produced at least one hit. Returns `None` when no headline matched
/// any keyword — callers treat that as "no clear polarity" (empirical
/// motivation: catalyst_frontrun lost -$233 over 83 trades using the
/// gap-sign proxy for news direction).
pub fn headline_sentiment<S: AsRef<str>>(headlines: &[S]) -> Option<f64> {
    let mut sum = 0.0_f64;
    let mut scored = 0usize;
    for h in headlines {
        let lower = h.as_ref().to_lowercase();
        let pos = POSITIVE_KEYWORDS
            .iter()
            .filter(|k| lower.contains(*k))
            .count() as f64;
        let neg = NEGATIVE_KEYWORDS
            .iter()
            .filter(|k| lower.contains(*k))
            .count() as f64;
        if pos == 0.0 && neg == 0.0 {
            continue;
        }
        sum += (pos - neg).clamp(-1.0, 1.0);
        scored += 1;
    }
    if scored == 0 {
        None
    } else {
        Some((sum / scored as f64).clamp(-1.0, 1.0))
    }
}

// ─── Halt resolution (status sub OR heuristic) ─────────────────────

/// S13 single source of truth for the OR semantics:
///   `halted = halts.contains_key(sym) || (in_rth && age > 120s)`.
///
/// The fallback never mutates `MarketDataState.halts` — primary signal
/// observability stays clean (callers can always tell whether a halt
/// originated from the SIP status sub by inspecting the halts map
/// before calling this helper).
///
/// Used by `scanner/prefilter.rs` and `commands/discovery.rs::get_watchlist`
/// so both surfaces produce identical halt verdicts.
pub fn is_halted_combined(
    halts: &HashMap<String, HaltState>,
    symbol: &str,
    last_trade_age_seconds: u32,
    now_et: DateTime<Tz>,
) -> bool {
    // F2: map may retain resumed rows (`resumed_at=Some`) for TRUE_RESUME.
    if halts
        .get(symbol)
        .map(|h| h.resumed_at.is_none())
        .unwrap_or(false)
    {
        return true;
    }
    in_rth(now_et) && last_trade_age_seconds > 120
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::market_state::HaltSource;
    use chrono::{TimeZone, Utc};

    fn default_weights() -> CatalystWeights {
        CatalystWeights::default()
    }

    /// Wednesday 2026-04-29 10:30 ET — solidly inside RTH.
    fn rth_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 10, 30, 0)
            .unwrap()
    }

    /// Wednesday 2026-04-29 07:30 ET — premarket, outside RTH.
    fn premarket_now() -> DateTime<Tz> {
        chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 4, 29, 7, 30, 0)
            .unwrap()
    }

    fn halted_state() -> HaltState {
        HaltState {
            halted_since: Utc::now(),
            reason: Some("Trading Halt".to_string()),
            source: HaltSource::StatusSub,
            halt_code: Some("H".into()),
            pre_halt_price: None,
            resumed_at: None,
            resume_prints: Vec::new(),
        }
    }

    // ─── 1. Hot setup ──────────────────────────────────────────────
    #[test]
    fn catalyst_score_hot_setup() {
        // earn + news + gap_up (>=5%) + vol_spike (>=3x), no halt
        let input = CatalystInput {
            symbol: "TSLA",
            change_pct: 8.0,
            gap_pct: 7.5,
            rvol: 5.0,
            has_earnings: true,
            has_recent_news: true,
            is_halted: false,
            has_offering: false,
        };
        let f = CatalystFlags::compute(&input, &default_weights());
        // 30 + 25 + 20 + 25 = 100
        assert!((f.score - 100.0).abs() < f64::EPSILON, "score={}", f.score);
        assert!(f.score > 70.0);
        assert!(!f.halted);
        assert!(f.earnings);
        assert!(f.news_recent);
        assert_eq!(f.gap_dir, GapDir::Up);
        assert!(f.vol_spike);
    }

    // ─── 2. Halt blocks ────────────────────────────────────────────
    #[test]
    fn catalyst_score_halted_blocks() {
        // Halt should short-circuit even with all positive flags
        let input = CatalystInput {
            symbol: "AAPL",
            change_pct: 8.0,
            gap_pct: 7.5,
            rvol: 5.0,
            has_earnings: true,
            has_recent_news: true,
            is_halted: true,
            has_offering: false,
        };
        let f = CatalystFlags::compute(&input, &default_weights());
        assert!(f.halted);
        assert_eq!(f.score, 0.0);
        assert!(!f.earnings, "halt resets all positive flags");
        assert!(!f.news_recent);
        assert_eq!(f.gap_dir, GapDir::None);
        assert!(!f.vol_spike);
    }

    // ─── 3. Neutral (no flags) ─────────────────────────────────────
    #[test]
    fn catalyst_score_neutral_no_flags() {
        let input = CatalystInput {
            symbol: "MSFT",
            change_pct: 0.5,
            gap_pct: 1.0, // below 5% threshold
            rvol: 1.0,    // below 3.0 threshold
            has_earnings: false,
            has_recent_news: false,
            is_halted: false,
            has_offering: false,
        };
        let f = CatalystFlags::compute(&input, &default_weights());
        assert_eq!(f.score, 0.0);
        assert_eq!(f.gap_dir, GapDir::None);
        assert!(!f.vol_spike);
        assert!(!f.halted);
    }

    // ─── 4. Pills (typical) ────────────────────────────────────────
    #[test]
    fn catalyst_pills_render_correct() {
        let f = CatalystFlags {
            earnings: true,
            news_recent: false,
            halted: false,
            gap_dir: GapDir::Up,
            vol_spike: false,
            offering_risk: false,
            score: 50.0,
        };
        let pills = f.pills();
        assert_eq!(pills, vec!["EARN", "GAP↑"]);
    }

    // ─── 5. Pills (halt only) ──────────────────────────────────────
    #[test]
    fn catalyst_pills_halt_only() {
        let f = CatalystFlags {
            earnings: true,
            news_recent: true,
            halted: true,
            gap_dir: GapDir::Up,
            vol_spike: true,
            offering_risk: false,
            score: 0.0,
        };
        assert_eq!(f.pills(), vec!["HALT"]);
    }

    #[test]
    fn catalyst_pills_offering_survives_halt_and_leads_order() {
        let mut f = CatalystFlags {
            offering_risk: true,
            news_recent: true,
            ..CatalystFlags::default()
        };
        // Dilution risk leads the pill order (danger first).
        assert_eq!(f.pills(), vec!["OFFER", "NEWS"]);
        // …and is the only pill that survives the HALT short-circuit.
        f.halted = true;
        assert_eq!(f.pills(), vec!["HALT", "OFFER"]);
    }

    // ─── 6. Gap classification ─────────────────────────────────────
    #[test]
    fn gap_dir_classification() {
        let w = default_weights();
        let mk = |gap: f64| {
            CatalystFlags::compute(
                &CatalystInput {
                    symbol: "X",
                    change_pct: 0.0,
                    gap_pct: gap,
                    rvol: 0.0,
                    has_earnings: false,
                    has_recent_news: false,
                    is_halted: false,
            has_offering: false,
                },
                &w,
            )
            .gap_dir
        };
        assert_eq!(mk(5.5), GapDir::Up, "+5.5% must be Up");
        assert_eq!(mk(5.0), GapDir::Up, "exactly threshold is Up (>=)");
        assert_eq!(mk(2.0), GapDir::None, "+2% below threshold");
        assert_eq!(mk(-5.5), GapDir::Down, "-5.5% must be Down");
        assert_eq!(mk(-5.0), GapDir::Down, "exactly -threshold is Down (<=)");
        assert_eq!(mk(-2.0), GapDir::None, "-2% above -threshold");
    }

    // ─── 7. Weights serde ──────────────────────────────────────────
    #[test]
    fn weights_serde_roundtrip() {
        let w = CatalystWeights {
            w_earn: 33.0,
            w_news: 22.0,
            w_halt: -75.0,
            w_gap: 17.0,
            w_vol: 28.0,
            gap_threshold: 4.0,
            rvol_threshold: 2.5,
        };
        let json = serde_json::to_string(&w).unwrap();
        let loaded: CatalystWeights = serde_json::from_str(&json).unwrap();
        assert!((loaded.w_earn - 33.0).abs() < f64::EPSILON);
        assert!((loaded.w_news - 22.0).abs() < f64::EPSILON);
        assert!((loaded.w_halt + 75.0).abs() < f64::EPSILON);
        assert!((loaded.gap_threshold - 4.0).abs() < f64::EPSILON);
        assert!((loaded.rvol_threshold - 2.5).abs() < f64::EPSILON);
    }

    // ─── 8. Default flags serde ────────────────────────────────────
    #[test]
    fn catalyst_flags_default_serializable() {
        let f = CatalystFlags::default();
        let json = serde_json::to_string(&f).unwrap();
        let loaded: CatalystFlags = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, f);
        assert_eq!(loaded.gap_dir, GapDir::None);
        assert!(!loaded.halted);
        assert_eq!(loaded.score, 0.0);

        // Legacy JSON without catalyst block should also load via default()
        let empty: CatalystFlags = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, CatalystFlags::default());
    }

    // ─── 9. Heuristic in RTH after 120s ────────────────────────────
    #[test]
    fn halt_heuristic_triggers_after_120s_age_rth() {
        let halts: HashMap<String, HaltState> = HashMap::new();
        // 130s of trade silence in RTH → heuristic halt
        assert!(is_halted_combined(&halts, "AAPL", 130, rth_now()));
        // exactly at boundary (120) is NOT halted (strict >)
        assert!(!is_halted_combined(&halts, "AAPL", 120, rth_now()));
        assert!(!is_halted_combined(&halts, "AAPL", 60, rth_now()));
    }

    // ─── 10. Heuristic does not trigger premarket ──────────────────
    #[test]
    fn halt_heuristic_does_not_trigger_premarket_long_quiet_period() {
        let halts: HashMap<String, HaltState> = HashMap::new();
        // 5 minutes of premarket silence is normal for illiquid names
        assert!(!is_halted_combined(&halts, "MICRO", 300, premarket_now()));
        // even an extreme age must not flip the heuristic outside RTH
        assert!(!is_halted_combined(&halts, "MICRO", 36_000, premarket_now()));
    }

    // ─── 11.5 (S15.5) — gap_dir ↔ gap_pct invariant ─────────────────
    /// Locks the contract that the JS `derivePillsFromFlags` helper
    /// relies on: `gap_dir != None` iff `|gap_pct| >= gap_threshold`.
    /// If this ever drifts, the BRLS-style "pill says GAP↑ but column
    /// says 0.0%" desync becomes possible again at the source.
    #[test]
    fn catalyst_invariant_gap_dir_consistent_with_gap_pct() {
        let weights = CatalystWeights::default();
        let threshold = weights.gap_threshold;
        for &gap_pct in &[
            -10.0_f64, -5.5, -5.0, -4.999_9, 0.0, 4.999_9, 5.0, 5.5, 10.0,
        ] {
            let flags = CatalystFlags::compute(
                &CatalystInput {
                    symbol: "TEST",
                    change_pct: 0.0,
                    gap_pct,
                    rvol: 1.0,
                    has_earnings: false,
                    has_recent_news: false,
                    is_halted: false,
            has_offering: false,
                },
                &weights,
            );
            let expected_up = gap_pct >= threshold;
            let expected_down = gap_pct <= -threshold;
            match flags.gap_dir {
                GapDir::Up => assert!(
                    expected_up,
                    "gap_pct={gap_pct} produced gap_dir=Up but |gap_pct|<threshold"
                ),
                GapDir::Down => assert!(
                    expected_down,
                    "gap_pct={gap_pct} produced gap_dir=Down but |gap_pct|<threshold"
                ),
                GapDir::None => assert!(
                    !expected_up && !expected_down,
                    "gap_pct={gap_pct} produced gap_dir=None but \
                     expected_up={expected_up} expected_down={expected_down}"
                ),
            }
        }
    }

    // ─── 12. Headline sentiment (PRIME F0.6) ────────────────────────
    #[test]
    fn headline_sentiment_polarity() {
        // Clearly bullish
        let s = headline_sentiment(&["ACME beats earnings, raises guidance"]).unwrap();
        assert!(s > 0.0, "bullish headline must be positive (got {s})");
        // Clearly bearish
        let s = headline_sentiment(&["ACME announces dilutive offering after SEC charges"]).unwrap();
        assert!(s < 0.0, "bearish headline must be negative (got {s})");
        // Mixed set averages
        let s = headline_sentiment(&[
            "ACME beats estimates",
            "ACME faces lawsuit over recall",
        ])
        .unwrap();
        assert!((-1.0..=1.0).contains(&s));
        // No keyword hit → None (no clear polarity)
        assert!(headline_sentiment(&["ACME to present at conference"]).is_none());
        // Empty input → None
        assert!(headline_sentiment::<&str>(&[]).is_none());
    }

    // ─── 11. Combined: status sub takes priority ────────────────────
    #[test]
    fn halt_combined_status_sub_takes_priority() {
        let mut halts: HashMap<String, HaltState> = HashMap::new();
        halts.insert("AAPL".to_string(), halted_state());

        // Even with age=10s in RTH, status sub already says halted.
        assert!(is_halted_combined(&halts, "AAPL", 10, rth_now()));

        // The primary signal observability must remain StatusSub.
        let h = halts.get("AAPL").expect("AAPL halt entry");
        assert_eq!(h.source, HaltSource::StatusSub);

        // For an unrelated symbol with no entry but heuristic-eligible
        // age in RTH, the function must still return true via fallback.
        assert!(is_halted_combined(&halts, "OTHER", 200, rth_now()));
        // …without contaminating the halts map.
        assert!(!halts.contains_key("OTHER"));
    }
}
