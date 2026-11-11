//! 0001_foundation: locales, currencies, exchange rates.

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;

use super::types::{CurrencyCode, Locale};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocaleRecord {
    pub code: Locale,
    pub name: String,
    pub native_name: String,
    pub is_rtl: bool,
    pub is_default: bool,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Currency {
    pub code: CurrencyCode,
    pub name: String,
    pub symbol: String,
    pub minor_units: i16,
    pub is_default: bool,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ExchangeRate {
    pub base_currency: CurrencyCode,
    pub quote_currency: CurrencyCode,
    pub rate: Decimal,
    pub valid_on: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Active locales, default first then by sort order.
pub async fn active_locales(db: impl PgExecutor<'_>) -> sqlx::Result<Vec<LocaleRecord>> {
    sqlx::query_as!(
        LocaleRecord,
        r#"SELECT code AS "code: Locale", name, native_name, is_rtl, is_default, is_active, sort_order,
                  created_at, updated_at
           FROM locales
           WHERE is_active
           ORDER BY is_default DESC, sort_order, code"#
    )
    .fetch_all(db)
    .await
}

pub async fn default_currency(db: impl PgExecutor<'_>) -> sqlx::Result<Currency> {
    sqlx::query_as!(
        Currency,
        r#"SELECT code AS "code: CurrencyCode", name, symbol, minor_units, is_default, is_active,
                  created_at, updated_at
           FROM currencies
           WHERE is_default"#
    )
    .fetch_one(db)
    .await
}
