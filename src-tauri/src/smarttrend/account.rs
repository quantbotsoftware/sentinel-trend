use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use titan_risk::{SizingParams, calculate_position_size};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SizingDecision {
    pub qty: f64,
    pub notional: f64,
    pub risk_amount_effective: f64,
    pub capped: bool,
}

/// Tracks allocated capital, PnL, drawdown, and trade statistics
/// for the SmartTrend sub-system. Simplified vs. Titan HFT
/// (no Kelly, no Bayesian adaptation).
///
/// **Model capital** — `allocated_capital` is a local accumulator
/// (initial + Σ fills). Reflects P&L mediated by the bot; can diverge
/// from broker_equity (manual trades, fees, dividends). This is the
/// single source for `compute_size_usd`.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartTrendAccount {
    #[serde(with = "crate::serde_decimal")]
    pub allocated_capital: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub initial_capital: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub realized_pnl_today: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub total_realized_pnl: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub high_water_mark: Decimal,
    pub max_drawdown_pct: f64,
    pub trade_count_today: u32,
    pub win_count_today: u32,
    pub loss_count_today: u32,
    pub total_trades: u64,
    pub total_wins: u64,
}

impl SmartTrendAccount {
    pub fn new(capital: Decimal) -> Self {
        Self {
            allocated_capital: capital,
            initial_capital: capital,
            realized_pnl_today: Decimal::ZERO,
            total_realized_pnl: Decimal::ZERO,
            high_water_mark: capital,
            max_drawdown_pct: 0.0,
            trade_count_today: 0,
            win_count_today: 0,
            loss_count_today: 0,
            total_trades: 0,
            total_wins: 0,
        }
    }

    pub fn record_fill(&mut self, pnl: Decimal) {
        self.realized_pnl_today += pnl;
        self.total_realized_pnl += pnl;
        self.allocated_capital += pnl;
        self.trade_count_today += 1;
        self.total_trades += 1;

        if pnl > Decimal::ZERO {
            self.win_count_today += 1;
            self.total_wins += 1;
        } else if pnl < Decimal::ZERO {
            self.loss_count_today += 1;
        }

        if self.allocated_capital > self.high_water_mark {
            self.high_water_mark = self.allocated_capital;
        }

        let dd = self.drawdown_pct();
        if dd > self.max_drawdown_pct {
            self.max_drawdown_pct = dd;
        }
    }

    /// Current drawdown as percentage of high-water mark (0.0 = at peak).
    pub fn drawdown_pct(&self) -> f64 {
        if self.high_water_mark <= Decimal::ZERO {
            return 0.0;
        }
        let dd = (self.high_water_mark - self.allocated_capital) * dec!(100) / self.high_water_mark;
        dd.to_f64().unwrap_or(0.0).max(0.0)
    }

    pub fn compute_size_usd(&self, risk_per_trade: f64) -> f64 {
        let cap = self.allocated_capital.to_f64().unwrap_or(0.0);
        (cap * risk_per_trade).max(0.0)
    }

    /// Risk-based sizing via titan_risk::calculate_position_size.
    #[allow(dead_code)]
    pub fn position_size_risk_based(
        &self,
        entry_price: f64,
        stop_loss_price: f64,
        risk_per_trade: f64,
        breaker_multiplier: f64,
        max_notional_frac: f64,
    ) -> Option<SizingDecision> {
        let params = SizingParams {
            equity: self.allocated_capital,
            risk_pct: Decimal::from_f64(risk_per_trade * 100.0)?,
            entry_price: Decimal::from_f64(entry_price)?,
            stop_loss_price: Decimal::from_f64(stop_loss_price)?,
            breaker_multiplier: Decimal::from_f64(breaker_multiplier)?,
            max_notional_frac: Decimal::from_f64(max_notional_frac)?,
            min_stop_frac: Decimal::from_f64(0.0001)?,
        };
        let r = calculate_position_size(&params)?;
        Some(SizingDecision {
            qty: r.qty.to_f64()?,
            notional: r.notional.to_f64()?,
            risk_amount_effective: r.risk_amount_effective.to_f64()?,
            capped: r.capped,
        })
    }

    pub fn reset_daily(&mut self) {
        self.realized_pnl_today = Decimal::ZERO;
        self.trade_count_today = 0;
        self.win_count_today = 0;
        self.loss_count_today = 0;
    }

    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        self.total_wins as f64 / self.total_trades as f64
    }
}

impl Default for SmartTrendAccount {
    fn default() -> Self {
        Self::new(dec!(100_000))
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  VirtualWallet — virtual balance for User Account / Bot Account
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualWallet {
    #[serde(with = "crate::serde_decimal")]
    pub balance: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub initial_deposit: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub total_deposited: Decimal,
    #[serde(with = "crate::serde_decimal")]
    pub total_withdrawn: Decimal,
}

impl VirtualWallet {
    pub fn new() -> Self {
        Self {
            balance: Decimal::ZERO,
            initial_deposit: Decimal::ZERO,
            total_deposited: Decimal::ZERO,
            total_withdrawn: Decimal::ZERO,
        }
    }

    pub fn topup(&mut self, amount: Decimal) {
        if amount <= Decimal::ZERO {
            return;
        }
        if self.total_deposited == Decimal::ZERO {
            self.initial_deposit = amount;
        }
        self.balance += amount;
        self.total_deposited += amount;
    }

    /// Add funds back to balance WITHOUT incrementing total_deposited.
    /// Used for refunds (order fail, dry-run, rejected) and close returns.
    pub fn credit(&mut self, amount: Decimal) {
        if amount <= Decimal::ZERO {
            return;
        }
        self.balance += amount;
    }

    /// **User-initiated** cash withdrawal. Decreases `balance` and
    /// **increments** `total_withdrawn` so the wallet's lifetime P/L
    /// formula reflects the operator pulling money back out.
    pub fn withdraw(&mut self, amount: Decimal) -> Result<(), String> {
        if amount <= Decimal::ZERO {
            return Err("Withdraw amount must be positive".into());
        }
        if amount > self.balance {
            return Err(format!(
                "Insufficient balance: requested {amount}, available {}",
                self.balance
            ));
        }
        self.balance -= amount;
        self.total_withdrawn += amount;
        Ok(())
    }

    /// **Trade-internal** debit. Decreases `balance` only; does *not*
    /// touch `total_withdrawn`. This avoids the historical UI bug where
    /// every entry-then-fail cycle inflated the wallet's "P/L" by the
    /// debit amount because the matching `credit()` refund never
    /// decremented the withdrawn counter.
    ///
    /// Use for: bot order debits, transfers between internal sub-pools.
    /// Do NOT use for operator-driven cash withdrawals — that's
    /// `withdraw()`.
    pub fn debit(&mut self, amount: Decimal) -> Result<(), String> {
        if amount <= Decimal::ZERO {
            return Err("Debit amount must be positive".into());
        }
        if amount > self.balance {
            return Err(format!(
                "Insufficient balance: requested {amount}, available {}",
                self.balance
            ));
        }
        self.balance -= amount;
        Ok(())
    }
}

impl Default for VirtualWallet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_account_values() {
        let a = SmartTrendAccount::new(dec!(50_000));
        assert_eq!(a.allocated_capital, dec!(50_000));
        assert_eq!(a.high_water_mark, dec!(50_000));
        assert_eq!(a.total_trades, 0);
        assert!((a.drawdown_pct()).abs() < f64::EPSILON);
    }

    #[test]
    fn record_win_updates_correctly() {
        let mut a = SmartTrendAccount::new(dec!(10_000));
        a.record_fill(dec!(500));
        assert_eq!(a.allocated_capital, dec!(10_500));
        assert_eq!(a.realized_pnl_today, dec!(500));
        assert_eq!(a.high_water_mark, dec!(10_500));
        assert_eq!(a.total_wins, 1);
        assert_eq!(a.win_count_today, 1);
        assert!((a.drawdown_pct()).abs() < f64::EPSILON);
    }

    #[test]
    fn record_loss_tracks_drawdown() {
        let mut a = SmartTrendAccount::new(dec!(10_000));
        a.record_fill(dec!(-200));
        assert_eq!(a.allocated_capital, dec!(9_800));
        assert!((a.drawdown_pct() - 2.0).abs() < f64::EPSILON);
        assert!((a.max_drawdown_pct - 2.0).abs() < f64::EPSILON);
        assert_eq!(a.loss_count_today, 1);
        assert_eq!(a.total_wins, 0);
    }

    #[test]
    fn drawdown_sequence() {
        let mut a = SmartTrendAccount::new(dec!(10_000));
        a.record_fill(dec!(1_000));
        a.record_fill(dec!(-2_000));
        let dd = a.drawdown_pct();
        assert!((dd - 18.181818181818183).abs() < 0.01);
        assert!(a.max_drawdown_pct >= 18.0);
    }

    #[test]
    fn compute_size_usd_basic() {
        let a = SmartTrendAccount::new(dec!(100_000));
        let size = a.compute_size_usd(0.01);
        assert!((size - 1_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_size_zero_capital() {
        let a = SmartTrendAccount::new(dec!(0));
        assert!((a.compute_size_usd(0.01)).abs() < f64::EPSILON);
    }

    #[test]
    fn daily_reset_clears_daily_only() {
        let mut a = SmartTrendAccount::new(dec!(10_000));
        a.record_fill(dec!(100));
        a.record_fill(dec!(-50));
        a.reset_daily();
        assert_eq!(a.realized_pnl_today, Decimal::ZERO);
        assert_eq!(a.trade_count_today, 0);
        assert_eq!(a.win_count_today, 0);
        assert_eq!(a.loss_count_today, 0);
        assert_eq!(a.total_trades, 2);
        assert_eq!(a.total_wins, 1);
        assert_eq!(a.total_realized_pnl, dec!(50));
    }

    #[test]
    fn win_rate_calculation() {
        let mut a = SmartTrendAccount::new(dec!(10_000));
        assert!((a.win_rate()).abs() < f64::EPSILON);
        a.record_fill(dec!(100));
        a.record_fill(dec!(-50));
        a.record_fill(dec!(80));
        assert!((a.win_rate() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn breakeven_zero_pnl_not_counted_as_win() {
        let mut a = SmartTrendAccount::new(dec!(10_000));
        a.record_fill(Decimal::ZERO);
        assert_eq!(a.win_count_today, 0, "zero PnL must not be a win");
        assert_eq!(a.loss_count_today, 0, "zero PnL must not be a loss");
        assert_eq!(a.trade_count_today, 1, "zero PnL is still a trade");
    }

    #[test]
    fn serde_roundtrip() {
        let mut a = SmartTrendAccount::new(dec!(25_000));
        a.record_fill(dec!(300));
        let json = serde_json::to_string(&a).unwrap();
        let loaded: SmartTrendAccount = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.allocated_capital, a.allocated_capital);
        assert_eq!(loaded.total_trades, a.total_trades);
    }

    #[test]
    fn serde_backward_compat_from_float() {
        let json = r#"{"allocated_capital":50000.0,"initial_capital":50000.0,"realized_pnl_today":0.0,"total_realized_pnl":0.0,"high_water_mark":50000.0,"max_drawdown_pct":0.0,"trade_count_today":0,"win_count_today":0,"loss_count_today":0,"total_trades":0,"total_wins":0}"#;
        let loaded: SmartTrendAccount = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.allocated_capital, dec!(50000));
    }

    // ─── VirtualWallet tests ────────────────────────────────────────

    #[test]
    fn wallet_new_zero_balance() {
        let w = VirtualWallet::new();
        assert_eq!(w.balance, Decimal::ZERO);
        assert_eq!(w.initial_deposit, Decimal::ZERO);
        assert_eq!(w.total_deposited, Decimal::ZERO);
        assert_eq!(w.total_withdrawn, Decimal::ZERO);
    }

    #[test]
    fn wallet_topup_basic() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(1000));
        assert_eq!(w.balance, dec!(1000));
        assert_eq!(w.total_deposited, dec!(1000));
        w.topup(dec!(500));
        assert_eq!(w.balance, dec!(1500));
        assert_eq!(w.total_deposited, dec!(1500));
    }

    #[test]
    fn wallet_first_topup_sets_initial() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(2000));
        assert_eq!(w.initial_deposit, dec!(2000));
        w.topup(dec!(500));
        assert_eq!(w.initial_deposit, dec!(2000));
    }

    #[test]
    fn wallet_withdraw_ok() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(1000));
        assert!(w.withdraw(dec!(400)).is_ok());
        assert_eq!(w.balance, dec!(600));
        assert_eq!(w.total_withdrawn, dec!(400));
    }

    #[test]
    fn wallet_withdraw_insufficient() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(100));
        let result = w.withdraw(dec!(200));
        assert!(result.is_err());
        assert_eq!(w.balance, dec!(100));
    }

    #[test]
    fn wallet_negative_amount_ignored() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(-50));
        assert_eq!(w.balance, Decimal::ZERO);
        assert!(w.withdraw(dec!(-10)).is_err());
    }

    #[test]
    fn wallet_credit_does_not_increase_total_deposited() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(1000));
        w.credit(dec!(100));
        assert_eq!(w.balance, dec!(1100));
        assert_eq!(w.total_deposited, dec!(1000));
    }

    #[test]
    fn s16_0_debit_does_not_inflate_total_withdrawn() {
        // The historical UI bug: every entry-then-fail cycle inflated
        // `total_withdrawn` because trade debits used `withdraw()`.
        // After 100 cycles the wallet's "P/L" indicator showed
        // +$25K despite zero real profit. `debit()` must keep
        // `total_withdrawn` at zero so the lifetime-P/L formula
        // `balance - total_deposited + total_withdrawn` stays honest.
        let mut w = VirtualWallet::new();
        w.topup(dec!(1000));
        // Simulate 100 entry-then-fail cycles ($100 each).
        for _ in 0..100 {
            assert!(w.debit(dec!(100)).is_ok());
            w.credit(dec!(100));
        }
        assert_eq!(w.balance, dec!(1000), "balance must round-trip");
        assert_eq!(w.total_withdrawn, Decimal::ZERO,
                   "trade debits must NOT touch total_withdrawn");
        assert_eq!(w.total_deposited, dec!(1000));
    }

    #[test]
    fn s16_0_debit_insufficient_balance() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(50));
        let res = w.debit(dec!(100));
        assert!(res.is_err());
        assert_eq!(w.balance, dec!(50));
        assert_eq!(w.total_withdrawn, Decimal::ZERO);
    }

    #[test]
    fn s16_0_user_withdraw_still_increments_total_withdrawn() {
        // Operator-initiated withdrawals continue to bump the
        // counter so the lifetime-P/L formula reflects cash that
        // actually left the wallet.
        let mut w = VirtualWallet::new();
        w.topup(dec!(1000));
        assert!(w.withdraw(dec!(300)).is_ok());
        assert_eq!(w.balance, dec!(700));
        assert_eq!(w.total_withdrawn, dec!(300));
    }

    #[test]
    fn wallet_topup_increases_total_deposited() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(100));
        w.topup(dec!(200));
        assert_eq!(w.balance, dec!(300));
        assert_eq!(w.total_deposited, dec!(300));
    }

    #[test]
    fn wallet_credit_negative_ignored() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(500));
        w.credit(dec!(-50));
        w.credit(Decimal::ZERO);
        assert_eq!(w.balance, dec!(500));
    }

    #[test]
    fn wallet_withdraw_credit_roundtrip() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(1000));
        w.withdraw(dec!(250)).unwrap();
        w.credit(dec!(250));
        assert_eq!(w.balance, dec!(1000));
        assert_eq!(w.total_deposited, dec!(1000));
        assert_eq!(w.total_withdrawn, dec!(250));
    }

    #[test]
    fn wallet_serde_roundtrip() {
        let mut w = VirtualWallet::new();
        w.topup(dec!(5000));
        w.withdraw(dec!(1200)).unwrap();
        let json = serde_json::to_string(&w).unwrap();
        let loaded: VirtualWallet = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.balance, w.balance);
        assert_eq!(loaded.total_deposited, w.total_deposited);
        assert_eq!(loaded.total_withdrawn, w.total_withdrawn);
        assert_eq!(loaded.initial_deposit, w.initial_deposit);
    }

    #[test]
    fn wallet_serde_backward_compat_from_float() {
        let json = r#"{"balance":1500.0,"initial_deposit":2000.0,"total_deposited":2000.0,"total_withdrawn":500.0}"#;
        let loaded: VirtualWallet = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.balance, dec!(1500));
    }

    // ─── position_size_risk_based tests ──────────────────────────────

    #[test]
    fn position_size_risk_based_basic() {
        let a = SmartTrendAccount::new(dec!(100_000));
        let d = a.position_size_risk_based(200.0, 196.0, 0.01, 1.0, 0.75).unwrap();
        assert!((d.qty - 250.0).abs() < 0.01, "qty should be ~250, got {}", d.qty);
        assert!(!d.capped);
        assert!((d.risk_amount_effective - 1000.0).abs() < 0.01);
    }

    #[test]
    fn position_size_risk_based_capped() {
        let a = SmartTrendAccount::new(dec!(10_000));
        let d = a.position_size_risk_based(10.0, 9.0, 0.50, 1.0, 0.25).unwrap();
        assert!(d.capped, "should be capped when risk demands >25% equity");
        assert!(d.notional <= 2500.01, "notional {} should be <= 2500", d.notional);
        assert!(d.risk_amount_effective < 5000.0, "effective risk {} should be < raw risk", d.risk_amount_effective);
    }
}
