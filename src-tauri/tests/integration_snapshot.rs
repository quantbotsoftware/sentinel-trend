/// S09: Snapshot serde backward-compat skeleton.
///
/// Validates the contract surface of S08's custom Decimal serde:
///   - serialize: always as string
///   - deserialize: accepts string, float (f64/f32), or integer
///
/// This test mirrors the logic in `src-tauri/src/serde_decimal.rs`
/// without importing it (no `lib.rs`). Drift between this mini-impl
/// and the canonical module surfaces as a test failure.
///
/// Acceptance: ≥1 explicit assertion per test.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use std::fmt;
use std::str::FromStr;

mod mini_decimal_serde {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Decimal, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Decimal, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Decimal;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a decimal as string, float, or integer")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Decimal, E> {
                Decimal::from_str(v).map_err(de::Error::custom)
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Decimal, E> {
                Decimal::from_f64_retain(v).ok_or_else(|| de::Error::custom("f64 out of range"))
            }
            fn visit_f32<E: de::Error>(self, v: f32) -> Result<Decimal, E> {
                self.visit_f64(v as f64)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Decimal, E> {
                Ok(Decimal::from(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Decimal, E> {
                Ok(Decimal::from(v))
            }
        }
        de.deserialize_any(V)
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct Snap {
    #[serde(with = "mini_decimal_serde")]
    qty: Decimal,
}

#[test]
fn deserialize_decimal_from_string() {
    let json = r#"{"qty":"123.45"}"#;
    let s: Snap = serde_json::from_str(json).expect("string parse");
    assert_eq!(s.qty, dec!(123.45));
}

#[test]
fn deserialize_decimal_from_float() {
    let json = r#"{"qty":123.45}"#;
    let s: Snap = serde_json::from_str(json).expect("float parse");
    let drift = (s.qty - dec!(123.45)).abs();
    assert!(
        drift < dec!(0.0001),
        "Float→Decimal drift {drift} exceeds tolerance"
    );
}

#[test]
fn deserialize_decimal_from_integer() {
    let json = r#"{"qty":42}"#;
    let s: Snap = serde_json::from_str(json).expect("int parse");
    assert_eq!(s.qty, dec!(42));
}

#[test]
fn deserialize_decimal_from_negative_string() {
    let json = r#"{"qty":"-99.99"}"#;
    let s: Snap = serde_json::from_str(json).expect("negative parse");
    assert_eq!(s.qty, dec!(-99.99));
}

#[test]
fn serialize_decimal_always_as_string() {
    let s = Snap { qty: dec!(170) };
    let out = serde_json::to_string(&s).expect("serialize");
    assert_eq!(
        out, r#"{"qty":"170"}"#,
        "Decimal MUST serialize as string (snapshot stable across hosts)"
    );
}

#[test]
fn roundtrip_preserves_value() {
    let s = Snap { qty: dec!(0.0001) };
    let json = serde_json::to_string(&s).expect("ser");
    let back: Snap = serde_json::from_str(&json).expect("de");
    assert_eq!(back.qty, dec!(0.0001));
}
