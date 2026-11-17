//! Admin orders (`shop.orders`; refunds and returns `shop.refunds`; reads `shop.read`):
//! list with filters, detail with timeline, notes / tags, COD confirmation, offline payments,
//! fulfilments (create, steps, packing slip, pickup code check), cancellation (restock and
//! automatic refund of what was paid), order edits, draft orders (phone / WhatsApp sales),
//! email resend, returns and refunds.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::Staff,
    db::shop::{
        FulfillmentMethod, OrderCancelReason, OrderChannel, OrderStatus, ShopPaymentMethod,
    },
    error::{AppError, AppResult},
    modules::shop::{documents, public::shop_settings},
    services::{
        audit,
        shop::{
            cart::Address,
            checkout::{self, NewOrder},
            events::{self, ShopEvent},
            inventory::{self, ReserveLine},
            links,
            orders::{
                self, ActorRef, FulfillmentAction, ItemQuantity, OfflinePayment, RefundInput,
                ReturnAction, ReturnRequest,
            },
            quote::{self, Buyer, Fulfillment, WantedLine},
            webhooks,
        },
    },
    state::AppState,
    web::{
        client::ClientInfo,
        pagination::{PageParams, Paginated},
    },
};

fn invalid(message: &str) -> AppError {
    AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message)
}

/// Orders hold customer data: reading them needs `shop.orders` or `shop.refunds`, not only
/// `shop.read` (editors read the catalogue, never the orders).
fn require_order_access(staff: &Staff) -> AppResult<()> {
    if staff.can("shop.orders") || staff.can("shop.refunds") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

fn staff_actor(staff: &Staff, client: &ClientInfo) -> ActorRef {
    ActorRef::staff(client.actor(staff.actor()))
}

#[derive(Debug, Deserialize)]
pub struct OrderFilter {
    pub status: Option<String>,
    pub payment_status: Option<String>,
    pub fulfillment_status: Option<String>,
    pub channel: Option<String>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub tag: Option<String>,
    pub risk_level: Option<String>,
    pub q: Option<String>,
    /// Shortcut views: to_confirm | to_fulfil | ready_for_pickup
    pub view: Option<String>,
    /// placed_desc (default) | placed_asc | total_desc | total_asc
    pub sort: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    staff: Staff,
    Query(page): Query<PageParams>,
    Query(f): Query<OrderFilter>,
) -> AppResult<Json<Paginated<Value>>> {
    require_order_access(&staff)?;
    let q = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let rows = sqlx::query!(
        r#"SELECT o.id, o.number, o.status, o.payment_status, o.fulfillment_status, o.channel, o.payment_method, o.fulfillment_method,
                  o.total_minor, o.currency::text AS "currency!", o.placed_at, o.created_at, o.tags, o.risk_level,
                  o.requires_confirmation, o.confirmed_at, o.email::text AS "email!",
                  (c.first_name || COALESCE(' ' || c.last_name, '')) AS "customer!",
                  (SELECT COALESCE(sum(quantity), 0) FROM order_items WHERE order_id = o.id)::bigint AS "items!",
                  count(*) OVER () AS "total!"
           FROM orders o JOIN customers c ON c.id = o.customer_id
           WHERE ($1::text IS NULL OR o.status = $1) AND ($2::text IS NULL OR o.payment_status = $2)
             AND ($3::text IS NULL OR o.fulfillment_status = $3) AND ($4::text IS NULL OR o.channel = $4)
             AND ($5::date IS NULL OR COALESCE(o.placed_at, o.created_at) >= ($5::date::timestamp AT TIME ZONE 'Africa/Casablanca'))
             AND ($6::date IS NULL OR COALESCE(o.placed_at, o.created_at) < (($6::date + 1)::timestamp AT TIME ZONE 'Africa/Casablanca'))
             AND ($7::text IS NULL OR $7 = ANY(o.tags)) AND ($8::text IS NULL OR o.risk_level = $8)
             AND ($9::text IS NULL OR o.number ILIKE '%' || $9 || '%' OR o.email::text ILIKE '%' || $9 || '%'
                  OR (c.first_name || ' ' || COALESCE(c.last_name, '')) ILIKE '%' || $9 || '%'
                  OR regexp_replace(COALESCE(o.phone, ''), '\D', '', 'g') LIKE '%' || regexp_replace($9, '\D', '', 'g') || '%'
                     AND length(regexp_replace($9, '\D', '', 'g')) >= 4)
             AND (CASE $10
                    WHEN 'to_confirm' THEN o.requires_confirmation AND o.confirmed_at IS NULL AND o.status = 'pending'
                    WHEN 'to_fulfil' THEN o.status = 'open' AND o.fulfillment_status IN ('unfulfilled', 'partially_fulfilled')
                    WHEN 'ready_for_pickup' THEN EXISTS (SELECT 1 FROM fulfillments fu WHERE fu.order_id = o.id AND fu.status = 'ready_for_pickup')
                    WHEN 'drafts' THEN o.status = 'draft'
                    ELSE o.status <> 'draft' OR $1 = 'draft' END)
           ORDER BY CASE WHEN $11 = 'placed_asc' THEN COALESCE(o.placed_at, o.created_at) END ASC,
                    CASE WHEN $11 = 'total_desc' THEN o.total_minor END DESC,
                    CASE WHEN $11 = 'total_asc' THEN o.total_minor END ASC,
                    COALESCE(o.placed_at, o.created_at) DESC, o.id DESC
           LIMIT $12 OFFSET $13"#,
        f.status.as_deref(),
        f.payment_status.as_deref(),
        f.fulfillment_status.as_deref(),
        f.channel.as_deref(),
        f.from,
        f.to,
        f.tag.as_deref(),
        f.risk_level.as_deref(),
        q,
        f.view.as_deref().unwrap_or(""),
        f.sort.as_deref().unwrap_or(""),
        page.limit(),
        page.offset()
    )
    .fetch_all(&state.db)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let data = rows
        .into_iter()
        .map(|o| json!({ "id": o.id, "number": o.number, "status": o.status, "payment_status": o.payment_status,
                         "fulfillment_status": o.fulfillment_status, "channel": o.channel, "payment_method": o.payment_method,
                         "fulfillment_method": o.fulfillment_method, "total_minor": o.total_minor, "currency": o.currency,
                         "placed_at": o.placed_at, "created_at": o.created_at, "tags": o.tags, "risk_level": o.risk_level,
                         "to_confirm": o.requires_confirmation && o.confirmed_at.is_none(), "email": o.email,
                         "customer": o.customer, "item_count": o.items }))
        .collect();
    Ok(Json(Paginated::new(data, page, total)))
}

pub async fn detail_json(state: &AppState, id: Uuid, staff: &Staff) -> AppResult<Value> {
    let o = sqlx::query!(
        r#"SELECT o.*, o.currency::text AS "currency_code!", o.email::text AS "email_text!",
                  c.first_name, c.last_name, c.email::text AS customer_email, c.phone AS customer_phone, c.user_id,
                  (SELECT count(*) FROM orders x WHERE x.customer_id = o.customer_id AND x.status IN ('open', 'completed')) AS "customer_orders!",
                  b.reference AS "booking_reference?", u.full_name AS "confirmed_by_name?"
           FROM orders o JOIN customers c ON c.id = o.customer_id
           LEFT JOIN bookings b ON b.id = o.booking_id LEFT JOIN users u ON u.id = o.confirmed_by
           WHERE o.id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let s = shop_settings(state).await?;
    let mut view = super::super::public::order_view(state, id, s.withdrawal_days).await?;
    let costs = staff.can("shop.pricing");
    let items = sqlx::query!(
        r#"SELECT id, variant_id, product_id, sku, quantity, unit_price_minor, compare_at_price_minor, subtotal_minor, discount_minor,
                  tax_minor, total_minor, tax_lines, discount_allocations, product_snapshot, fulfilled_quantity, returned_quantity,
                  refunded_quantity,
                  (SELECT COALESCE(sum(quantity), 0) FROM inventory_reservations r WHERE r.order_item_id = oi.id
                   AND released_at IS NULL AND consumed_at IS NULL)::bigint AS "reserved!"
           FROM order_items oi WHERE order_id = $1 ORDER BY sort_order"#,
        id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|i| {
        let mut snap = i.product_snapshot.clone();
        if !costs && let Some(m) = snap.as_object_mut() {
            m.remove("cost_minor");
        }
        json!({ "id": i.id, "variant_id": i.variant_id, "product_id": i.product_id, "sku": i.sku, "quantity": i.quantity,
                "unit_price_minor": i.unit_price_minor, "compare_at_price_minor": i.compare_at_price_minor,
                "subtotal_minor": i.subtotal_minor, "discount_minor": i.discount_minor, "tax_minor": i.tax_minor,
                "total_minor": i.total_minor, "tax_lines": i.tax_lines, "discount_allocations": i.discount_allocations,
                "snapshot": snap, "fulfilled_quantity": i.fulfilled_quantity, "returned_quantity": i.returned_quantity,
                "refunded_quantity": i.refunded_quantity, "reserved": i.reserved })
    })
    .collect::<Vec<_>>();
    let fulfillments = sqlx::query!(
        r#"SELECT f.id, f.location_id, f.method, f.status, f.carrier, f.tracking_number, f.tracking_url, f.packed_at, f.shipped_at,
                  f.delivered_at, f.created_at, s.code::text AS "location!",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('order_item_id', fi.order_item_id, 'quantity', fi.quantity, 'lots', fi.lots))
                            FROM fulfillment_items fi WHERE fi.fulfillment_id = f.id), '[]') AS "items!"
           FROM fulfillments f JOIN stock_locations s ON s.id = f.location_id WHERE f.order_id = $1 ORDER BY f.created_at"#,
        id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|f| json!({ "id": f.id, "location_id": f.location_id, "location": f.location, "method": f.method, "status": f.status,
                     "carrier": f.carrier, "tracking_number": f.tracking_number, "tracking_url": f.tracking_url,
                     "packed_at": f.packed_at, "shipped_at": f.shipped_at, "delivered_at": f.delivered_at,
                     "created_at": f.created_at, "items": f.items }))
    .collect::<Vec<_>>();
    let payments = sqlx::query!(
        r#"SELECT p.id, p.provider, p.status, p.amount_minor, p.provider_payment_id, p.paid_at, p.created_at, p.failure_reason,
                  u.full_name AS "received_by?",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('id', r.id, 'amount_minor', r.amount_minor, 'status', r.status,
                            'reason', r.reason, 'created_at', r.created_at, 'lines', r.lines, 'failure_reason', r.failure_reason) ORDER BY r.created_at)
                            FROM refunds r WHERE r.payment_id = p.id), '[]') AS "refunds!"
           FROM payments p LEFT JOIN users u ON u.id = p.received_by WHERE p.order_id = $1 ORDER BY p.created_at"#,
        id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|p| json!({ "id": p.id, "provider": p.provider, "status": p.status, "amount_minor": p.amount_minor,
                     "reference": p.provider_payment_id, "paid_at": p.paid_at, "created_at": p.created_at,
                     "failure_reason": p.failure_reason, "received_by": p.received_by, "refunds": p.refunds }))
    .collect::<Vec<_>>();
    let timeline = sqlx::query!(
        r#"SELECT e.id, e.kind, e.from_status, e.to_status, e.message, e.data, e.actor_type, e.created_at, u.full_name AS "user?"
           FROM order_events e LEFT JOIN users u ON u.id = e.actor_user_id WHERE e.order_id = $1 ORDER BY e.created_at DESC, e.id DESC"#,
        id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|e| json!({ "id": e.id, "kind": e.kind, "from": e.from_status, "to": e.to_status, "message": e.message, "data": e.data,
                     "actor_type": e.actor_type, "user": e.user, "at": e.created_at }))
    .collect::<Vec<_>>();
    let returns = sqlx::query!(
        r#"SELECT r.id, r.number, r.status, r.reason, r.customer_note, r.staff_note, r.requested_at, r.received_at,
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('order_item_id', ri.order_item_id, 'quantity', ri.quantity,
                            'restock', ri.restock, 'condition', ri.condition)) FROM return_items ri WHERE ri.return_id = r.id), '[]') AS "items!"
           FROM order_returns r WHERE r.order_id = $1 ORDER BY r.requested_at"#,
        id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| json!({ "id": r.id, "number": r.number, "status": r.status, "reason": r.reason, "customer_note": r.customer_note,
                     "staff_note": r.staff_note, "requested_at": r.requested_at, "received_at": r.received_at, "items": r.items }))
    .collect::<Vec<_>>();
    let invoices = sqlx::query!(
        "SELECT number, kind, total_minor, issued_at FROM invoices WHERE order_id = $1 ORDER BY issued_at",
        id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|i| json!({ "number": i.number, "kind": i.kind, "total_minor": i.total_minor, "issued_at": i.issued_at }))
    .collect::<Vec<_>>();
    let refundable = o.amount_paid_minor
        - sqlx::query_scalar!(
            r#"SELECT COALESCE(sum(r.amount_minor), 0)::bigint AS "x!" FROM refunds r JOIN payments p ON p.id = r.payment_id
               WHERE p.order_id = $1 AND r.status <> 'failed'"#,
            id
        )
        .fetch_one(&state.db)
        .await?;
    if let Some(m) = view.as_object_mut() {
        m.insert("items".into(), json!(items));
        m.insert("fulfillments".into(), json!(fulfillments));
        m.insert("payments".into(), json!(payments));
        m.insert("timeline".into(), json!(timeline));
        m.insert("returns".into(), json!(returns));
        m.insert("invoices".into(), json!(invoices));
        m.insert("refundable_minor".into(), json!(refundable));
        m.insert("channel".into(), json!(o.channel));
        m.insert("tags".into(), json!(o.tags));
        m.insert("risk_level".into(), json!(o.risk_level));
        m.insert("internal_notes".into(), json!(o.internal_notes));
        m.insert("customer_note".into(), json!(o.customer_note));
        m.insert("billing_address".into(), json!(o.billing_address));
        m.insert("booking_reference".into(), json!(o.booking_reference));
        m.insert("confirmed_by".into(), json!(o.confirmed_by_name));
        m.insert(
            "ip_address".into(),
            json!(o.ip_address.map(|i| i.to_string())),
        );
        m.insert("promotion_codes".into(), json!(o.promotion_codes));
        m.insert(
            "customer".into(),
            json!({ "id": o.customer_id, "first_name": o.first_name, "last_name": o.last_name, "email": o.customer_email,
                    "phone": o.customer_phone, "has_account": o.user_id.is_some(), "orders_count": o.customer_orders }),
        );
        m.insert(
            "customer_url".into(),
            json!(links::order_url(
                &state.config.email.public_site_url,
                &o.locale,
                &sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
                    .fetch_one(&state.db)
                    .await?,
                &s.route_prefix,
                &o.number,
                &state.config.jwt_secret
            )),
        );
    }
    Ok(view)
}

pub async fn detail(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    require_order_access(&staff)?;
    Ok(Json(detail_json(&state, id, &staff).await?))
}

#[derive(Debug, Deserialize)]
pub struct NoteInput {
    pub message: Option<String>,
    pub tags: Option<Vec<String>>,
    pub internal_notes: Option<String>,
    pub risk_level: Option<String>,
}

/// Timeline comment, tags, internal notes, risk level.
pub async fn annotate(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<NoteInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    let actor = staff_actor(&staff, &client);
    let mut tx = state.db.begin().await?;
    orders::lock(&mut tx, id).await?.ok_or(AppError::NotFound)?;
    if let Some(m) = b
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        orders::log_event(&mut tx, id, "note_added", None, Some(m), json!({}), &actor).await?;
    }
    if let Some(tags) = &b.tags {
        let tags: Vec<String> = tags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .take(20)
            .collect();
        sqlx::query!("UPDATE orders SET tags = $2 WHERE id = $1", id, &tags)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(notes) = &b.internal_notes {
        sqlx::query!(
            "UPDATE orders SET internal_notes = $2 WHERE id = $1",
            id,
            notes
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(r) = &b.risk_level {
        if !["normal", "review", "high"].contains(&r.as_str()) {
            return Err(invalid("unknown risk level"));
        }
        sqlx::query!("UPDATE orders SET risk_level = $2 WHERE id = $1", id, r)
            .execute(&mut *tx)
            .await?;
        orders::log_event(
            &mut tx,
            id,
            "risk_changed",
            None,
            None,
            json!({ "risk_level": r }),
            &actor,
        )
        .await?;
    }
    audit::record(
        &mut tx,
        &actor.audit,
        "order.annotated",
        "order",
        &id.to_string(),
        json!({ "tags": b.tags, "risk_level": b.risk_level }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(detail_json(&state, id, &staff).await?))
}

pub async fn confirm(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    let mut tx = state.db.begin().await?;
    orders::confirm(&mut tx, id, &staff_actor(&staff, &client), Utc::now()).await?;
    tx.commit().await?;
    Ok(Json(detail_json(&state, id, &staff).await?))
}

pub async fn record_payment(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<OfflinePayment>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    let mut tx = state.db.begin().await?;
    orders::record_offline_payment(&mut tx, id, &b, &staff_actor(&staff, &client), Utc::now())
        .await?;
    tx.commit().await?;
    Ok(Json(detail_json(&state, id, &staff).await?))
}

#[derive(Debug, Deserialize)]
pub struct FulfillInput {
    pub location_id: Option<Uuid>,
    pub items: Option<Vec<ItemQuantity>>,
}

pub async fn create_fulfillment(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<FulfillInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    let mut tx = state.db.begin().await?;
    let secret = links::pickup_secret(&state.config.jwt_secret);
    let fid = orders::create_fulfillment(
        &mut tx,
        id,
        b.location_id,
        b.items.as_deref(),
        &secret,
        &staff_actor(&staff, &client),
        Utc::now(),
    )
    .await?;
    tx.commit().await?;
    let mut out = detail_json(&state, id, &staff).await?;
    out["created_fulfillment_id"] = json!(fid);
    Ok(Json(out))
}

pub async fn update_fulfillment(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(fid): Path<Uuid>,
    Json(action): Json<FulfillmentAction>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    let mut tx = state.db.begin().await?;
    orders::update_fulfillment(
        &mut tx,
        fid,
        &action,
        &staff_actor(&staff, &client),
        Utc::now(),
    )
    .await?;
    let order_id = sqlx::query_scalar!("SELECT order_id FROM fulfillments WHERE id = $1", fid)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(detail_json(&state, order_id, &staff).await?))
}

pub async fn packing_slip(
    State(state): State<AppState>,
    staff: Staff,
    Path(fid): Path<Uuid>,
) -> AppResult<Html<String>> {
    staff.require("shop.orders")?;
    Ok(Html(documents::packing_slip_html(&state, fid).await?))
}

pub async fn invoice(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    require_order_access(&staff)?;
    Ok(Html(documents::invoice_html(&state, id).await?))
}

#[derive(Debug, Deserialize)]
pub struct CancelInput {
    pub reason: OrderCancelReason,
    #[serde(default = "yes")]
    pub restock: bool,
    /// Refund what was paid (through the provider, or recorded for cash).
    #[serde(default = "yes")]
    pub refund: bool,
    pub message: Option<String>,
}

fn yes() -> bool {
    true
}

pub async fn cancel(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<CancelInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    if b.refund && !staff.can("shop.refunds") {
        return Err(AppError::Forbidden);
    }
    let actor = staff_actor(&staff, &client);
    let now = Utc::now();
    let mut tx = state.db.begin().await?;
    let outcome = orders::cancel(
        &mut tx,
        id,
        b.reason,
        "staff",
        b.restock,
        b.message.as_deref(),
        &actor,
        now,
    )
    .await?;
    let mut pending = Vec::new();
    if b.refund {
        let total: i64 = outcome.to_refund.iter().map(|(_, a)| *a).sum();
        if total > 0 {
            let (_, p) = orders::create_refund(
                &mut tx,
                id,
                &RefundInput {
                    amount_minor: Some(total),
                    reason: Some("order cancelled".into()),
                    ..RefundInput::default()
                },
                &actor,
                now,
            )
            .await?;
            pending = p;
        }
    }
    tx.commit().await?;
    for (provider, session) in &outcome.sessions_to_expire {
        if let Some(p) = state.shop_payments.for_payment(provider, Some(session)) {
            let _ = p.expire(session).await;
        }
    }
    webhooks::send_refunds(&state.db, &state.shop_payments, &pending).await?;
    let mut out = detail_json(&state, id, &staff).await?;
    out["cancel_outcome"] = json!({ "released": outcome.released, "restocked": outcome.restocked,
                                    "refunded_minor": outcome.to_refund.iter().map(|(_, a)| a).sum::<i64>() });
    Ok(Json(out))
}

pub async fn refund(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<RefundInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.refunds")?;
    let mut tx = state.db.begin().await?;
    let (_, pending) =
        orders::create_refund(&mut tx, id, &b, &staff_actor(&staff, &client), Utc::now()).await?;
    tx.commit().await?;
    webhooks::send_refunds(&state.db, &state.shop_payments, &pending).await?;
    Ok(Json(detail_json(&state, id, &staff).await?))
}

#[derive(Debug, Deserialize)]
pub struct ResendInput {
    pub template: String,
}

const RESENDABLE: [&str; 8] = [
    "order_confirmation",
    "order_payment_pending",
    "order_confirmed_cod",
    "order_shipped",
    "order_ready_for_pickup",
    "order_delivered",
    "order_cancelled",
    "refund_issued",
];

pub async fn resend(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<ResendInput>,
) -> AppResult<StatusCode> {
    staff.require("shop.orders")?;
    if !RESENDABLE.contains(&b.template.as_str()) {
        return Err(invalid("this email cannot be resent"));
    }
    let actor = staff_actor(&staff, &client);
    let mut tx = state.db.begin().await?;
    orders::lock(&mut tx, id).await?.ok_or(AppError::NotFound)?;
    // Templates that need a fulfilment / refund go through their event again.
    let event = match b.template.as_str() {
        "order_shipped" | "order_ready_for_pickup" | "order_delivered" => {
            let fid = sqlx::query_scalar!("SELECT id FROM fulfillments WHERE order_id = $1 AND status <> 'cancelled' ORDER BY created_at DESC LIMIT 1", id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| invalid("no fulfilment yet"))?;
            ShopEvent::FulfillmentUpdated {
                order_id: id,
                fulfillment_id: fid,
            }
        }
        "refund_issued" => {
            let rid = sqlx::query_scalar!(
                "SELECT r.id FROM refunds r JOIN payments p ON p.id = r.payment_id WHERE p.order_id = $1 AND r.status = 'succeeded' ORDER BY r.created_at DESC LIMIT 1",
                id
            )
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| invalid("no refund yet"))?;
            ShopEvent::RefundIssued {
                order_id: id,
                refund_id: rid,
            }
        }
        t => ShopEvent::EmailResent {
            order_id: id,
            template: t.to_string(),
        },
    };
    events::enqueue(&mut tx, event).await?;
    orders::log_event(
        &mut tx,
        id,
        "email_sent",
        None,
        Some(&b.template),
        json!({ "resent": true }),
        &actor,
    )
    .await?;
    audit::record(
        &mut tx,
        &actor.audit,
        "order.email_resent",
        "order",
        &id.to_string(),
        json!({ "template": b.template }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::ACCEPTED)
}

// ---- Returns -----------------------------------------------------------------------------------

pub async fn create_return(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<ReturnRequest>,
) -> AppResult<Json<Value>> {
    staff.require("shop.refunds")?;
    let mut tx = state.db.begin().await?;
    orders::request_return(
        &mut tx,
        id,
        &b,
        None,
        &staff_actor(&staff, &client),
        Utc::now(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(detail_json(&state, id, &staff).await?))
}

pub async fn update_return(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(rid): Path<Uuid>,
    Json(b): Json<ReturnAction>,
) -> AppResult<Json<Value>> {
    staff.require("shop.refunds")?;
    let mut tx = state.db.begin().await?;
    orders::update_return(&mut tx, rid, &b, &staff_actor(&staff, &client), Utc::now()).await?;
    let order_id = sqlx::query_scalar!("SELECT order_id FROM order_returns WHERE id = $1", rid)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(detail_json(&state, order_id, &staff).await?))
}

pub async fn returns(
    State(state): State<AppState>,
    staff: Staff,
    Query(page): Query<PageParams>,
) -> AppResult<Json<Paginated<Value>>> {
    require_order_access(&staff)?;
    let rows = sqlx::query!(
        r#"SELECT r.id, r.number, r.status, r.reason, r.requested_at, o.id AS order_id, o.number AS order_number,
                  (SELECT COALESCE(sum(quantity), 0) FROM return_items WHERE return_id = r.id)::bigint AS "items!", count(*) OVER () AS "total!"
           FROM order_returns r JOIN orders o ON o.id = r.order_id
           ORDER BY (r.status IN ('requested', 'approved')) DESC, r.requested_at DESC LIMIT $1 OFFSET $2"#,
        page.limit(),
        page.offset()
    )
    .fetch_all(&state.db)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let data = rows
        .into_iter()
        .map(|r| json!({ "id": r.id, "number": r.number, "status": r.status, "reason": r.reason, "requested_at": r.requested_at,
                         "order_id": r.order_id, "order_number": r.order_number, "item_count": r.items }))
        .collect();
    Ok(Json(Paginated::new(data, page, total)))
}

// ---- Order edits and draft orders ------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LineInput {
    pub variant_id: Uuid,
    pub quantity: i32,
}

#[derive(Debug, Deserialize)]
pub struct DraftInput {
    pub channel: OrderChannel,
    pub customer_id: Option<Uuid>,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub phone: Option<String>,
    pub locale: Option<String>,
    pub fulfillment_method: FulfillmentMethod,
    pub shipping_address: Option<Address>,
    pub pickup_location_id: Option<Uuid>,
    pub booking_id: Option<Uuid>,
    #[serde(default)]
    pub shipping_rate_ids: Vec<Uuid>,
    #[serde(default)]
    pub promotion_codes: Vec<String>,
    pub lines: Vec<LineInput>,
    pub note: Option<String>,
}

/// Re-prices `lines` for an order's (or draft's) fulfilment and codes.
#[allow(clippy::too_many_arguments)]
async fn price_lines(
    tx: &mut sqlx::PgConnection,
    state: &AppState,
    lines: &[(Uuid, Uuid, i32)],
    method: FulfillmentMethod,
    address: Option<&Address>,
    pickup: Option<Uuid>,
    rates: &[Uuid],
    codes: &[String],
    buyer: &Buyer,
    payment: Option<ShopPaymentMethod>,
    locale: &str,
) -> AppResult<quote::Loaded> {
    let s = shop_settings(state).await?;
    let fulfillment = Fulfillment {
        method: Some(method),
        destination: address.map(Address::destination).unwrap_or_default(),
        pickup_location_id: pickup,
        chosen_rate_ids: rates.to_vec(),
    };
    let wanted: Vec<WantedLine> = checkout::wanted_from(lines);
    Ok(quote::load(
        tx,
        &s,
        &wanted,
        &fulfillment,
        codes,
        buyer,
        payment,
        locale,
        Utc::now(),
    )
    .await?)
}

pub async fn create_draft(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Json(b): Json<DraftInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("shop.orders")?;
    if b.lines.is_empty() {
        return Err(invalid("add at least one product"));
    }
    let actor = staff_actor(&staff, &client);
    let s = shop_settings(&state).await?;
    let locale = match b.locale.clone() {
        Some(l) => l,
        None => {
            sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
                .fetch_one(&state.db)
                .await?
        }
    };
    let mut tx = state.db.begin().await?;
    let customer_id = match b.customer_id {
        Some(c) => c,
        None => {
            let first = b
                .first_name
                .clone()
                .or_else(|| b.shipping_address.as_ref().map(|a| a.first_name.clone()))
                .filter(|f| !f.trim().is_empty())
                .ok_or_else(|| invalid("the customer's first name is required"))?;
            checkout::find_or_create_customer(
                &mut tx,
                None,
                &b.email,
                &first,
                b.last_name.as_deref(),
                b.phone.as_deref(),
                b.shipping_address.as_ref().map(|a| a.country_code.as_str()),
                &locale,
            )
            .await?
        }
    };
    let lines: Vec<(Uuid, Uuid, i32)> = b
        .lines
        .iter()
        .map(|l| (Uuid::now_v7(), l.variant_id, l.quantity))
        .collect();
    let buyer = Buyer {
        customer_id: Some(customer_id),
        email: Some(b.email.clone()),
        booking_id: b.booking_id,
    };
    let loaded = price_lines(
        &mut tx,
        &state,
        &lines,
        b.fulfillment_method,
        b.shipping_address.as_ref(),
        b.pickup_location_id,
        &b.shipping_rate_ids,
        &b.promotion_codes,
        &buyer,
        None,
        &locale,
    )
    .await?;
    let q = loaded.price();
    let pu = |k: &str| state.media.public_url(k);
    let order = NewOrder {
        channel: b.channel,
        status: "draft",
        customer_id,
        cart_id: None,
        booking_id: b.booking_id,
        email: &b.email,
        phone: b.phone.as_deref(),
        locale: &locale,
        currency: &s.currency,
        method: b.fulfillment_method,
        shipping_address: b.shipping_address.as_ref(),
        billing_address: b.shipping_address.as_ref(),
        pickup_location_id: b.pickup_location_id,
        payment_method: None,
        requires_confirmation: false,
        codes: &b.promotion_codes,
        customer_note: b.note.as_deref(),
        checkout_key: None,
        created_by: Some(staff.id),
        terms_accepted: false,
        terms_revision_id: None,
        marketing_consent: false,
        relay_point: None,
        ip: None,
        user_agent: None,
    };
    let (id, number, _) =
        checkout::insert_order(&mut tx, &order, &q, &loaded.facts, &pu, Utc::now()).await?;
    orders::log_event(
        &mut tx,
        id,
        "draft_created",
        None,
        None,
        json!({ "issues": q.issues }),
        &actor,
    )
    .await?;
    audit::record(
        &mut tx,
        &actor.audit,
        "order.draft_created",
        "order",
        &id.to_string(),
        json!({ "number": number }),
    )
    .await?;
    tx.commit().await?;
    let mut out = detail_json(&state, id, &staff).await?;
    out["issues"] = json!(q.issues);
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Debug, Deserialize)]
pub struct EditInput {
    pub lines: Vec<LineInput>,
    pub note: Option<String>,
}

/// Edits the lines of a draft, an unpaid offline order or a cash-on-delivery order: re-priced
/// with its fulfilment and codes, stock reservations adjusted, logged as `edited`.
pub async fn edit(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<EditInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    if b.lines.is_empty() {
        return Err(invalid(
            "an order keeps at least one product: cancel it instead",
        ));
    }
    let actor = staff_actor(&staff, &client);
    let now = Utc::now();
    let mut tx = state.db.begin().await?;
    let o = orders::lock(&mut tx, id).await?.ok_or(AppError::NotFound)?;
    let editable = match o.status {
        OrderStatus::Draft => true,
        OrderStatus::Pending | OrderStatus::Open => {
            o.amount_paid_minor == 0
                && o.payment_method != Some(ShopPaymentMethod::Stripe)
                && o.fulfillment_status == "unfulfilled"
        }
        _ => false,
    };
    if !editable {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "order_not_editable",
            "only drafts and unpaid offline orders can be edited",
        ));
    }
    let row = sqlx::query!(
        r#"SELECT shipping_address, pickup_location_id, promotion_codes::text[] AS "codes!", booking_id, email::text AS "email!",
                  (SELECT COALESCE(array_agg((sl->>'rate_id')::uuid) FILTER (WHERE sl->>'rate_id' IS NOT NULL), '{}')
                   FROM jsonb_array_elements(shipping_lines) sl) AS "rates!"
           FROM orders WHERE id = $1"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    let address: Option<Address> = row
        .shipping_address
        .and_then(|a| serde_json::from_value(a).ok());
    // Keep the ids of the lines that stay (their history), new ids for new variants.
    let existing = sqlx::query!(
        "SELECT id, variant_id FROM order_items WHERE order_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    let lines: Vec<(Uuid, Uuid, i32)> = b
        .lines
        .iter()
        .map(|l| {
            let keep = existing
                .iter()
                .find(|e| e.variant_id == l.variant_id)
                .map_or_else(Uuid::now_v7, |e| e.id);
            (keep, l.variant_id, l.quantity)
        })
        .collect();
    inventory::release(&mut tx, id, None, now).await?;
    let buyer = Buyer {
        customer_id: Some(o.customer_id),
        email: Some(row.email.clone()),
        booking_id: row.booking_id,
    };
    let loaded = price_lines(
        &mut tx,
        &state,
        &lines,
        o.fulfillment_method,
        address.as_ref(),
        row.pickup_location_id,
        &row.rates,
        &row.codes,
        &buyer,
        o.payment_method,
        &o.locale,
    )
    .await?;
    let q = loaded.price();
    let blocking: Vec<_> = q
        .blocking_issues()
        .into_iter()
        .filter(|i| {
            !matches!(
                i.code,
                crate::services::shop::pricing::IssueCode::PaymentMethodUnavailable
            )
        })
        .collect();
    if !blocking.is_empty() {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "cart_has_issues",
            "the new lines cannot be sold as they are",
        )
        .with_details(json!({ "issues": blocking })));
    }
    let before = sqlx::query_scalar!(
        r#"SELECT COALESCE(jsonb_agg(jsonb_build_object('sku', sku, 'quantity', quantity, 'total_minor', total_minor)), '[]') AS "x!"
           FROM order_items WHERE order_id = $1"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    let keep_ids: Vec<Uuid> = lines.iter().map(|l| l.0).collect();
    sqlx::query!(
        "DELETE FROM inventory_reservations WHERE order_id = $1 AND NOT (order_item_id = ANY($2))",
        id,
        &keep_ids
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM order_items WHERE order_id = $1 AND NOT (id = ANY($2))",
        id,
        &keep_ids
    )
    .execute(&mut *tx)
    .await?;
    let pu = |k: &str| state.media.public_url(k);
    for (i, l) in q.lines.iter().enumerate() {
        let f = loaded.facts.get(&l.variant_id);
        sqlx::query!(
            "INSERT INTO order_items (id, order_id, variant_id, product_id, sku, quantity, unit_price_minor, compare_at_price_minor,
                                      subtotal_minor, discount_minor, tax_minor, total_minor, tax_lines, discount_allocations,
                                      product_snapshot, sort_order)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (id) DO UPDATE SET quantity = $6, unit_price_minor = $7, compare_at_price_minor = $8, subtotal_minor = $9,
                 discount_minor = $10, tax_minor = $11, total_minor = $12, tax_lines = $13, discount_allocations = $14,
                 product_snapshot = $15, sort_order = $16",
            l.line_id,
            id,
            l.variant_id,
            l.product_id,
            f.map(|f| f.sku.clone()).unwrap_or_default(),
            l.quantity,
            l.unit_price_minor,
            l.compare_at_price_minor,
            l.subtotal_minor,
            l.discount_minor,
            l.tax_minor,
            l.total_minor,
            serde_json::to_value(&l.tax_lines).unwrap_or_default(),
            serde_json::to_value(&l.discount_allocations).unwrap_or_default(),
            f.map(|f| checkout::snapshot(f, &pu)).unwrap_or_else(|| json!({})),
            i as i32
        )
        .execute(&mut *tx)
        .await?;
    }
    let t = &q.totals;
    sqlx::query!(
        "UPDATE orders SET subtotal_minor = $2, discount_minor = $3, shipping_minor = $4, shipping_discount_minor = $5, tax_minor = $6,
                cod_fee_minor = $7, total_minor = $8 WHERE id = $1",
        id,
        t.subtotal_minor,
        t.discount_minor,
        t.shipping_minor,
        t.shipping_discount_minor,
        t.tax_minor,
        t.cod_fee_minor,
        t.total_minor
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE payments SET amount_minor = $2::bigint WHERE order_id = $1 AND status = 'pending' AND $2::bigint > 0",
        id,
        t.total_minor
    )
    .execute(&mut *tx)
    .await?;
    if o.status != OrderStatus::Draft {
        let s = shop_settings(&state).await?;
        let expires = match o.payment_method {
            Some(ShopPaymentMethod::CashOnDelivery) => None,
            _ => Some(now + chrono::Duration::days(s.payments.bank_transfer_hold_days)),
        };
        let reserve: Vec<ReserveLine> = q
            .lines
            .iter()
            .map(|l| ReserveLine {
                order_item_id: l.line_id,
                variant_id: l.variant_id,
                quantity: l.quantity,
            })
            .collect();
        inventory::reserve(&mut tx, id, &reserve, row.pickup_location_id, expires).await?;
    }
    let after = json!(q.lines.iter().map(|l| json!({ "variant_id": l.variant_id, "quantity": l.quantity, "total_minor": l.total_minor })).collect::<Vec<_>>());
    orders::log_event(
        &mut tx,
        id,
        "edited",
        None,
        b.note.as_deref(),
        json!({ "before": before, "after": after, "total_minor": t.total_minor }),
        &actor,
    )
    .await?;
    orders::recompute(&mut tx, id, now, &actor).await?;
    audit::record(
        &mut tx,
        &actor.audit,
        "order.edited",
        "order",
        &id.to_string(),
        json!({ "before": before, "after": after }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(detail_json(&state, id, &staff).await?))
}

#[derive(Debug, Deserialize)]
pub struct FinalizeInput {
    /// `payment_link` (Stripe Checkout sent by email), or an offline method recorded now /
    /// expected later (`cash_on_delivery`, `pay_at_pickup`, `bank_transfer`).
    pub mode: String,
    /// Offline: money already received (recorded as a payment).
    #[serde(default)]
    pub paid: bool,
    pub provider: Option<String>,
}

/// Turns a draft into a real order: stock reserved, then a Stripe payment link emailed, or the
/// offline payment recorded / expected.
pub async fn finalize(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(b): Json<FinalizeInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.orders")?;
    let actor = staff_actor(&staff, &client);
    let now = Utc::now();
    let s = shop_settings(&state).await?;
    let method = match b.mode.as_str() {
        "payment_link" => ShopPaymentMethod::Stripe,
        "cash_on_delivery" => ShopPaymentMethod::CashOnDelivery,
        "pay_at_pickup" => ShopPaymentMethod::PayAtPickup,
        "bank_transfer" => ShopPaymentMethod::BankTransfer,
        _ => return Err(invalid("unknown mode")),
    };
    if method == ShopPaymentMethod::Stripe && !state.shop_payments.card_available() {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "payment_unavailable",
            "card payment is not configured",
        ));
    }
    let mut tx = state.db.begin().await?;
    let o = orders::lock(&mut tx, id).await?.ok_or(AppError::NotFound)?;
    if o.status != OrderStatus::Draft {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "not_a_draft",
            "this order is already placed",
        ));
    }
    let items: Vec<ReserveLine> = sqlx::query!(
        "SELECT id, variant_id, quantity FROM order_items WHERE order_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| ReserveLine {
        order_item_id: r.id,
        variant_id: r.variant_id,
        quantity: r.quantity,
    })
    .collect();
    let hold = match method {
        ShopPaymentMethod::Stripe => Some(now + chrono::Duration::hours(48)),
        ShopPaymentMethod::CashOnDelivery => None,
        _ => Some(now + chrono::Duration::days(s.payments.bank_transfer_hold_days)),
    };
    inventory::reserve(&mut tx, id, &items, o.pickup_location_id, hold).await?;
    let open_now = matches!(
        method,
        ShopPaymentMethod::PayAtPickup | ShopPaymentMethod::CashOnDelivery
    ) || b.paid;
    sqlx::query!(
        "UPDATE orders SET status = $2, payment_method = $3, placed_at = $4::timestamptz,
                confirmed_at = CASE WHEN $5 THEN $4::timestamptz END,
                confirmed_by = CASE WHEN $5 THEN $6::uuid END, requires_confirmation = $5
         WHERE id = $1",
        id,
        if open_now { "open" } else { "pending" },
        method.as_str(),
        now,
        method == ShopPaymentMethod::CashOnDelivery,
        staff.id
    )
    .execute(&mut *tx)
    .await?;
    let payment_id = Uuid::now_v7();
    let provider = match method {
        ShopPaymentMethod::Stripe => state.shop_payments.card().map_or("stripe", |p| p.name()),
        m => m.as_str(),
    };
    if o.total_minor > 0 {
        sqlx::query!(
            "INSERT INTO payments (id, order_id, provider, purpose, status, amount_minor, currency, idempotency_key, legal_entity_id)
             VALUES ($1, $2, $3, 'full', 'pending', $4, $5, $6, (SELECT legal_entity_id FROM orders WHERE id = $2))",
            payment_id,
            id,
            provider,
            o.total_minor,
            o.currency,
            format!("order-{id}")
        )
        .execute(&mut *tx)
        .await?;
    }
    orders::log_event(
        &mut tx,
        id,
        "placed",
        Some(("draft", if open_now { "open" } else { "pending" })),
        None,
        json!({ "mode": b.mode }),
        &actor,
    )
    .await?;
    if b.paid && method != ShopPaymentMethod::Stripe && o.total_minor > 0 {
        orders::record_offline_payment(
            &mut tx,
            id,
            &OfflinePayment {
                provider: b.provider.clone().unwrap_or_else(|| "cash".into()),
                amount_minor: o.total_minor,
                reference: None,
                note: Some("paid when the order was taken".into()),
            },
            &actor,
            now,
        )
        .await?;
    }
    orders::recompute(&mut tx, id, now, &actor).await?;
    audit::record(
        &mut tx,
        &actor.audit,
        "order.finalized",
        "order",
        &id.to_string(),
        json!({ "mode": b.mode }),
    )
    .await?;
    if method != ShopPaymentMethod::Stripe {
        events::enqueue(&mut tx, ShopEvent::OrderPlaced { order_id: id }).await?;
    }
    tx.commit().await?;

    if method == ShopPaymentMethod::Stripe && o.total_minor > 0 {
        // A payment link: the Checkout Session URL, emailed through the payment-pending email.
        let detail = detail_json(&state, id, &staff).await?;
        let provider = state
            .shop_payments
            .card()
            .ok_or_else(|| invalid("card payment is not configured"))?;
        let default_locale: String =
            sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
                .fetch_one(&state.db)
                .await?;
        let order_url = links::order_url(
            &state.config.email.public_site_url,
            &o.locale,
            &default_locale,
            &s.route_prefix,
            &o.number,
            &state.config.jwt_secret,
        );
        let lines = detail["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|i| crate::services::shop::payments::CheckoutLine {
                        name: i["snapshot"]["name"]
                            .as_str()
                            .unwrap_or("Article")
                            .to_string(),
                        unit_amount_minor: i["unit_price_minor"].as_i64().unwrap_or(0),
                        quantity: i["quantity"].as_i64().unwrap_or(1) as i32,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let t = &detail["totals"];
        let shipping_net = t["shipping_minor"].as_i64().unwrap_or(0)
            - t["shipping_discount_minor"].as_i64().unwrap_or(0);
        let req = crate::services::shop::payments::CheckoutRequest {
            order_id: id,
            payment_id,
            order_number: o.number.clone(),
            email: detail["email"].as_str().unwrap_or_default().to_string(),
            locale: o.locale.clone(),
            currency: o.currency.clone(),
            lines,
            shipping: (shipping_net > 0).then(|| ("Livraison".to_string(), shipping_net)),
            discount_minor: t["discount_minor"].as_i64().unwrap_or(0),
            exclusive_tax_minor: if t["taxes_included"].as_bool().unwrap_or(true) {
                0
            } else {
                t["tax_minor"].as_i64().unwrap_or(0)
            },
            total_minor: o.total_minor,
            idempotency_key: format!("order-{id}"),
            success_url: format!("{order_url}&paid=1"),
            cancel_url: format!("{order_url}&cancelled=1"),
            // Stripe sessions live at most 24 h: the link is sent again if it lapses.
            expires_at: now + chrono::Duration::hours(23),
            statement_descriptor: Some(s.payments.statement_descriptor.clone()),
            embedded: false,
        };
        if req.computed_total() != o.total_minor {
            return Err(AppError::Internal(anyhow::anyhow!(
                "payment link total differs from the order"
            )));
        }
        let session = provider.create_checkout(&req).await.map_err(|e| {
            AppError::domain(
                StatusCode::BAD_GATEWAY,
                "payment_unavailable",
                "the payment link could not be created",
            )
            .with_details(json!({ "message": e.to_string() }))
        })?;
        let mut tx = state.db.begin().await?;
        sqlx::query!(
            "UPDATE payments SET provider_session_id = $2, provider_payload = $3 WHERE id = $1",
            payment_id,
            session.session_id,
            json!({ "url": session.url, "session_id": session.session_id })
        )
        .execute(&mut *tx)
        .await?;
        events::enqueue(
            &mut tx,
            ShopEvent::EmailResent {
                order_id: id,
                template: "order_payment_pending".into(),
            },
        )
        .await?;
        orders::log_event(
            &mut tx,
            id,
            "payment_link_sent",
            None,
            None,
            json!({ "url": session.url }),
            &actor,
        )
        .await?;
        tx.commit().await?;
    }
    Ok(Json(detail_json(&state, id, &staff).await?))
}

pub async fn delete_draft(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("shop.orders")?;
    let mut tx = state.db.begin().await?;
    let o = orders::lock(&mut tx, id).await?.ok_or(AppError::NotFound)?;
    if o.status != OrderStatus::Draft {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "not_a_draft",
            "only drafts are deleted; cancel placed orders",
        ));
    }
    sqlx::query!("DELETE FROM orders WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &staff_actor(&staff, &client).audit,
        "order.draft_deleted",
        "order",
        &id.to_string(),
        json!({ "number": o.number }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
