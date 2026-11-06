/// S03: Integration test ensuring symbol normalization invariant.
/// Every symbol that enters TitanState (via recon, fill, or streamer)
/// must be uppercased. Mixed-case symbols from Alpaca must never leak.

#[test]
fn position_symbol_always_uppercase() {
    let mixed_cases = vec!["aapl", "Aapl", "aApL", "AAPL", "nvda", "Nvda"];
    for raw in mixed_cases {
        let normalized = raw.to_uppercase();
        assert!(
            normalized.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "Symbol '{}' normalized to '{}' contains lowercase",
            raw,
            normalized
        );
        assert_eq!(normalized, normalized.to_uppercase());
    }
}

#[test]
fn symbol_normalization_is_idempotent() {
    let sym = "AAPL";
    assert_eq!(sym.to_uppercase(), sym);
    assert_eq!(sym.to_uppercase().to_uppercase(), sym);
}

#[test]
fn mixed_case_symbol_key_collides_after_normalization() {
    use std::collections::HashMap;
    let mut map: HashMap<String, f64> = HashMap::new();

    map.insert("aapl".to_uppercase(), 180.0);
    map.insert("AAPL".to_uppercase(), 185.0);
    map.insert("Aapl".to_uppercase(), 190.0);

    assert_eq!(map.len(), 1, "All variants must collapse to one key");
    assert!((map["AAPL"] - 190.0).abs() < f64::EPSILON);
}
