//! Plans scheduled emails as outbox events with a future `available_at`, so they share the
//! dispatcher's retries and idempotency. A `dedupe_key` per booking makes planning idempotent:
//! the scheduler can run any number of times (and on several instances) without duplicates.
//!
//! Send times are computed in Rust (`crate::camp_time`), never with SQL `AT TIME ZONE`.

use chrono::{DateTime, Days, NaiveDate, NaiveTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    camp_time,
    services::{outbox::types, settings},
};

pub const PRE_ARRIVAL_DAYS_KEY: &str = "email.pre_arrival_days";
pub const REVIEW_DAYS_KEY: &str = "email.review_request_days";
/// Local time the scheduled emails go out.
pub const PRE_ARRIVAL_TIME: (u32, u32) = (9, 0);
pub const REVIEW_TIME: (u32, u32) = (10, 0);
/// Stays completed longer ago than this never get a review request.
const REVIEW_WINDOW_DAYS: i64 = 30;

pub const BALANCE_TIME: (u32, u32) = (9, 0);
pub const PRECHECKIN_TIME: (u32, u32) = (10, 0);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Planned {
    pub pre_arrival: u64,
    pub review_request: u64,
    pub balance_reminder: u64,
    pub precheckin_reminder: u64,
}

pub async fn plan(pool: &PgPool, now: DateTime<Utc>) -> sqlx::Result<Planned> {
    let today = camp_time::today(now);
    let pre_days = settings::number(pool, PRE_ARRIVAL_DAYS_KEY, 3).await?;
    let review_days = settings::number(pool, REVIEW_DAYS_KEY, 2).await?;

    // Every confirmed future stay has one pending pre-arrival event; when the arrival date
    // changes before it is sent, the event follows (only if it never ran).
    let stays = sqlx::query!(
        r#"SELECT id, arrival_date AS "arrival_date!" FROM bookings
           WHERE status = 'confirmed' AND arrival_date >= $1"#,
        today
    )
    .fetch_all(pool)
    .await?;
    let (ids, due): (Vec<Uuid>, Vec<DateTime<Utc>>) = stays
        .iter()
        .map(|s| (s.id, pre_arrival_due_at(s.arrival_date, pre_days)))
        .unzip();
    let pre_arrival = sqlx::query!(
        r#"INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload, available_at, dedupe_key)
           SELECT 'booking', t.id::text, $3, jsonb_build_object('booking_id', t.id), t.due, $3 || ':' || t.id
           FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t (id, due)
           ON CONFLICT (dedupe_key) DO UPDATE SET available_at = EXCLUDED.available_at
           WHERE outbox_events.processed_at IS NULL AND outbox_events.failed_at IS NULL
             AND outbox_events.attempts = 0 AND outbox_events.available_at <> EXCLUDED.available_at"#,
        &ids,
        &due,
        types::BOOKING_PRE_ARRIVAL_DUE
    )
    .execute(pool)
    .await?
    .rows_affected();

    let completed = sqlx::query!(
        r#"SELECT b.id, max(h.created_at) AS "completed_at!"
           FROM bookings b
           JOIN booking_status_history h ON h.booking_id = b.id AND h.to_status = 'completed'
           WHERE b.status = 'completed'
           GROUP BY b.id
           HAVING max(h.created_at) >= $1"#,
        now - chrono::Duration::days(REVIEW_WINDOW_DAYS)
    )
    .fetch_all(pool)
    .await?;
    let (ids, due): (Vec<Uuid>, Vec<DateTime<Utc>>) = completed
        .iter()
        .map(|c| {
            let day = camp_time::today(c.completed_at) + Days::new(review_days.max(0) as u64);
            (c.id, local_at(day, REVIEW_TIME))
        })
        .unzip();
    let review_request = sqlx::query!(
        r#"INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload, available_at, dedupe_key)
           SELECT 'booking', t.id::text, $3, jsonb_build_object('booking_id', t.id), t.due, $3 || ':' || t.id
           FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t (id, due)
           ON CONFLICT (dedupe_key) DO NOTHING"#,
        &ids,
        &due,
        types::BOOKING_REVIEW_DUE
    )
    .execute(pool)
    .await?
    .rows_affected();

    // Balance payment link, N days before the balance is due (payment-link collection only:
    // saved cards are charged by the balance job on the due date).
    let reminder_on = sqlx::query_scalar!(
        "SELECT value <> 'false'::jsonb AS \"on!\" FROM settings WHERE key = 'email.balance_reminder_enabled'"
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(true);
    let reminder_days = settings::number(pool, "booking.balance_reminder_days", 3).await?;
    let balances = sqlx::query!(
        r#"SELECT id, balance_due_on AS "balance_due_on!" FROM bookings
           WHERE status = 'confirmed' AND balance_collection = 'payment_link' AND balance_due_on IS NOT NULL
             AND total_minor > amount_paid_minor AND balance_due_on >= $1"#,
        today
    )
    .fetch_all(pool)
    .await?;
    let (ids, due): (Vec<Uuid>, Vec<DateTime<Utc>>) = balances
        .iter()
        .filter(|_| reminder_on)
        .map(|b| {
            (
                b.id,
                local_at(
                    b.balance_due_on - Days::new(reminder_days.max(0) as u64),
                    BALANCE_TIME,
                ),
            )
        })
        .unzip();
    let balance_reminder = sqlx::query!(
        r#"INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload, available_at, dedupe_key)
           SELECT 'booking', t.id::text, $3, jsonb_build_object('booking_id', t.id), t.due, $3 || ':' || t.id
           FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t (id, due)
           ON CONFLICT (dedupe_key) DO NOTHING"#,
        &ids,
        &due,
        types::BOOKING_BALANCE_DUE
    )
    .execute(pool)
    .await?
    .rows_affected();

    // Guest register reminder, N days before arrival, while it is not complete.
    let precheckin_days = settings::number(pool, "booking.precheckin_reminder_days", 5).await?;
    let arrivals = sqlx::query!(
        r#"SELECT id, arrival_date AS "arrival_date!" FROM bookings
           WHERE status = 'confirmed' AND precheckin_completed_at IS NULL AND arrival_date >= $1"#,
        today
    )
    .fetch_all(pool)
    .await?;
    let (ids, due): (Vec<Uuid>, Vec<DateTime<Utc>>) = arrivals
        .iter()
        .map(|b| {
            let day = (b.arrival_date - Days::new(precheckin_days.max(0) as u64)).max(today);
            (b.id, local_at(day, PRECHECKIN_TIME).max(now))
        })
        .unzip();
    let precheckin_reminder = sqlx::query!(
        r#"INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload, available_at, dedupe_key)
           SELECT 'booking', t.id::text, $3, jsonb_build_object('booking_id', t.id), t.due, $3 || ':' || t.id
           FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t (id, due)
           ON CONFLICT (dedupe_key) DO NOTHING"#,
        &ids,
        &due,
        types::BOOKING_PRECHECKIN_DUE
    )
    .execute(pool)
    .await?
    .rows_affected();

    if pre_arrival + review_request + balance_reminder + precheckin_reminder > 0 {
        tracing::debug!(
            pre_arrival,
            review_request,
            balance_reminder,
            precheckin_reminder,
            "scheduled emails planned"
        );
    }
    Ok(Planned {
        pre_arrival,
        review_request,
        balance_reminder,
        precheckin_reminder,
    })
}

/// When the pre-arrival email of a stay arriving on `arrival` is due.
pub fn pre_arrival_due_at(arrival: NaiveDate, days_before: i64) -> DateTime<Utc> {
    local_at(
        arrival - Days::new(days_before.max(0) as u64),
        PRE_ARRIVAL_TIME,
    )
}

fn local_at(day: NaiveDate, (hour, minute): (u32, u32)) -> DateTime<Utc> {
    let time = NaiveTime::from_hms_opt(hour, minute, 0).expect("valid constant");
    camp_time::to_utc(day.and_time(time))
}
