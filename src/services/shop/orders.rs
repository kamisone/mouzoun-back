//! Orders after checkout: timeline, status recomputation, confirmation, offline payments,
//! fulfilments, returns, refunds, cancellation and invoices.
//!
//! Three independent statuses, recomputed from the records in the same transaction as every
//! change ([`recompute`]):
//!
//! ```text
//! status:             draft ─► pending ─► open ─► completed        (any ─► cancelled)
//! payment_status:     unpaid · pending · partially_paid · paid · partially_refunded · refunded · voided
//! fulfillment_status: unfulfilled · partially_fulfilled · fulfilled · partially_returned · returned
//! ```
//!
//! - `pending`: waiting for the online payment (Stripe), the staff confirmation (cash on
//!   delivery) or the bank transfer; `open`: being fulfilled; `completed`: fulfilled and paid.
//! - Money: `amount_paid_minor` / `amount_refunded_minor` are caches of `payments` / `refunds`.
//! - Every write locks the order row (`FOR UPDATE`) first.

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

use super::{
    events::{self, ShopEvent},
    inventory::{self, MovementRefs},
};
use crate::{
    db::shop::{
        FulfillmentMethod, FulfillmentStatus, OrderActor, OrderCancelReason, OrderStatus,
        ReturnCondition, ReturnReason, ReturnStatus, ShopPaymentMethod,
    },
    error::{AppError, AppResult},
    services::audit::{self, Actor},
};

use axum::http::StatusCode;

/// Who acts on an order (timeline + audit log).
#[derive(Debug, Clone)]
pub struct ActorRef {
    pub kind: OrderActor,
    pub user_id: Option<Uuid>,
    pub audit: Actor,
}

impl ActorRef {
    pub fn staff(actor: Actor) -> Self {
        Self {
            kind: OrderActor::Staff,
            user_id: actor.user_id,
            audit: actor,
        }
    }
    pub fn customer(actor: Actor) -> Self {
        Self {
            kind: OrderActor::Customer,
            user_id: None,
            audit: actor,
        }
    }
    pub fn system() -> Self {
        Self {
            kind: OrderActor::System,
            user_id: None,
            audit: Actor::system(),
        }
    }
    pub fn webhook() -> Self {
        Self {
            kind: OrderActor::Webhook,
            user_id: None,
            audit: Actor::webhook(),
        }
    }
}

fn conflict(code: &'static str, message: &str) -> AppError {
    AppError::domain(StatusCode::CONFLICT, code, message)
}

/// Adds a line to the order timeline.
pub async fn log_event(
    conn: &mut PgConnection,
    order_id: Uuid,
    kind: &str,
    transition: Option<(&str, &str)>,
    message: Option<&str>,
    data: Value,
    actor: &ActorRef,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO order_events (order_id, kind, from_status, to_status, message, data, actor_type, actor_user_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        order_id,
        kind,
        transition.map(|t| t.0),
        transition.map(|t| t.1),
        message,
        data,
        actor.kind.as_str(),
        actor.user_id
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// `MZS-<yy>-<000123>` (Africa/Casablanca year). From a sequence: gaps are allowed.
pub async fn next_number(db: impl PgExecutor<'_>, now: DateTime<Utc>) -> sqlx::Result<String> {
    let n = sqlx::query_scalar!("SELECT nextval('order_number_seq') AS \"n!\"")
        .fetch_one(db)
        .await?;
    Ok(format!(
        "MZS-{:02}-{n:06}",
        crate::camp_time::local(now).year() % 100
    ))
}

pub async fn next_return_number(
    db: impl PgExecutor<'_>,
    now: DateTime<Utc>,
) -> sqlx::Result<String> {
    let n = sqlx::query_scalar!("SELECT nextval('order_return_number_seq') AS \"n!\"")
        .fetch_one(db)
        .await?;
    Ok(format!(
        "MZR-{:02}-{n:06}",
        crate::camp_time::local(now).year() % 100
    ))
}

#[derive(Debug, Clone)]
pub struct LockedOrder {
    pub id: Uuid,
    pub number: String,
    pub status: OrderStatus,
    pub payment_status: String,
    pub fulfillment_status: String,
    pub payment_method: Option<ShopPaymentMethod>,
    pub fulfillment_method: FulfillmentMethod,
    pub requires_confirmation: bool,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub total_minor: i64,
    pub amount_paid_minor: i64,
    pub amount_refunded_minor: i64,
    pub currency: String,
    pub pickup_location_id: Option<Uuid>,
    pub customer_id: Uuid,
    pub placed_at: Option<DateTime<Utc>>,
    pub locale: String,
}

pub async fn lock(conn: &mut PgConnection, order_id: Uuid) -> sqlx::Result<Option<LockedOrder>> {
    Ok(sqlx::query!(
        r#"SELECT id, number, status AS "status: OrderStatus", payment_status, fulfillment_status,
                  payment_method AS "payment_method: ShopPaymentMethod",
                  fulfillment_method AS "fulfillment_method: FulfillmentMethod",
                  requires_confirmation, confirmed_at, total_minor, amount_paid_minor, amount_refunded_minor,
                  currency::text AS "currency!", pickup_location_id, customer_id, placed_at, locale
           FROM orders WHERE id = $1 FOR UPDATE"#,
        order_id
    )
    .fetch_optional(conn)
    .await?
    .map(|r| LockedOrder {
        id: r.id,
        number: r.number,
        status: r.status,
        payment_status: r.payment_status,
        fulfillment_status: r.fulfillment_status,
        payment_method: r.payment_method,
        fulfillment_method: r.fulfillment_method,
        requires_confirmation: r.requires_confirmation,
        confirmed_at: r.confirmed_at,
        total_minor: r.total_minor,
        amount_paid_minor: r.amount_paid_minor,
        amount_refunded_minor: r.amount_refunded_minor,
        currency: r.currency,
        pickup_location_id: r.pickup_location_id,
        customer_id: r.customer_id,
        placed_at: r.placed_at,
        locale: r.locale,
    }))
}

async fn lock_or_404(conn: &mut PgConnection, order_id: Uuid) -> AppResult<LockedOrder> {
    lock(conn, order_id).await?.ok_or(AppError::NotFound)
}

/// Payment status from the money facts.
pub fn payment_status(
    total: i64,
    paid: i64,
    refunded: i64,
    pending_payment: bool,
    cancelled: bool,
) -> &'static str {
    if refunded > 0 && refunded >= paid {
        "refunded"
    } else if refunded > 0 {
        "partially_refunded"
    } else if paid > 0 && paid >= total {
        "paid"
    } else if paid > 0 {
        "partially_paid"
    } else if cancelled {
        "voided"
    } else if total == 0 {
        "paid"
    } else if pending_payment {
        "pending"
    } else {
        "unpaid"
    }
}

/// Fulfilment status from the item quantities (ordered, fulfilled, returned).
pub fn fulfillment_status(quantity: i64, fulfilled: i64, returned: i64) -> &'static str {
    if returned > 0 {
        if returned >= quantity {
            "returned"
        } else {
            "partially_returned"
        }
    } else if fulfilled == 0 {
        "unfulfilled"
    } else if fulfilled < quantity {
        "partially_fulfilled"
    } else {
        "fulfilled"
    }
}

/// Recomputes the money caches and the three statuses from the records. Returns true when the
/// order just became paid (for the `order.paid` event).
pub async fn recompute(
    conn: &mut PgConnection,
    order_id: Uuid,
    now: DateTime<Utc>,
    actor: &ActorRef,
) -> sqlx::Result<bool> {
    let facts = sqlx::query!(
        r#"SELECT o.status AS "status: OrderStatus", o.payment_status, o.fulfillment_status, o.total_minor, o.paid_at,
                  COALESCE((SELECT sum(amount_minor) FROM payments
                            WHERE order_id = o.id AND status IN ('succeeded', 'refunded', 'partially_refunded')), 0)::bigint AS "paid!",
                  COALESCE((SELECT sum(r.amount_minor) FROM refunds r JOIN payments p ON p.id = r.payment_id
                            WHERE p.order_id = o.id AND r.status = 'succeeded'), 0)::bigint AS "refunded!",
                  EXISTS (SELECT 1 FROM payments WHERE order_id = o.id AND status IN ('pending', 'requires_action', 'authorized')) AS "pending!",
                  COALESCE((SELECT sum(quantity) FROM order_items WHERE order_id = o.id), 0)::bigint AS "qty!",
                  COALESCE((SELECT sum(fulfilled_quantity) FROM order_items WHERE order_id = o.id), 0)::bigint AS "fulfilled!",
                  COALESCE((SELECT sum(returned_quantity) FROM order_items WHERE order_id = o.id), 0)::bigint AS "returned!"
           FROM orders o WHERE o.id = $1"#,
        order_id
    )
    .fetch_one(&mut *conn)
    .await?;
    let cancelled = facts.status == OrderStatus::Cancelled;
    let pay = payment_status(
        facts.total_minor,
        facts.paid,
        facts.refunded,
        facts.pending,
        cancelled,
    );
    let ful = fulfillment_status(facts.qty, facts.fulfilled, facts.returned);
    let money_done = matches!(pay, "paid" | "partially_refunded" | "refunded");
    let goods_done = matches!(ful, "fulfilled" | "partially_returned" | "returned");
    let status = match facts.status {
        OrderStatus::Open if money_done && goods_done => OrderStatus::Completed,
        // A completed order whose last item comes back stays completed (history).
        s => s,
    };
    let became_paid = pay == "paid"
        && facts.payment_status != "paid"
        && facts.paid_at.is_none()
        && facts.total_minor > 0;
    sqlx::query!(
        "UPDATE orders SET amount_paid_minor = $2, amount_refunded_minor = $3, payment_status = $4,
                fulfillment_status = $5, status = $6,
                paid_at = CASE WHEN $4 = 'paid' AND paid_at IS NULL THEN $7 ELSE paid_at END,
                completed_at = CASE WHEN $6 = 'completed' AND completed_at IS NULL THEN $7 ELSE completed_at END
         WHERE id = $1",
        order_id,
        facts.paid,
        facts.refunded,
        pay,
        ful,
        status.as_str(),
        now
    )
    .execute(&mut *conn)
    .await?;
    if pay != facts.payment_status {
        log_event(
            conn,
            order_id,
            "payment_status_changed",
            Some((&facts.payment_status, pay)),
            None,
            json!({}),
            actor,
        )
        .await?;
    }
    if ful != facts.fulfillment_status {
        log_event(
            conn,
            order_id,
            "fulfillment_status_changed",
            Some((&facts.fulfillment_status, ful)),
            None,
            json!({}),
            actor,
        )
        .await?;
    }
    if status != facts.status {
        log_event(
            conn,
            order_id,
            "status_changed",
            Some((facts.status.as_str(), status.as_str())),
            None,
            json!({}),
            actor,
        )
        .await?;
    }
    Ok(became_paid)
}

/// Moves a `pending` order to `open` (paid online, COD confirmed, transfer received).
async fn open(
    conn: &mut PgConnection,
    order: &LockedOrder,
    now: DateTime<Utc>,
    actor: &ActorRef,
) -> sqlx::Result<()> {
    if order.status != OrderStatus::Pending {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE orders SET status = 'open', placed_at = COALESCE(placed_at, $2) WHERE id = $1",
        order.id,
        now
    )
    .execute(&mut *conn)
    .await?;
    log_event(
        conn,
        order.id,
        "status_changed",
        Some(("pending", "open")),
        None,
        json!({}),
        actor,
    )
    .await
}

// ---- Payment from the provider (webhook) -------------------------------------------------------

/// The online payment succeeded: payment row, stock consumed, order open, `order.paid`.
/// A different amount or currency still records the payment but flags the order for review.
pub async fn online_payment_succeeded(
    conn: &mut PgConnection,
    payment_id: Uuid,
    provider_payment_id: Option<&str>,
    amount_minor: i64,
    currency: &str,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let p = sqlx::query!(
        r#"SELECT order_id AS "order_id!", status, amount_minor, currency::text AS "currency!" FROM payments
           WHERE id = $1 AND order_id IS NOT NULL"#,
        payment_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let actor = ActorRef::webhook();
    let order = lock_or_404(conn, p.order_id).await?;
    if p.status == "succeeded" || p.status == "refunded" || p.status == "partially_refunded" {
        return Ok(()); // already applied
    }
    sqlx::query!(
        "UPDATE payments SET status = 'succeeded', provider_payment_id = COALESCE($2, provider_payment_id),
                amount_minor = $3, paid_at = $4, failure_reason = NULL
         WHERE id = $1",
        payment_id,
        provider_payment_id,
        amount_minor.max(1),
        now
    )
    .execute(&mut *conn)
    .await?;
    log_event(
        conn,
        order.id,
        "payment_succeeded",
        None,
        None,
        json!({ "payment_id": payment_id, "amount_minor": amount_minor, "currency": currency }),
        &actor,
    )
    .await?;
    let mismatch = amount_minor != p.amount_minor || !currency.eq_ignore_ascii_case(&p.currency);
    if order.status == OrderStatus::Cancelled {
        // Paid after the hold ended: keep the money recorded, staff decide (refund or ship).
        sqlx::query!(
            "UPDATE orders SET risk_level = 'review' WHERE id = $1",
            order.id
        )
        .execute(&mut *conn)
        .await?;
        events::enqueue(
            conn,
            ShopEvent::PaymentIssue {
                order_id: order.id,
                issue: "paid after the order was cancelled: refund it or reopen it".into(),
            },
        )
        .await?;
        recompute(conn, order.id, now, &actor).await?;
        return Ok(());
    }
    inventory::consume(conn, order.id, None, now).await?;
    open(conn, &order, now, &actor).await?;
    if mismatch {
        sqlx::query!(
            "UPDATE orders SET risk_level = 'review' WHERE id = $1 AND risk_level = 'normal'",
            order.id
        )
        .execute(&mut *conn)
        .await?;
        log_event(
            conn,
            order.id,
            "payment_mismatch",
            None,
            Some("paid amount or currency differs from the order"),
            json!({ "expected": p.amount_minor, "paid": amount_minor, "currency": currency }),
            &actor,
        )
        .await?;
        events::enqueue(
            conn,
            ShopEvent::PaymentIssue {
                order_id: order.id,
                issue: format!(
                    "paid {amount_minor} {currency} instead of {} {}",
                    p.amount_minor, p.currency
                ),
            },
        )
        .await?;
    }
    if recompute(conn, order.id, now, &actor).await? {
        events::enqueue(conn, ShopEvent::OrderPaid { order_id: order.id }).await?;
    }
    Ok(())
}

/// The online payment failed or its session expired: stock released and the order cancelled
/// (`payment_failed`) unless another payment succeeded.
pub async fn online_payment_ended(
    conn: &mut PgConnection,
    payment_id: Uuid,
    status: &str,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let p = sqlx::query!(
        r#"SELECT order_id AS "order_id!", status FROM payments WHERE id = $1 AND order_id IS NOT NULL"#,
        payment_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let order = lock_or_404(conn, p.order_id).await?;
    if p.status != "pending" && p.status != "requires_action" {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE payments SET status = $2 WHERE id = $1",
        payment_id,
        status
    )
    .execute(&mut *conn)
    .await?;
    let actor = ActorRef::webhook();
    log_event(
        conn,
        order.id,
        &format!("payment_{status}"),
        None,
        None,
        json!({ "payment_id": payment_id }),
        &actor,
    )
    .await?;
    let paid_other = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM payments WHERE order_id = $1 AND status IN ('succeeded', 'pending', 'requires_action')) AS \"x!\"",
        order.id
    )
    .fetch_one(&mut *conn)
    .await?;
    if !paid_other && matches!(order.status, OrderStatus::Pending) {
        cancel_in_tx(
            conn,
            &order,
            OrderCancelReason::PaymentFailed,
            "system",
            false,
            None,
            now,
            &actor,
        )
        .await?;
    } else {
        recompute(conn, order.id, now, &actor).await?;
    }
    Ok(())
}

// ---- Staff actions ------------------------------------------------------------------------------

/// Staff confirmed a cash-on-delivery order (phone / WhatsApp): it can be shipped.
pub async fn confirm(
    conn: &mut PgConnection,
    order_id: Uuid,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let order = lock_or_404(conn, order_id).await?;
    if !order.requires_confirmation
        || order.confirmed_at.is_some()
        || order.status != OrderStatus::Pending
    {
        return Err(conflict(
            "not_confirmable",
            "this order does not wait for a confirmation",
        ));
    }
    sqlx::query!(
        "UPDATE orders SET confirmed_at = $2, confirmed_by = $3 WHERE id = $1",
        order_id,
        now,
        actor.user_id
    )
    .execute(&mut *conn)
    .await?;
    log_event(conn, order_id, "confirmed", None, None, json!({}), actor).await?;
    open(conn, &order, now, actor).await?;
    recompute(conn, order_id, now, actor).await?;
    audit::record(
        conn,
        &actor.audit,
        "order.confirmed",
        "order",
        &order_id.to_string(),
        json!({}),
    )
    .await?;
    events::enqueue(conn, ShopEvent::OrderConfirmed { order_id }).await?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct OfflinePayment {
    /// `cash`, `bank_transfer`, `card_terminal`, `cash_on_delivery`, `pay_at_pickup`.
    pub provider: String,
    pub amount_minor: i64,
    pub reference: Option<String>,
    pub note: Option<String>,
}

/// Money received outside a provider (cash, transfer, card terminal): recorded as succeeded.
pub async fn record_offline_payment(
    conn: &mut PgConnection,
    order_id: Uuid,
    p: &OfflinePayment,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<Uuid> {
    if ![
        "cash",
        "bank_transfer",
        "card_terminal",
        "cash_on_delivery",
        "pay_at_pickup",
    ]
    .contains(&p.provider.as_str())
    {
        return Err(AppError::BadRequest("unknown payment method".into()));
    }
    let order = lock_or_404(conn, order_id).await?;
    if order.status == OrderStatus::Cancelled || order.status == OrderStatus::Draft {
        return Err(conflict(
            "order_not_payable",
            "this order cannot take a payment",
        ));
    }
    let balance = order.total_minor - order.amount_paid_minor;
    if p.amount_minor <= 0 || p.amount_minor > balance {
        return Err(conflict(
            "amount_invalid",
            "the amount must be positive and at most the balance",
        ));
    }
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO payments (id, order_id, provider, purpose, status, amount_minor, currency, idempotency_key,
                               provider_payment_id, paid_at, received_by, provider_payload, legal_entity_id)
         VALUES ($1, $2, $3, 'full', 'succeeded', $4, $5, $6, $7, $8, $9, $10, (SELECT legal_entity_id FROM orders WHERE id = $2))",
        id,
        order_id,
        p.provider,
        p.amount_minor,
        order.currency,
        format!("offline-{id}"),
        p.reference.as_deref().map(|r| format!("{}:{r}", p.provider)),
        now,
        actor.user_id,
        json!({ "note": p.note })
    )
    .execute(&mut *conn)
    .await?;
    // A pending online payment of this order is no longer expected.
    sqlx::query!(
        "UPDATE payments SET status = 'cancelled' WHERE order_id = $1 AND id <> $2 AND status IN ('pending', 'requires_action')",
        order_id,
        id
    )
    .execute(&mut *conn)
    .await?;
    log_event(
        conn,
        order_id,
        "payment_recorded",
        None,
        p.note.as_deref(),
        json!({ "payment_id": id, "provider": p.provider, "amount_minor": p.amount_minor }),
        actor,
    )
    .await?;
    if order.amount_paid_minor + p.amount_minor >= order.total_minor {
        inventory::consume(conn, order_id, None, now).await?;
        if !order.requires_confirmation
            || order.confirmed_at.is_some()
            || p.provider != "cash_on_delivery"
        {
            open(conn, &order, now, actor).await?;
        }
    }
    let paid = recompute(conn, order_id, now, actor).await?;
    audit::record(
        conn,
        &actor.audit,
        "order.payment_recorded",
        "order",
        &order_id.to_string(),
        json!({ "provider": p.provider, "amount_minor": p.amount_minor }),
    )
    .await?;
    if paid {
        events::enqueue(conn, ShopEvent::OrderPaid { order_id }).await?;
    }
    Ok(id)
}

// ---- Cancellation -------------------------------------------------------------------------------

/// What a cancellation leaves to do outside the transaction: refunds through the provider.
#[derive(Debug, Clone, Serialize)]
pub struct CancelOutcome {
    pub released: u64,
    pub restocked: i64,
    /// Succeeded payments with an unrefunded amount: `(payment_id, amount)`.
    pub to_refund: Vec<(Uuid, i64)>,
    /// Pending online sessions to expire: `(provider, session_id)`.
    pub sessions_to_expire: Vec<(String, String)>,
}

/// Cancels inside the caller's transaction (order already locked).
#[allow(clippy::too_many_arguments)]
async fn cancel_in_tx(
    conn: &mut PgConnection,
    order: &LockedOrder,
    reason: OrderCancelReason,
    by: &str,
    restock: bool,
    message: Option<&str>,
    now: DateTime<Utc>,
    actor: &ActorRef,
) -> AppResult<CancelOutcome> {
    if order.status == OrderStatus::Cancelled {
        return Err(conflict(
            "already_cancelled",
            "this order is already cancelled",
        ));
    }
    if order.status == OrderStatus::Completed {
        return Err(conflict(
            "order_completed",
            "a completed order is refunded or returned, not cancelled",
        ));
    }
    let shipped = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM fulfillments WHERE order_id = $1
                        AND status IN ('shipped', 'in_transit', 'delivered', 'picked_up')) AS \"x!\"",
        order.id
    )
    .fetch_one(&mut *conn)
    .await?;
    if shipped {
        return Err(conflict(
            "order_shipped",
            "goods already left: create a return instead",
        ));
    }
    let released = inventory::release(conn, order.id, None, now).await?;
    // Stock already sold (paid orders): put it back when asked, from the lots it left.
    let mut restocked = 0i64;
    if restock {
        let sold = sqlx::query!(
            r#"SELECT variant_id, location_id, lot_id, (-sum(delta))::int AS "q!"
               FROM inventory_movements WHERE order_id = $1 AND reason IN ('sale', 'return_restock')
               GROUP BY variant_id, location_id, lot_id HAVING sum(delta) < 0
               ORDER BY variant_id, location_id"#,
            order.id
        )
        .fetch_all(&mut *conn)
        .await?;
        for s in sold {
            inventory::restock_return(
                conn,
                s.variant_id,
                s.location_id,
                s.lot_id,
                s.q,
                &MovementRefs {
                    order_id: Some(order.id),
                    note: Some("order cancelled".into()),
                    created_by: actor.user_id,
                    ..MovementRefs::default()
                },
            )
            .await?;
            restocked += i64::from(s.q);
        }
    }
    sqlx::query!(
        "UPDATE fulfillments SET status = 'cancelled' WHERE order_id = $1 AND status IN ('pending', 'packed', 'ready_for_pickup')",
        order.id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE order_items SET fulfilled_quantity = 0 WHERE order_id = $1",
        order.id
    )
    .execute(&mut *conn)
    .await?;
    let sessions: Vec<(String, String)> = sqlx::query!(
        "UPDATE payments SET status = 'cancelled' WHERE order_id = $1 AND status IN ('pending', 'requires_action')
         RETURNING provider, provider_session_id",
        order.id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .filter_map(|r| r.provider_session_id.map(|s| (r.provider, s)))
    .collect();
    sqlx::query!(
        "UPDATE orders SET status = 'cancelled', cancelled_at = $2, cancel_reason = $3, cancelled_by = $4 WHERE id = $1",
        order.id,
        now,
        reason.as_str(),
        by
    )
    .execute(&mut *conn)
    .await?;
    log_event(
        conn,
        order.id,
        "cancelled",
        Some((order.status.as_str(), "cancelled")),
        message,
        json!({ "reason": reason, "restock": restock }),
        actor,
    )
    .await?;
    recompute(conn, order.id, now, actor).await?;
    let to_refund = sqlx::query!(
        r#"SELECT p.id, (p.amount_minor - COALESCE((SELECT sum(r.amount_minor) FROM refunds r
                                                   WHERE r.payment_id = p.id AND r.status <> 'failed'), 0))::bigint AS "left!"
           FROM payments p WHERE p.order_id = $1 AND p.status IN ('succeeded', 'partially_refunded')"#,
        order.id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .filter(|r| r.left > 0)
    .map(|r| (r.id, r.left))
    .collect();
    audit::record(
        conn,
        &actor.audit,
        "order.cancelled",
        "order",
        &order.id.to_string(),
        json!({ "reason": reason, "restock": restock }),
    )
    .await?;
    events::enqueue(conn, ShopEvent::OrderCancelled { order_id: order.id }).await?;
    Ok(CancelOutcome {
        released,
        restocked,
        to_refund,
        sessions_to_expire: sessions,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn cancel(
    conn: &mut PgConnection,
    order_id: Uuid,
    reason: OrderCancelReason,
    by: &str,
    restock: bool,
    message: Option<&str>,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<CancelOutcome> {
    let order = lock_or_404(conn, order_id).await?;
    cancel_in_tx(conn, &order, reason, by, restock, message, now, actor).await
}

// ---- Fulfilment ------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ItemQuantity {
    pub order_item_id: Uuid,
    pub quantity: i32,
}

/// 6-digit pickup code of a fulfilment (derived, so it can be shown again; only its hash is
/// stored to check it at the counter).
pub fn pickup_code(secret: &[u8], fulfillment_id: Uuid) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let tag = ring::hmac::sign(&key, fulfillment_id.as_bytes());
    let n = u32::from_be_bytes([
        tag.as_ref()[0],
        tag.as_ref()[1],
        tag.as_ref()[2],
        tag.as_ref()[3],
    ]);
    format!("{:06}", n % 1_000_000)
}

fn hash_code(code: &str) -> Vec<u8> {
    Sha256::digest(code.trim().as_bytes()).to_vec()
}

/// Creates a fulfilment for some items (default: everything left). Stock not consumed yet
/// (offline payments) leaves now; the lots picked are recorded (FEFO, from the sale movements).
pub async fn create_fulfillment(
    conn: &mut PgConnection,
    order_id: Uuid,
    location_id: Option<Uuid>,
    items: Option<&[ItemQuantity]>,
    code_secret: &[u8],
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<Uuid> {
    let order = lock_or_404(conn, order_id).await?;
    if order.status != OrderStatus::Open {
        return Err(conflict(
            "order_not_open",
            "only open orders are fulfilled (pending: paid or confirmed first)",
        ));
    }
    let rows = sqlx::query!(
        "SELECT id, variant_id, quantity, fulfilled_quantity FROM order_items WHERE order_id = $1 ORDER BY sort_order FOR UPDATE",
        order_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let wanted: Vec<(Uuid, Uuid, i32)> = match items {
        Some(list) => {
            let mut out = Vec::new();
            for it in list {
                let row = rows
                    .iter()
                    .find(|r| r.id == it.order_item_id)
                    .ok_or_else(|| AppError::BadRequest("unknown order item".into()))?;
                if it.quantity <= 0 || it.quantity > row.quantity - row.fulfilled_quantity {
                    return Err(conflict(
                        "quantity_invalid",
                        "more than what is left to fulfil",
                    ));
                }
                out.push((row.id, row.variant_id, it.quantity));
            }
            out
        }
        None => rows
            .iter()
            .filter(|r| r.quantity > r.fulfilled_quantity)
            .map(|r| (r.id, r.variant_id, r.quantity - r.fulfilled_quantity))
            .collect(),
    };
    if wanted.is_empty() {
        return Err(conflict(
            "nothing_to_fulfil",
            "every item is already fulfilled",
        ));
    }
    let location_id = match location_id.or(order.pickup_location_id) {
        Some(l) => l,
        None => {
            // Where the stock was reserved, else the first online location.
            sqlx::query_scalar!(
                "SELECT location_id FROM inventory_reservations WHERE order_id = $1
                 UNION ALL (SELECT id FROM stock_locations s WHERE is_active AND fulfills_online_orders AND (s.legal_entity_id IS NULL OR s.legal_entity_id = shop_entity_id()) ORDER BY sort_order LIMIT 1)
                 LIMIT 1",
                order_id
            )
            .fetch_optional(&mut *conn)
            .await?
            .flatten()
            .ok_or_else(|| AppError::BadRequest("no stock location".into()))?
        }
    };
    let item_ids: Vec<Uuid> = wanted.iter().map(|w| w.0).collect();
    inventory::consume(conn, order_id, Some(&item_ids), now).await?;
    let id = Uuid::now_v7();
    let code_hash = (order.fulfillment_method == FulfillmentMethod::Pickup)
        .then(|| hash_code(&pickup_code(code_secret, id)));
    sqlx::query!(
        "INSERT INTO fulfillments (id, order_id, location_id, method, status, pickup_code_hash, created_by, relay_point)
         VALUES ($1, $2, $3, $4, 'pending', $5, $6, (SELECT relay_point FROM orders WHERE id = $2))",
        id,
        order_id,
        location_id,
        order.fulfillment_method.as_str(),
        code_hash,
        actor.user_id
    )
    .execute(&mut *conn)
    .await?;
    for (item, variant, q) in &wanted {
        // Lots that left for this item (sale movements of the order for the variant).
        let lots = sqlx::query_scalar!(
            r#"SELECT COALESCE(jsonb_agg(jsonb_build_object('lot_id', l.id, 'lot_code', l.lot_code, 'best_before', l.best_before,
                                                           'quantity', -m.delta) ORDER BY l.best_before NULLS LAST), '[]') AS "lots!"
               FROM inventory_movements m JOIN inventory_lots l ON l.id = m.lot_id
               WHERE m.order_id = $1 AND m.variant_id = $2 AND m.reason = 'sale'"#,
            order_id,
            variant
        )
        .fetch_one(&mut *conn)
        .await?;
        sqlx::query!(
            "INSERT INTO fulfillment_items (fulfillment_id, order_item_id, quantity, lots) VALUES ($1, $2, $3, $4)",
            id,
            item,
            q,
            lots
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            "UPDATE order_items SET fulfilled_quantity = fulfilled_quantity + $2 WHERE id = $1",
            item,
            q
        )
        .execute(&mut *conn)
        .await?;
    }
    log_event(
        conn,
        order_id,
        "fulfillment_created",
        None,
        None,
        json!({ "fulfillment_id": id }),
        actor,
    )
    .await?;
    recompute(conn, order_id, now, actor).await?;
    audit::record(
        conn,
        &actor.audit,
        "fulfillment.created",
        "order",
        &order_id.to_string(),
        json!({ "fulfillment_id": id }),
    )
    .await?;
    Ok(id)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FulfillmentAction {
    Pack,
    ReadyForPickup,
    Ship {
        carrier: Option<String>,
        tracking_number: Option<String>,
        tracking_url: Option<String>,
    },
    InTransit,
    Deliver,
    /// Handed over at the camp: the customer's code is checked. `collect_payment` records the
    /// balance paid at the counter (pay at pickup) in the same step.
    PickUp {
        code: String,
        #[serde(default)]
        collect_payment: bool,
    },
    Fail,
    Cancel,
}

pub async fn update_fulfillment(
    conn: &mut PgConnection,
    fulfillment_id: Uuid,
    action: &FulfillmentAction,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<FulfillmentStatus> {
    let f = sqlx::query!(
        r#"SELECT order_id, status AS "status: FulfillmentStatus", method AS "method: FulfillmentMethod", pickup_code_hash
           FROM fulfillments WHERE id = $1"#,
        fulfillment_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let order = lock_or_404(conn, f.order_id).await?;
    use FulfillmentStatus as S;
    let next = match action {
        FulfillmentAction::Pack => S::Packed,
        FulfillmentAction::ReadyForPickup if f.method != FulfillmentMethod::Shipping => {
            S::ReadyForPickup
        }
        FulfillmentAction::Ship { .. } if f.method != FulfillmentMethod::Pickup => S::Shipped,
        FulfillmentAction::InTransit if f.method == FulfillmentMethod::Shipping => S::InTransit,
        FulfillmentAction::Deliver if f.method != FulfillmentMethod::Pickup => S::Delivered,
        FulfillmentAction::PickUp { .. } if f.method == FulfillmentMethod::Pickup => S::PickedUp,
        FulfillmentAction::Fail => S::Failed,
        FulfillmentAction::Cancel => S::Cancelled,
        _ => {
            return Err(conflict(
                "action_invalid",
                "this step does not apply to this delivery method",
            ));
        }
    };
    if f.status.is_final() {
        return Err(conflict(
            "fulfillment_closed",
            "this fulfilment is already closed",
        ));
    }
    if let FulfillmentAction::PickUp {
        code,
        collect_payment,
    } = action
    {
        let ok = f.pickup_code_hash.as_deref().is_some_and(|h| {
            crate::services::accounts::tokens::constant_time_eq(h, &hash_code(code))
        });
        if !ok {
            return Err(conflict(
                "pickup_code_invalid",
                "the pickup code does not match",
            ));
        }
        let balance = order.total_minor - order.amount_paid_minor;
        if balance > 0 {
            if !collect_payment {
                return Err(conflict(
                    "balance_due",
                    "the order is not paid: collect the payment first",
                ));
            }
            record_offline_payment(
                conn,
                order.id,
                &OfflinePayment {
                    provider: "pay_at_pickup".into(),
                    amount_minor: balance,
                    reference: None,
                    note: Some("paid at pickup".into()),
                },
                actor,
                now,
            )
            .await?;
        }
    }
    if let FulfillmentAction::Ship {
        carrier,
        tracking_number,
        tracking_url,
    } = action
    {
        if tracking_url
            .as_deref()
            .is_some_and(|u| !u.starts_with("https://") && !u.starts_with("http://"))
        {
            return Err(AppError::BadRequest(
                "the tracking link must start with https://".into(),
            ));
        }
        sqlx::query!(
            "UPDATE fulfillments SET carrier = COALESCE($2, carrier), tracking_number = COALESCE($3, tracking_number),
                    tracking_url = COALESCE($4, tracking_url) WHERE id = $1",
            fulfillment_id,
            carrier.as_deref(),
            tracking_number.as_deref(),
            tracking_url.as_deref()
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!(
        "UPDATE fulfillments SET status = $2,
                packed_at = CASE WHEN $2 = 'packed' THEN $3 ELSE packed_at END,
                shipped_at = CASE WHEN $2 IN ('shipped', 'ready_for_pickup') THEN COALESCE(shipped_at, $3) ELSE shipped_at END,
                delivered_at = CASE WHEN $2 IN ('delivered', 'picked_up') THEN $3 ELSE delivered_at END
         WHERE id = $1",
        fulfillment_id,
        next.as_str(),
        now
    )
    .execute(&mut *conn)
    .await?;
    if matches!(next, S::Delivered | S::PickedUp) {
        // Right of withdrawal: 14 days from the delivery of the last goods (L221-18).
        sqlx::query!(
            "UPDATE orders o SET withdrawal_deadline = (
                 SELECT (max(f.delivered_at) AT TIME ZONE 'Europe/Paris')::date
                        + COALESCE((SELECT (value #>> '{}')::int FROM settings WHERE key = 'shop.withdrawal_days'), 14)
                 FROM fulfillments f WHERE f.order_id = o.id AND f.status IN ('delivered', 'picked_up'))
             WHERE o.id = (SELECT order_id FROM fulfillments WHERE id = $1)",
            fulfillment_id
        )
        .execute(&mut *conn)
        .await?;
    }
    if matches!(next, S::Cancelled | S::Failed) {
        // Its items go back to "to fulfil" (the stock stays sold: restock through a count or return).
        sqlx::query!(
            "UPDATE order_items oi SET fulfilled_quantity = oi.fulfilled_quantity - fi.quantity
             FROM fulfillment_items fi WHERE fi.fulfillment_id = $1 AND fi.order_item_id = oi.id",
            fulfillment_id
        )
        .execute(&mut *conn)
        .await?;
    }
    log_event(
        conn,
        order.id,
        "fulfillment_updated",
        Some((f.status.as_str(), next.as_str())),
        None,
        json!({ "fulfillment_id": fulfillment_id }),
        actor,
    )
    .await?;
    recompute(conn, order.id, now, actor).await?;
    audit::record(
        conn,
        &actor.audit,
        "fulfillment.updated",
        "fulfillment",
        &fulfillment_id.to_string(),
        json!({ "status": next }),
    )
    .await?;
    if matches!(
        next,
        S::ReadyForPickup | S::Shipped | S::Delivered | S::PickedUp
    ) {
        events::enqueue(
            conn,
            ShopEvent::FulfillmentUpdated {
                order_id: order.id,
                fulfillment_id,
            },
        )
        .await?;
    }
    Ok(next)
}

// ---- Returns -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ReturnItem {
    pub order_item_id: Uuid,
    pub quantity: i32,
    /// Hygiene-sensitive goods (`unsealed_hygiene`): the withdrawal needs them still sealed.
    #[serde(default)]
    pub still_sealed: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReturnRequest {
    pub items: Vec<ReturnItem>,
    pub reason: ReturnReason,
    pub note: Option<String>,
}

/// Consumer deadlines (French Consumer Code) applied to customer requests.
#[derive(Debug, Clone, Copy)]
pub struct ReturnPolicy {
    /// Right of withdrawal: days after delivery (L221-18, 14).
    pub withdrawal_days: i64,
    /// Refund deadline after a withdrawal (L221-24, 14 days).
    pub refund_deadline_days: i64,
    /// Legal guarantee of conformity (L217-3, 2 years) for non-conformity and damage claims.
    pub guarantee_years: i64,
}

/// A return or withdrawal request. Customer requests (`policy` set) follow the consumer
/// rules: a withdrawal within `withdrawal_days` of delivery, never for perishable or
/// personalised goods, and for hygiene-sensitive goods only while sealed; non-conformity and
/// damage within the legal guarantee. Staff (`policy` None) can record any return.
pub async fn request_return(
    conn: &mut PgConnection,
    order_id: Uuid,
    req: &ReturnRequest,
    policy: Option<ReturnPolicy>,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<Uuid> {
    let order = lock_or_404(conn, order_id).await?;
    if req.items.is_empty() {
        return Err(AppError::BadRequest("choose the items to return".into()));
    }
    let withdrawal = req.reason == ReturnReason::Withdrawal;
    let mut refund_due_by = None;
    if let Some(policy) = policy {
        let delivered = sqlx::query_scalar!(
            "SELECT max(COALESCE(delivered_at, shipped_at)) FROM fulfillments WHERE order_id = $1 AND status <> 'cancelled'",
            order_id
        )
        .fetch_one(&mut *conn)
        .await?;
        let Some(delivered) = delivered else {
            return Err(conflict(
                "not_delivered",
                "returns open once the order is delivered",
            ));
        };
        let limit = if withdrawal {
            delivered + chrono::Duration::days(policy.withdrawal_days)
        } else {
            delivered + chrono::Duration::days(365 * policy.guarantee_years)
        };
        if now > limit {
            return Err(conflict(
                if withdrawal {
                    "withdrawal_period_over"
                } else {
                    "return_window_closed"
                },
                if withdrawal {
                    "the 14-day withdrawal period is over"
                } else {
                    "the return period is over"
                },
            ));
        }
        if withdrawal {
            refund_due_by = Some(
                crate::camp_time::local(now).date()
                    + chrono::Duration::days(policy.refund_deadline_days),
            );
        }
    }
    let open_returns = sqlx::query!(
        r#"SELECT ri.order_item_id, sum(ri.quantity)::int AS "q!" FROM return_items ri JOIN order_returns r ON r.id = ri.return_id
           WHERE r.order_id = $1 AND r.status IN ('requested', 'approved') GROUP BY ri.order_item_id"#,
        order_id
    )
    .fetch_all(&mut *conn)
    .await?;
    for it in &req.items {
        let item = sqlx::query!(
            r#"SELECT fulfilled_quantity, returned_quantity, COALESCE(product_snapshot->>'withdrawal_exemption', 'none') AS "exemption!"
               FROM order_items WHERE id = $1 AND order_id = $2"#,
            it.order_item_id,
            order_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| AppError::BadRequest("unknown order item".into()))?;
        if withdrawal && policy.is_some() {
            match item.exemption.as_str() {
                "perishable" | "personalised" => {
                    return Err(conflict(
                        "withdrawal_excluded",
                        "the right of withdrawal does not apply to this product (perishable or personalised)",
                    )
                    .with_details(json!({ "order_item_id": it.order_item_id, "exemption": item.exemption })));
                }
                "unsealed_hygiene" if it.still_sealed != Some(true) => {
                    return Err(conflict(
                        "withdrawal_unsealed",
                        "this product can only be withdrawn while its seal is intact",
                    )
                    .with_details(json!({ "order_item_id": it.order_item_id })));
                }
                _ => {}
            }
        }
        let pending = open_returns
            .iter()
            .find(|r| r.order_item_id == it.order_item_id)
            .map_or(0, |r| r.q);
        if it.quantity <= 0
            || it.quantity > item.fulfilled_quantity - item.returned_quantity - pending
        {
            return Err(conflict("quantity_invalid", "more than what was delivered"));
        }
    }
    let id = Uuid::now_v7();
    let number = next_return_number(&mut *conn, now).await?;
    sqlx::query!(
        "INSERT INTO order_returns (id, order_id, number, status, reason, customer_note, staff_note, requested_at, created_by, refund_due_by)
         VALUES ($1, $2, $3, 'requested', $4, $5, $6, $7, $8, $9)",
        id,
        order_id,
        number,
        req.reason.as_str(),
        (actor.kind == OrderActor::Customer).then_some(req.note.as_deref()).flatten(),
        (actor.kind == OrderActor::Staff).then_some(req.note.as_deref()).flatten(),
        now,
        actor.user_id,
        refund_due_by
    )
    .execute(&mut *conn)
    .await?;
    for it in &req.items {
        sqlx::query!(
            "INSERT INTO return_items (return_id, order_item_id, quantity, still_sealed) VALUES ($1, $2, $3, $4)",
            id,
            it.order_item_id,
            it.quantity,
            it.still_sealed
        )
        .execute(&mut *conn)
        .await?;
    }
    log_event(
        conn,
        order.id,
        "return_requested",
        None,
        req.note.as_deref(),
        json!({ "return_id": id, "number": number }),
        actor,
    )
    .await?;
    audit::record(
        conn,
        &actor.audit,
        "return.requested",
        "order",
        &order_id.to_string(),
        json!({ "return_id": id }),
    )
    .await?;
    events::enqueue(
        conn,
        ShopEvent::ReturnUpdated {
            order_id,
            return_id: id,
        },
    )
    .await?;
    Ok(id)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReceivedItem {
    pub order_item_id: Uuid,
    pub restock: bool,
    pub condition: ReturnCondition,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReturnAction {
    Approve {
        note: Option<String>,
    },
    Reject {
        note: Option<String>,
    },
    /// Goods received: restocked per line when sellable and asked.
    Receive {
        items: Vec<ReceivedItem>,
    },
    Close,
}

pub async fn update_return(
    conn: &mut PgConnection,
    return_id: Uuid,
    action: &ReturnAction,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<ReturnStatus> {
    let r = sqlx::query!(
        r#"SELECT order_id, status AS "status: ReturnStatus" FROM order_returns WHERE id = $1"#,
        return_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let order = lock_or_404(conn, r.order_id).await?;
    let (next, note) = match (action, r.status) {
        (ReturnAction::Approve { note }, ReturnStatus::Requested) => {
            (ReturnStatus::Approved, note.clone())
        }
        (ReturnAction::Reject { note }, ReturnStatus::Requested | ReturnStatus::Approved) => {
            (ReturnStatus::Rejected, note.clone())
        }
        (ReturnAction::Receive { .. }, ReturnStatus::Approved | ReturnStatus::Requested) => {
            (ReturnStatus::Received, None)
        }
        (ReturnAction::Close, ReturnStatus::Received | ReturnStatus::Rejected) => {
            (ReturnStatus::Closed, None)
        }
        _ => {
            return Err(conflict(
                "return_step_invalid",
                "this step does not follow the current one",
            ));
        }
    };
    if let ReturnAction::Receive { items } = action {
        let lines = sqlx::query!(
            "SELECT ri.order_item_id, ri.quantity, oi.variant_id FROM return_items ri
             JOIN order_items oi ON oi.id = ri.order_item_id WHERE ri.return_id = $1",
            return_id
        )
        .fetch_all(&mut *conn)
        .await?;
        for line in lines {
            let choice = items.iter().find(|i| i.order_item_id == line.order_item_id);
            let (restock, condition) = choice.map_or((false, ReturnCondition::Sellable), |c| {
                (c.restock, c.condition)
            });
            sqlx::query!(
                "UPDATE return_items SET restock = $3, condition = $4 WHERE return_id = $1 AND order_item_id = $2",
                return_id,
                line.order_item_id,
                restock && condition == ReturnCondition::Sellable,
                condition.as_str()
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(
                "UPDATE order_items SET returned_quantity = returned_quantity + $2 WHERE id = $1",
                line.order_item_id,
                line.quantity
            )
            .execute(&mut *conn)
            .await?;
            if restock && condition == ReturnCondition::Sellable {
                let from = sqlx::query!(
                    "SELECT location_id, lot_id FROM inventory_movements
                     WHERE order_id = $1 AND variant_id = $2 AND reason = 'sale' ORDER BY id LIMIT 1",
                    order.id,
                    line.variant_id
                )
                .fetch_optional(&mut *conn)
                .await?;
                let location = match &from {
                    Some(f) => f.location_id,
                    None => sqlx::query_scalar!(
                        "SELECT id FROM stock_locations WHERE is_active ORDER BY fulfills_online_orders DESC, sort_order LIMIT 1"
                    )
                    .fetch_one(&mut *conn)
                    .await?,
                };
                inventory::restock_return(
                    conn,
                    line.variant_id,
                    location,
                    from.and_then(|f| f.lot_id),
                    line.quantity,
                    &MovementRefs {
                        order_id: Some(order.id),
                        return_id: Some(return_id),
                        created_by: actor.user_id,
                        ..MovementRefs::default()
                    },
                )
                .await?;
            }
        }
    }
    sqlx::query!(
        "UPDATE order_returns SET status = $2, staff_note = COALESCE($3, staff_note),
                received_at = CASE WHEN $2 = 'received' THEN $4 ELSE received_at END
         WHERE id = $1",
        return_id,
        next.as_str(),
        note.as_deref(),
        now
    )
    .execute(&mut *conn)
    .await?;
    log_event(
        conn,
        order.id,
        "return_updated",
        Some((r.status.as_str(), next.as_str())),
        note.as_deref(),
        json!({ "return_id": return_id }),
        actor,
    )
    .await?;
    recompute(conn, order.id, now, actor).await?;
    audit::record(
        conn,
        &actor.audit,
        "return.updated",
        "order_return",
        &return_id.to_string(),
        json!({ "status": next }),
    )
    .await?;
    events::enqueue(
        conn,
        ShopEvent::ReturnUpdated {
            order_id: order.id,
            return_id,
        },
    )
    .await?;
    Ok(next)
}

// ---- Refunds -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RefundInput {
    #[serde(default)]
    pub items: Vec<ItemQuantity>,
    #[serde(default)]
    pub shipping_minor: i64,
    /// Custom amount instead of lines (goodwill gesture...).
    pub amount_minor: Option<i64>,
    #[serde(default)]
    pub restock: bool,
    pub reason: Option<String>,
    pub return_id: Option<Uuid>,
}

/// A refund to send to the provider after the transaction commits.
#[derive(Debug, Clone)]
pub struct PendingRefund {
    pub refund_id: Uuid,
    pub provider: String,
    pub provider_payment_id: Option<String>,
    pub session_id: Option<String>,
    pub amount_minor: i64,
    pub currency: String,
    pub idempotency_key: String,
}

/// Amount of `items` of an order: each line's total prorated by quantity (rounded half up).
pub async fn items_amount(
    conn: &mut PgConnection,
    order_id: Uuid,
    items: &[ItemQuantity],
) -> AppResult<(i64, Vec<Value>)> {
    let mut total = 0i64;
    let mut lines = Vec::new();
    for it in items {
        let row = sqlx::query!(
            "SELECT quantity, refunded_quantity, total_minor FROM order_items WHERE id = $1 AND order_id = $2 FOR UPDATE",
            it.order_item_id,
            order_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| AppError::BadRequest("unknown order item".into()))?;
        if it.quantity <= 0 || it.quantity > row.quantity - row.refunded_quantity {
            return Err(conflict(
                "quantity_invalid",
                "more than what is left to refund on this line",
            ));
        }
        let amount = (2 * row.total_minor * i64::from(it.quantity) + i64::from(row.quantity))
            / (2 * i64::from(row.quantity));
        total += amount;
        lines.push(json!({ "order_item_id": it.order_item_id, "quantity": it.quantity, "amount_minor": amount }));
    }
    Ok((total, lines))
}

/// Records a refund (never above the refundable balance, checked under the order lock), split
/// over the succeeded payments (largest balance first). Offline payments are refunded at once;
/// provider refunds are returned to be sent after commit (`pending` until the webhook).
pub async fn create_refund(
    conn: &mut PgConnection,
    order_id: Uuid,
    input: &RefundInput,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<(Vec<Uuid>, Vec<PendingRefund>)> {
    let order = lock_or_404(conn, order_id).await?;
    let (lines_amount, mut lines) = items_amount(conn, order_id, &input.items).await?;
    if input.shipping_minor > 0 {
        lines.push(json!({ "shipping": true, "amount_minor": input.shipping_minor }));
    }
    let amount = input
        .amount_minor
        .unwrap_or(lines_amount + input.shipping_minor.max(0));
    let committed = sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(r.amount_minor), 0)::bigint AS "x!" FROM refunds r JOIN payments p ON p.id = r.payment_id
           WHERE p.order_id = $1 AND r.status <> 'failed'"#,
        order_id
    )
    .fetch_one(&mut *conn)
    .await?;
    let refundable = order.amount_paid_minor - committed;
    if amount <= 0 {
        return Err(AppError::BadRequest("nothing to refund".into()));
    }
    if amount > refundable {
        return Err(
            conflict("refund_exceeds_balance", "more than the refundable balance")
                .with_details(json!({ "refundable_minor": refundable })),
        );
    }
    let payments = sqlx::query!(
        r#"SELECT p.id, p.provider, p.provider_payment_id, p.provider_session_id, p.currency::text AS "currency!",
                  (p.amount_minor - COALESCE((SELECT sum(r.amount_minor) FROM refunds r
                                              WHERE r.payment_id = p.id AND r.status <> 'failed'), 0))::bigint AS "left!"
           FROM payments p WHERE p.order_id = $1 AND p.status IN ('succeeded', 'partially_refunded')
           ORDER BY 6 DESC, p.created_at"#,
        order_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut left = amount;
    let mut created = Vec::new();
    let mut pending = Vec::new();
    for p in payments {
        if left == 0 {
            break;
        }
        let take = left.min(p.left);
        if take <= 0 {
            continue;
        }
        left -= take;
        let id = Uuid::now_v7();
        let online = matches!(p.provider.as_str(), "stripe" | "paypal" | "cmi")
            || p.provider_session_id
                .as_deref()
                .is_some_and(|s| s.starts_with("fake_"));
        let key = format!("refund-{id}");
        sqlx::query!(
            "INSERT INTO refunds (id, payment_id, amount_minor, status, reason, created_by, idempotency_key, return_id, lines,
                                  restock, processed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            id,
            p.id,
            take,
            if online { "pending" } else { "succeeded" },
            input.reason.as_deref(),
            actor.user_id,
            key,
            input.return_id,
            Value::Array(lines.clone()),
            input.restock,
            (!online).then_some(now)
        )
        .execute(&mut *conn)
        .await?;
        created.push(id);
        if online {
            pending.push(PendingRefund {
                refund_id: id,
                provider: p.provider,
                provider_payment_id: p.provider_payment_id,
                session_id: p.provider_session_id,
                amount_minor: take,
                currency: p.currency,
                idempotency_key: key,
            });
        } else {
            refund_settled(conn, id, actor, now).await?;
        }
    }
    for it in &input.items {
        sqlx::query!(
            "UPDATE order_items SET refunded_quantity = refunded_quantity + $2 WHERE id = $1",
            it.order_item_id,
            it.quantity
        )
        .execute(&mut *conn)
        .await?;
    }
    if input.restock && input.return_id.is_none() {
        // Refund of goods that never left (or came back without a return record).
        for it in &input.items {
            let variant = sqlx::query_scalar!(
                "SELECT variant_id FROM order_items WHERE id = $1",
                it.order_item_id
            )
            .fetch_one(&mut *conn)
            .await?;
            let from = sqlx::query!(
                "SELECT location_id, lot_id FROM inventory_movements WHERE order_id = $1 AND variant_id = $2 AND reason = 'sale' ORDER BY id LIMIT 1",
                order_id,
                variant
            )
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(f) = from {
                inventory::restock_return(
                    conn,
                    variant,
                    f.location_id,
                    f.lot_id,
                    it.quantity,
                    &MovementRefs {
                        order_id: Some(order_id),
                        created_by: actor.user_id,
                        note: Some("restocked with a refund".into()),
                        ..MovementRefs::default()
                    },
                )
                .await?;
            }
        }
    }
    log_event(
        conn,
        order_id,
        "refund_created",
        None,
        input.reason.as_deref(),
        json!({ "amount_minor": amount, "refund_ids": created }),
        actor,
    )
    .await?;
    audit::record(
        conn,
        &actor.audit,
        "order.refunded",
        "order",
        &order_id.to_string(),
        json!({ "amount_minor": amount, "lines": lines }),
    )
    .await?;
    Ok((created, pending))
}

/// A refund is final (offline at once, provider ones from the webhook): payment status, order
/// amounts, credit note, `order.refund_issued`.
pub async fn refund_settled(
    conn: &mut PgConnection,
    refund_id: Uuid,
    actor: &ActorRef,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let r = sqlx::query!(
        r#"SELECT r.payment_id, r.amount_minor, r.lines, p.order_id AS "order_id!", p.currency::text AS "currency!"
           FROM refunds r JOIN payments p ON p.id = r.payment_id WHERE r.id = $1 AND p.order_id IS NOT NULL"#,
        refund_id
    )
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE refunds SET status = 'succeeded', processed_at = COALESCE(processed_at, $2) WHERE id = $1",
        refund_id,
        now
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE payments p SET status = CASE
             WHEN (SELECT COALESCE(sum(amount_minor), 0) FROM refunds WHERE payment_id = p.id AND status = 'succeeded')
                  >= p.amount_minor THEN 'refunded' ELSE 'partially_refunded' END
         WHERE p.id = $1",
        r.payment_id
    )
    .execute(&mut *conn)
    .await?;
    recompute(conn, r.order_id, now, actor).await?;
    issue_credit_note(
        conn,
        r.order_id,
        refund_id,
        r.amount_minor,
        &r.currency,
        &r.lines,
        now,
    )
    .await?;
    events::enqueue(
        conn,
        ShopEvent::RefundIssued {
            order_id: r.order_id,
            refund_id,
        },
    )
    .await?;
    Ok(())
}

// ---- Invoices (French rules: see invoices.rs) -----------------------------------------------

pub use super::invoices::{issue_credit_note, issue_invoice};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_follow_the_records() {
        assert_eq!(payment_status(1000, 0, 0, false, false), "unpaid");
        assert_eq!(payment_status(1000, 0, 0, true, false), "pending");
        assert_eq!(payment_status(1000, 400, 0, false, false), "partially_paid");
        assert_eq!(payment_status(1000, 1000, 0, false, false), "paid");
        assert_eq!(
            payment_status(1000, 1000, 300, false, false),
            "partially_refunded"
        );
        assert_eq!(payment_status(1000, 1000, 1000, false, true), "refunded");
        assert_eq!(payment_status(1000, 0, 0, false, true), "voided");
        assert_eq!(fulfillment_status(3, 0, 0), "unfulfilled");
        assert_eq!(fulfillment_status(3, 2, 0), "partially_fulfilled");
        assert_eq!(fulfillment_status(3, 3, 0), "fulfilled");
        assert_eq!(fulfillment_status(3, 3, 1), "partially_returned");
        assert_eq!(fulfillment_status(3, 3, 3), "returned");
    }

    #[test]
    fn pickup_codes_are_six_stable_digits() {
        let id = Uuid::from_u128(42);
        let a = pickup_code(b"secret", id);
        assert_eq!(a.len(), 6);
        assert_eq!(a, pickup_code(b"secret", id));
        assert_ne!(a, pickup_code(b"other", id));
    }
}
