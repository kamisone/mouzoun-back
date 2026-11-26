//! Percentages crossing the API (`discount_percent`, `deposit_percent`, VAT shares...).
//!
//! One format everywhere: the API **answers** a decimal string (`"12.5"`, how `rust_decimal`
//! serialises with the `serde-str` feature) and **accepts** a decimal string or a JSON number
//! (`12.5`, `70`). A number is read from its JSON text, never through float arithmetic, so
//! `12.35` stays exactly 12.35.
//!
//! Without this, `Decimal` fields refused JSON numbers ("invalid type: integer, expected a
//! Decimal") — the gift box VAT split bug: the editor sent `share_percent: 70`.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, de};

#[derive(Deserialize)]
#[serde(untagged)]
enum Raw {
    Text(String),
    Number(serde_json::Number),
}

fn parse(raw: Raw) -> Result<Decimal, String> {
    let text = match raw {
        Raw::Text(s) => s.trim().replace(',', "."),
        Raw::Number(n) => n.to_string(),
    };
    Decimal::from_str(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .map(|d| d.normalize())
        .map_err(|_| format!("invalid percentage {text:?} (expected e.g. 12.5)"))
}

/// `#[serde(deserialize_with = "crate::web::percent::decimal")]`
pub fn decimal<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
    parse(Raw::deserialize(d)?).map_err(de::Error::custom)
}

/// `#[serde(default, deserialize_with = "crate::web::percent::option")]`
pub fn option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
    match Option::<Raw>::deserialize(d)? {
        None => Ok(None),
        Some(Raw::Text(s)) if s.trim().is_empty() => Ok(None),
        Some(raw) => parse(raw).map(Some).map_err(de::Error::custom),
    }
}

/// A percentage stored in JSON (a jsonb column) as either a number or a string.
pub fn from_json(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::Number(n) => parse(Raw::Number(n.clone())).ok(),
        serde_json::Value::String(s) => parse(Raw::Text(s.clone())).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[derive(Deserialize)]
    struct P {
        #[serde(deserialize_with = "decimal")]
        p: Decimal,
        #[serde(default, deserialize_with = "option")]
        o: Option<Decimal>,
    }

    fn p(json: &str) -> P {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn numbers_and_strings_give_the_same_exact_decimal() {
        assert_eq!(p(r#"{"p": 70}"#).p, dec("70"));
        assert_eq!(p(r#"{"p": "70"}"#).p, dec("70"));
        assert_eq!(p(r#"{"p": 12.35}"#).p, dec("12.35"));
        assert_eq!(p(r#"{"p": "12,35"}"#).p, dec("12.35"));
        assert_eq!(p(r#"{"p": 0.1}"#).p + p(r#"{"p": 0.2}"#).p, dec("0.3"));
        assert_eq!(p(r#"{"p": 10, "o": null}"#).o, None);
        assert_eq!(p(r#"{"p": 10, "o": ""}"#).o, None);
        assert_eq!(p(r#"{"p": 10, "o": 7.5}"#).o, Some(dec("7.5")));
        assert!(serde_json::from_str::<P>(r#"{"p": "dix"}"#).is_err());
        // Answers are strings: the one format the front reads back.
        assert_eq!(
            serde_json::to_value(dec("12.50").normalize()).unwrap(),
            "12.5"
        );
        assert_eq!(from_json(&serde_json::json!(70)), Some(dec("70")));
        assert_eq!(from_json(&serde_json::json!("30")), Some(dec("30")));
    }
}
