//! Hourly booking housekeeping. Every step is idempotent and safe on several instances.
//!
//! - charges due balances on saved cards (consent given at checkout); a decline becomes a staff
//!   task + emails, never an automatic cancellation;
//! - proposes no-shows to staff (a task the day after a missed arrival, never automatic);
//! - completes checked-in stays the day after departure (check-out was forgotten);
//! - erases guest ID numbers after the legal retention period;
//! - records the lowest nightly price per product and date (EU Omnibus reference price).

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    camp_time,
    db::{
        bookings::{BookingStatus, StaffTaskKind},
        foundation,
    },
    services::{
        audit::Actor,
        booking::{register, settings, state},
        payments::{self, Payments},
        pricing::{self, GuestCount, PricingRequest, ServiceTime},
    },
};

pub async fn run_once(db: PgPool, payments: Payments) -> anyhow::Result<()> {
    let now = Utc::now();
    let today = camp_time::today(now);
    let settings = settings::load(&db).await?;

    // Balances due on saved cards.
    if settings.balance_auto_charge {
        let due = sqlx::query_scalar!(
            "SELECT id FROM bookings
             WHERE status = 'confirmed' AND balance_collection = 'saved_card' AND saved_payment_method_id IS NOT NULL
               AND balance_due_on <= $1 AND total_minor > amount_paid_minor
             ORDER BY balance_due_on LIMIT 50",
            today
        )
        .fetch_all(&db)
        .await?;
        for booking_id in due {
            if let Err(e) = payments::charge_balance(&db, &payments, booking_id, now).await {
                tracing::warn!(%booking_id, error = %e, "balance charge failed, retried next run");
            }
        }
    }

    // No-show proposals: arrival day passed, never checked in.
    let missed = sqlx::query!(
        "SELECT id, reference FROM bookings
         WHERE status = 'confirmed' AND arrival_date < $1 AND arrival_date >= $1 - 7",
        today
    )
    .fetch_all(&db)
    .await?;
    for b in missed {
        let mut conn = db.acquire().await?;
        payments::create_task(
            &mut conn,
            StaffTaskKind::NoShowReview,
            Some(b.id),
            None,
            "Guest did not check in: mark as no-show or check in",
            json!({ "reference": b.reference }),
            Some(&format!("no_show_review:{}", b.id)),
        )
        .await?;
    }

    // Stays left checked in after their departure day.
    let finished = sqlx::query_scalar!(
        "SELECT id FROM bookings WHERE status = 'checked_in' AND departure_date < $1 LIMIT 100",
        today
    )
    .fetch_all(&db)
    .await?;
    for booking_id in finished {
        let mut tx = db.begin().await?;
        match state::transition(
            &mut tx,
            booking_id,
            BookingStatus::Completed,
            &Actor::system(),
            Some("automatic check-out"),
            now,
        )
        .await
        {
            Ok(_) => tx.commit().await?,
            Err(e) => tracing::warn!(%booking_id, error = %e, "automatic check-out skipped"),
        }
    }

    let purged = register::purge_identity_numbers(&db, settings.id_retention_days, today).await?;
    if purged > 0 {
        tracing::info!(purged, "guest ID numbers erased after retention");
    }

    record_price_observations(&db, today).await?;
    Ok(())
}

/// Lowest nightly price (standard occupancy, no promotion) of every accommodation for the next
/// 180 days, once a day. Kept 35 days: enough for the 30-day Omnibus reference.
pub async fn record_price_observations(db: &PgPool, today: NaiveDate) -> anyhow::Result<()> {
    let done = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM price_observations WHERE observed_on = $1) AS \"done!\"",
        today
    )
    .fetch_one(db)
    .await?;
    if done {
        return Ok(());
    }
    let currency = foundation::default_currency(db).await?.code;
    let adult = sqlx::query_scalar!("SELECT id FROM guest_categories WHERE code = 'adult'")
        .fetch_optional(db)
        .await?;
    let Some(adult) = adult else { return Ok(()) };
    let products = sqlx::query!(
        "SELECT a.product_id, a.standard_occupancy FROM accommodation_types a
         JOIN products p ON p.id = a.product_id WHERE p.status = 'published' AND p.deleted_at IS NULL"
    )
    .fetch_all(db)
    .await?;
    let horizon = today + Duration::days(180);
    let mut rows: HashMap<(Uuid, NaiveDate), i64> = HashMap::new();
    let mut conn = db.acquire().await?;
    for p in products {
        let guests = [GuestCount {
            guest_category_id: adult,
            count: p.standard_occupancy,
        }];
        let ctx =
            pricing::load_context(&mut conn, p.product_id, &currency, today, horizon, None).await?;
        let mut day = today;
        while day < horizon {
            let req = PricingRequest {
                product_id: p.product_id,
                rate_plan_id: None,
                service: ServiceTime::Stay {
                    arrival: day,
                    departure: day + Duration::days(1),
                },
                guests: &guests,
                quantity: 1,
                standard_occupancy: Some(p.standard_occupancy),
                booked_at: Utc::now(),
                booked_on: today,
            };
            if let Ok((_, amount)) = pricing::engine::price_lines(&req, &ctx) {
                rows.insert((p.product_id, day), amount);
            }
            day += Duration::days(1);
        }
    }
    let (products, (days, amounts)): (Vec<Uuid>, (Vec<NaiveDate>, Vec<i64>)) =
        rows.into_iter().map(|((p, d), a)| (p, (d, a))).unzip();
    sqlx::query!(
        "INSERT INTO price_observations (product_id, service_date, observed_on, amount_minor, currency)
         SELECT p, d, $4, a, $5 FROM UNNEST($1::uuid[], $2::date[], $3::int8[]) AS t (p, d, a)
         ON CONFLICT DO NOTHING",
        &products,
        &days,
        &amounts,
        today,
        currency.as_str()
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM price_observations WHERE observed_on < $1",
        today - Duration::days(35)
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}
