//! Human-facing numbers.
//!
//! - Booking reference `MZ-<yy>-<000123>`: restarts at 1 every year (Africa/Casablanca year).
//!   One atomic upsert on the year's `booking_reference_counters` row, run outside the booking
//!   transaction: concurrent bookings never wait on each other here; gaps are harmless.
//! - Invoice number `<yyyy>-<000001>`: gap-free per year. The year's row of `invoice_counters` is
//!   locked `FOR UPDATE` inside the transaction that issues the invoice, so a rolled-back
//!   invoice gives its number back and concurrent invoices get consecutive numbers.

use chrono::{DateTime, Datelike, Utc};
use sqlx::{PgConnection, PgExecutor};

/// Pass the pool, not a transaction: inside a transaction the counter row would stay locked
/// until commit and serialize every booking of the year.
pub async fn next_booking_reference(
    db: impl PgExecutor<'_>,
    now: DateTime<Utc>,
) -> sqlx::Result<String> {
    let year = crate::camp_time::local(now).year();
    let year_key = i16::try_from(year).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let number = sqlx::query_scalar!(
        "INSERT INTO booking_reference_counters (year, last_number) VALUES ($1, 1)
         ON CONFLICT (year) DO UPDATE SET last_number = booking_reference_counters.last_number + 1
         RETURNING last_number",
        year_key
    )
    .fetch_one(db)
    .await?;
    Ok(format!("MZ-{:02}-{number:06}", year % 100))
}

/// Must be called inside the transaction that inserts the invoice.
pub async fn next_invoice_number(tx: &mut PgConnection, year: i32) -> sqlx::Result<String> {
    let year_key = i16::try_from(year).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    sqlx::query!(
        "INSERT INTO invoice_counters (year) VALUES ($1) ON CONFLICT (year) DO NOTHING",
        year_key
    )
    .execute(&mut *tx)
    .await?;
    let last = sqlx::query_scalar!(
        "SELECT last_number FROM invoice_counters WHERE year = $1 FOR UPDATE",
        year_key
    )
    .fetch_one(&mut *tx)
    .await?;
    let next = last + 1;
    sqlx::query!(
        "UPDATE invoice_counters SET last_number = $2 WHERE year = $1",
        year_key,
        next
    )
    .execute(&mut *tx)
    .await?;
    Ok(format!("{year}-{next:06}"))
}
