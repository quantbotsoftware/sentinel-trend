use super::engine::{MomentumInput, PillarDirection, PillarScore};

// ═══════════════════════════════════════════════════════════════════════
//  Individual pillar evaluators — pure functions, no I/O
// ═══════════════════════════════════════════════════════════════════════

pub fn evaluate_momentum(input: &MomentumInput) -> PillarScore {
    let rvol_mult = if input.rvol > 2.0 {
        1.2
    } else if input.rvol >= 1.0 {
        1.0 + (input.rvol - 1.0) * 0.2
    } else {
        0.85
    };

    let roc_signal = (input.roc / 2.0).clamp(-1.0, 1.0);

    let mut score = (50.0 + roc_signal * rvol_mult * 40.0).clamp(0.0, 100.0);

    // Climax Volume Exhaustion Guard: if RVOL is extremely high (climax) on a positive move,
    // it likely represents buying exhaustion/FOMO rather than sustainable trend startup.
    // Cap the score to 25.0 (bearish) to prevent entries on all timeframes.
    if input.rvol > 3.0 && input.roc > 0.0 {
        score = score.min(25.0);
    }

    let direction = if score > 52.0 {
        PillarDirection::Bullish
    } else if score < 48.0 {
        PillarDirection::Bearish
    } else {
        PillarDirection::Neutral
    };

    let confidence = (input.rvol.min(3.0) / 3.0 * 0.6 + roc_signal.abs() * 0.4).clamp(0.0, 1.0);

    let stale = input.rvol < 0.01 && input.roc.abs() < 0.001;

    PillarScore {
        score,
        direction,
        confidence,
        stale,
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn momentum_rvol_boost() {
        let high_rvol = MomentumInput {
            rvol: 2.5,
            roc: 1.0,
        };
        let low_rvol = MomentumInput {
            rvol: 0.5,
            roc: 1.0,
        };
        let score_high = evaluate_momentum(&high_rvol).score;
        let score_low = evaluate_momentum(&low_rvol).score;
        assert!(
            score_high > score_low,
            "High RVOL should boost: high={score_high}, low={score_low}"
        );
    }

    #[test]
    fn momentum_bearish_roc() {
        let input = MomentumInput {
            rvol: 1.5,
            roc: -3.0,
        };
        let ps = evaluate_momentum(&input);
        assert!(ps.score < 45.0, "Negative ROC should be bearish, got {}", ps.score);
        assert_eq!(ps.direction, PillarDirection::Bearish);
    }
}
