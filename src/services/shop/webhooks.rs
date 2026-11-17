//! Provider webhooks for shop orders.
//!
//! The HTTP handler verifies the signature, stores the event in `payment_webhook_events`
//! (`ON CONFLICT (provider, event_id) DO NOTHING`), writes `payment_webhook.received` to the
//! outbox and answers 200 at once. The worker ([`WebhookHandler`]) then applies it idempotently:
//! the order is locked `FOR UPDATE`, events already applied are ignored, and an event that
//! arrives before the one it depends on (a refund before the payment) is retried later.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    events::{self, ShopEvent, WebhookPayload},
    orders::{self, ActorRef},
    payments::{ShopOutcome, ShopPayments},
};
use crate::{
    db::operations::OutboxEvent,
    error::AppResult,
    worker::{HandlerError, OutboxHandler},
};

/// Stores a verified event and queues it. Returns false for a duplicate delivery.
pub async fn ingest(pool: &PgPool, provider: &str, event: &Value) -> sqlx::Result<bool> {
    let event_id = event["id"].as_str().unwrap_or_default();
    let event_type = event["type"].as_str().unwrap_or_default();
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query_scalar!(
        "INSERT INTO payment_webhook_events (provider, event_id, event_type, payload)
         VALUES ($1, $2, $3, $4) ON CONFLICT (provider, event_id) DO NOTHING RETURNING id",
        provider,
        event_id,
        event_type,
        event
    )
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        tx.rollback().await?;
        return Ok(false);
    }
    events::enqueue(
        &mut tx,
        ShopEvent::PaymentWebhookReceived {
            provider: provider.to_string(),
            event_id: event_id.to_string(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Does this Stripe event concern a shop order? Checkout sessions and objects with our
/// `order_id` metadata do; refunds and disputes when their payment intent is an order payment.
pub async fn is_shop_event(pool: &PgPool, event: &Value) -> sqlx::Result<bool> {
    if super::payments::event_mentions_order(event) {
        return Ok(true);
    }
    let object = &event["data"]["object"];
    let intent = object["payment_intent"].as_str().or_else(|| {
        event["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("payment_intent."))
            .then(|| object["id"].as_str())
            .flatten()
    });
    if let Some(pi) = intent {
        return sqlx::query_scalar!(
            "SELECT EXISTS (SELECT 1 FROM payments WHERE provider_payment_id = $1 AND order_id IS NOT NULL) AS \"x!\"",
            pi
        )
        .fetch_one(pool)
        .await;
    }
    if event["type"]
        .as_str()
        .is_some_and(|t| t.starts_with("refund."))
    {
        let id = object["id"].as_str().unwrap_or_default();
        return sqlx::query_scalar!(
            "SELECT EXISTS (SELECT 1 FROM refunds r JOIN payments p ON p.id = r.payment_id
                            WHERE r.provider_refund_id = $1 AND p.order_id IS NOT NULL) AS \"x!\"",
            id
        )
        .fetch_one(pool)
        .await;
    }
    Ok(false)
}

/// Why an event cannot be applied yet.
#[derive(Debug)]
pub enum ApplyError {
    /// Depends on an event not processed yet (out of order): retry later.
    NotYet(String),
    Failed(anyhow::Error),
}

impl From<sqlx::Error> for ApplyError {
    fn from(e: sqlx::Error) -> Self {
        Self::Failed(e.into())
    }
}

impl From<crate::error::AppError> for ApplyError {
    fn from(e: crate::error::AppError) -> Self {
        Self::Failed(anyhow::anyhow!("{e}"))
    }
}

async fn payment_by_session(
    conn: &mut PgConnection,
    session_id: &str,
) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar!(
        "SELECT id FROM payments WHERE provider_session_id = $1 AND order_id IS NOT NULL",
        session_id
    )
    .fetch_optional(conn)
    .await
}

/// Applies one outcome in the caller's transaction.
pub async fn apply(conn: &mut PgConnection, outcome: &ShopOutcome) -> Result<(), ApplyError> {
    let now = Utc::now();
    match outcome {
        ShopOutcome::SessionPaid {
            session_id,
            payment_intent,
            amount_minor,
            currency,
        } => {
            let Some(payment_id) = payment_by_session(conn, session_id).await? else {
                // The session row is written right after Stripe answers: give it a moment.
                return Err(ApplyError::NotYet(format!(
                    "no payment for session {session_id} yet"
                )));
            };
            orders::online_payment_succeeded(
                conn,
                payment_id,
                payment_intent.as_deref(),
                *amount_minor,
                currency,
                now,
            )
            .await?;
        }
        ShopOutcome::SessionPending { session_id } => {
            if let Some(payment_id) = payment_by_session(conn, session_id).await? {
                sqlx::query!(
                    "UPDATE payments SET status = 'requires_action' WHERE id = $1 AND status = 'pending'",
                    payment_id
                )
                .execute(&mut *conn)
                .await?;
            }
        }
        ShopOutcome::SessionFailed { session_id } | ShopOutcome::SessionExpired { session_id } => {
            let status = if matches!(outcome, ShopOutcome::SessionFailed { .. }) {
                "failed"
            } else {
                "cancelled"
            };
            match payment_by_session(conn, session_id).await? {
                Some(payment_id) => {
                    orders::online_payment_ended(conn, payment_id, status, now).await?
                }
                None => tracing::info!(session_id, "session event for an unknown payment, ignored"),
            }
        }
        ShopOutcome::ChargeRefunded {
            payment_intent,
            payment_id,
            amount_refunded_minor,
        } => {
            let payment = sqlx::query!(
                r#"SELECT id, status, order_id AS "order_id!" FROM payments
                   WHERE order_id IS NOT NULL AND (provider_payment_id = $1 OR id = $2)"#,
                payment_intent,
                *payment_id
            )
            .fetch_optional(&mut *conn)
            .await?;
            let Some(p) =
                payment.filter(|p| p.status != "pending" && p.status != "requires_action")
            else {
                return Err(ApplyError::NotYet(
                    "refund before the payment was applied".into(),
                ));
            };
            orders::lock(conn, p.order_id).await?;
            // Refunds made from the Stripe dashboard have no row: record the difference.
            let known = sqlx::query_scalar!(
                "SELECT COALESCE(sum(amount_minor), 0)::bigint AS \"x!\" FROM refunds WHERE payment_id = $1 AND status <> 'failed'",
                p.id
            )
            .fetch_one(&mut *conn)
            .await?;
            if *amount_refunded_minor > known {
                let id = Uuid::now_v7();
                sqlx::query!(
                    "INSERT INTO refunds (id, payment_id, amount_minor, status, reason, idempotency_key, processed_at)
                     VALUES ($1, $2, $3, 'pending', 'refunded from the provider dashboard', $4, $5)",
                    id,
                    p.id,
                    amount_refunded_minor - known,
                    format!("external-{id}"),
                    now
                )
                .execute(&mut *conn)
                .await?;
                orders::log_event(
                    conn,
                    p.order_id,
                    "refund_created",
                    None,
                    Some("refunded from the provider dashboard"),
                    json!({ "amount_minor": amount_refunded_minor - known }),
                    &ActorRef::webhook(),
                )
                .await?;
                orders::refund_settled(conn, id, &ActorRef::webhook(), now).await?;
            }
            // Our own pending refunds of this payment are settled by `refund.updated`.
        }
        ShopOutcome::RefundUpdated {
            refund_id,
            status,
            failure_reason,
            ..
        } => {
            let refund = sqlx::query!(
                r#"SELECT r.id, r.status, p.order_id AS "order_id!" FROM refunds r JOIN payments p ON p.id = r.payment_id
                   WHERE r.provider_refund_id = $1 AND p.order_id IS NOT NULL"#,
                refund_id
            )
            .fetch_optional(&mut *conn)
            .await?;
            let Some(r) = refund else {
                return Err(ApplyError::NotYet(format!(
                    "refund {refund_id} not recorded yet"
                )));
            };
            orders::lock(conn, r.order_id).await?;
            match (status.as_str(), r.status.as_str()) {
                ("succeeded", "pending") => {
                    orders::refund_settled(conn, r.id, &ActorRef::webhook(), now).await?
                }
                ("failed" | "canceled", "pending") => {
                    sqlx::query!(
                        "UPDATE refunds SET status = 'failed', failure_reason = $2, processed_at = $3 WHERE id = $1",
                        r.id,
                        failure_reason.as_deref(),
                        now
                    )
                    .execute(&mut *conn)
                    .await?;
                    orders::log_event(
                        conn,
                        r.order_id,
                        "refund_failed",
                        None,
                        failure_reason.as_deref(),
                        json!({ "refund_id": r.id }),
                        &ActorRef::webhook(),
                    )
                    .await?;
                    events::enqueue(
                        conn,
                        ShopEvent::PaymentIssue {
                            order_id: r.order_id,
                            issue: format!(
                                "a refund failed: {}",
                                failure_reason.as_deref().unwrap_or("unknown reason")
                            ),
                        },
                    )
                    .await?;
                }
                _ => {}
            }
        }
        ShopOutcome::Dispute {
            payment_intent,
            amount_minor,
            reason,
        } => {
            let Some(order_id) = sqlx::query_scalar!(
                r#"SELECT order_id AS "order_id!" FROM payments WHERE provider_payment_id = $1 AND order_id IS NOT NULL"#,
                payment_intent
            )
            .fetch_optional(&mut *conn)
            .await?
            else {
                return Err(ApplyError::NotYet("dispute before the payment was applied".into()));
            };
            orders::lock(conn, order_id).await?;
            sqlx::query!(
                "UPDATE orders SET risk_level = 'high' WHERE id = $1",
                order_id
            )
            .execute(&mut *conn)
            .await?;
            orders::log_event(
                conn,
                order_id,
                "dispute_opened",
                None,
                Some(reason),
                json!({ "amount_minor": amount_minor }),
                &ActorRef::webhook(),
            )
            .await?;
            events::enqueue(
                conn,
                ShopEvent::PaymentIssue {
                    order_id,
                    issue: format!("the customer's bank opened a dispute ({reason})"),
                },
            )
            .await?;
        }
        ShopOutcome::Ignored => {}
    }
    Ok(())
}

/// Processes a stored event once (idempotent: `processed_at`).
pub async fn process(
    pool: &PgPool,
    payments: &ShopPayments,
    provider: &str,
    event_id: &str,
) -> Result<(), ApplyError> {
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query!(
        "SELECT payload, processed_at FROM payment_webhook_events WHERE provider = $1 AND event_id = $2 FOR UPDATE",
        provider,
        event_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(());
    };
    if row.processed_at.is_some() {
        return Ok(());
    }
    let outcome = match payments.get(provider) {
        Some(p) => p.parse_event(&row.payload),
        None => super::payments::parse_stripe_event(&row.payload),
    };
    match apply(&mut tx, &outcome).await {
        Ok(()) => {
            sqlx::query!(
                "UPDATE payment_webhook_events SET processed_at = now(), error = NULL WHERE provider = $1 AND event_id = $2",
                provider,
                event_id
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(())
        }
        Err(e) => {
            drop(tx);
            let message = match &e {
                ApplyError::NotYet(m) => m.clone(),
                ApplyError::Failed(err) => format!("{err:#}"),
            };
            sqlx::query!(
                "UPDATE payment_webhook_events SET error = $3 WHERE provider = $1 AND event_id = $2",
                provider,
                event_id,
                message
            )
            .execute(pool)
            .await?;
            Err(e)
        }
    }
}

/// Outbox handler: applies `payment_webhook.received`.
pub struct WebhookHandler {
    pool: PgPool,
    payments: Arc<ShopPayments>,
}

impl WebhookHandler {
    pub fn new(pool: PgPool, payments: ShopPayments) -> Self {
        Self {
            pool,
            payments: Arc::new(payments),
        }
    }
}

#[async_trait]
impl OutboxHandler for WebhookHandler {
    fn name(&self) -> &'static str {
        "shop_webhooks"
    }

    fn event_types(&self) -> &'static [&'static str] {
        &[events::types::PAYMENT_WEBHOOK_RECEIVED]
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let p: WebhookPayload = serde_json::from_value(event.payload.clone())
            .map_err(|e| HandlerError::Permanent(anyhow::anyhow!("invalid payload: {e}")))?;
        match process(&self.pool, &self.payments, &p.provider, &p.event_id).await {
            Ok(()) => Ok(()),
            Err(ApplyError::NotYet(m)) => {
                tracing::info!(event_id = %p.event_id, reason = %m, "webhook applied later");
                Err(HandlerError::Retry(anyhow::anyhow!(m)))
            }
            Err(ApplyError::Failed(e)) => Err(HandlerError::Retry(e)),
        }
    }
}

/// Starts a refund at the provider (after the refund rows were committed).
pub async fn send_refunds(
    pool: &PgPool,
    payments: &ShopPayments,
    pending: &[orders::PendingRefund],
) -> AppResult<()> {
    for r in pending {
        let Some(provider) = payments.for_payment(&r.provider, r.session_id.as_deref()) else {
            tracing::error!(refund_id = %r.refund_id, provider = %r.provider, alert = true, "refund provider unavailable");
            continue;
        };
        let Some(pi) = r.provider_payment_id.as_deref() else {
            tracing::error!(refund_id = %r.refund_id, alert = true, "refund of a payment without provider id");
            continue;
        };
        let currency = crate::db::types::CurrencyCode(r.currency.clone());
        let result = provider
            .refund(&crate::services::payments::gateway::RefundRequest {
                provider_payment_id: pi,
                amount_minor: r.amount_minor,
                currency: &currency,
                idempotency_key: &r.idempotency_key,
            })
            .await;
        match result {
            Ok(done) => {
                sqlx::query!(
                    "UPDATE refunds SET provider_refund_id = $2 WHERE id = $1",
                    r.refund_id,
                    done.provider_refund_id
                )
                .execute(pool)
                .await?;
            }
            Err(e) => {
                let mut tx = pool.begin().await?;
                sqlx::query!(
                    "UPDATE refunds SET status = 'failed', failure_reason = $2, processed_at = now() WHERE id = $1 AND status = 'pending'",
                    r.refund_id,
                    e.to_string()
                )
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Err(crate::error::AppError::domain(
                    axum::http::StatusCode::BAD_GATEWAY,
                    "refund_failed",
                    "the payment provider refused the refund",
                )
                .with_details(json!({ "message": e.to_string() })));
            }
        }
    }
    Ok(())
}
