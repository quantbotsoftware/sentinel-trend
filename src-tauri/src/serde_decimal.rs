use rust_decimal::Decimal;
use serde::{self, Deserializer, Serializer};
use std::str::FromStr;

struct DecimalVisitor;

impl<'de> serde::de::Visitor<'de> for DecimalVisitor {
    type Value = Decimal;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a decimal string or number")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Decimal, E> {
        Decimal::from_str(v).map_err(E::custom)
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Decimal, E> {
        Decimal::try_from(v).map_err(E::custom)
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Decimal, E> {
        Ok(Decimal::from(v))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Decimal, E> {
        Ok(Decimal::from(v))
    }
}

pub fn serialize<S: Serializer>(val: &Decimal, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&val.to_string())
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
    d.deserialize_any(DecimalVisitor)
}

pub mod option {
    use rust_decimal::Decimal;
    use serde::{self, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(val: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error> {
        match val {
            Some(d) => s.serialize_some(&d.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
        use serde::de;
        use std::str::FromStr;

        struct OptVisitor;

        impl<'de> de::Visitor<'de> for OptVisitor {
            type Value = Option<Decimal>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("null, a decimal string, or a number")
            }

            fn visit_none<E: de::Error>(self) -> Result<Option<Decimal>, E> {
                Ok(None)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Option<Decimal>, E> {
                Ok(None)
            }

            fn visit_some<D2: Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<Option<Decimal>, D2::Error> {
                d.deserialize_any(InnerVisitor).map(Some)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<Decimal>, E> {
                Decimal::from_str(v).map(Some).map_err(E::custom)
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Option<Decimal>, E> {
                Decimal::try_from(v).map(Some).map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<Decimal>, E> {
                Ok(Some(Decimal::from(v)))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<Decimal>, E> {
                Ok(Some(Decimal::from(v)))
            }
        }

        struct InnerVisitor;

        impl<'de> de::Visitor<'de> for InnerVisitor {
            type Value = Decimal;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a decimal string or number")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Decimal, E> {
                Decimal::from_str(v).map_err(E::custom)
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Decimal, E> {
                Decimal::try_from(v).map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Decimal, E> {
                Ok(Decimal::from(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Decimal, E> {
                Ok(Decimal::from(v))
            }
        }

        d.deserialize_option(OptVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn roundtrip_string() {
        #[derive(serde::Serialize, serde::Deserialize, Debug)]
        struct T {
            #[serde(with = "super")]
            val: Decimal,
        }
        let t = T { val: dec!(123.456) };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"123.456\""));
        let loaded: T = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.val, dec!(123.456));
    }

    #[test]
    fn deserialize_from_float() {
        #[derive(serde::Deserialize)]
        struct T {
            #[serde(with = "super")]
            val: Decimal,
        }
        let json = r#"{"val": 42.5}"#;
        let t: T = serde_json::from_str(json).unwrap();
        assert_eq!(t.val, dec!(42.5));
    }

    #[test]
    fn deserialize_from_integer() {
        #[derive(serde::Deserialize)]
        struct T {
            #[serde(with = "super")]
            val: Decimal,
        }
        let json = r#"{"val": 100}"#;
        let t: T = serde_json::from_str(json).unwrap();
        assert_eq!(t.val, dec!(100));
    }

    #[test]
    fn option_roundtrip() {
        #[derive(serde::Serialize, serde::Deserialize, Debug)]
        struct T {
            #[serde(with = "super::option")]
            val: Option<Decimal>,
        }
        let t = T { val: Some(dec!(99.99)) };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"99.99\""));
        let loaded: T = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.val, Some(dec!(99.99)));

        let t_none = T { val: None };
        let json_none = serde_json::to_string(&t_none).unwrap();
        assert!(json_none.contains("null"));
        let loaded_none: T = serde_json::from_str(&json_none).unwrap();
        assert_eq!(loaded_none.val, None);
    }

    #[test]
    fn option_from_float() {
        #[derive(serde::Deserialize)]
        struct T {
            #[serde(with = "super::option")]
            val: Option<Decimal>,
        }
        let json = r#"{"val": 185.50}"#;
        let t: T = serde_json::from_str(json).unwrap();
        assert_eq!(t.val, Some(dec!(185.50)));
    }
}
