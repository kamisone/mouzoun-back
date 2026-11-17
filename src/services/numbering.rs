//! Human-facing numbers.
//!
//! - Booking reference `MZ-<yy>-<000123>`: restarts at 1 every year (Africa/Casablanca year).
//!   One atomic upsert on the year's `booking_reference_counters` row, run outside the booking
//!   transaction: concurrent bookings never wait on each other here; gaps are harmless.
//! - Invoice number `<prefix><yyyy>-<000001>`: gap-free per legal entity and year. The counter
//!   row `(legal_entity_id, year)` is locked `FOR UPDATE` inside the transaction that issues the
//!   invoice, so a rolled-back invoice gives its number back and concurrent invoices get
//!   consecutive numbers.

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

/// Invoice number `<prefix><yyyy>-<000001>`, gap-free per legal entity and per year. Must be
/// called inside the transaction that inserts the invoice (the counter row stays locked until it
/// commits, so a rolled-back invoice gives its number back).
pub async fn next_invoice_number(
    tx: &mut PgConnection,
    legal_entity_id: uuid::Uuid,
    year: i32,
) -> sqlx::Result<String> {
    let year_key = i16::try_from(year).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    sqlx::query!(
        "INSERT INTO invoice_entity_counters (legal_entity_id, year) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        legal_entity_id,
        year_key
    )
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query!(
        r#"SELECT c.last_number, e.invoice_prefix FROM invoice_entity_counters c
           JOIN legal_entities e ON e.id = c.legal_entity_id
           WHERE c.legal_entity_id = $1 AND c.year = $2 FOR UPDATE OF c"#,
        legal_entity_id,
        year_key
    )
    .fetch_one(&mut *tx)
    .await?;
    let next = row.last_number + 1;
    sqlx::query!(
        "UPDATE invoice_entity_counters SET last_number = $3 WHERE legal_entity_id = $1 AND year = $2",
        legal_entity_id,
        year_key,
        next
    )
    .execute(&mut *tx)
    .await?;
    Ok(format!("{}{year}-{next:06}", row.invoice_prefix))
}
