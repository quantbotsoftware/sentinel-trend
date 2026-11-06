use crate::state::{BreakerLevel, TitanState};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use titan_risk::{BreakerLevel as TitanBreakerLevel, CircuitBreaker};

const CAUTION_DD_PCT: f64 = 2.0;
const HALT_DD_PCT: f64 = 4.0;
const CAUTION_LOSS_PCT: f64 = 1.0;
const HALT_LOSS_PCT: f64 = 2.5;

fn titan_level_rank(l: &TitanBreakerLevel) -> u8 {
    match l {
        TitanBreakerLevel::Normal => 0,
        TitanBreakerLevel::Caution => 1,
        TitanBreakerLevel::Halt => 2,
        TitanBreakerLevel::Panic => 3,
    }
}

fn map_level(t: TitanBreakerLevel) -> BreakerLevel {
    match t {
        TitanBreakerLevel::Normal => BreakerLevel::Normal,
        TitanBreakerLevel::Caution => BreakerLevel::Caution,
        TitanBreakerLevel::Halt => BreakerLevel::Halt,
        TitanBreakerLevel::Panic => BreakerLevel::Panic,
    }
}

fn to_dec(v: f64) -> Decimal {
    Decimal::from_f64(v).unwrap_or(Decimal::ZERO)
}

/// Evaluate circuit breaker level from drawdown and daily loss percentages.
/// Uses two parallel titan_risk::CircuitBreaker instances (one per axis)
/// and takes the worst (highest) level.
///
/// Thresholds (percentage of capital, all positive):
///   Drawdown axis:  Caution ≥ 2%, Halt ≥ 4%, Panic ≥ max_dd_config
///   Daily loss axis: Caution ≥ 1%, Halt ≥ 2.5%, Panic ≥ max_dd_config
pub fn evaluate_breaker(
    drawdown_pct: f64,
    daily_loss_pct: f64,
    max_dd_config: f64,
) -> BreakerLevel {
    let dd = to_dec(drawdown_pct.max(0.0));
    let loss = to_dec(daily_loss_pct.max(0.0));
    let panic_thresh = to_dec(max_dd_config);

    let cb_dd = CircuitBreaker::new(to_dec(CAUTION_DD_PCT), to_dec(HALT_DD_PCT), panic_thresh);
    let cb_loss = CircuitBreaker::new(to_dec(CAUTION_LOSS_PCT), to_dec(HALT_LOSS_PCT), panic_thresh);

    let level_dd = cb_dd.evaluate(dd, dd);
    let level_loss = cb_loss.evaluate(loss, Decimal::ZERO);

    let winner = if titan_level_rank(&level_dd) >= titan_level_rank(&level_loss) {
        level_dd
    } else {
        level_loss
    };
    map_level(winner)
}

/// Update RiskState from a fresh equity reading.
/// Returns `true` if breaker level changed (caller should emit event).
pub fn update_risk_from_equity(
    state: &TitanState,
    equity: f64,
    initial_equity: f64,
) -> Option<(BreakerLevel, BreakerLevel)> {
    let max_dd_config = {
        let cfg = state.config.lock().unwrap();
        cfg.smarttrend.max_drawdown_pct
    };

    let mut risk = state.risk.lock().unwrap();

    risk.session_equity = equity;
    if equity > risk.peak_equity {
        risk.peak_equity = equity;
    }

    risk.current_drawdown_pct = if risk.peak_equity > 0.0 {
        ((risk.peak_equity - equity) / risk.peak_equity * 100.0).max(0.0)
    } else {
        0.0
    };

    risk.daily_pnl = equity - initial_equity;

    let daily_loss_pct = if initial_equity > 0.0 {
        ((-risk.daily_pnl).max(0.0) / initial_equity) * 100.0
    } else {
        0.0
    };

    let prev_level = risk.breaker;
    let new_level = evaluate_breaker(risk.current_drawdown_pct, daily_loss_pct, max_dd_config);

    if prev_level == BreakerLevel::Panic {
        return None;
    }

    if new_level != prev_level {
        risk.breaker = new_level;
        Some((prev_level, new_level))
    } else {
        None
    }
}

/// Push an equity value into the history ring (max 10 entries).
pub fn push_equity_history(state: &TitanState, equity: f64) {
    let mut risk = state.risk.lock().unwrap();
    const MAX_HISTORY: usize = 10;
    if risk.equity_history.len() >= MAX_HISTORY {
        risk.equity_history.remove(0);
    }
    risk.equity_history.push(equity);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_normal() {
        assert_eq!(evaluate_breaker(0.0, 0.0, 5.0), BreakerLevel::Normal);
        assert_eq!(evaluate_breaker(1.9, 0.9, 5.0), BreakerLevel::Normal);
    }

    #[test]
    fn breaker_caution_on_drawdown() {
        assert_eq!(evaluate_breaker(2.0, 0.0, 5.0), BreakerLevel::Caution);
        assert_eq!(evaluate_breaker(3.5, 0.0, 5.0), BreakerLevel::Caution);
    }

    #[test]
    fn breaker_caution_on_daily_loss() {
        assert_eq!(evaluate_breaker(0.0, 1.0, 5.0), BreakerLevel::Caution);
        assert_eq!(evaluate_breaker(0.0, 2.4, 5.0), BreakerLevel::Caution);
    }

    #[test]
    fn breaker_halt_on_drawdown() {
        assert_eq!(evaluate_breaker(4.0, 0.0, 5.0), BreakerLevel::Halt);
        assert_eq!(evaluate_breaker(4.9, 0.0, 5.0), BreakerLevel::Halt);
    }

    #[test]
    fn breaker_halt_on_daily_loss() {
        assert_eq!(evaluate_breaker(0.0, 2.5, 5.0), BreakerLevel::Halt);
        assert_eq!(evaluate_breaker(0.0, 4.0, 5.0), BreakerLevel::Halt);
    }

    #[test]
    fn breaker_panic_at_max_dd() {
        assert_eq!(evaluate_breaker(5.0, 0.0, 5.0), BreakerLevel::Panic);
        assert_eq!(evaluate_breaker(8.0, 0.0, 5.0), BreakerLevel::Panic);
    }

    #[test]
    fn breaker_combined_worst_wins() {
        assert_eq!(evaluate_breaker(4.5, 3.0, 5.0), BreakerLevel::Halt);
        assert_eq!(evaluate_breaker(5.0, 3.0, 5.0), BreakerLevel::Panic);
    }

    #[test]
    fn update_risk_from_equity_transitions() {
        use crate::config::RuntimeConfig;
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut risk = state.risk.lock().unwrap();
            risk.peak_equity = 100_000.0;
        }

        // dd=1.5%, daily_loss=1.5% -> Caution (transition from Normal)
        let result = update_risk_from_equity(&state, 98_500.0, 100_000.0);
        assert!(result.is_some());
        let (prev, new) = result.unwrap();
        assert_eq!(prev, BreakerLevel::Normal);
        assert_eq!(new, BreakerLevel::Caution);

        // Same equity again -> same level, no transition
        let result = update_risk_from_equity(&state, 98_500.0, 100_000.0);
        assert!(result.is_none());
    }

    #[test]
    fn push_equity_history_caps_at_10() {
        use crate::config::RuntimeConfig;
        let state = TitanState::new(RuntimeConfig::default());
        for i in 0..15 {
            push_equity_history(&state, 100_000.0 + i as f64);
        }
        let risk = state.risk.lock().unwrap();
        assert_eq!(risk.equity_history.len(), 10);
        assert!((risk.equity_history[0] - 100_005.0).abs() < f64::EPSILON);
    }

    #[test]
    fn panic_is_sticky() {
        use crate::config::RuntimeConfig;
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut risk = state.risk.lock().unwrap();
            risk.peak_equity = 100_000.0;
            risk.breaker = BreakerLevel::Panic;
        }
        let result = update_risk_from_equity(&state, 100_000.0, 100_000.0);
        assert!(result.is_none());
        let risk = state.risk.lock().unwrap();
        assert_eq!(risk.breaker, BreakerLevel::Panic);
    }
}
