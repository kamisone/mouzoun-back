use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    Payments,
    gateway::{
        CreatePayment, GatewayError, OffSessionCharge, RefundRequest, WebhookEvent, WebhookOutcome,
    },
};
use crate::{
    db::{
        bookings::{BookingStatus, PaymentProvider, PaymentPurpose, PaymentStatus, StaffTaskKind},
        types::CurrencyCode,
    },
    services::{
        audit::{self, Actor},
        booking::{BookingError, state},
        outbox::{self, Event},
    },
};

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("no online payment provider is configured")]
    NoProvider,
    #[error("nothing to pay")]
    NothingToPay,
    #[error("this booking cannot be paid now ({0})")]
    NotPayable(BookingStatus),
    #[error("refund larger than what remains refundable ({0})")]
    TooMuch(i64),
    #[error(transparent)]
    Gateway(#[from] GatewayError),
    #[error(transparent)]
    Booking(#[from] BookingError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl From<PaymentError> for crate::error::AppError {
    fn from(e: PaymentError) -> Self {
        use axum::http::StatusCode;

        use crate::error::AppError;
        match e {
            PaymentError::NoProvider => AppError::domain(
                StatusCode::SERVICE_UNAVAILABLE,
                "payments_unavailable",
                e.to_string(),
            ),
            PaymentError::NothingToPay => {
                AppError::domain(StatusCode::CONFLICT, "nothing_to_pay", e.to_string())
            }
            PaymentError::NotPayable(status) => {
                AppError::domain(StatusCode::CONFLICT, "not_payable", e.to_string())
                    .with_details(json!({ "status": status }))
            }
            PaymentError::TooMuch(max) => AppError::domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "refund_too_large",
                e.to_string(),
            )
            .with_details(json!({ "refundable_minor": max })),
            PaymentError::Gateway(GatewayError::Declined(message)) => {
                AppError::domain(StatusCode::PAYMENT_REQUIRED, "payment_declined", message)
            }
            PaymentError::Gateway(GatewayError::Unavailable(message)) => {
                tracing::error!(error = %message, "payment provider unavailable");
                AppError::domain(
                    StatusCode::BAD_GATEWAY,
                    "payment_provider_unavailable",
                    "the payment provider is not reachable, try again",
                )
            }
            PaymentError::Gateway(e) => AppError::BadRequest(e.to_string()),
            PaymentError::Booking(e) => e.into(),
            PaymentError::Database(e) => AppError::Database(e),
        }
    }
}

/// What the browser needs to show the payment form.
#[derive(Debug, Clone, Serialize)]
pub struct StartedPayment {
    pub payment_id: Uuid,
    pub provider: &'static str,
    pub client_secret: String,
    pub amount_minor: i64,
    pub currency: CurrencyCode,
}

/// Starts (or resumes) an online payment of `amount_minor` for a booking.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    pool: &PgPool,
    payments: &Payments,
    provider: Option<&str>,
    booking_id: Uuid,
    purpose: PaymentPurpose,
    amount_minor: i64,
    save_card: bool,
) -> Result<StartedPayment, PaymentError> {
    let (name, gateway) = match provider {
        Some(p) => payments.get(p),
        None => payments.default_provider(),
    }
    .ok_or(PaymentError::NoProvider)?;
    if amount_minor <= 0 {
        return Err(PaymentError::NothingToPay);
    }
    let booking = sqlx::query!(
        r#"SELECT b.reference, b.status AS "status: BookingStatus", b.currency AS "currency: CurrencyCode",
                  c.email::text AS "email?", c.first_name, c.last_name, c.stripe_customer_id
           FROM bookings b JOIN customers c ON c.id = b.customer_id WHERE b.id = $1"#,
        booking_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or(BookingError::NotFound)?;
    let payable = match purpose {
        PaymentPurpose::Deposit | PaymentPurpose::Full => {
            booking.status == BookingStatus::AwaitingPayment
        }
        PaymentPurpose::Balance | PaymentPurpose::Extra => {
            matches!(
                booking.status,
                BookingStatus::Confirmed | BookingStatus::CheckedIn
            )
        }
    };
    if !payable {
        return Err(PaymentError::NotPayable(booking.status));
    }

    // A reload of the payment page resumes the same attempt instead of opening another one.
    let provider_value = gateway.provider();
    if let Some(existing) = sqlx::query!(
        r#"SELECT id, provider_payload->>'client_secret' AS "client_secret?"
           FROM payments
           WHERE booking_id = $1 AND purpose = $2 AND provider = $3 AND amount_minor = $4
             AND status IN ('pending', 'requires_action') AND provider_payment_id IS NOT NULL
           ORDER BY created_at DESC LIMIT 1"#,
        booking_id,
        purpose.as_str(),
        provider_value.as_str(),
        amount_minor
    )
    .fetch_optional(pool)
    .await?
        && let Some(client_secret) = existing.client_secret
    {
        return Ok(StartedPayment {
            payment_id: existing.id,
            provider: name,
            client_secret,
            amount_minor,
            currency: booking.currency,
        });
    }

    let payment_id = Uuid::now_v7();
    let idempotency_key = format!("pay_{}", payment_id.simple());
    sqlx::query!(
        "INSERT INTO payments (id, booking_id, provider, purpose, status, amount_minor, currency, idempotency_key)
         VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7)",
        payment_id,
        booking_id,
        provider_value.as_str(),
        purpose.as_str(),
        amount_minor,
        booking.currency.as_str(),
        idempotency_key
    )
    .execute(pool)
    .await?;

    let name_text = [
        Some(booking.first_name.as_str()),
        booking.last_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let created = gateway
        .create_payment(&CreatePayment {
            amount_minor,
            currency: &booking.currency,
            idempotency_key: &idempotency_key,
            reference: &booking.reference,
            description: format!("Mouzoun {}", booking.reference),
            booking_id,
            payment_id,
            email: booking.email.as_deref(),
            name: Some(&name_text),
            save_card,
            provider_customer_id: booking.stripe_customer_id.as_deref(),
        })
        .await;
    let created = match created {
        Ok(c) => c,
        Err(e) => {
            sqlx::query!(
                "UPDATE payments SET status = 'failed', failure_reason = $2 WHERE id = $1",
                payment_id,
                e.to_string()
            )
            .execute(pool)
            .await?;
            return Err(e.into());
        }
    };
    let mut payload = created.payload.clone();
    payload["client_secret"] = json!(created.client_secret);
    sqlx::query!(
        "UPDATE payments SET provider_payment_id = $2, provider_payload = $3 WHERE id = $1",
        payment_id,
        created.provider_payment_id,
        payload
    )
    .execute(pool)
    .await?;
    if let Some(customer) = &created.provider_customer_id
        && provider_value == PaymentProvider::Stripe
    {
        sqlx::query!(
            "UPDATE customers SET stripe_customer_id = $2
             WHERE id = (SELECT customer_id FROM bookings WHERE id = $1) AND stripe_customer_id IS NULL",
            booking_id,
            customer
        )
        .execute(pool)
        .await?;
    }
    Ok(StartedPayment {
        payment_id,
        provider: name,
        client_secret: created.client_secret,
        amount_minor,
        currency: booking.currency,
    })
}

/// Stores the event (once per provider + event id) and processes it. Returns `false` for a
/// duplicate delivery that was already processed.
pub async fn handle_webhook(
    pool: &PgPool,
    payments: &Payments,
    provider: &str,
    event: &WebhookEvent,
    now: DateTime<Utc>,
) -> Result<bool, PaymentError> {
    let inserted = sqlx::query_scalar!(
        "INSERT INTO payment_webhook_events (provider, event_id, event_type, payload)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider, event_id) DO NOTHING
         RETURNING id",
        provider,
        event.event_id,
        event.event_type,
        event.payload
    )
    .fetch_optional(pool)
    .await?;
    if inserted.is_none() {
        let processed = sqlx::query_scalar!(
            "SELECT processed_at IS NOT NULL AS \"processed!\" FROM payment_webhook_events
             WHERE provider = $1 AND event_id = $2",
            provider,
            event.event_id
        )
        .fetch_one(pool)
        .await?;
        if processed {
            tracing::info!(provider, event_id = %event.event_id, "duplicate webhook ignored");
            return Ok(false);
        }
    }
    let outcome = process(pool, payments, event, now).await;
    match &outcome {
        Ok(()) => {
            sqlx::query!(
                "UPDATE payment_webhook_events SET processed_at = $3, error = NULL
                 WHERE provider = $1 AND event_id = $2",
                provider,
                event.event_id,
                now
            )
            .execute(pool)
            .await?;
        }
        Err(e) => {
            tracing::error!(provider, event_id = %event.event_id, error = %e, alert = true, "webhook processing failed");
            sqlx::query!(
                "UPDATE payment_webhook_events SET error = $3 WHERE provider = $1 AND event_id = $2",
                provider,
                event.event_id,
                e.to_string()
            )
            .execute(pool)
            .await?;
        }
    }
    outcome.map(|()| true)
}

async fn process(
    pool: &PgPool,
    payments: &Payments,
    event: &WebhookEvent,
    now: DateTime<Utc>,
) -> Result<(), PaymentError> {
    match &event.outcome {
        WebhookOutcome::PaymentSucceeded {
            provider_payment_id,
            payment_method_id,
            ..
        } => {
            payment_succeeded(
                pool,
                payments,
                provider_payment_id,
                payment_method_id.as_deref(),
                now,
            )
            .await
        }
        WebhookOutcome::PaymentFailed {
            provider_payment_id,
            message,
        } => payment_failed(pool, provider_payment_id, message, now).await,
        WebhookOutcome::RequiresAction {
            provider_payment_id,
        } => {
            // 3-D Secure in progress: keep the inventory a little longer.
            sqlx::query!(
                "WITH p AS (UPDATE payments SET status = 'requires_action'
                            WHERE provider_payment_id = $1 AND status = 'pending' RETURNING booking_id)
                 UPDATE bookings SET hold_expires_at = GREATEST(hold_expires_at, $2)
                 WHERE id IN (SELECT booking_id FROM p) AND status = 'awaiting_payment'",
                provider_payment_id,
                now + Duration::minutes(10)
            )
            .execute(pool)
            .await?;
            Ok(())
        }
        WebhookOutcome::RefundSucceeded { provider_refund_id } => {
            let mut tx = pool.begin().await?;
            let refund = sqlx::query!(
                "UPDATE refunds SET status = 'succeeded', processed_at = $2
                 WHERE provider_refund_id = $1 AND status <> 'succeeded'
                 RETURNING id, payment_id",
                provider_refund_id,
                now
            )
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(refund) = refund {
                let booking_id = after_refund(&mut tx, refund.payment_id).await?;
                outbox::enqueue(
                    &mut tx,
                    &Event::RefundSucceeded {
                        refund_id: refund.id,
                        booking_id,
                    },
                )
                .await?;
            }
            tx.commit().await?;
            Ok(())
        }
        WebhookOutcome::RefundFailed {
            provider_refund_id,
            message,
        } => {
            sqlx::query!(
                "UPDATE refunds SET status = 'failed', failure_reason = $2 WHERE provider_refund_id = $1",
                provider_refund_id,
                message
            )
            .execute(pool)
            .await?;
            Ok(())
        }
        WebhookOutcome::PaymentCanceled {
            provider_payment_id,
        } => {
            sqlx::query!(
                "UPDATE payments SET status = 'cancelled'
                 WHERE provider_payment_id = $1 AND status IN ('pending', 'requires_action')",
                provider_payment_id
            )
            .execute(pool)
            .await?;
            Ok(())
        }
        WebhookOutcome::DisputeOpened {
            provider_payment_id,
            amount_minor,
            reason,
            evidence_due_by,
        } => {
            dispute_opened(
                pool,
                provider_payment_id,
                *amount_minor,
                reason,
                *evidence_due_by,
            )
            .await
        }
        WebhookOutcome::Ignored => Ok(()),
    }
}

/// A chargeback: staff must answer at the provider before the evidence deadline.
async fn dispute_opened(
    pool: &PgPool,
    provider_payment_id: &str,
    amount_minor: Option<i64>,
    reason: &str,
    evidence_due_by: Option<i64>,
) -> Result<(), PaymentError> {
    let mut tx = pool.begin().await?;
    let Some(payment) = sqlx::query!(
        "SELECT id, booking_id FROM payments WHERE provider_payment_id = $1",
        provider_payment_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tracing::warn!(
            provider_payment_id,
            alert = true,
            "dispute on an unknown payment"
        );
        return Ok(());
    };
    let due = evidence_due_by.and_then(|t| DateTime::<Utc>::from_timestamp(t, 0));
    create_task(
        &mut tx,
        StaffTaskKind::Dispute,
        Some(payment.booking_id),
        Some(payment.id),
        "Card payment disputed: answer in the payment provider's dashboard before the deadline",
        json!({ "amount_minor": amount_minor, "reason": reason, "evidence_due_by": due }),
        Some(&format!("dispute:{}", payment.id)),
    )
    .await?;
    audit::record(
        &mut tx,
        &Actor::webhook(),
        "payment.disputed",
        "payment",
        &payment.id.to_string(),
        json!({ "booking_id": payment.booking_id, "amount_minor": amount_minor, "reason": reason }),
    )
    .await?;
    tx.commit().await?;
    tracing::warn!(payment = %payment.id, alert = true, "payment disputed");
    Ok(())
}

/// Payments still open on bookings whose hold expired are cancelled at the provider, so the
/// guest can no longer pay a booking whose inventory was released. Returns how many.
pub async fn cancel_abandoned(
    pool: &PgPool,
    payments: &Payments,
    limit: i64,
) -> Result<usize, PaymentError> {
    let open = sqlx::query!(
        r#"SELECT p.id, p.provider, p.provider_payment_id AS "provider_payment_id!"
           FROM payments p JOIN bookings b ON b.id = p.booking_id
           WHERE b.status IN ('expired', 'cancelled') AND p.status IN ('pending', 'requires_action')
             AND p.provider_payment_id IS NOT NULL
           ORDER BY p.created_at LIMIT $1"#,
        limit
    )
    .fetch_all(pool)
    .await?;
    let mut cancelled = 0;
    for p in open {
        let Some(gateway) = payments.for_payment(&p.provider, Some(&p.provider_payment_id)) else {
            continue;
        };
        if let Err(e) = gateway
            .cancel_payment(&p.provider_payment_id, &format!("cancel-{}", p.id))
            .await
        {
            tracing::warn!(payment = %p.id, error = %e, "could not cancel an abandoned payment, retried next run");
            continue;
        }
        sqlx::query!(
            "UPDATE payments SET status = 'cancelled' WHERE id = $1 AND status IN ('pending', 'requires_action')",
            p.id
        )
        .execute(pool)
        .await?;
        cancelled += 1;
    }
    Ok(cancelled)
}

/// `amount_paid_minor` = succeeded payments − succeeded refunds (one statement, in the caller's
/// transaction).
pub async fn recompute_paid(conn: &mut PgConnection, booking_id: Uuid) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"UPDATE bookings SET amount_paid_minor =
               (SELECT COALESCE(sum(amount_minor), 0) FROM payments
                WHERE booking_id = $1 AND status IN ('succeeded', 'refunded', 'partially_refunded'))
             - (SELECT COALESCE(sum(r.amount_minor), 0) FROM refunds r JOIN payments p ON p.id = r.payment_id
                WHERE p.booking_id = $1 AND r.status = 'succeeded')
           WHERE id = $1
           RETURNING amount_paid_minor"#,
        booking_id
    )
    .fetch_one(conn)
    .await
}

async fn payment_succeeded(
    pool: &PgPool,
    payments: &Payments,
    provider_payment_id: &str,
    payment_method_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), PaymentError> {
    let mut tx = pool.begin().await?;
    let Some(payment) = sqlx::query!(
        r#"SELECT id, booking_id, status AS "status: PaymentStatus", purpose AS "purpose: PaymentPurpose",
                  amount_minor, provider
           FROM payments WHERE provider_payment_id = $1 FOR UPDATE"#,
        provider_payment_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tracing::warn!(provider_payment_id, "webhook for an unknown payment, ignored");
        return Ok(());
    };
    if matches!(
        payment.status,
        PaymentStatus::Succeeded | PaymentStatus::Refunded | PaymentStatus::PartiallyRefunded
    ) {
        return Ok(());
    }
    // Lock order: booking after payment, everywhere.
    let booking_status = state::lock(&mut tx, payment.booking_id).await?;
    sqlx::query!(
        "UPDATE payments SET status = 'succeeded', paid_at = $2, failure_reason = NULL WHERE id = $1",
        payment.id,
        now
    )
    .execute(&mut *tx)
    .await?;
    if let Some(pm) = payment_method_id {
        sqlx::query!(
            "UPDATE bookings SET saved_payment_method_id = $2
             WHERE id = $1 AND balance_collection = 'saved_card'",
            payment.booking_id,
            pm
        )
        .execute(&mut *tx)
        .await?;
    }
    recompute_paid(&mut tx, payment.booking_id).await?;
    let actor = Actor::webhook();
    audit::record(
        &mut tx,
        &actor,
        "payment.succeeded",
        "payment",
        &payment.id.to_string(),
        json!({ "booking_id": payment.booking_id, "amount_minor": payment.amount_minor }),
    )
    .await?;

    let mut refund_late = false;
    match booking_status {
        BookingStatus::AwaitingPayment => {
            state::transition(
                &mut tx,
                payment.booking_id,
                BookingStatus::Confirmed,
                &actor,
                Some("payment received"),
                now,
            )
            .await?;
            outbox::enqueue(
                &mut tx,
                &Event::BookingConfirmed {
                    booking_id: payment.booking_id,
                },
            )
            .await?;
        }
        BookingStatus::Expired | BookingStatus::Cancelled => {
            // The hold ran out before the payment arrived: the inventory may be sold again, so
            // the payment is refunded in full (the guest is told by email) and staff is warned.
            refund_late = true;
            create_task(
                &mut tx,
                StaffTaskKind::PaymentAfterExpiry,
                Some(payment.booking_id),
                Some(payment.id),
                "Payment received after the booking expired (refunded automatically)",
                json!({ "amount_minor": payment.amount_minor }),
                Some(&format!("payment_after_expiry:{}", payment.id)),
            )
            .await?;
        }
        _ => {}
    }
    outbox::enqueue(
        &mut tx,
        &Event::PaymentSucceeded {
            payment_id: payment.id,
            booking_id: payment.booking_id,
        },
    )
    .await?;
    tx.commit().await?;

    if refund_late {
        refund(
            pool,
            payments,
            payment.id,
            payment.amount_minor,
            Some("booking expired before the payment"),
            None,
            now,
        )
        .await?;
    }
    Ok(())
}

async fn payment_failed(
    pool: &PgPool,
    provider_payment_id: &str,
    message: &str,
    now: DateTime<Utc>,
) -> Result<(), PaymentError> {
    let mut tx = pool.begin().await?;
    let Some(payment) = sqlx::query!(
        r#"UPDATE payments SET status = 'failed', failure_reason = $2
           WHERE provider_payment_id = $1 AND status IN ('pending', 'requires_action')
           RETURNING id, booking_id, purpose AS "purpose: PaymentPurpose""#,
        provider_payment_id,
        message
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(());
    };
    // The guest is still on the payment page (inline message, other method, hold kept). Only an
    // off-session balance charge needs emails and a staff task.
    let off_session = sqlx::query_scalar!(
        "SELECT (provider_payload->>'off_session')::boolean FROM payments WHERE id = $1",
        payment.id
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(false);
    if payment.purpose == PaymentPurpose::Balance && off_session {
        balance_failed(&mut tx, payment.booking_id, payment.id, message, now).await?;
    }
    outbox::enqueue(
        &mut tx,
        &Event::PaymentFailed {
            payment_id: payment.id,
            booking_id: payment.booking_id,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn balance_failed(
    tx: &mut PgConnection,
    booking_id: Uuid,
    payment_id: Uuid,
    message: &str,
    now: DateTime<Utc>,
) -> Result<(), PaymentError> {
    create_task(
        tx,
        StaffTaskKind::BalanceFailed,
        Some(booking_id),
        Some(payment_id),
        "Balance charge failed: contact the guest",
        json!({ "reason": message, "failed_at": now }),
        Some(&format!("balance_failed:{booking_id}")),
    )
    .await?;
    outbox::enqueue(tx, &Event::BookingBalanceFailed { booking_id }).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn create_task(
    conn: &mut PgConnection,
    kind: StaffTaskKind,
    booking_id: Option<Uuid>,
    payment_id: Option<Uuid>,
    title: &str,
    details: serde_json::Value,
    dedupe_key: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO staff_tasks (kind, booking_id, payment_id, title, details, dedupe_key)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (dedupe_key) DO NOTHING",
        kind.as_str(),
        booking_id,
        payment_id,
        title,
        details,
        dedupe_key
    )
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct RefundOutcome {
    pub refund_id: Uuid,
    /// `succeeded`, `pending` (provider confirms later) or `staff_task` (offline payment).
    pub status: &'static str,
    pub amount_minor: i64,
}

/// Refunds `amount_minor` of a payment through its provider (offline payments: staff task).
pub async fn refund(
    pool: &PgPool,
    payments: &Payments,
    payment_id: Uuid,
    amount_minor: i64,
    reason: Option<&str>,
    created_by: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<RefundOutcome, PaymentError> {
    let mut tx = pool.begin().await?;
    let payment = sqlx::query!(
        r#"SELECT p.booking_id, p.provider, p.provider_payment_id, p.amount_minor,
                  p.currency AS "currency: CurrencyCode", p.status AS "status: PaymentStatus",
                  (p.amount_minor - COALESCE((SELECT sum(r.amount_minor) FROM refunds r
                                              WHERE r.payment_id = p.id AND r.status <> 'failed'), 0))::bigint AS "refundable!"
           FROM payments p WHERE p.id = $1 FOR UPDATE"#,
        payment_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(BookingError::NotFound)?;
    if !matches!(
        payment.status,
        PaymentStatus::Succeeded | PaymentStatus::PartiallyRefunded
    ) {
        return Err(PaymentError::TooMuch(0));
    }
    if amount_minor <= 0 || amount_minor > payment.refundable {
        return Err(PaymentError::TooMuch(payment.refundable));
    }
    let refund_id = Uuid::now_v7();
    let idempotency_key = format!("ref_{}", refund_id.simple());
    sqlx::query!(
        "INSERT INTO refunds (id, payment_id, amount_minor, status, reason, created_by, idempotency_key)
         VALUES ($1, $2, $3, 'pending', $4, $5, $6)",
        refund_id,
        payment_id,
        amount_minor,
        reason,
        created_by,
        idempotency_key
    )
    .execute(&mut *tx)
    .await?;
    let gateway = payments.for_payment(&payment.provider, payment.provider_payment_id.as_deref());
    let (Some(gateway), Some(provider_payment_id)) = (gateway, payment.provider_payment_id.clone())
    else {
        // Cash, bank transfer, card terminal: a person gives the money back.
        create_task(
            &mut tx,
            StaffTaskKind::RefundOffline,
            Some(payment.booking_id),
            Some(payment_id),
            "Refund to give back (offline payment)",
            json!({ "amount_minor": amount_minor, "refund_id": refund_id, "provider": payment.provider }),
            Some(&format!("refund_offline:{refund_id}")),
        )
        .await?;
        tx.commit().await?;
        return Ok(RefundOutcome {
            refund_id,
            status: "staff_task",
            amount_minor,
        });
    };
    tx.commit().await?;

    let result = gateway
        .refund(&RefundRequest {
            provider_payment_id: &provider_payment_id,
            amount_minor,
            currency: &payment.currency,
            idempotency_key: &idempotency_key,
        })
        .await;
    let mut tx = pool.begin().await?;
    let status = match result {
        Ok(r) => {
            sqlx::query!(
                "UPDATE refunds SET provider_refund_id = $2, status = $3, processed_at = $4 WHERE id = $1",
                refund_id,
                r.provider_refund_id,
                if r.succeeded { "succeeded" } else { "pending" },
                r.succeeded.then_some(now)
            )
            .execute(&mut *tx)
            .await?;
            if r.succeeded {
                let booking_id = after_refund(&mut tx, payment_id).await?;
                outbox::enqueue(
                    &mut tx,
                    &Event::RefundSucceeded {
                        refund_id,
                        booking_id,
                    },
                )
                .await?;
                "succeeded"
            } else {
                "pending"
            }
        }
        Err(e) => {
            sqlx::query!(
                "UPDATE refunds SET status = 'failed', failure_reason = $2 WHERE id = $1",
                refund_id,
                e.to_string()
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(e.into());
        }
    };
    tx.commit().await?;
    Ok(RefundOutcome {
        refund_id,
        status,
        amount_minor,
    })
}

/// Payment status and the booking's paid amount after a refund. Returns the booking id.
async fn after_refund(tx: &mut PgConnection, payment_id: Uuid) -> sqlx::Result<Uuid> {
    let booking_id = sqlx::query_scalar!(
        "UPDATE payments p SET status = CASE
             WHEN (SELECT COALESCE(sum(amount_minor), 0) FROM refunds WHERE payment_id = p.id AND status = 'succeeded')
                  >= p.amount_minor THEN 'refunded' ELSE 'partially_refunded' END
         WHERE p.id = $1 RETURNING p.booking_id",
        payment_id
    )
    .fetch_one(&mut *tx)
    .await?;
    recompute_paid(tx, booking_id).await?;
    Ok(booking_id)
}

/// Cash, bank transfer, card terminal... recorded by staff.
#[allow(clippy::too_many_arguments)]
pub async fn record_offline(
    pool: &PgPool,
    booking_id: Uuid,
    provider: PaymentProvider,
    purpose: PaymentPurpose,
    amount_minor: i64,
    received_by: Uuid,
    note: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Uuid, PaymentError> {
    if amount_minor <= 0 {
        return Err(PaymentError::NothingToPay);
    }
    let mut tx = pool.begin().await?;
    let status = state::lock(&mut tx, booking_id).await?;
    let currency = sqlx::query_scalar!(
        r#"SELECT currency AS "currency: CurrencyCode" FROM bookings WHERE id = $1"#,
        booking_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let payment_id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO payments (id, booking_id, provider, purpose, status, amount_minor, currency, idempotency_key,
                               paid_at, received_by, provider_payload)
         VALUES ($1, $2, $3, $4, 'succeeded', $5, $6, $7, $8, $9, $10)",
        payment_id,
        booking_id,
        provider.as_str(),
        purpose.as_str(),
        amount_minor,
        currency.as_str(),
        format!("offline_{}", payment_id.simple()),
        now,
        received_by,
        json!({ "note": note })
    )
    .execute(&mut *tx)
    .await?;
    recompute_paid(&mut tx, booking_id).await?;
    let actor = Actor::staff(received_by);
    audit::record(
        &mut tx,
        &actor,
        "payment.recorded",
        "payment",
        &payment_id.to_string(),
        json!({ "booking_id": booking_id, "provider": provider, "amount_minor": amount_minor, "note": note }),
    )
    .await?;
    if status == BookingStatus::AwaitingPayment {
        state::transition(
            &mut tx,
            booking_id,
            BookingStatus::Confirmed,
            &actor,
            Some("offline payment"),
            now,
        )
        .await?;
        outbox::enqueue(&mut tx, &Event::BookingConfirmed { booking_id }).await?;
    }
    outbox::enqueue(
        &mut tx,
        &Event::PaymentSucceeded {
            payment_id,
            booking_id,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(payment_id)
}

/// Charges the balance on the saved card (consent given at checkout). A decline creates a staff
/// task and emails the guest; the booking is never cancelled automatically.
pub async fn charge_balance(
    pool: &PgPool,
    payments: &Payments,
    booking_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Option<Uuid>, PaymentError> {
    let booking = sqlx::query!(
        r#"SELECT b.reference, b.total_minor, b.amount_paid_minor, b.saved_payment_method_id,
                  b.currency AS "currency: CurrencyCode", c.stripe_customer_id
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           WHERE b.id = $1 AND b.status = 'confirmed' AND b.balance_collection = 'saved_card'"#,
        booking_id
    )
    .fetch_optional(pool)
    .await?;
    let Some(b) = booking else { return Ok(None) };
    let due = b.total_minor - b.amount_paid_minor;
    let (Some(pm), Some(customer)) = (
        b.saved_payment_method_id,
        b.stripe_customer_id.or_else(|| {
            payments
                .get("fake")
                .map(|_| format!("fake_cus_{}", booking_id.simple()))
        }),
    ) else {
        return Ok(None);
    };
    if due <= 0 {
        return Ok(None);
    }
    let (name, gateway) = if pm.starts_with("fake_") {
        payments.get("fake")
    } else {
        payments.get("stripe")
    }
    .ok_or(PaymentError::NoProvider)?;
    let payment_id = Uuid::now_v7();
    let idempotency_key = format!("bal_{}_{}", booking_id.simple(), due);
    let inserted = sqlx::query!(
        "INSERT INTO payments (id, booking_id, provider, purpose, status, amount_minor, currency, idempotency_key,
                               provider_payload)
         VALUES ($1, $2, $3, 'balance', 'pending', $4, $5, $6, '{\"off_session\": true}')
         ON CONFLICT (idempotency_key) DO NOTHING",
        payment_id,
        booking_id,
        gateway.provider().as_str(),
        due,
        b.currency.as_str(),
        idempotency_key
    )
    .execute(pool)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(None); // already attempted for this amount
    }
    tracing::info!(booking = %b.reference, provider = name, "charging the balance off-session");
    match gateway
        .charge_off_session(&OffSessionCharge {
            amount_minor: due,
            currency: &b.currency,
            idempotency_key: &idempotency_key,
            provider_customer_id: &customer,
            payment_method_id: &pm,
            reference: &b.reference,
            booking_id,
            payment_id,
        })
        .await
    {
        Ok(p) => {
            sqlx::query!(
                "UPDATE payments SET provider_payment_id = $2,
                        provider_payload = provider_payload || $3 WHERE id = $1",
                payment_id,
                p.provider_payment_id,
                p.payload
            )
            .execute(pool)
            .await?;
            // The webhook confirms; the fake provider has none, so it confirms right away.
            if p.payload["fake"] == true {
                payment_succeeded(pool, payments, &p.provider_payment_id, None, now).await?;
            }
            Ok(Some(payment_id))
        }
        Err(GatewayError::Declined(message)) => {
            let mut tx = pool.begin().await?;
            sqlx::query!(
                "UPDATE payments SET status = 'failed', failure_reason = $2 WHERE id = $1",
                payment_id,
                message
            )
            .execute(&mut *tx)
            .await?;
            balance_failed(&mut tx, booking_id, payment_id, &message, now).await?;
            outbox::enqueue(
                &mut tx,
                &Event::PaymentFailed {
                    payment_id,
                    booking_id,
                },
            )
            .await?;
            tx.commit().await?;
            Ok(Some(payment_id))
        }
        Err(e) => {
            // Outage: forget the attempt so the next run retries.
            sqlx::query!(
                "DELETE FROM payments WHERE id = $1 AND status = 'pending'",
                payment_id
            )
            .execute(pool)
            .await?;
            Err(e.into())
        }
    }
}
