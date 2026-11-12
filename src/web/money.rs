//! Amounts typed by staff in euros ("45.50", "45,50", "45"), converted to cents at the API
//! boundary. Never floats: the string is parsed digit by digit.

use serde::{Deserialize, Deserializer, de};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Euros(pub i64);

pub fn parse_euros(raw: &str) -> Option<i64> {
    let s = raw.trim().replace([' ', '\u{a0}', '\u{202f}'], "");
    let s = s.strip_suffix('€').unwrap_or(&s).trim();
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let s = s.replace(',', ".");
    let (units, cents) = match s.split_once('.') {
        Some((u, c)) => (u, c),
        None => (s.as_str(), ""),
    };
    if units.is_empty() && cents.is_empty() {
        return None;
    }
    if !units.chars().all(|c| c.is_ascii_digit())
        || !cents.chars().all(|c| c.is_ascii_digit())
        || cents.len() > 2
    {
        return None;
    }
    let units: i64 = if units.is_empty() {
        0
    } else {
        units.parse().ok()?
    };
    let cents: i64 = match cents.len() {
        0 => 0,
        1 => cents.parse::<i64>().ok()? * 10,
        _ => cents.parse().ok()?,
    };
    let total = units.checked_mul(100)?.checked_add(cents)?;
    Some(if negative { -total } else { total })
}

impl<'de> Deserialize<'de> for Euros {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Integer(i64),
        }
        match Raw::deserialize(d)? {
            Raw::Text(s) => parse_euros(&s).map(Euros).ok_or_else(|| {
                de::Error::custom(format!("invalid amount {s:?} (expected e.g. 45.50)"))
            }),
            Raw::Integer(n) => n
                .checked_mul(100)
                .map(Euros)
                .ok_or_else(|| de::Error::custom("amount too large")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn euro_strings_become_exact_cents() {
        assert_eq!(parse_euros("45.50"), Some(4_550));
        assert_eq!(parse_euros("45,5"), Some(4_550));
        assert_eq!(parse_euros("45"), Some(4_500));
        assert_eq!(parse_euros("1 250,00 €"), Some(125_000));
        assert_eq!(parse_euros("0.07"), Some(7));
        assert_eq!(parse_euros("-3.20"), Some(-320));
        assert_eq!(parse_euros("4.555"), None, "no sub-cent amounts");
        assert_eq!(parse_euros("abc"), None);
        assert_eq!(parse_euros(""), None);
        let e: Euros = serde_json::from_str("\"19.99\"").unwrap();
        assert_eq!(e, Euros(1_999));
        let e: Euros = serde_json::from_str("20").unwrap();
        assert_eq!(e, Euros(2_000));
        assert!(serde_json::from_str::<Euros>("19.99").is_err(), "no floats");
    }
}
