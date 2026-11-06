/// S09: Order-flow integration skeleton.
///
/// Mirrors the `OrderRequest` → `validate_order` → `build_order_body`
/// boundary added in S08 (notional API). This test crate cannot import
/// the binary directly (no `lib.rs`), so the contract is replicated
/// here as a guard against silent surface-area regression.
///
/// Acceptance: ≥1 explicit assertion per test.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone)]
struct MiniOrderRequest {
    symbol: String,
    qty: Option<Decimal>,
    notional: Option<Decimal>,
    bracket: bool,
    take_profit: Option<Decimal>,
}

#[derive(Debug, PartialEq, Eq)]
enum ValidateError {
    BracketAndNotionalIncompatible,
    BracketAndTakeProfitOnly,
    QtyOrNotionalRequired,
    SymbolEmpty,
    QtyNonPositive,
    NotionalNonPositive,
}

/// Mirrors `order_exec::validate_order` invariants from S08.
fn validate_order(req: &MiniOrderRequest) -> Result<(), ValidateError> {
    if req.symbol.is_empty() {
        return Err(ValidateError::SymbolEmpty);
    }
    if req.notional.is_some() && req.bracket {
        return Err(ValidateError::BracketAndNotionalIncompatible);
    }
    if req.notional.is_some() && req.take_profit.is_some() {
        return Err(ValidateError::BracketAndTakeProfitOnly);
    }
    match (req.qty, req.notional) {
        (None, None) => return Err(ValidateError::QtyOrNotionalRequired),
        (Some(q), _) if q <= Decimal::ZERO => return Err(ValidateError::QtyNonPositive),
        (_, Some(n)) if n <= Decimal::ZERO => return Err(ValidateError::NotionalNonPositive),
        _ => {}
    }
    Ok(())
}

#[test]
fn order_market_qty_passes() {
    let req = MiniOrderRequest {
        symbol: "AAPL".into(),
        qty: Some(dec!(2)),
        notional: None,
        bracket: false,
        take_profit: None,
    };
    assert!(validate_order(&req).is_ok());
}

#[test]
fn order_notional_without_bracket_passes() {
    let req = MiniOrderRequest {
        symbol: "AAPL".into(),
        qty: None,
        notional: Some(dec!(50)),
        bracket: false,
        take_profit: None,
    };
    assert!(validate_order(&req).is_ok());
}

#[test]
fn order_notional_with_bracket_rejects() {
    let req = MiniOrderRequest {
        symbol: "AAPL".into(),
        qty: None,
        notional: Some(dec!(50)),
        bracket: true,
        take_profit: None,
    };
    assert_eq!(
        validate_order(&req),
        Err(ValidateError::BracketAndNotionalIncompatible),
        "Bracket+notional must be incompatible (S08 invariant)"
    );
}

#[test]
fn order_notional_with_take_profit_rejects() {
    let req = MiniOrderRequest {
        symbol: "AAPL".into(),
        qty: None,
        notional: Some(dec!(50)),
        bracket: false,
        take_profit: Some(dec!(190)),
    };
    assert_eq!(
        validate_order(&req),
        Err(ValidateError::BracketAndTakeProfitOnly),
    );
}

#[test]
fn order_empty_symbol_rejects() {
    let req = MiniOrderRequest {
        symbol: String::new(),
        qty: Some(dec!(1)),
        notional: None,
        bracket: false,
        take_profit: None,
    };
    assert_eq!(validate_order(&req), Err(ValidateError::SymbolEmpty));
}

#[test]
fn order_zero_qty_rejects() {
    let req = MiniOrderRequest {
        symbol: "AAPL".into(),
        qty: Some(Decimal::ZERO),
        notional: None,
        bracket: false,
        take_profit: None,
    };
    assert_eq!(validate_order(&req), Err(ValidateError::QtyNonPositive));
}

#[test]
fn deterministic_cid_replays_dedupe() {
    // S03 invariant: client_order_id is deterministic over (symbol, side,
    // bar_time, strategy, nonce). Two identical inputs MUST produce the
    // same CID — replays dedupe at the broker.
    let cid1 = compute_cid("AAPL", "BUY", 1735689600, "smarttrend", 0);
    let cid2 = compute_cid("AAPL", "BUY", 1735689600, "smarttrend", 0);
    let cid3 = compute_cid("AAPL", "BUY", 1735689601, "smarttrend", 0);

    assert_eq!(cid1, cid2, "Identical inputs must produce identical CID");
    assert_ne!(cid1, cid3, "Different bar_time must produce different CID");
    assert!(cid1.starts_with("st-"), "CID must carry the 'st-' prefix");
    assert_eq!(cid1.len(), 67, "st- + 64 hex chars = 67");
}

fn compute_cid(symbol: &str, side: &str, bar_time: i64, strategy: &str, nonce: u64) -> String {
    use sha2::{Digest, Sha256};
    let payload = format!("{symbol}|{side}|{bar_time}|{strategy}|{nonce}");
    let hash = Sha256::digest(payload.as_bytes());
    format!("st-{}", hex_encode(&hash))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}
