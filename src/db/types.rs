//! Shared value types of the data layer: money, locales, translations, weekday bitmask, and the
//! `text_enum!` macro that mirrors every `text + CHECK (... IN (...))` column.

use std::{collections::HashMap, fmt};

use chrono::{Datelike, NaiveDate};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

/// Declares a Rust enum mirroring a `text` column constrained by `CHECK (col IN (...))`.
///
/// Each variant states its stored value explicitly; `rename_all = "snake_case"` encodes the same
/// value for sqlx, and `tests/it/schema.rs` proves the three agree with the database constraint.
#[macro_export]
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $value:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, sqlx::Type)]
        #[sqlx(type_name = "text", rename_all = "snake_case")]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The value stored in the database.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

// ---- Locales & translations ----------------------------------------------------------------

/// A language code from the `locales` table (`fr`, `en`, `ar`, ...). Never hard-coded: the
/// list of languages lives in the database.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, sqlx::Type,
)]
#[sqlx(transparent)]
#[serde(transparent)]
pub struct Locale(pub String);

impl Locale {
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Locale {
    fn from(code: &str) -> Self {
        Self(code.to_owned())
    }
}

/// Translations of an entity, keyed by locale (the `x_translations` rows of one parent).
pub type Translations<T> = HashMap<Locale, T>;

/// Field-by-field fallback: a translated value is used when present and non-empty, otherwise
/// the value of the default locale.
pub trait Fallback {
    fn or_fallback(self, default: Self) -> Self;
}

impl Fallback for String {
    fn or_fallback(self, default: Self) -> Self {
        if self.trim().is_empty() {
            default
        } else {
            self
        }
    }
}

impl<T> Fallback for Option<T>
where
    T: Fallback,
{
    fn or_fallback(self, default: Self) -> Self {
        match (self, default) {
            (Some(v), Some(d)) => Some(v.or_fallback(d)),
            (Some(v), None) => Some(v),
            (None, d) => d,
        }
    }
}

impl<T> Fallback for Vec<T> {
    fn or_fallback(self, default: Self) -> Self {
        if self.is_empty() { default } else { self }
    }
}

// ---- Money ------------------------------------------------------------------------------------

/// ISO 4217 currency code (`EUR`, `MAD`, `USD`), as stored in `currencies.code`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(transparent)]
#[serde(transparent)]
pub struct CurrencyCode(pub String);

impl CurrencyCode {
    pub fn new(code: &str) -> Self {
        Self(code.trim().to_ascii_uppercase())
    }
    pub fn eur() -> Self {
        Self("EUR".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("currency mismatch: {0} vs {1}")]
    CurrencyMismatch(CurrencyCode, CurrencyCode),
    #[error("amount overflow")]
    Overflow,
}

/// An amount of money in minor units (cents). Never a float.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub amount_minor: i64,
    pub currency: CurrencyCode,
}

impl Money {
    pub fn new(amount_minor: i64, currency: CurrencyCode) -> Self {
        Self {
            amount_minor,
            currency,
        }
    }

    pub fn zero(currency: CurrencyCode) -> Self {
        Self::new(0, currency)
    }

    pub fn checked_add(&self, other: &Money) -> Result<Money, MoneyError> {
        self.same_currency(other)?;
        let amount = self
            .amount_minor
            .checked_add(other.amount_minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(amount, self.currency.clone()))
    }

    pub fn checked_sub(&self, other: &Money) -> Result<Money, MoneyError> {
        self.same_currency(other)?;
        let amount = self
            .amount_minor
            .checked_sub(other.amount_minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(amount, self.currency.clone()))
    }

    pub fn checked_mul(&self, factor: i64) -> Result<Money, MoneyError> {
        let amount = self
            .amount_minor
            .checked_mul(factor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(amount, self.currency.clone()))
    }

    fn same_currency(&self, other: &Money) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch(
                self.currency.clone(),
                other.currency.clone(),
            ))
        }
    }
}

/// `amount × percent / 100`, rounded half away from zero to the cent.
pub fn percent_of(amount_minor: i64, percent: Decimal) -> Result<i64, MoneyError> {
    (Decimal::from(amount_minor) * percent / Decimal::ONE_HUNDRED)
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
        .to_i64()
        .ok_or(MoneyError::Overflow)
}

/// Tax contained in a tax-inclusive amount: `gross × rate / (100 + rate)`, rounded half up.
pub fn inclusive_tax_of(gross_minor: i64, rate_percent: Decimal) -> Result<i64, MoneyError> {
    (Decimal::from(gross_minor) * rate_percent / (Decimal::ONE_HUNDRED + rate_percent))
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
        .to_i64()
        .ok_or(MoneyError::Overflow)
}

// ---- Weekday bitmask --------------------------------------------------------------------------

/// `days_of_week` bitmask: Mon=1, Tue=2, Wed=4, Thu=8, Fri=16, Sat=32, Sun=64 (127 = every day).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaysOfWeek(pub i16);

impl DaysOfWeek {
    pub const ALL: DaysOfWeek = DaysOfWeek(127);

    pub fn bit(day: NaiveDate) -> i16 {
        1 << day.weekday().num_days_from_monday()
    }

    pub fn contains(self, day: NaiveDate) -> bool {
        self.0 & Self::bit(day) != 0
    }

    pub fn is_every_day(self) -> bool {
        self.0 & 127 == 127
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_never_mixes_currencies() {
        let eur = Money::new(1_000, CurrencyCode::eur());
        let mad = Money::new(1_000, CurrencyCode::new("mad"));
        assert_eq!(eur.checked_add(&eur).unwrap().amount_minor, 2_000);
        assert!(matches!(
            eur.checked_add(&mad),
            Err(MoneyError::CurrencyMismatch(..))
        ));
        assert_eq!(
            Money::new(i64::MAX, CurrencyCode::eur()).checked_mul(2),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn percentages_round_half_up_to_the_cent() {
        assert_eq!(percent_of(1_005, Decimal::new(10, 0)).unwrap(), 101); // 100.5 → 101
        assert_eq!(percent_of(1_004, Decimal::new(10, 0)).unwrap(), 100); // 100.4 → 100
        assert_eq!(percent_of(12_345, Decimal::new(125, 1)).unwrap(), 1_543); // 12.5 %
        assert_eq!(
            inclusive_tax_of(12_000, Decimal::new(20, 0)).unwrap(),
            2_000
        );
    }

    #[test]
    fn weekday_bitmask_is_monday_first() {
        let monday = NaiveDate::from_ymd_opt(2027, 1, 4).unwrap();
        let sunday = NaiveDate::from_ymd_opt(2027, 1, 10).unwrap();
        assert_eq!(DaysOfWeek::bit(monday), 1);
        assert_eq!(DaysOfWeek::bit(sunday), 64);
        let weekend = DaysOfWeek(32 | 64);
        assert!(weekend.contains(sunday) && !weekend.contains(monday));
        assert!(DaysOfWeek::ALL.is_every_day());
    }

    #[test]
    fn translations_fall_back_field_by_field() {
        assert_eq!("".to_string().or_fallback("fr".into()), "fr");
        assert_eq!(
            Some("en".to_string()).or_fallback(Some("fr".into())),
            Some("en".into())
        );
        assert_eq!(
            Some("  ".to_string()).or_fallback(Some("fr".into())),
            Some("fr".into())
        );
        assert_eq!(
            None::<String>.or_fallback(Some("fr".into())),
            Some("fr".into())
        );
        assert_eq!(
            Vec::<String>::new().or_fallback(vec!["fr".into()]),
            vec!["fr".to_string()]
        );
    }

    crate::text_enum!(Sample { Draft = "draft", AwaitingPayment = "awaiting_payment", CheckedIn = "checked_in" });

    #[test]
    fn text_enum_values_match_their_serde_encoding() {
        for v in Sample::ALL {
            assert_eq!(
                serde_json::to_string(v).unwrap(),
                format!("\"{}\"", v.as_str())
            );
        }
    }
}

// ---- Ranges -----------------------------------------------------------------------------------

/// Half-open date range `[start, end)` (`daterange`): nights of a stay, season periods...
/// `None` means unbounded on that side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRange {
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
}

/// Half-open instant range `[start, end)` (`tstzrange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsRange {
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub end: Option<chrono::DateTime<chrono::Utc>>,
}

impl DateRange {
    pub fn new(start: NaiveDate, end: NaiveDate) -> Self {
        Self {
            start: Some(start),
            end: Some(end),
        }
    }

    /// True when `day` is inside `[start, end)`.
    pub fn contains(&self, day: NaiveDate) -> bool {
        self.start.is_none_or(|s| day >= s) && self.end.is_none_or(|e| day < e)
    }

    /// Nights of a `[arrival, departure)` stay.
    pub fn nights(&self) -> Option<i64> {
        Some((self.end? - self.start?).num_days())
    }
}

impl TsRange {
    pub fn new(start: chrono::DateTime<chrono::Utc>, end: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            start: Some(start),
            end: Some(end),
        }
    }

    pub fn contains(&self, at: chrono::DateTime<chrono::Utc>) -> bool {
        self.start.is_none_or(|s| at >= s) && self.end.is_none_or(|e| at < e)
    }
}

mod pg_ranges {
    //! sqlx encoding of the range wrappers through `PgRange` (Postgres canonicalises discrete
    //! ranges to `[)`, and the application only ever builds `[)` ranges).
    use std::ops::Bound;

    use chrono::{DateTime, NaiveDate, Utc};
    use sqlx::{
        Decode, Encode, Postgres, Type,
        encode::IsNull,
        error::BoxDynError,
        postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef, types::PgRange},
    };

    use super::{DateRange, TsRange};

    fn lower<T>(bound: Bound<T>) -> Option<T> {
        match bound {
            Bound::Included(v) | Bound::Excluded(v) => Some(v),
            Bound::Unbounded => None,
        }
    }

    fn to_pg<T>(start: Option<T>, end: Option<T>) -> PgRange<T> {
        PgRange {
            start: start.map_or(Bound::Unbounded, Bound::Included),
            end: end.map_or(Bound::Unbounded, Bound::Excluded),
        }
    }

    macro_rules! range_type {
        ($wrapper:ty, $inner:ty) => {
            impl Type<Postgres> for $wrapper {
                fn type_info() -> PgTypeInfo {
                    <PgRange<$inner> as Type<Postgres>>::type_info()
                }
                fn compatible(ty: &PgTypeInfo) -> bool {
                    <PgRange<$inner> as Type<Postgres>>::compatible(ty)
                }
            }

            impl<'r> Decode<'r, Postgres> for $wrapper {
                fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
                    let range = <PgRange<$inner> as Decode<Postgres>>::decode(value)?;
                    Ok(Self {
                        start: lower(range.start),
                        end: lower(range.end),
                    })
                }
            }

            impl Encode<'_, Postgres> for $wrapper {
                fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
                    <PgRange<$inner> as Encode<Postgres>>::encode_by_ref(
                        &to_pg(self.start, self.end),
                        buf,
                    )
                }
            }
        };
    }

    range_type!(DateRange, NaiveDate);
    range_type!(TsRange, DateTime<Utc>);
}

// ---- Translated DTOs --------------------------------------------------------------------------

/// A record with its translations: `{ ...record fields, "translations": { "fr": {...}, ... } }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithTranslations<R, T> {
    #[serde(flatten)]
    pub record: R,
    pub translations: Translations<T>,
}

/// Groups translation rows by parent id: `rows` carry `(parent_id, locale)`.
pub fn group_translations<T>(
    rows: impl IntoIterator<Item = T>,
    key: impl Fn(&T) -> (uuid::Uuid, Locale),
) -> HashMap<uuid::Uuid, Translations<T>> {
    let mut grouped: HashMap<uuid::Uuid, Translations<T>> = HashMap::new();
    for row in rows {
        let (parent, locale) = key(&row);
        grouped.entry(parent).or_default().insert(locale, row);
    }
    grouped
}
