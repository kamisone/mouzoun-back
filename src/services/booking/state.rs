//! The booking state machine: the only place that decides which status changes are legal.
//!
//! ```text
//! draft ──► pending ──────────► awaiting_payment ──► confirmed ──► checked_in ──► completed
//!   │          │  │                   │   │              │  │
//!   │          │  └──► confirmed      │   └──► expired   │  └──► no_show
//!   │          ├──► cancelled         └──► cancelled     └──► cancelled
//!   │          └──► expired
//!   ├──► awaiting_payment
//!   └──► confirmed
//! ```
//!
//! Every change goes through [`transition`]: it locks the booking row, refuses anything not in
//! the table with `BookingError::InvalidTransition`, stamps the matching timestamp, and writes
//! `booking_status_history` and `audit_logs` in the caller's transaction.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{BookingError, common::record_status};
use crate::{
    db::bookings::BookingStatus,
    services::audit::{self, Actor},
};

/// `true` when `from → to` is a legal transition.
pub fn allowed(from: BookingStatus, to: BookingStatus) -> bool {
    use BookingStatus::*;
    matches!(
        (from, to),
        (Draft, Pending | AwaitingPayment | Confirmed)
            | (Pending, AwaitingPayment | Confirmed | Cancelled | Expired)
            | (AwaitingPayment, Confirmed | Cancelled | Expired)
            | (Confirmed, CheckedIn | Cancelled | NoShow)
            | (CheckedIn, Completed)
    )
}

/// Statuses a booking can still be cancelled from (inventory is held).
pub fn cancellable(status: BookingStatus) -> bool {
    allowed(status, BookingStatus::Cancelled)
}

/// Locks the booking and returns its status.
pub async fn lock(
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

/// Moves the (locked or not) booking to `to`. Returns the previous status.
pub async fn transition(
    conn: &mut PgConnection,
    booking_id: Uuid,
    to: BookingStatus,
    actor: &Actor,
    note: Option<&str>,
    now: DateTime<Utc>,
) -> Result<BookingStatus, BookingError> {
    let from = lock(conn, booking_id).await?;
    if !allowed(from, to) {
        return Err(BookingError::InvalidTransition(from));
    }
    sqlx::query!(
        "UPDATE bookings
         SET status = $2,
             confirmed_at   = CASE WHEN $2 = 'confirmed'  THEN $3 ELSE confirmed_at END,
             checked_in_at  = CASE WHEN $2 = 'checked_in' THEN $3 ELSE checked_in_at END,
             checked_out_at = CASE WHEN $2 = 'completed'  THEN $3 ELSE checked_out_at END,
             hold_expires_at = CASE WHEN $2 IN ('confirmed', 'checked_in', 'completed', 'cancelled',
                                                'expired', 'no_show') THEN NULL ELSE hold_expires_at END
         WHERE id = $1",
        booking_id,
        to.as_str(),
        now
    )
    .execute(&mut *conn)
    .await?;
    record_status(conn, booking_id, Some(from), to, actor.user_id, note).await?;
    audit::record(
        conn,
        actor,
        "booking.status_changed",
        "booking",
        &booking_id.to_string(),
        json!({ "before": { "status": from }, "after": { "status": to }, "note": note }),
    )
    .await?;
    Ok(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transition_table_is_exactly_the_documented_one() {
        use BookingStatus::*;
        let legal = [
            (Draft, Pending),
            (Draft, AwaitingPayment),
            (Draft, Confirmed),
            (Pending, AwaitingPayment),
            (Pending, Confirmed),
            (Pending, Cancelled),
            (Pending, Expired),
            (AwaitingPayment, Confirmed),
            (AwaitingPayment, Cancelled),
            (AwaitingPayment, Expired),
            (Confirmed, CheckedIn),
            (Confirmed, Cancelled),
            (Confirmed, NoShow),
            (CheckedIn, Completed),
        ];
        let mut allowed_count = 0;
        for from in BookingStatus::ALL {
            for to in BookingStatus::ALL {
                let expected = legal.contains(&(*from, *to));
                assert_eq!(allowed(*from, *to), expected, "{from} → {to}");
                allowed_count += usize::from(expected);
            }
        }
        assert_eq!(allowed_count, legal.len());
        // Terminal statuses never move again.
        for terminal in [Completed, Cancelled, NoShow, Expired] {
            assert!(BookingStatus::ALL.iter().all(|to| !allowed(terminal, *to)));
        }
    }
}
