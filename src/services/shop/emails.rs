//! Shop outbox handlers: customer and staff emails, and the invoice of a paid order.
//!
//! | event                        | customer email                                  | staff email          |
//! |------------------------------|-------------------------------------------------|----------------------|
//! | order.placed (offline pay)   | order_payment_pending (transfer) / confirmation | staff_new_order      |
//! | order.paid (card)            | order_confirmation                              | staff_new_order      |
//! | order.confirmed (COD)        | order_confirmed_cod                             |                      |
//! | order.fulfillment_updated    | order_shipped / order_ready_for_pickup / order_delivered |             |
//! | order.cancelled              | order_cancelled (not for abandoned card payments) |                    |
//! | order.refund_issued          | refund_issued                                   |                      |
//! | order.return_updated         | return_update                                   |                      |
//! | order.payment_issue          |                                                 | staff_payment_issue  |
//! | order.email_resent           | the template asked                              |                      |
//! | stock.low                    |                                                 | staff_low_stock      |
//! | stock.back_in_stock          | back_in_stock (each waiting address)            |                      |
//! | shop_cart.abandoned          | abandoned_cart                                  |                      |
//!
//! Staff emails go to `shop.notify_emails`, else `notifications.staff_email`. Each email is sent
//! at most once per (event, recipient, template) by the mailer's `email_log` claim.

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    events::{OrderPayload, StockPayload, types},
    links,
    settings::{self, ShopSettings},
};
use crate::{
    db::operations::OutboxEvent,
    services::email::{EmailJob, Mailer, Source, context},
    worker::{HandlerError, OutboxHandler},
};

pub struct ShopEmailHandler {
    pool: PgPool,
    mailer: Mailer,
    secret: String,
}

impl ShopEmailHandler {
    pub fn new(pool: PgPool, mailer: Mailer, secret: String) -> Self {
        Self {
            pool,
            mailer,
            secret,
        }
    }
}

const EVENTS: &[&str] = &[
    types::ORDER_PLACED,
    types::ORDER_PAID,
    types::ORDER_CONFIRMED,
    types::ORDER_CANCELLED,
    types::ORDER_FULFILLMENT_UPDATED,
    types::ORDER_REFUND_ISSUED,
    types::ORDER_RETURN_UPDATED,
    types::ORDER_PAYMENT_ISSUE,
    types::ORDER_EMAIL_RESENT,
    types::STOCK_LOW,
    types::STOCK_BACK_IN_STOCK,
    types::SHOP_CART_ABANDONED,
];

fn retry<E: Into<anyhow::Error>>(e: E) -> HandlerError {
    HandlerError::Retry(e.into())
}

async fn default_locale(pool: &PgPool) -> Result<String, HandlerError> {
    Ok(
        sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
            .fetch_optional(pool)
            .await
            .map_err(retry)?
            .unwrap_or_else(|| "fr".into()),
    )
}

fn labels(locale: &str) -> Value {
    match locale {
        "fr" => {
            json!({ "details": "Commande", "discount": "Remise", "shipping": "Livraison", "cod_fee": "Frais de paiement à la livraison", "total": "Total", "tax_included": "dont TVA" })
        }
        "ar" => {
            json!({ "details": "الطلب", "discount": "الخصم", "shipping": "التوصيل", "cod_fee": "رسوم الدفع عند الاستلام", "total": "المجموع", "tax_included": "منها الضريبة" })
        }
        _ => {
            json!({ "details": "Order", "discount": "Discount", "shipping": "Shipping", "cod_fee": "Cash on delivery fee", "total": "Total", "tax_included": "incl. VAT" })
        }
    }
}

fn return_label(status: &str, locale: &str) -> &'static str {
    match (locale, status) {
        ("fr", "requested") => "demandé",
        ("fr", "approved") => "accepté",
        ("fr", "rejected") => "refusé",
        ("fr", "received") => "reçu",
        ("fr", _) => "clôturé",
        ("ar", "requested") => "مطلوب",
        ("ar", "approved") => "مقبول",
        ("ar", "rejected") => "مرفوض",
        ("ar", "received") => "تم الاستلام",
        ("ar", _) => "مغلق",
        (_, "requested") => "requested",
        (_, "approved") => "approved",
        (_, "rejected") => "rejected",
        (_, "received") => "received",
        _ => "closed",
    }
}

struct OrderFacts {
    email: String,
    locale: String,
    first_name: String,
    vars: serde_json::Map<String, Value>,
    payment_method: Option<String>,
    status: String,
    cancel_reason: Option<String>,
    amount_paid: i64,
    number: String,
    terms_revision_id: Option<Uuid>,
    seller: Value,
}

impl ShopEmailHandler {
    async fn order(
        &self,
        order_id: Uuid,
        settings: &ShopSettings,
    ) -> Result<Option<OrderFacts>, HandlerError> {
        let Some(o) = sqlx::query!(
            r#"SELECT o.number, o.email::text AS "email!", o.phone, o.locale, o.status, o.payment_method, o.fulfillment_method,
                      o.requires_confirmation, o.subtotal_minor, o.discount_minor, o.shipping_minor, o.shipping_discount_minor,
                      o.cod_fee_minor, o.tax_minor, o.total_minor, o.amount_paid_minor, o.currency::text AS "currency!",
                      o.cancel_reason, o.placed_at, c.first_name, c.last_name, o.legal_entity_id, o.terms_page_revision_id,
                      o.withdrawal_deadline,
                      COALESCE((SELECT jsonb_agg(jsonb_build_object('name', product_snapshot->>'name',
                                 'options', COALESCE(product_snapshot->>'options_label', ''), 'quantity', quantity,
                                 'total_minor', total_minor) ORDER BY sort_order) FROM order_items WHERE order_id = o.id), '[]') AS "lines!",
                      (SELECT max(expires_at) FROM inventory_reservations r WHERE r.order_id = o.id) AS hold_until
               FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.id = $1"#,
            order_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(retry)?
        else {
            return Ok(None);
        };
        let default_locale = default_locale(&self.pool).await?;
        let url = links::order_url(
            &self.mailer.settings().public_site_url,
            &o.locale,
            &default_locale,
            &settings.route_prefix,
            &o.number,
            &self.secret,
        );
        let mut vars = serde_json::Map::new();
        vars.insert(
            "customer".into(),
            json!({ "first_name": o.first_name, "full_name": format!("{} {}", o.first_name, o.last_name.unwrap_or_default()).trim() }),
        );
        vars.insert(
            "order".into(),
            json!({
                "number": o.number, "url": url, "status": o.status, "email": o.email, "phone": o.phone,
                "payment_method": o.payment_method, "fulfillment_method": o.fulfillment_method,
                "requires_confirmation": o.requires_confirmation,
                "subtotal_minor": o.subtotal_minor, "discount_minor": o.discount_minor, "shipping_minor": o.shipping_minor,
                "shipping_discount_minor": o.shipping_discount_minor, "cod_fee_minor": o.cod_fee_minor,
                "tax_minor": o.tax_minor, "total_minor": o.total_minor, "amount_paid_minor": o.amount_paid_minor,
                "currency": o.currency,
                "hold_until": o.hold_until.or(o.placed_at).map(|d| d.to_rfc3339()).unwrap_or_else(|| Utc::now().to_rfc3339()),
                "labels": labels(&o.locale),
            }),
        );
        vars.insert("lines".into(), o.lines);
        vars.insert(
            "bank_transfer_details".into(),
            json!(ShopSettings::localized(
                &settings.payments.bank_transfer_details,
                &o.locale
            )),
        );
        let payment_url = sqlx::query_scalar!(
            "SELECT provider_payload->>'url' FROM payments WHERE order_id = $1 AND status = 'pending' ORDER BY created_at DESC LIMIT 1",
            order_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(retry)?
        .flatten();
        vars.insert("payment_url".into(), json!(payment_url));
        let mut conn = self.pool.acquire().await.map_err(retry)?;
        let seller = super::invoices::seller_snapshot(&mut conn, o.legal_entity_id)
            .await
            .map_err(retry)?;
        for (k, v) in super::legal::email_vars(settings, &seller, o.withdrawal_deadline, None) {
            vars.insert(k.into(), v);
        }
        vars.insert(
            "admin_url".into(),
            json!(format!(
                "{}/admin/shop/orders/{order_id}",
                self.mailer.settings().public_site_url.trim_end_matches('/')
            )),
        );
        Ok(Some(OrderFacts {
            email: o.email,
            locale: o.locale,
            first_name: o.first_name,
            vars,
            payment_method: o.payment_method,
            status: o.status,
            cancel_reason: o.cancel_reason,
            amount_paid: o.amount_paid_minor,
            number: o.number,
            terms_revision_id: o.terms_page_revision_id,
            seller,
        }))
    }

    fn customer_job(&self, template: &str, f: &OrderFacts, extra: &[(&str, Value)]) -> EmailJob {
        let mut vars = f.vars.clone();
        for (k, v) in extra {
            vars.insert((*k).to_string(), v.clone());
        }
        EmailJob {
            template: template.into(),
            to_email: f.email.clone(),
            to_name: Some(f.first_name.clone()),
            locales: vec![Some(f.locale.clone())],
            booking_id: None,
            source: Source::Data(Value::Object(vars)),
        }
    }

    /// The order confirmation, with the accepted terms of sale and the withdrawal form attached
    /// (durable medium, Consumer Code L221-13).
    async fn confirmation_job(&self, f: &OrderFacts) -> Result<EmailJob, HandlerError> {
        let mut attachments = Vec::new();
        if let Some(rev) = f.terms_revision_id {
            let mut conn = self.pool.acquire().await.map_err(retry)?;
            if let Some(a) = super::legal::terms_pdf(&mut conn, rev, &f.seller)
                .await
                .map_err(retry)?
            {
                attachments.push(a);
            }
        }
        attachments.push(super::legal::withdrawal_form_pdf(&f.seller, &f.number));
        let mut job = self.customer_job("order_confirmation", f, &[]);
        job.source = Source::Shop {
            vars: Value::Object(f.vars.clone()),
            attachments,
        };
        Ok(job)
    }

    async fn staff_jobs(
        &self,
        template: &str,
        settings: &ShopSettings,
        vars: Value,
    ) -> Result<Vec<EmailJob>, HandlerError> {
        let mut to: Vec<String> = settings
            .notify_emails
            .iter()
            .filter(|e| e.contains('@'))
            .cloned()
            .collect();
        if to.is_empty()
            && let Some(e) = context::staff_email(&self.pool).await.map_err(retry)?
        {
            to.push(e);
        }
        Ok(to
            .into_iter()
            .map(|email| EmailJob {
                template: template.into(),
                to_email: email,
                to_name: None,
                locales: vec![],
                booking_id: None,
                source: Source::Data(vars.clone()),
            })
            .collect())
    }

    async fn jobs(&self, event: &OutboxEvent) -> Result<Vec<EmailJob>, HandlerError> {
        let settings = settings::load(&self.pool).await.map_err(retry)?;
        let t = event.event_type.as_str();
        if t == types::STOCK_LOW || t == types::STOCK_BACK_IN_STOCK {
            return self.stock_jobs(event, &settings).await;
        }
        if t == types::SHOP_CART_ABANDONED {
            return self.cart_jobs(event).await;
        }
        let p: OrderPayload = serde_json::from_value(event.payload.clone())
            .map_err(|e| HandlerError::Permanent(anyhow::anyhow!("invalid payload: {e}")))?;
        let Some(f) = self.order(p.order_id, &settings).await? else {
            return Ok(vec![]);
        };
        let staff_vars = Value::Object(f.vars.clone());
        let mut jobs = Vec::new();
        match t {
            types::ORDER_PLACED => {
                match f.payment_method.as_deref() {
                    Some("stripe") => {} // emails when paid
                    Some("bank_transfer") => {
                        jobs.push(self.customer_job("order_payment_pending", &f, &[]))
                    }
                    _ => jobs.push(self.confirmation_job(&f).await?),
                }
                if f.payment_method.as_deref() != Some("stripe") {
                    jobs.extend(
                        self.staff_jobs("staff_new_order", &settings, staff_vars)
                            .await?,
                    );
                }
            }
            types::ORDER_PAID => {
                // Card and transfer orders are confirmed once paid (the others when placed).
                if matches!(
                    f.payment_method.as_deref(),
                    Some("stripe" | "bank_transfer")
                ) {
                    jobs.push(self.confirmation_job(&f).await?);
                }
                if f.payment_method.as_deref() == Some("stripe") {
                    jobs.extend(
                        self.staff_jobs("staff_new_order", &settings, staff_vars)
                            .await?,
                    );
                }
            }
            types::ORDER_CONFIRMED => jobs.push(self.customer_job("order_confirmed_cod", &f, &[])),
            types::ORDER_CANCELLED => {
                let abandoned_card = f.payment_method.as_deref() == Some("stripe")
                    && f.cancel_reason.as_deref() == Some("payment_failed")
                    && f.amount_paid == 0;
                if f.status == "cancelled" && !abandoned_card {
                    jobs.push(self.customer_job("order_cancelled", &f, &[]));
                }
            }
            types::ORDER_FULFILLMENT_UPDATED => {
                let Some(fid) = p.fulfillment_id else {
                    return Ok(vec![]);
                };
                let row = sqlx::query!(
                    r#"SELECT f.status, f.method, f.carrier, f.tracking_number, f.tracking_url,
                              COALESCE(t.name, d.name, s.code::text) AS "location!", COALESCE(t.pickup_instructions, d.pickup_instructions) AS instructions
                       FROM fulfillments f JOIN stock_locations s ON s.id = f.location_id
                       LEFT JOIN stock_location_translations t ON t.location_id = s.id AND t.locale = $2
                       LEFT JOIN stock_location_translations d ON d.location_id = s.id AND d.locale = (SELECT code FROM locales WHERE is_default)
                       WHERE f.id = $1"#,
                    fid,
                    f.locale
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(retry)?;
                let Some(r) = row else { return Ok(vec![]) };
                let code = super::orders::pickup_code(&links::pickup_secret(&self.secret), fid);
                let fulfillment = json!({
                    "carrier": r.carrier, "tracking_number": r.tracking_number, "tracking_url": r.tracking_url,
                    "location_name": r.location, "pickup_instructions": r.instructions,
                    "pickup_code": if r.method == "pickup" { code } else { String::new() },
                });
                let template = match r.status.as_str() {
                    "shipped" if r.method == "shipping" => Some("order_shipped"),
                    "ready_for_pickup" => Some("order_ready_for_pickup"),
                    "delivered" | "picked_up" => Some("order_delivered"),
                    _ => None,
                };
                if let Some(t) = template {
                    jobs.push(self.customer_job(t, &f, &[("fulfillment", fulfillment)]));
                }
            }
            types::ORDER_REFUND_ISSUED => {
                let Some(rid) = p.refund_id else {
                    return Ok(vec![]);
                };
                let r = sqlx::query!(
                    r#"SELECT r.amount_minor, p.currency::text AS "currency!" FROM refunds r JOIN payments p ON p.id = r.payment_id WHERE r.id = $1"#,
                    rid
                )
                .fetch_one(&self.pool)
                .await
                .map_err(retry)?;
                jobs.push(self.customer_job(
                    "refund_issued",
                    &f,
                    &[(
                        "refund",
                        json!({ "amount_minor": r.amount_minor, "currency": r.currency }),
                    )],
                ));
            }
            types::ORDER_RETURN_UPDATED => {
                let Some(rid) = p.return_id else {
                    return Ok(vec![]);
                };
                let r = sqlx::query!(
                    "SELECT number, status, staff_note, reason, requested_at, refund_due_by FROM order_returns WHERE id = $1",
                    rid
                )
                .fetch_one(&self.pool)
                .await
                .map_err(retry)?;
                if r.reason == "withdrawal" && r.status == "requested" {
                    // Acknowledgment of the withdrawal on a durable medium (L221-21).
                    let mut extra = vec![(
                        "order_return",
                        json!({ "number": r.number, "status": r.status, "requested_at": r.requested_at.to_rfc3339() }),
                    )];
                    let [_, (k, v), _] =
                        super::legal::email_vars(&settings, &f.seller, None, r.refund_due_by);
                    extra.push((k, v));
                    jobs.push(self.customer_job("withdrawal_acknowledgment", &f, &extra));
                    return Ok(jobs);
                }
                jobs.push(self.customer_job(
                    "return_update",
                    &f,
                    &[(
                        "order_return",
                        json!({ "number": r.number, "status": r.status, "status_label": return_label(&r.status, &f.locale), "staff_note": r.staff_note }),
                    )],
                ));
            }
            types::ORDER_PAYMENT_ISSUE => {
                let mut vars = f.vars.clone();
                vars.insert("issue".into(), json!(p.issue.unwrap_or_default()));
                jobs.extend(
                    self.staff_jobs("staff_payment_issue", &settings, Value::Object(vars))
                        .await?,
                );
            }
            types::ORDER_EMAIL_RESENT => {
                if let Some(template) = p.template {
                    if template == "order_confirmation" {
                        jobs.push(self.confirmation_job(&f).await?);
                    } else {
                        jobs.push(self.customer_job(&template, &f, &[]));
                    }
                }
            }
            _ => {}
        }
        Ok(jobs)
    }

    async fn stock_jobs(
        &self,
        event: &OutboxEvent,
        settings: &ShopSettings,
    ) -> Result<Vec<EmailJob>, HandlerError> {
        let p: StockPayload = serde_json::from_value(event.payload.clone())
            .map_err(|e| HandlerError::Permanent(anyhow::anyhow!("invalid payload: {e}")))?;
        let Some(variant_id) = p.variant_id else {
            return Ok(vec![]);
        };
        let site = self
            .mailer
            .settings()
            .public_site_url
            .trim_end_matches('/')
            .to_string();
        if event.event_type == types::STOCK_LOW {
            let r = sqlx::query!(
                r#"SELECT v.sku::text AS "sku!", COALESCE(t.name, v.sku::text) AS "name!",
                          COALESCE(st.name, s.code::text) AS "location!",
                          (l.on_hand - l.reserved) AS "available!",
                          COALESCE(v.low_stock_threshold, $3) AS "threshold!"
                   FROM product_variants v
                   JOIN inventory_levels l ON l.variant_id = v.id AND l.location_id = $2
                   JOIN stock_locations s ON s.id = l.location_id
                   LEFT JOIN stock_location_translations st ON st.location_id = s.id AND st.locale = (SELECT code FROM locales WHERE is_default)
                   LEFT JOIN product_translations t ON t.product_id = v.product_id AND t.locale = (SELECT code FROM locales WHERE is_default)
                   WHERE v.id = $1"#,
                variant_id,
                p.location_id,
                i32::try_from(settings.low_stock_threshold).unwrap_or(5)
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(retry)?;
            let Some(r) = r else { return Ok(vec![]) };
            return self
                .staff_jobs(
                    "staff_low_stock",
                    settings,
                    json!({ "stock": { "sku": r.sku, "name": r.name, "location": r.location, "available": r.available, "threshold": r.threshold },
                            "admin_url": format!("{site}/admin/shop/inventory?variant={variant_id}") }),
                )
                .await;
        }
        let waiting = sqlx::query!(
            r#"SELECT n.id, n.email::text AS "email!", n.locale, COALESCE(t.name, d.name, v.sku::text) AS "name!",
                      COALESCE(t.slug, d.slug) AS slug,
                      (SELECT string_agg(COALESCE(vt.label, ov.code::text), ' · ' ORDER BY o.sort_order)
                       FROM product_variant_option_values pvo JOIN product_options o ON o.id = pvo.option_id
                       JOIN product_option_values ov ON ov.id = pvo.option_value_id
                       LEFT JOIN product_option_value_translations vt ON vt.option_value_id = ov.id AND vt.locale = n.locale
                       WHERE pvo.variant_id = v.id) AS variant_label
               FROM stock_notifications n JOIN product_variants v ON v.id = n.variant_id
               LEFT JOIN product_translations t ON t.product_id = v.product_id AND t.locale = n.locale
               LEFT JOIN product_translations d ON d.product_id = v.product_id AND d.locale = (SELECT code FROM locales WHERE is_default)
               WHERE n.variant_id = $1 AND n.notified_at IS NULL ORDER BY n.created_at LIMIT 500"#,
            variant_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(retry)?;
        let default_locale = default_locale(&self.pool).await?;
        Ok(waiting
            .into_iter()
            .map(|w| EmailJob {
                template: "back_in_stock".into(),
                to_email: w.email,
                to_name: None,
                locales: vec![Some(w.locale.clone())],
                booking_id: None,
                source: Source::Data(json!({ "product": {
                    "name": w.name, "variant": w.variant_label,
                    "url": format!("{}?variant={variant_id}", links::shop_url(&site, &w.locale, &default_locale, &settings.route_prefix,
                        &format!("p/{}", w.slug.unwrap_or_default()))),
                }})),
            })
            .collect())
    }

    async fn cart_jobs(&self, event: &OutboxEvent) -> Result<Vec<EmailJob>, HandlerError> {
        let Some(cart_id) = event.payload["cart_id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
        else {
            return Ok(vec![]);
        };
        let cart = sqlx::query!(
            r#"SELECT email::text AS email, locale, status FROM carts WHERE id = $1"#,
            cart_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(retry)?;
        let Some(cart) = cart.filter(|c| c.status == "active") else {
            return Ok(vec![]);
        };
        let Some(email) = cart.email else {
            return Ok(vec![]);
        };
        let lines = sqlx::query_scalar!(
            r#"SELECT COALESCE(jsonb_agg(jsonb_build_object('name', COALESCE(t.name, v.sku::text), 'quantity', ci.quantity)), '[]') AS "x!"
               FROM cart_items ci JOIN product_variants v ON v.id = ci.variant_id
               LEFT JOIN product_translations t ON t.product_id = v.product_id AND t.locale = $2
               WHERE ci.cart_id = $1"#,
            cart_id,
            cart.locale
        )
        .fetch_one(&self.pool)
        .await
        .map_err(retry)?;
        let site = self.mailer.settings().public_site_url.trim_end_matches('/');
        let settings = settings::load(&self.pool).await.map_err(retry)?;
        let default_locale = default_locale(&self.pool).await?;
        let cart_url = links::shop_url(
            site,
            &cart.locale,
            &default_locale,
            &settings.route_prefix,
            "cart",
        );
        Ok(vec![EmailJob {
            template: "abandoned_cart".into(),
            to_email: email,
            to_name: None,
            locales: vec![Some(cart.locale.clone())],
            booking_id: None,
            source: Source::Data(json!({ "lines": lines, "cart_url": cart_url })),
        }])
    }
}

#[async_trait]
impl OutboxHandler for ShopEmailHandler {
    fn name(&self) -> &'static str {
        "shop_emails"
    }

    fn event_types(&self) -> &'static [&'static str] {
        EVENTS
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let jobs = self.jobs(event).await?;
        let mut sent_to = Vec::new();
        for job in &jobs {
            match self.mailer.deliver(Some(event.id), job).await {
                Ok(_) => sent_to.push(job.to_email.clone()),
                Err(e) if e.is_transient() => {
                    return Err(HandlerError::Retry(anyhow::anyhow!("{e}")));
                }
                Err(e) => {
                    tracing::warn!(event_id = event.id, template = %job.template, error = %e, "shop email not sent")
                }
            }
        }
        if event.event_type == types::STOCK_BACK_IN_STOCK
            && !sent_to.is_empty()
            && let Some(variant) = event.payload["variant_id"]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
        {
            {
                sqlx::query!(
                    "UPDATE stock_notifications SET notified_at = now()
                     WHERE variant_id = $1 AND notified_at IS NULL AND email = ANY($2::text[]::citext[])",
                    variant,
                    &sent_to
                )
                .execute(&self.pool)
                .await
                .map_err(retry)?;
            }
        }
        Ok(())
    }
}

/// Issues the invoice when an order becomes paid (idempotent).
/// Issues the invoice when an order is paid, then stores and seals the PDF of every new
/// document of the order (invoice, credit notes) in the bucket.
pub struct InvoiceHandler {
    pool: PgPool,
    storage: Option<crate::infra::storage::SharedStorage>,
}

impl InvoiceHandler {
    pub fn new(pool: PgPool, storage: Option<crate::infra::storage::SharedStorage>) -> Self {
        Self { pool, storage }
    }
}

#[async_trait]
impl OutboxHandler for InvoiceHandler {
    fn name(&self) -> &'static str {
        "shop_invoices"
    }

    fn event_types(&self) -> &'static [&'static str] {
        &[types::ORDER_PAID, types::ORDER_REFUND_ISSUED]
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let p: OrderPayload = serde_json::from_value(event.payload.clone())
            .map_err(|e| HandlerError::Permanent(anyhow::anyhow!("invalid payload: {e}")))?;
        let mut tx = self.pool.begin().await.map_err(retry)?;
        sqlx::query!("SELECT id FROM orders WHERE id = $1 FOR UPDATE", p.order_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(retry)?;
        if event.event_type == types::ORDER_PAID {
            super::orders::issue_invoice(&mut tx, p.order_id, Utc::now())
                .await
                .map_err(retry)?;
        }
        tx.commit().await.map_err(retry)?;
        if let Some(storage) = &self.storage {
            super::invoices::seal_pending(&self.pool, storage, Some(p.order_id))
                .await
                .map_err(HandlerError::Retry)?;
        }
        Ok(())
    }
}
