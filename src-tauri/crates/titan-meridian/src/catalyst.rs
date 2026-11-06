//! Catalyst classification of a news headline into a fixed taxonomy (X5).
//!
//! Pure, deterministic, no regex crate: lowercase substring/keyword rules
//! tuned on Benzinga's structured headline formats ("… Q3 EPS $1.02 Beats
//! $0.95 Estimate, Sales $2.1B Beat $2.0B Estimate", "… Announces $5M
//! Registered Direct Offering", "FDA Approves …", "… to Acquire … for
//! $12.00/Share in Cash", "… Wins $67M Contract …"). Negative classes are
//! detected first so that "beats estimate, announces offering" is an
//! `Offering`, never an `EarningsBeat`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum CatalystClass {
    /// EPS and revenue both beat, or an unambiguous beat.
    EarningsBeat,
    /// One beat, one miss ("mixed").
    EarningsMixed,
    EarningsMiss,
    GuidanceUp,
    GuidanceDown,
    FdaPositive,
    FdaNegative,
    /// Company is (to be) acquired, or a merger/takeover proposal.
    MergerAcquisition,
    /// Contract, award, order win.
    Contract,
    /// Partnership / collaboration / strategic agreement.
    Partnership,
    /// Dilution: offering, registered direct, private placement, warrants,
    /// reverse split, ATM program.
    Offering,
    /// Rating / price-target changes, coverage initiations.
    Analyst,
    /// Trading halt notices.
    Halt,
    Other,
}

impl CatalystClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            CatalystClass::EarningsBeat => "EarningsBeat",
            CatalystClass::EarningsMixed => "EarningsMixed",
            CatalystClass::EarningsMiss => "EarningsMiss",
            CatalystClass::GuidanceUp => "GuidanceUp",
            CatalystClass::GuidanceDown => "GuidanceDown",
            CatalystClass::FdaPositive => "FdaPositive",
            CatalystClass::FdaNegative => "FdaNegative",
            CatalystClass::MergerAcquisition => "M&A",
            CatalystClass::Contract => "Contract",
            CatalystClass::Partnership => "Partnership",
            CatalystClass::Offering => "Offering",
            CatalystClass::Analyst => "Analyst",
            CatalystClass::Halt => "Halt",
            CatalystClass::Other => "Other",
        }
    }

    /// Classes C-PM may trade long. `Analyst`/`Other`/`M&A` are control
    /// arms: M&A is excluded from momentum on purpose — acquirers tend to
    /// fall and targets pin to the deal price (merger arbitrage, not a
    /// continuation trade).
    pub fn is_tradeable_long(&self) -> bool {
        matches!(
            self,
            CatalystClass::EarningsBeat | CatalystClass::GuidanceUp | CatalystClass::FdaPositive | CatalystClass::Contract
        )
    }

    /// Hard exclusion regardless of tape (dilution, negative outcome).
    pub fn is_excluded(&self) -> bool {
        matches!(
            self,
            CatalystClass::Offering
                | CatalystClass::EarningsMiss
                | CatalystClass::GuidanceDown
                | CatalystClass::FdaNegative
                | CatalystClass::Halt
        )
    }
}

fn any(h: &str, keys: &[&str]) -> bool {
    keys.iter().any(|k| h.contains(k))
}

pub fn classify(headline: &str) -> CatalystClass {
    let h = headline.to_lowercase();
    let h = h.as_str();

    // 1. Dilution / negative structural events dominate everything.
    if any(
        h,
        &[
            "offering",
            "registered direct",
            "private placement",
            "at-the-market",
            " atm ",
            "pricing of",
            "prices $",
            "priced at",
            "warrants",
            "reverse split",
            "reverse stock split",
            "dilution",
            "dilutive",
            "convertible note",
            "shelf registration",
        ],
    ) {
        return CatalystClass::Offering;
    }
    if any(h, &["trading halt", "halted", "halt pending", "resumes trading", "resumption of trading"]) {
        return CatalystClass::Halt;
    }

    // 2. FDA / clinical.
    let fda_ctx = any(
        h,
        &["fda", "phase 1", "phase 2", "phase 3", "phase i", "phase ii", "phase iii", "topline", "top-line", "pdufa", "510(k)", "ema ", "chmp", "clinical", "trial"],
    );
    if fda_ctx {
        if any(
            h,
            &["complete response letter", "crl", "fails", "failed", "did not meet", "does not meet", "misses primary", "clinical hold", "discontinu", "rejects", "refuse", "adverse", "death", "halts trial", "pauses"],
        ) {
            return CatalystClass::FdaNegative;
        }
        if any(
            h,
            &["approv", "clearance", "clears", "breakthrough", "fast track", "orphan", "positive", "meets primary", "met primary", "met its primary", "achieves", "statistically significant", "accept", "priority review", "grants", "authoriz", "designation"],
        ) {
            return CatalystClass::FdaPositive;
        }
    }

    // 3. Earnings (Benzinga structured format).
    let beats = any(h, &["beats", " beat ", "beat estimate", "tops estimate", "above estimate", "ahead of estimate"]);
    let misses = any(h, &["misses", " miss ", "miss estimate", "below estimate", "falls short"]);
    let earnings_ctx = any(h, &["eps", "earnings", "sales", "revenue", "q1", "q2", "q3", "q4", "fy2", "fy1", "quarter"]);
    if earnings_ctx && (beats || misses) {
        return match (beats, misses) {
            (true, true) => CatalystClass::EarningsMixed,
            (true, false) => CatalystClass::EarningsBeat,
            _ => CatalystClass::EarningsMiss,
        };
    }

    // 4. Guidance.
    let guide_ctx = any(h, &["guidance", "outlook", "sees fy", "sees q", "expects fy", "expects q", "forecast", "raises fy", "raises full", "reaffirm", "affirms", "expects to hit", "upper end", "lower end"]);
    if guide_ctx {
        if any(h, &["raises", "boosts", "lifts", "hikes", "upper end", "above", "increase", "strong"]) {
            return CatalystClass::GuidanceUp;
        }
        if any(h, &["cuts", "lowers", "slashes", "trims", "below", "withdraws", "suspends", "lower end", "weak", "reduces"]) {
            return CatalystClass::GuidanceDown;
        }
    }

    // 5. Analyst actions (very frequent; identified before M&A so that
    //    "raises price target" never leaks into other classes).
    if any(
        h,
        &["price target", "upgrades", "downgrades", "initiates coverage", "maintains", "reiterates", "reinstates", "resumes coverage", " rating", "outperform", "underperform", "overweight", "underweight", "neutral on", "buy on", "hold on", "sell on", "analyst ratings"],
    ) {
        return CatalystClass::Analyst;
    }

    // 6. M&A.
    if any(
        h,
        &["to acquire", "acquires", "acquisition of", "to be acquired", "agrees to be acquired", "merger", "merge with", "takeover", "buyout", "take-private", "go-private", "per share in cash", "/share in cash", "proposes to acquire", "receives proposal", "unsolicited proposal", "definitive agreement to be", "tender offer", "strategic alternatives"],
    ) {
        return CatalystClass::MergerAcquisition;
    }

    // 7. Contracts / awards.
    if any(h, &["wins", "awarded", "contract", "award", "purchase order", "order worth", "orders worth", "order valued", "selected by", "secures", "task order", "idiq", "supply agreement", "delivery order"]) {
        return CatalystClass::Contract;
    }

    // 8. Partnerships.
    if any(h, &["partnership", "partners with", "collaboration", "collaborat", "strategic agreement", "teams up", "joint venture", "alliance", "agreement with", "signs agreement", "license agreement", "licensing"]) {
        return CatalystClass::Partnership;
    }
    CatalystClass::Other
}

#[cfg(test)]
mod tests {
    use super::*;
    use CatalystClass::*;

    #[test]
    fn benzinga_formats() {
        let cases = [
            ("Alibaba Group Holding Q3 EPS $3.38 Beats $3.25 Estimate, Sales $33.88B Beat $33.35B Estimate", EarningsBeat),
            ("Duluth Holdings Q2 Adj. EPS $0.50 Beats $(0.01) Estimate, Sales $150M Beat $140M Estimate", EarningsBeat),
            ("Aon Q4 2024 Adj EPS $4.42 Beats $4.25 Estimate, Sales $4.15B Miss $4.20B Estimate", EarningsMixed),
            ("CommVault Systems Q3 Adj EPS $0.62 Misses $0.65 Estimate, Sales $195.10M Miss $197.41M Estimate", EarningsMiss),
            ("Duluth Holdings Affirms FY2026 Sales Guidance of $540.000M-$560.000M", Other),
            ("Acme Raises FY2026 EPS Guidance From $1.00-$1.10 To $1.20-$1.30", GuidanceUp),
            ("Pediatrix Medical Expects To Hit The Upper End Of Its Projected Range For 2024 Adjusted Ebitda", GuidanceUp),
            ("Beta Corp Cuts FY24 Revenue Outlook, Sees $500M vs $600M Prior", GuidanceDown),
            ("FDA Approves Gamma Pharma's Drug For Rare Disease", FdaPositive),
            ("Delta Bio Announces Positive Topline Results From Phase 3 Trial", FdaPositive),
            ("Epsilon Therapeutics Receives Complete Response Letter From FDA", FdaNegative),
            ("Zeta Bio Phase 2 Trial Did Not Meet Primary Endpoint", FdaNegative),
            ("UPDATE: MNG Enterprises Proposes to Acquire Gannett for $12.00/Share in Cash", MergerAcquisition),
            ("Eta Inc. Agrees To Be Acquired By Theta Holdings For $45 Per Share In Cash", MergerAcquisition),
            ("ManTech Wins $67M Award To Provide Scientific And Technical Support", Contract),
            ("Iota Systems Secures $20M Purchase Order From Fortune 500 Customer", Contract),
            ("Fisker Says Developing Dealer Partnership Model In North America", Partnership),
            ("Lifeward Announces $5M Registered Direct Offering Priced At-The-Market; Issues 1.82M Shares", Offering),
            ("Kappa Corp Announces 1-for-20 Reverse Stock Split", Offering),
            ("Stryve Foods Reports Secured $35M In Private Offering", Offering),
            ("Piper Sandler Maintains Overweight on EPAM Sys, Lowers Price Target to $399", Analyst),
            ("Goldman Sachs Initiates Coverage On QuantumScape with Neutral Rating", Analyst),
            ("Jefferies Downgrades Northrop Grumman to Hold, Announces $410 Price Target", Analyst),
            ("Lambda Corp Shares Halted, News Pending", Halt),
            ("Leidos Reports $200M Accelerated Buyback", Other),
            ("Nokia Q4 Non-IFRS Diluted EPS €0.13 vs €0.13 In Q4 2017", Other),
            ("22 Stocks Moving in Tuesday's Pre-Market Session", Other),
        ];
        let mut wrong = Vec::new();
        for (h, want) in cases {
            let got = classify(h);
            if got != want {
                wrong.push(format!("{h:?}: got {got:?}, want {want:?}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn offering_dominates_beat_and_flags() {
        assert_eq!(classify("Mu Corp Q2 EPS Beats Estimate; Announces $50M Public Offering"), Offering);
        assert!(Offering.is_excluded() && !Offering.is_tradeable_long());
        assert!(EarningsBeat.is_tradeable_long() && !EarningsBeat.is_excluded());
        assert!(!Analyst.is_tradeable_long() && !Analyst.is_excluded());
        assert!(!MergerAcquisition.is_tradeable_long() && !MergerAcquisition.is_excluded());
        assert_eq!(MergerAcquisition.as_str(), "M&A");
    }

    #[test]
    fn analyst_raise_is_not_guidance_or_contract() {
        assert_eq!(classify("Barclays Maintains Overweight on Merck & Co, Raises Price Target to $135"), Analyst);
        assert_eq!(classify("Cantor Fitzgerald Initiates Coverage On iBio with Overweight Rating, Announces Price Target of $5"), Analyst);
    }
}
