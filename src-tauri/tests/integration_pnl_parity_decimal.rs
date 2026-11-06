/// S08: Decimal PnL parity integration test.
///
/// Validates that Decimal-based PnL accumulation is exact (no floating-point
/// drift). Mirrors the SmartTrendAccount::record_fill accumulation path.
///
/// Acceptance: 100 random fills → sum(fills) == accumulated total (exact match,
/// no EPSILON tolerance).

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

struct MiniAccount {
    allocated_capital: Decimal,
    total_realized_pnl: Decimal,
    total_trades: u32,
    wins: u32,
    losses: u32,
}

impl MiniAccount {
    fn new(initial: Decimal) -> Self {
        Self {
            allocated_capital: initial,
            total_realized_pnl: Decimal::ZERO,
            total_trades: 0,
            wins: 0,
            losses: 0,
        }
    }

    fn record_fill(&mut self, pnl: Decimal) {
        self.total_realized_pnl += pnl;
        self.allocated_capital += pnl;
        self.total_trades += 1;
        if pnl > Decimal::ZERO {
            self.wins += 1;
        } else if pnl < Decimal::ZERO {
            self.losses += 1;
        }
    }
}

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn next_i64_bounded(&mut self, min: i64, max: i64) -> i64 {
        let range = (max - min) as u64;
        let val = self.next_u64() % range;
        min + val as i64
    }
}

#[test]
fn pnl_parity_100_random_fills_exact() {
    let seed: u64 = 0xDEAD_BEEF_CAFE_1234;
    let mut rng = SimpleRng::new(seed);

    let mut account = MiniAccount::new(dec!(10000));
    let mut manual_sum = Decimal::ZERO;

    for _ in 0..100 {
        let raw = rng.next_i64_bounded(-100_0000, 100_0000);
        let pnl = Decimal::new(raw, 4);
        account.record_fill(pnl);
        manual_sum += pnl;
    }

    assert_eq!(
        account.total_realized_pnl, manual_sum,
        "Decimal accumulation must be exact — no epsilon tolerance"
    );

    assert_eq!(
        account.allocated_capital,
        dec!(10000) + manual_sum,
        "allocated_capital = initial + sum(fills)"
    );

    assert_eq!(account.total_trades, 100);
}

#[test]
fn pnl_parity_alternating_fills_cancel_to_zero() {
    let mut account = MiniAccount::new(dec!(5000));

    for _ in 0..50 {
        account.record_fill(dec!(123.4567));
        account.record_fill(dec!(-123.4567));
    }

    assert_eq!(account.total_realized_pnl, Decimal::ZERO);
    assert_eq!(account.allocated_capital, dec!(5000));
    assert_eq!(account.total_trades, 100);
}

#[test]
fn pnl_parity_small_fills_no_drift() {
    let mut account = MiniAccount::new(dec!(1000));
    let fill = dec!(0.0001);

    for _ in 0..10_000 {
        account.record_fill(fill);
    }

    assert_eq!(account.total_realized_pnl, dec!(1));
    assert_eq!(account.allocated_capital, dec!(1001));
}

#[test]
fn f64_would_drift_but_decimal_does_not() {
    let mut f64_sum: f64 = 0.0;
    let mut dec_sum = Decimal::ZERO;

    for _ in 0..10_000 {
        f64_sum += 0.0001;
        dec_sum += dec!(0.0001);
    }

    assert_eq!(dec_sum, dec!(1), "Decimal stays exact");
    assert!(
        (f64_sum - 1.0).abs() > 1e-15,
        "f64 drifts (expected ~{f64_sum})"
    );
}
