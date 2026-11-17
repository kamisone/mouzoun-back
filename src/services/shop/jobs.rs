//! Shop background work (safe on several instances: row locks, SKIP LOCKED, idempotent emails).
//!
//! - every minute: reservations past their hold are released and their unpaid orders cancelled
//!   (card payments: the Stripe session is expired too; bank transfer and pay-at-pickup holds
//!   use `payments.bank_transfer.hold_days`, so the same job expires them);
//! - hourly: abandoned-cart emails (once per cart, consent required unless the setting says
//!   otherwise) and expired carts closed.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    events::{self, ShopEvent},
    orders::{self, ActorRef},
    payments::ShopPayments,
    settings,
};
use crate::db::shop::OrderCancelReason;

/// Releases expired holds and cancels the orders that were never paid. Returns how many orders
/// were cancelled.
pub async fn expire_holds(
    pool: &PgPool,
    payments: &ShopPayments,
    now: DateTime<Utc>,
) -> anyhow::Result<usize> {
    let candidates: Vec<Uuid> = sqlx::query_scalar!(
        r#"SELECT DISTINCT order_id AS "order_id!" FROM inventory_reservations
           WHERE expires_at <= $1 AND released_at IS NULL AND consumed_at IS NULL
           LIMIT 200"#,
        now
    )
    .fetch_all(pool)
    .await?;
    let mut cancelled = 0;
    for order_id in candidates {
        let mut tx = pool.begin().await?;
        // Another instance may hold it: skip, it will be handled there.
        let row = sqlx::query!(
            "SELECT status, payment_status FROM orders WHERE id = $1 FOR UPDATE SKIP LOCKED",
            order_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(o) = row else { continue };
        let paid = matches!(
            o.payment_status.as_str(),
            "paid" | "partially_refunded" | "refunded"
        );
        if paid || !matches!(o.status.as_str(), "pending" | "open") {
            // Paid in the meantime (stock consumed) or already closed: just free what is left.
            crate::services::shop::inventory::release(&mut tx, order_id, None, now).await?;
            tx.commit().await?;
            continue;
        }
        let outcome = orders::cancel(
            &mut tx,
            order_id,
            OrderCancelReason::PaymentFailed,
            "system",
            false,
            Some("payment not received before the hold ended"),
            &ActorRef::system(),
            now,
        )
        .await;
        match outcome {
            Ok(outcome) => {
                tx.commit().await?;
                cancelled += 1;
                for (provider, session) in outcome.sessions_to_expire {
                    if let Some(p) = payments.for_payment(&provider, Some(&session))
                        && let Err(e) = p.expire(&session).await
                    {
                        tracing::warn!(%order_id, error = %e, "could not expire the checkout session");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(%order_id, error = %e, "expired hold not cancelled");
            }
        }
    }
    Ok(cancelled)
}

/// Daily import of Stripe fees and payouts onto the card payments (accounting exports).
/// Looks back 10 days so late payouts are caught; updates are idempotent.
pub async fn import_settlements(
    pool: &PgPool,
    payments: &super::payments::ShopPayments,
    now: DateTime<Utc>,
) -> anyhow::Result<usize> {
    let Some(provider) = payments.get("stripe") else {
        return Ok(0);
    };
    let since = (now - chrono::Duration::days(10)).timestamp();
    let rows = provider
        .settlements(since)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut updated = 0;
    for s in rows {
        updated += sqlx::query!(
            "UPDATE payments SET fee_minor = $2, net_minor = $3, payout_id = $4, settled_at = to_timestamp($5)
             WHERE provider = 'stripe' AND provider_payment_id = $1
               AND (payout_id IS DISTINCT FROM $4 OR fee_minor IS DISTINCT FROM $2)",
            s.payment_intent,
            s.fee_minor,
            s.net_minor,
            s.payout_id,
            s.arrival_date as f64
        )
        .execute(pool)
        .await?
        .rows_affected() as usize;
    }
    Ok(updated)
}

/// Abandoned-cart emails and cart expiry.
pub async fn carts(pool: &PgPool, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let s = settings::load(pool).await?;
    sqlx::query!(
        "UPDATE carts SET status = 'expired' WHERE status = 'active' AND expires_at <= $1",
        now
    )
    .execute(pool)
    .await?;
    if !s.is_open() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let carts: Vec<Uuid> = sqlx::query_scalar!(
        r#"SELECT c.id FROM carts c
           WHERE c.status = 'active' AND c.email IS NOT NULL AND c.abandoned_email_sent_at IS NULL
             AND c.last_activity_at <= $1 AND c.last_activity_at > $1 - interval '7 days'
             AND EXISTS (SELECT 1 FROM cart_items WHERE cart_id = c.id)
             AND (NOT $2 OR EXISTS (SELECT 1 FROM customers cu WHERE cu.email = c.email AND cu.marketing_consent)
                  OR EXISTS (SELECT 1 FROM newsletter_subscribers n WHERE n.email = c.email AND n.status = 'subscribed'))
           ORDER BY c.last_activity_at LIMIT 100 FOR UPDATE SKIP LOCKED"#,
        now - Duration::hours(s.abandoned_cart_hours),
        s.abandoned_cart_requires_consent
    )
    .fetch_all(&mut *tx)
    .await?;
    for id in &carts {
        sqlx::query!(
            "UPDATE carts SET abandoned_email_sent_at = $2 WHERE id = $1",
            id,
            now
        )
        .execute(&mut *tx)
        .await?;
        events::enqueue(&mut tx, ShopEvent::CartAbandoned { cart_id: *id }).await?;
    }
    tx.commit().await?;
    Ok(carts.len())
}

/// Retention (CNIL, `privacy.data_retention`): carts deleted after `abandoned_carts_days`,
/// back-in-stock requests after `prospects_years`, marketing consent of customers without any
/// order for `customers_marketing_years` withdrawn. Invoices, orders and payments are kept
/// (`accounting_years`, Commercial Code L123-22: 10 years) and never deleted here.
pub async fn retention(pool: &PgPool, now: DateTime<Utc>) -> anyhow::Result<u64> {
    let s = settings::load(pool).await?;
    let r = &s.data_retention;
    let carts_days = r["abandoned_carts_days"].as_i64().unwrap_or(30).max(1);
    let prospects_years = r["prospects_years"].as_i64().unwrap_or(3).max(1);
    let marketing_years = r["customers_marketing_years"].as_i64().unwrap_or(3).max(1);
    let mut n = sqlx::query!(
        "DELETE FROM carts WHERE status <> 'active' AND converted_order_id IS NULL AND updated_at < $1",
        now - Duration::days(carts_days)
    )
    .execute(pool)
    .await?
    .rows_affected();
    n += sqlx::query!(
        "DELETE FROM stock_notifications WHERE created_at < $1",
        now - Duration::days(365 * prospects_years)
    )
    .execute(pool)
    .await?
    .rows_affected();
    n += sqlx::query!(
        "UPDATE customers c SET marketing_consent = false
         WHERE c.marketing_consent AND COALESCE(c.marketing_consent_at, c.created_at) < $1
           AND NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.placed_at >= $1)
           AND NOT EXISTS (SELECT 1 FROM bookings b WHERE b.customer_id = c.id AND b.created_at >= $1)",
        now - Duration::days(365 * marketing_years)
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}
