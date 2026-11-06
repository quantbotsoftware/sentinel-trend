use serde::{Deserialize, Serialize};

use crate::state::Direction;

// ═══════════════════════════════════════════════════════════════════════
//  Types
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    SignalExit,
    MaxHold,
    CircuitBreaker,
    ManualClose,
    StaleData,
    /// S17.4 — adaptive exit: position is bleeding, cut it before
    /// it hits the wider bracket SL. Catches the slow-decay names
    /// where momentum dies inside the first few minutes.
    QuickCut,
    /// S17.4 — adaptive exit: lock in a meaningful gain before the
    /// typical small-cap reversal. Default trigger ≥ +1.5% after
    /// holding ≥ 90 s.
    ProfitTake,
    /// v2.4.2 — flat-by-close safety rail: liquidate at 15:55 ET
    /// regardless of FSM state, so we never carry a small-cap
    /// momentum position overnight.
    FlatByClose,
    /// S31 (AUDIT RISK 5, 2026-05-28) — Tournament sub-wallet kill-switch.
    /// Strategy's `current_sub_wallet` went negative (large-slip event
    /// or runaway losing streak); blown-strategy rail force-closes every
    /// open position tagged with that strategy. Distinct from
    /// `MaxHold`/`QuickCut` because it's NOT a position-level signal —
    /// it's a strategy-level risk envelope breach.
    TournamentExit,
    /// S44 — Smart Trailing Stop exit.
    TrailingStop,
    /// PRIME F2c — asymmetric exit ladder (hard stop at −1R, breakeven
    /// after +1R, R-based trail after +1.5R). Distinct from
    /// `TrailingStop` (percent rail) so attribution can separate the
    /// two policies in the journal.
    PrimeLadder,
}

/// S02: Reasons an entry can be skipped at Gate 0 (compliance gates).
/// Distinct from `ExitReason` — these block *new* entries before any order
/// is sent. Each variant maps to a dimensional counter in `BotMetrics` and
/// surfaces in the UI via `bot-blocked` events for debuggability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Pattern Day Trader rule: account < $25k AND (daytrade_count >= 3 OR
    /// pattern_day_trader flag set by Alpaca).
    PdtBlocked,
    /// Estimated cost (qty * price) exceeds 98% of broker buying power.
    BuyingPowerInsufficient,
    /// `cfg.execution.allow_short == false` and direction is short.
    ShortDisabled,
    /// Asset metadata says either `shortable == false` or `easy_to_borrow == false`.
    NotShortable,
    /// Asset is not fractionable but qty has fractional part (would be
    /// silently floored to integer prior to S02 — caused over-sizing).
    NotFractionable,
    /// Asset cache miss — Gate 0 has no metadata to validate against. Reject
    /// conservatively (cf. S02 risk mitigation: never trade without flags).
    AssetUnknown,
    /// `RiskState.last_account_sync_at` is `0` (never synced) or older than
    /// 5 minutes. Prevents PDT/buying-power gates passing on stale snapshots.
    AccountStale,
    /// S03: Circuit breaker at Panic — all new entries blocked.
    BreakerHalt,
    /// S03: Circuit breaker at Caution AND `caution_action == Block`.
    CautionBlock,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PdtBlocked => write!(f, "PDT_BLOCKED"),
            Self::BuyingPowerInsufficient => write!(f, "BUYING_POWER_INSUFFICIENT"),
            Self::ShortDisabled => write!(f, "SHORT_DISABLED"),
            Self::NotShortable => write!(f, "NOT_SHORTABLE"),
            Self::NotFractionable => write!(f, "NOT_FRACTIONABLE"),
            Self::AssetUnknown => write!(f, "ASSET_UNKNOWN"),
            Self::AccountStale => write!(f, "ACCOUNT_STALE"),
            Self::BreakerHalt => write!(f, "BREAKER_HALT"),
            Self::CautionBlock => write!(f, "CAUTION_BLOCK"),
        }
    }
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignalExit => write!(f, "SIGNAL_EXIT"),
            Self::MaxHold => write!(f, "MAX_HOLD"),
            Self::CircuitBreaker => write!(f, "CIRCUIT_BREAKER"),
            Self::ManualClose => write!(f, "MANUAL_CLOSE"),
            Self::StaleData => write!(f, "STALE_DATA"),
            Self::QuickCut => write!(f, "QUICK_CUT"),
            Self::ProfitTake => write!(f, "PROFIT_TAKE"),
            Self::FlatByClose => write!(f, "FLAT_BY_CLOSE"),
            Self::TournamentExit => write!(f, "TOURNAMENT_SUB_WALLET_BLOWN"),
            Self::TrailingStop => write!(f, "TRAILING_STOP"),
            Self::PrimeLadder => write!(f, "PRIME_LADDER"),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BotAction {
    pub symbol: String,
    pub kind: BotActionKind,
    pub direction: Direction,
    pub score: f64,
    pub bar_time: i64,
    pub price: f64,
    pub timeframe: String,
    /// Sesiunea 1 — D Evolution Protocol. When the action originates
    /// from the new tournament-based strategy router this is `Some(_)`
    /// and the bot uses the strategy's `TradingWindow` to decide
    /// whether to honour the entry outside RTH. When `None` (legacy
    /// SentinelTrend / pre-S20 entries), the historical RTH-only hard
    /// gate still applies — preserves S17.0 wallet-leak fix bit-for-bit.
    pub strategy_id: Option<crate::smarttrend::strategies::StrategyId>,
    /// Sesiunea 3 — Tournament explicit risk parameters. If populated, these override
    /// the bot's global ATR-based calculations.
    pub stop_loss: Option<rust_decimal::Decimal>,
    pub take_profit: Option<rust_decimal::Decimal>,
    pub max_hold_minutes: Option<u32>,
    /// Sesiunea 3 — OMEGA ML Hook: Features built at evaluation time.
    /// If present, `entry.rs` will persist these to `MemoryDb` and feed the
    /// resulting `setup_id` to `dplus.on_trade_open()`.
    pub setup_features: Option<crate::smarttrend::memory_db::SetupFeatures>,
    /// PRIME F2 — fused conviction in `[0, 1]` when the action passed
    /// through the PRIME arbitration. `None` for legacy paths (exits,
    /// stale-data liquidations) — sizing falls back to pre-PRIME rules.
    pub conviction: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BotActionKind {
    Entry,
    Exit(ExitReason),
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_reason_display() {
        assert_eq!(ExitReason::SignalExit.to_string(), "SIGNAL_EXIT");
        assert_eq!(ExitReason::MaxHold.to_string(), "MAX_HOLD");
        assert_eq!(ExitReason::CircuitBreaker.to_string(), "CIRCUIT_BREAKER");
        assert_eq!(ExitReason::TrailingStop.to_string(), "TRAILING_STOP");
    }

    #[test]
    fn exit_reason_equality() {
        assert_eq!(ExitReason::StaleData, ExitReason::StaleData);
        assert_ne!(ExitReason::MaxHold, ExitReason::ManualClose);
    }

    #[test]
    fn skip_reason_display_strings() {
        assert_eq!(SkipReason::PdtBlocked.to_string(), "PDT_BLOCKED");
        assert_eq!(
            SkipReason::BuyingPowerInsufficient.to_string(),
            "BUYING_POWER_INSUFFICIENT"
        );
        assert_eq!(SkipReason::ShortDisabled.to_string(), "SHORT_DISABLED");
        assert_eq!(SkipReason::NotShortable.to_string(), "NOT_SHORTABLE");
        assert_eq!(SkipReason::NotFractionable.to_string(), "NOT_FRACTIONABLE");
        assert_eq!(SkipReason::AssetUnknown.to_string(), "ASSET_UNKNOWN");
        assert_eq!(SkipReason::AccountStale.to_string(), "ACCOUNT_STALE");
        assert_eq!(SkipReason::BreakerHalt.to_string(), "BREAKER_HALT");
        assert_eq!(SkipReason::CautionBlock.to_string(), "CAUTION_BLOCK");
    }

    #[test]
    fn caution_action_serde_roundtrip() {
        use crate::config::CautionAction;
        let actions = [CautionAction::Block, CautionAction::Scale50, CautionAction::Scale25];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let loaded: CautionAction = serde_json::from_str(&json).unwrap();
            assert_eq!(*action, loaded);
        }
    }
}
