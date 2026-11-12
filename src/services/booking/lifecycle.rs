//! Booking lifecycle (invariant 3): confirm, cancel, expire. Cancelling or expiring releases
//! everything the booking holds in one transaction: `reserved` counters, unit allocations and
//! resource assignments, with a status-history row and an outbox event.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{BookingError, common::record_status, state};
use crate::{
    db::{
        bookings::{BookingStatus, CancelledBy},
        catalog::ProductKind,
        types::DateRange,
    },
    services::outbox::{self, Event},
};

#[derive(Debug, Clone)]
pub struct Cancellation {
    pub by: CancelledBy,
    pub reason: Option<String>,
    /// Staff user, when cancelled from the back-office.
    pub changed_by: Option<Uuid>,
}

pub async fn confirm_booking(
    pool: &PgPool,
    booking_id: Uuid,
    changed_by: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<(), BookingError> {
    let mut tx = pool.begin().await?;
    let status = lock_status(&mut tx, booking_id).await?;
    if !state::allowed(status, BookingStatus::Confirmed) {
        return Err(BookingError::InvalidTransition(status));
    }
    sqlx::query!(
        "UPDATE bookings SET status = 'confirmed', confirmed_at = $2, hold_expires_at = NULL WHERE id = $1",
        booking_id,
        now
    )
    .execute(&mut *tx)
    .await?;
    record_status(
        &mut tx,
        booking_id,
        Some(status),
        BookingStatus::Confirmed,
        changed_by,
        None,
    )
    .await?;
    outbox::enqueue(&mut tx, &Event::BookingConfirmed { booking_id }).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn cancel_booking(
    pool: &PgPool,
    booking_id: Uuid,
    cancellation: &Cancellation,
    now: DateTime<Utc>,
) -> Result<(), BookingError> {
    let mut tx = pool.begin().await?;
    let status = lock_status(&mut tx, booking_id).await?;
    if !state::cancellable(status) {
        return Err(BookingError::InvalidTransition(status));
    }
    release(
        &mut tx,
        booking_id,
        status,
        BookingStatus::Cancelled,
        cancellation,
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Expires the bookings whose hold ran out (background job). Each booking is released in its
/// own transaction, so one failure never blocks the others. Returns how many were expired.
pub async fn expire_holds(
    pool: &PgPool,
    now: DateTime<Utc>,
    batch: i64,
) -> Result<usize, BookingError> {
    let candidates = sqlx::query_scalar!(
        "SELECT id FROM bookings
         WHERE status IN ('pending', 'awaiting_payment') AND hold_expires_at < $1
         ORDER BY hold_expires_at
         LIMIT $2",
        now,
        batch
    )
    .fetch_all(pool)
    .await?;
    let system = Cancellation {
        by: CancelledBy::System,
        reason: Some("hold expired".into()),
        changed_by: None,
    };

    let mut expired = 0;
    for booking_id in candidates {
        let mut tx = pool.begin().await?;
        let still_due = sqlx::query!(
            r#"SELECT status AS "status: BookingStatus", hold_expires_at FROM bookings WHERE id = $1 FOR UPDATE"#,
            booking_id
        )
        .fetch_one(&mut *tx)
        .await?;
        // Re-checked under the lock: a payment may have confirmed it meanwhile.
        let due = matches!(
            still_due.status,
            BookingStatus::Pending | BookingStatus::AwaitingPayment
        ) && still_due.hold_expires_at.is_some_and(|h| h < now);
        if !due {
            continue;
        }
        release(
            &mut tx,
            booking_id,
            still_due.status,
            BookingStatus::Expired,
            &system,
            now,
        )
        .await?;
        tx.commit().await?;
        expired += 1;
    }
    Ok(expired)
}

async fn lock_status(
    conn: &mut PgConnection,
    booking_id: Uuid,
) -> Result<BookingStatus, BookingError> {
    sqlx::query_scalar!(
        r#"SELECT status AS "status: BookingStatus" FROM bookings WHERE id = $1 FOR UPDATE"#,
        booking_id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(BookingError::NotFound)
}

/// Releases everything the booking holds and moves it to `to` (cancelled or expired). The
/// caller holds the booking row lock and has checked the transition.
/// Gives back the inventory of the active items `only` (all active items when `None`) of a
/// booking: night counters, slot seats, unit allocations and resource assignments, and marks
/// the items cancelled. Same lock order as booking (nights by product then day).
pub(crate) async fn release_items(
    tx: &mut PgConnection,
    booking_id: Uuid,
    only: Option<&[Uuid]>,
    now: DateTime<Utc>,
) -> Result<Vec<Uuid>, BookingError> {
    let items = sqlx::query!(
        r#"SELECT i.id, i.product_id, i.product_kind AS "product_kind: ProductKind", i.quantity,
                  i.service_period AS "service_period: DateRange", i.activity_slot_id,
                  COALESCE((SELECT sum(g.guest_count) FROM booking_item_guests g
                            JOIN guest_categories gc ON gc.id = g.guest_category_id
                            WHERE g.booking_item_id = i.id AND gc.counts_toward_capacity), 0)::int AS "participants!"
           FROM booking_items i
           WHERE i.booking_id = $1 AND i.status = 'active' AND ($2::uuid[] IS NULL OR i.id = ANY($2))
           ORDER BY i.product_id, i.id"#,
        booking_id,
        only.map(<[Uuid]>::to_vec) as Option<Vec<Uuid>>
    )
    .fetch_all(&mut *tx)
    .await?;

    for item in &items {
        match (
            item.product_kind,
            item.service_period,
            item.activity_slot_id,
        ) {
            (ProductKind::Accommodation, Some(period), _) => {
                // Same lock order as booking: nights in date order.
                sqlx::query!(
                    "SELECT day FROM inventory_calendar
                     WHERE product_id = $1 AND day >= $2 AND day < $3 ORDER BY day FOR UPDATE",
                    item.product_id,
                    period.start,
                    period.end
                )
                .fetch_all(&mut *tx)
                .await?;
                sqlx::query!(
                    "UPDATE inventory_calendar SET reserved = reserved - $4
                     WHERE product_id = $1 AND day >= $2 AND day < $3",
                    item.product_id,
                    period.start,
                    period.end,
                    item.quantity
                )
                .execute(&mut *tx)
                .await?;
            }
            (ProductKind::Activity, _, Some(slot_id)) => {
                sqlx::query!(
                    "UPDATE activity_slots
                     SET reserved = reserved - $2,
                         status = CASE WHEN status = 'full' THEN 'open' ELSE status END
                     WHERE id = $1",
                    slot_id,
                    item.participants
                )
                .execute(&mut *tx)
                .await?;
            }
            _ => {}
        }
    }

    let item_ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
    sqlx::query!(
        "UPDATE booking_items SET status = 'cancelled', cancelled_at = $2 WHERE id = ANY($1)",
        &item_ids,
        now
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE unit_allocations SET released_at = $2 WHERE booking_item_id = ANY($1) AND released_at IS NULL",
        &item_ids,
        now
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE resource_assignments SET released_at = $2 WHERE booking_item_id = ANY($1) AND released_at IS NULL",
        &item_ids,
        now
    )
    .execute(&mut *tx)
    .await?;
    Ok(item_ids)
}

pub(crate) async fn release(
    tx: &mut PgConnection,
    booking_id: Uuid,
    from: BookingStatus,
    to: BookingStatus,
    cancellation: &Cancellation,
    now: DateTime<Utc>,
) -> Result<(), BookingError> {
    release_as(tx, booking_id, from, to, cancellation, false, now).await
}

/// `declined`: the camp refused a request (`booking.declined` instead of `booking.cancelled`).
pub(crate) async fn release_as(
    tx: &mut PgConnection,
    booking_id: Uuid,
    from: BookingStatus,
    to: BookingStatus,
    cancellation: &Cancellation,
    declined: bool,
    now: DateTime<Utc>,
) -> Result<(), BookingError> {
    release_items(tx, booking_id, None, now).await?;
    sqlx::query!(
        "UPDATE bookings
         SET status = $2, cancelled_at = $3, cancelled_by = $4, cancellation_reason = $5, hold_expires_at = NULL
         WHERE id = $1",
        booking_id,
        to.as_str(),
        now,
        cancellation.by.as_str(),
        cancellation.reason.as_deref()
    )
    .execute(&mut *tx)
    .await?;
    record_status(
        tx,
        booking_id,
        Some(from),
        to,
        cancellation.changed_by,
        cancellation.reason.as_deref(),
    )
    .await?;
    crate::services::audit::record(
        tx,
        &match cancellation.changed_by {
            Some(user) => crate::services::audit::Actor::staff(user),
            None if cancellation.by == CancelledBy::Customer => {
                crate::services::audit::Actor::customer()
            }
            None => crate::services::audit::Actor::system(),
        },
        "booking.status_changed",
        "booking",
        &booking_id.to_string(),
        serde_json::json!({ "before": { "status": from }, "after": { "status": to },
                            "reason": cancellation.reason }),
    )
    .await?;
    let event = if to == BookingStatus::Expired {
        Event::BookingExpired { booking_id }
    } else if declined {
        Event::BookingDeclined { booking_id }
    } else {
        Event::BookingCancelled { booking_id }
    };
    outbox::enqueue(tx, &event).await?;
    // An unfinished online checkout of a guest who accepted marketing emails: one reminder at
    // most, ever (dedupe per customer), and only if they did not book since.
    if to == BookingStatus::Expired && from == BookingStatus::AwaitingPayment {
        let customer = sqlx::query!(
            r#"SELECT c.id, c.marketing_consent,
                      EXISTS (SELECT 1 FROM bookings o WHERE o.customer_id = c.id AND o.id <> $1
                              AND o.status IN ('confirmed', 'checked_in', 'completed', 'awaiting_payment', 'pending')
                              AND o.created_at > b.created_at) AS "booked_since!"
               FROM bookings b JOIN customers c ON c.id = b.customer_id WHERE b.id = $1"#,
            booking_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if customer.marketing_consent && !customer.booked_since {
            outbox::enqueue_scheduled(
                tx,
                &Event::CartAbandoned { booking_id },
                now + chrono::Duration::hours(1),
                &format!("cart.abandoned:{}", customer.id),
            )
            .await?;
        }
    }
    Ok(())
}
