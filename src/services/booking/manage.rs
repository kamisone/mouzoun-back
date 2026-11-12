//! Managing an existing booking: guest access (signed links, magic links), cancellation with a
//! refund computed from the frozen policy snapshot, staff decisions on requests, check-in /
//! check-out / no-show, item cancellation and detail updates.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, NaiveTime, Utc};
use ipnetwork::IpNetwork;
use rand::RngCore;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    BookingError, Cancellation,
    lifecycle::{release_as, release_items},
    state,
    trip::pricing::refund_percent,
};
use crate::{
    db::{
        bookings::{BookingStatus, CancelledBy, PaymentOption},
        types::percent_of,
    },
    services::{
        audit::{self, Actor},
        email::links,
        outbox::{self, Event},
        payments::{self, Payments, RefundOutcome},
    },
};

// ---- Access -----------------------------------------------------------------------------------

/// The stable signed token of a booking (same as the links in emails).
pub fn access_token(secret: &str, booking_id: Uuid) -> String {
    links::booking_token(secret, booking_id)
}

fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Booking id for `reference` when `token` is its signed token.
pub async fn resolve_access(
    pool: &PgPool,
    secret: &str,
    reference: &str,
    token: &str,
) -> Result<Option<Uuid>, BookingError> {
    let Some(id) = sqlx::query_scalar!("SELECT id FROM bookings WHERE reference = $1", reference)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let expected = access_token(secret, id);
    Ok(constant_time_eq(expected.as_bytes(), token.as_bytes()).then_some(id))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// "Find my booking": when `reference` and `email` match, emails a single-use link valid
/// `minutes`. Says nothing about whether they matched (no data leaks to the caller).
pub async fn request_magic_link(
    pool: &PgPool,
    reference: &str,
    email: &str,
    ip: Option<IpNetwork>,
    minutes: i64,
    now: DateTime<Utc>,
) -> Result<(), BookingError> {
    let booking = sqlx::query_scalar!(
        "SELECT b.id FROM bookings b JOIN customers c ON c.id = b.customer_id
         WHERE b.reference = $1 AND c.email = $2::text::citext AND c.deleted_at IS NULL",
        reference.trim().to_ascii_uppercase(),
        email.trim()
    )
    .fetch_optional(pool)
    .await?;
    let Some(booking_id) = booking else {
        tracing::info!("find-booking: no match");
        return Ok(());
    };
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let token = URL_SAFE_NO_PAD.encode(raw);
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO booking_access_tokens (booking_id, token_hash, expires_at, created_ip)
         VALUES ($1, $2, $3, $4)",
        booking_id,
        hash(&token),
        now + Duration::minutes(minutes),
        ip
    )
    .execute(&mut *tx)
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::BookingAccessRequested { booking_id, token },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Exchanges a magic-link token (single use, unexpired) for the booking's signed access token.
pub async fn redeem_magic_link(
    pool: &PgPool,
    secret: &str,
    reference: &str,
    token: &str,
    now: DateTime<Utc>,
) -> Result<Option<String>, BookingError> {
    let booking_id = sqlx::query_scalar!(
        "UPDATE booking_access_tokens t SET used_at = $3
         FROM bookings b
         WHERE b.id = t.booking_id AND b.reference = $1 AND t.token_hash = $2
           AND t.used_at IS NULL AND t.expires_at > $3
         RETURNING t.booking_id",
        reference,
        hash(token),
        now
    )
    .fetch_optional(pool)
    .await?;
    Ok(booking_id.map(|id| access_token(secret, id)))
}

// ---- Cancellation -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ItemCharge {
    pub item_id: Uuid,
    pub name: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub total_minor: i64,
    pub refund_percent: i64,
    pub fee_minor: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CancellationPreview {
    pub allowed: bool,
    /// Why the guest cannot cancel online (`status`, `started`, `disabled`).
    pub blocked_reason: Option<&'static str>,
    pub total_minor: i64,
    pub paid_minor: i64,
    /// Kept by the camp under the policy (never more than what was paid).
    pub fee_minor: i64,
    pub refund_minor: i64,
    pub items: Vec<ItemCharge>,
    pub policy: Option<Value>,
}

/// Refund if cancelled `now`, from the booking's frozen policy (never the current one):
/// each item keeps `100 − refund %` of its total as fee, the guest gets back what they paid
/// minus the fees (never below zero; nothing is charged when nothing was paid).
pub async fn preview_cancellation(
    conn: &mut PgConnection,
    booking_id: Uuid,
    only_items: Option<&[Uuid]>,
    now: DateTime<Utc>,
) -> Result<CancellationPreview, BookingError> {
    let booking = sqlx::query!(
        r#"SELECT status AS "status: BookingStatus", total_minor, amount_paid_minor,
                  cancellation_policy_snapshot
           FROM bookings WHERE id = $1"#,
        booking_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(BookingError::NotFound)?;
    let items = sqlx::query!(
        r#"SELECT id, total_minor, product_snapshot, starts_at, lower(service_period) AS "first_day?"
           FROM booking_items
           WHERE booking_id = $1 AND status = 'active' AND ($2::uuid[] IS NULL OR id = ANY($2) OR parent_item_id = ANY($2))
           ORDER BY created_at, id"#,
        booking_id,
        only_items.map(<[Uuid]>::to_vec) as Option<Vec<Uuid>>
    )
    .fetch_all(&mut *conn)
    .await?;
    let policy = booking.cancellation_policy_snapshot.clone();
    let mut charges = Vec::new();
    for item in items {
        let snapshot = &item.product_snapshot;
        let starts_at: Option<DateTime<Utc>> = snapshot
            .get("starts_at")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .or(item.starts_at)
            .or_else(|| {
                item.first_day.map(|d| {
                    crate::camp_time::to_utc(d.and_time(super::trip::pricing::DEFAULT_CHECK_IN))
                })
            });
        let item_policy = snapshot
            .get("cancellation_rules")
            .filter(|v| !v.is_null())
            .or(policy.as_ref());
        let percent = match starts_at {
            Some(start) => refund_percent(item_policy, start, now),
            None => 100,
        };
        let refundable =
            percent_of(item.total_minor, rust_decimal::Decimal::from(percent)).unwrap_or(0);
        charges.push(ItemCharge {
            item_id: item.id,
            name: snapshot["localized_name"]
                .as_str()
                .or(snapshot["name"].as_str())
                .unwrap_or_default()
                .to_string(),
            starts_at,
            total_minor: item.total_minor,
            refund_percent: percent,
            fee_minor: item.total_minor - refundable,
        });
    }
    let fee: i64 = charges.iter().map(|c| c.fee_minor).sum();
    let paid = booking.amount_paid_minor.max(0);
    let (allowed, blocked_reason) = if !state::cancellable(booking.status) {
        (false, Some("status"))
    } else {
        (true, None)
    };
    Ok(CancellationPreview {
        allowed,
        blocked_reason,
        total_minor: booking.total_minor,
        paid_minor: paid,
        fee_minor: fee.min(paid),
        refund_minor: (paid - fee).max(0),
        items: charges,
        policy,
    })
}

/// How much staff refunds when cancelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundChoice {
    /// As the policy says (guests always get this).
    Policy,
    Custom(i64),
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelOutcome {
    pub refund_minor: i64,
    pub refunds: Vec<RefundOutcome>,
}

#[allow(clippy::too_many_arguments)]
pub async fn cancel(
    pool: &PgPool,
    payments: &Payments,
    booking_id: Uuid,
    by: CancelledBy,
    actor: &Actor,
    reason: Option<String>,
    refund: RefundChoice,
    now: DateTime<Utc>,
) -> Result<CancelOutcome, BookingError> {
    let mut tx = pool.begin().await?;
    let status = state::lock(&mut tx, booking_id).await?;
    if !state::cancellable(status) {
        return Err(BookingError::InvalidTransition(status));
    }
    let preview = preview_cancellation(&mut tx, booking_id, None, now).await?;
    release_as(
        &mut tx,
        booking_id,
        status,
        BookingStatus::Cancelled,
        &Cancellation {
            by,
            reason,
            changed_by: actor.user_id,
        },
        false,
        now,
    )
    .await?;
    tx.commit().await?;

    let amount = match refund {
        RefundChoice::Policy => preview.refund_minor,
        RefundChoice::Custom(a) => a.clamp(0, preview.paid_minor),
        RefundChoice::None => 0,
    };
    let refunds = refund_amount(
        pool,
        payments,
        booking_id,
        amount,
        "booking cancelled",
        actor.user_id,
        now,
    )
    .await?;
    Ok(CancelOutcome {
        refund_minor: amount,
        refunds,
    })
}

/// Refunds `amount` over the booking's payments, most recent first.
async fn refund_amount(
    pool: &PgPool,
    payments: &Payments,
    booking_id: Uuid,
    amount: i64,
    reason: &str,
    created_by: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<Vec<RefundOutcome>, BookingError> {
    let mut left = amount;
    let mut out = Vec::new();
    if left <= 0 {
        return Ok(out);
    }
    let candidates = sqlx::query!(
        r#"SELECT p.id, (p.amount_minor - COALESCE((SELECT sum(r.amount_minor) FROM refunds r
                         WHERE r.payment_id = p.id AND r.status <> 'failed'), 0))::bigint AS "refundable!"
           FROM payments p
           WHERE p.booking_id = $1 AND p.status IN ('succeeded', 'partially_refunded')
           ORDER BY p.paid_at DESC NULLS LAST, p.created_at DESC"#,
        booking_id
    )
    .fetch_all(pool)
    .await?;
    for payment in candidates {
        if left <= 0 {
            break;
        }
        let part = left.min(payment.refundable);
        if part <= 0 {
            continue;
        }
        match payments::refund(
            pool,
            payments,
            payment.id,
            part,
            Some(reason),
            created_by,
            now,
        )
        .await
        {
            Ok(r) => {
                left -= part;
                out.push(r);
            }
            Err(e) => {
                // The booking is cancelled already: a failed refund becomes staff work.
                tracing::error!(%booking_id, error = %e, alert = true, "automatic refund failed");
                let mut conn = pool.acquire().await?;
                payments::create_task(
                    &mut conn,
                    crate::db::bookings::StaffTaskKind::RefundOffline,
                    Some(booking_id),
                    Some(payment.id),
                    "Automatic refund failed: refund by hand",
                    json!({ "amount_minor": part, "error": e.to_string() }),
                    Some(&format!("refund_failed:{}:{}", payment.id, part)),
                )
                .await?;
            }
        }
    }
    Ok(out)
}

/// Cancels some items of a booking (an activity of a trip) without cancelling the booking.
#[allow(clippy::too_many_arguments)]
pub async fn cancel_items(
    pool: &PgPool,
    payments: &Payments,
    booking_id: Uuid,
    item_ids: &[Uuid],
    actor: &Actor,
    refund: RefundChoice,
    now: DateTime<Utc>,
) -> Result<CancelOutcome, BookingError> {
    let mut tx = pool.begin().await?;
    let status = state::lock(&mut tx, booking_id).await?;
    if !state::cancellable(status) {
        return Err(BookingError::InvalidTransition(status));
    }
    let remaining = sqlx::query_scalar!(
        "SELECT count(*) FROM booking_items
         WHERE booking_id = $1 AND status = 'active' AND parent_item_id IS NULL AND NOT (id = ANY($2))",
        booking_id,
        item_ids
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(0);
    if remaining == 0 {
        return Err(BookingError::Invalid(
            "cancel the whole booking instead of its last item".into(),
        ));
    }
    let preview = preview_cancellation(&mut tx, booking_id, Some(item_ids), now).await?;
    let children = sqlx::query_scalar!(
        "SELECT id FROM booking_items WHERE booking_id = $1 AND status = 'active' AND parent_item_id = ANY($2)",
        booking_id,
        item_ids
    )
    .fetch_all(&mut *tx)
    .await?;
    let all: Vec<Uuid> = item_ids.iter().copied().chain(children).collect();
    let released = release_items(&mut tx, booking_id, Some(&all), now).await?;
    if released.is_empty() {
        return Err(BookingError::NotFound);
    }
    recompute_totals(&mut tx, booking_id).await?;
    audit::record(
        &mut tx,
        actor,
        "booking.items_cancelled",
        "booking",
        &booking_id.to_string(),
        json!({ "items": released }),
    )
    .await?;
    outbox::enqueue(&mut tx, &Event::BookingModified { booking_id }).await?;
    tx.commit().await?;

    let removed_total: i64 = preview.items.iter().map(|i| i.total_minor).sum();
    let policy_refund =
        (removed_total - preview.items.iter().map(|i| i.fee_minor).sum::<i64>()).max(0);
    // Only what was paid beyond the new total can come back.
    let overpaid = sqlx::query_scalar!(
        "SELECT GREATEST(amount_paid_minor - total_minor, 0) FROM bookings WHERE id = $1",
        booking_id
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    let amount = match refund {
        RefundChoice::Policy => policy_refund.min(overpaid),
        RefundChoice::Custom(a) => a.clamp(0, overpaid.max(0)),
        RefundChoice::None => 0,
    };
    let refunds = refund_amount(
        pool,
        payments,
        booking_id,
        amount,
        "item cancelled",
        actor.user_id,
        now,
    )
    .await?;
    Ok(CancelOutcome {
        refund_minor: amount,
        refunds,
    })
}

/// Booking totals = sum of its active items (after items were added or cancelled).
pub async fn recompute_totals(conn: &mut PgConnection, booking_id: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE bookings b SET
             subtotal_minor = t.subtotal, discount_minor = t.discount, tax_minor = t.tax, total_minor = t.total
         FROM (SELECT COALESCE(sum(subtotal_minor), 0)::bigint AS subtotal,
                      COALESCE(sum(discount_minor), 0)::bigint AS discount,
                      COALESCE(sum(tax_minor), 0)::bigint AS tax,
                      COALESCE(sum(total_minor), 0)::bigint AS total
               FROM booking_items WHERE booking_id = $1 AND status = 'active') t
         WHERE b.id = $1",
        booking_id
    )
    .execute(conn)
    .await?;
    Ok(())
}

// ---- Staff decisions and stay lifecycle ------------------------------------------------------

/// Accepts a request: the guest pays (held until the payment-link deadline) or, when nothing is
/// due now, the booking is confirmed.
pub async fn accept_request(
    pool: &PgPool,
    booking_id: Uuid,
    actor: &Actor,
    payment_link_hours: i64,
    now: DateTime<Utc>,
) -> Result<BookingStatus, BookingError> {
    let mut tx = pool.begin().await?;
    let status = state::lock(&mut tx, booking_id).await?;
    if status != BookingStatus::Pending {
        return Err(BookingError::InvalidTransition(status));
    }
    let b = sqlx::query!(
        r#"SELECT payment_option AS "payment_option: PaymentOption", deposit_due_minor, amount_paid_minor
           FROM bookings WHERE id = $1"#,
        booking_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let needs_payment = matches!(
        b.payment_option,
        Some(PaymentOption::Full | PaymentOption::Deposit)
    ) && b.deposit_due_minor > b.amount_paid_minor;
    let to = if needs_payment {
        BookingStatus::AwaitingPayment
    } else {
        BookingStatus::Confirmed
    };
    state::transition(
        &mut tx,
        booking_id,
        to,
        actor,
        Some("request accepted"),
        now,
    )
    .await?;
    if needs_payment {
        sqlx::query!(
            "UPDATE bookings SET hold_expires_at = $2 WHERE id = $1",
            booking_id,
            now + Duration::hours(payment_link_hours)
        )
        .execute(&mut *tx)
        .await?;
        outbox::enqueue(&mut tx, &Event::BookingRequestAccepted { booking_id }).await?;
    } else {
        outbox::enqueue(&mut tx, &Event::BookingConfirmed { booking_id }).await?;
    }
    tx.commit().await?;
    Ok(to)
}

pub async fn decline_request(
    pool: &PgPool,
    booking_id: Uuid,
    actor: &Actor,
    reason: Option<String>,
    now: DateTime<Utc>,
) -> Result<(), BookingError> {
    let mut tx = pool.begin().await?;
    let status = state::lock(&mut tx, booking_id).await?;
    if status != BookingStatus::Pending {
        return Err(BookingError::InvalidTransition(status));
    }
    release_as(
        &mut tx,
        booking_id,
        status,
        BookingStatus::Cancelled,
        &Cancellation {
            by: CancelledBy::Staff,
            reason,
            changed_by: actor.user_id,
        },
        true,
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Check-in, check-out (completed) and no-show: plain transitions.
pub async fn set_status(
    pool: &PgPool,
    booking_id: Uuid,
    to: BookingStatus,
    actor: &Actor,
    note: Option<&str>,
    now: DateTime<Utc>,
) -> Result<BookingStatus, BookingError> {
    if !matches!(
        to,
        BookingStatus::CheckedIn
            | BookingStatus::Completed
            | BookingStatus::NoShow
            | BookingStatus::Confirmed
    ) {
        return Err(BookingError::Invalid(
            "use cancel, accept or decline".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let from = state::transition(&mut tx, booking_id, to, actor, note, now).await?;
    if to == BookingStatus::Confirmed {
        outbox::enqueue(&mut tx, &Event::BookingConfirmed { booking_id }).await?;
    }
    tx.commit().await?;
    Ok(from)
}

/// Arrival time and special requests (guest self-service or staff).
pub async fn update_details(
    pool: &PgPool,
    booking_id: Uuid,
    estimated_arrival_time: Option<NaiveTime>,
    special_requests: Option<String>,
    actor: &Actor,
) -> Result<(), BookingError> {
    let mut tx = pool.begin().await?;
    let status = state::lock(&mut tx, booking_id).await?;
    if !matches!(
        status,
        BookingStatus::Pending | BookingStatus::AwaitingPayment | BookingStatus::Confirmed
    ) {
        return Err(BookingError::InvalidTransition(status));
    }
    let before = sqlx::query!(
        "UPDATE bookings b SET estimated_arrival_time = $2, special_requests = $3
         FROM (SELECT estimated_arrival_time, special_requests FROM bookings WHERE id = $1) old
         WHERE b.id = $1
         RETURNING old.estimated_arrival_time, old.special_requests",
        booking_id,
        estimated_arrival_time,
        special_requests
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    )
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        "booking.details_updated",
        "booking",
        &booking_id.to_string(),
        json!({
            "before": { "estimated_arrival_time": before.estimated_arrival_time, "special_requests": before.special_requests },
            "after": { "estimated_arrival_time": estimated_arrival_time, "special_requests": special_requests },
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
