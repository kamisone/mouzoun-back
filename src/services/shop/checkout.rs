//! Checkout: cart → order (immutable snapshots) → stock reserved → payment started.
//!
//! ```text
//! POST /checkout (Idempotency-Key)
//!   tx: lock cart, re-price, refuse blocking issues, customer (find or create by email),
//!       order `pending` + items with snapshots + redemptions, reserve stock, payment row,
//!       cart converted, outbox order.placed
//!   Stripe: create the Checkout Session from the ORDER (never the cart), check its
//!           amount_total == order total (else expire it, cancel the order), store the session
//! ```
//! The same key returns the same order (`orders.checkout_key`, so even without Redis).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    cart::{self, Address, CartRow},
    events::{self, ShopEvent},
    inventory::{self, ReserveLine},
    orders::{self, ActorRef},
    payments::{CheckoutLine, CheckoutRequest, ShopPayments},
    pricing::{IssueCode, Quote},
    quote::{self, Buyer, Fulfillment, VariantFacts, WantedLine},
    settings::ShopSettings,
};
use crate::{
    db::shop::{FulfillmentMethod, OrderCancelReason, OrderChannel, ShopPaymentMethod},
    error::{AppError, AppResult},
    services::audit,
};
use axum::http::StatusCode;

#[derive(Debug, Clone, Deserialize)]
pub struct CheckoutInput {
    pub payment_method: ShopPaymentMethod,
    pub email: String,
    pub phone: Option<String>,
    /// Required for pickup / stay delivery (shipping takes the address names).
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    #[serde(default)]
    pub accept_terms: bool,
    /// Revision of the terms of sale shown next to the checkbox. When given, it must still be
    /// the published one (otherwise the customer is asked to read the new terms).
    #[serde(default)]
    pub terms_revision_id: Option<Uuid>,
    /// Separate, unticked checkbox (newsletter / offers), never a condition of the order.
    #[serde(default)]
    pub marketing_consent: bool,
    pub customer_note: Option<String>,
}

/// The published revision of the terms of sale (`legal.cgv_page_id`).
pub async fn current_terms_revision(
    conn: &mut PgConnection,
    settings: &ShopSettings,
) -> sqlx::Result<Option<Uuid>> {
    let Some(page) = settings.legal_pages.get("cgv") else {
        return Ok(None);
    };
    sqlx::query_scalar!(
        "SELECT r.id FROM page_revisions r JOIN pages p ON p.id = r.page_id
         WHERE r.page_id = $1 AND r.status = 'published' AND p.status = 'published' AND p.deleted_at IS NULL
         ORDER BY r.revision_number DESC LIMIT 1",
        page
    )
    .fetch_optional(conn)
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutResult {
    pub order_id: Uuid,
    pub number: String,
    pub payment_method: ShopPaymentMethod,
    /// Stripe hosted page; otherwise the order page is shown.
    pub redirect_url: Option<String>,
    /// Stripe embedded checkout.
    pub client_secret: Option<String>,
}

pub struct CheckoutEnv<'a> {
    pub pool: &'a PgPool,
    pub payments: &'a ShopPayments,
    pub settings: &'a ShopSettings,
    pub site_url: &'a str,
    /// App secret (signed order links).
    pub secret: &'a str,
    pub default_locale: &'a str,
    pub public_url: &'a (dyn Fn(&str) -> String + Sync),
    pub ip: Option<ipnetwork::IpNetwork>,
    pub user_agent: Option<String>,
    pub now: DateTime<Utc>,
}

fn unprocessable(code: &'static str, message: &str) -> AppError {
    AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

fn valid_email(email: &str) -> bool {
    let e = email.trim();
    e.len() <= 254
        && e.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        })
        && !e.contains(char::is_whitespace)
}

/// What an order is made of (checkout or a staff draft order).
pub struct NewOrder<'a> {
    pub channel: OrderChannel,
    pub status: &'a str,
    pub customer_id: Uuid,
    pub cart_id: Option<Uuid>,
    pub booking_id: Option<Uuid>,
    pub email: &'a str,
    pub phone: Option<&'a str>,
    pub locale: &'a str,
    pub currency: &'a str,
    pub method: FulfillmentMethod,
    pub shipping_address: Option<&'a Address>,
    pub billing_address: Option<&'a Address>,
    pub pickup_location_id: Option<Uuid>,
    pub payment_method: Option<ShopPaymentMethod>,
    pub requires_confirmation: bool,
    pub codes: &'a [String],
    pub customer_note: Option<&'a str>,
    pub checkout_key: Option<&'a str>,
    pub created_by: Option<Uuid>,
    pub terms_accepted: bool,
    /// The accepted revision of the terms of sale (website orders).
    pub terms_revision_id: Option<Uuid>,
    pub marketing_consent: bool,
    pub relay_point: Option<&'a super::relay::RelayPoint>,
    pub ip: Option<ipnetwork::IpNetwork>,
    pub user_agent: Option<&'a str>,
}

/// The snapshot of a line, frozen in the order's language.
pub fn snapshot(f: &VariantFacts, public_url: &(dyn Fn(&str) -> String + Sync)) -> Value {
    json!({
        "name": f.name,
        "slug": f.slug,
        "options_label": f.options_label,
        "options": f.options,
        "sku": f.sku,
        "barcode": f.barcode,
        "image_url": f.image_key.as_deref().map(public_url),
        "weight_grams": f.weight_grams,
        "net_quantity": f.net_quantity,
        "net_unit": f.net_unit,
        "allergens": f.allergens,
        "is_perishable": f.is_perishable,
        "withdrawal_exemption": f.withdrawal_exemption,
        "country_of_origin": f.country_of_origin,
        "tax_class_code": f.tax_class_code,
        "track_lots": f.track_lots,
        "product_type": f.product_type,
        "cost_minor": f.cost_minor,
    })
}

/// Inserts the order, its items and its promotion redemptions from a quote.
pub async fn insert_order(
    conn: &mut PgConnection,
    o: &NewOrder<'_>,
    q: &Quote,
    facts: &std::collections::HashMap<Uuid, VariantFacts>,
    public_url: &(dyn Fn(&str) -> String + Sync),
    now: DateTime<Utc>,
) -> sqlx::Result<(Uuid, String, Vec<(Uuid, Uuid, i32)>)> {
    let id = Uuid::now_v7();
    let number = orders::next_number(&mut *conn, now).await?;
    let t = &q.totals;
    let shipping_lines: Value = serde_json::to_value(
        q.shipping_lines
            .iter()
            .map(|l| {
                json!({ "profile_id": l.profile_id, "rate_id": l.rate_id, "code": l.code, "name": l.name,
                        "carrier": l.carrier, "amount_minor": l.amount_minor, "discount_minor": l.discount_minor,
                        "tax_minor": l.tax_minor, "tax_lines": l.tax_lines })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    sqlx::query!(
        "INSERT INTO orders (id, number, customer_id, cart_id, booking_id, channel, status, payment_status, payment_method,
                             requires_confirmation, email, phone, locale, currency, fulfillment_method, shipping_address,
                             billing_address, pickup_location_id, shipping_lines, promotion_codes, subtotal_minor,
                             discount_minor, shipping_minor, shipping_discount_minor, tax_minor, cod_fee_minor, total_minor,
                             taxes_included, customer_note, checkout_key, created_by, terms_accepted_at, placed_at,
                             ip_address, user_agent, legal_entity_id, terms_page_revision_id, marketing_consent_snapshot,
                             vat_breakdown, relay_point)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'unpaid', $8, $9, $10::text::citext, $11, $12, $13, $14, $15, $16, $17, $18,
                 $19::text[]::citext[], $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33, $34,
                 shop_entity_id(), $35, $36, $37, $38)",
        id,
        number,
        o.customer_id,
        o.cart_id,
        o.booking_id,
        o.channel.as_str(),
        o.status,
        o.payment_method.map(|m| m.as_str()),
        o.requires_confirmation,
        o.email.trim(),
        o.phone,
        o.locale,
        o.currency,
        o.method.as_str(),
        o.shipping_address.map(|a| serde_json::to_value(a).unwrap_or_default()),
        o.billing_address.map(|a| serde_json::to_value(a).unwrap_or_default()),
        o.pickup_location_id,
        shipping_lines,
        o.codes,
        t.subtotal_minor,
        t.discount_minor,
        t.shipping_minor,
        t.shipping_discount_minor,
        t.tax_minor,
        t.cod_fee_minor,
        t.total_minor,
        t.taxes_included,
        o.customer_note,
        o.checkout_key,
        o.created_by,
        o.terms_accepted.then_some(now),
        (o.status != "draft").then_some(now),
        o.ip,
        o.user_agent,
        o.terms_revision_id,
        o.marketing_consent,
        serde_json::to_value(&t.vat_breakdown).unwrap_or_default(),
        o.relay_point.map(|p| serde_json::to_value(p).unwrap_or_default())
    )
    .execute(&mut *conn)
    .await?;
    let mut items = Vec::new();
    for (i, l) in q.lines.iter().enumerate() {
        let item_id = Uuid::now_v7();
        let f = facts.get(&l.variant_id);
        sqlx::query!(
            "INSERT INTO order_items (id, order_id, variant_id, product_id, sku, quantity, unit_price_minor,
                                      compare_at_price_minor, subtotal_minor, discount_minor, tax_minor, total_minor,
                                      tax_lines, discount_allocations, product_snapshot, sort_order)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            item_id,
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
            f.map(|f| snapshot(f, public_url)).unwrap_or_else(|| json!({})),
            i as i32
        )
        .execute(&mut *conn)
        .await?;
        items.push((item_id, l.variant_id, l.quantity));
    }
    for p in &q.promotions {
        sqlx::query!(
            "INSERT INTO promotion_redemptions (id, promotion_id, order_id, customer_id, discount_minor)
             VALUES ($1, $2, $3, $4, $5)",
            Uuid::now_v7(),
            p.promotion_id,
            id,
            o.customer_id,
            p.amount_minor + p.shipping_amount_minor
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok((id, number, items))
}

/// The customer of an email: existing one (also attached to the signed-in user), or new.
#[allow(clippy::too_many_arguments)]
pub async fn find_or_create_customer(
    conn: &mut PgConnection,
    signed_in: Option<Uuid>,
    email: &str,
    first_name: &str,
    last_name: Option<&str>,
    phone: Option<&str>,
    country: Option<&str>,
    locale: &str,
) -> sqlx::Result<Uuid> {
    if let Some(id) = signed_in {
        return Ok(id);
    }
    // Concurrent checkouts of a new address: one insert wins, the other reads it.
    sqlx::query!(
        "INSERT INTO customers (id, email, first_name, last_name, phone, country_code, preferred_locale)
         VALUES ($1, $2::text::citext, $3, $4, $5, $6, $7)
         ON CONFLICT (email) WHERE email IS NOT NULL AND deleted_at IS NULL DO NOTHING",
        Uuid::now_v7(),
        email.trim(),
        first_name.trim(),
        last_name.map(str::trim),
        phone,
        country,
        locale
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query_scalar!(
        "SELECT id FROM customers WHERE email = $1::text::citext AND deleted_at IS NULL",
        email.trim()
    )
    .fetch_one(&mut *conn)
    .await
}

/// The stored result of an earlier checkout with the same key.
async fn replay(pool: &PgPool, key: &str) -> sqlx::Result<Option<CheckoutResult>> {
    let row = sqlx::query!(
        r#"SELECT o.id, o.number, o.payment_method AS "payment_method: ShopPaymentMethod",
                  (SELECT provider_payload FROM payments p WHERE p.order_id = o.id ORDER BY created_at DESC LIMIT 1) AS payload
           FROM orders o WHERE o.checkout_key = $1"#,
        key
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| CheckoutResult {
        order_id: r.id,
        number: r.number,
        payment_method: r.payment_method.unwrap_or(ShopPaymentMethod::BankTransfer),
        redirect_url: r
            .payload
            .as_ref()
            .and_then(|p| p["url"].as_str())
            .map(str::to_string),
        client_secret: r
            .payload
            .as_ref()
            .and_then(|p| p["client_secret"].as_str())
            .map(str::to_string),
    }))
}

pub async fn checkout(
    env: &CheckoutEnv<'_>,
    cart_id: Uuid,
    signed_in_customer: Option<Uuid>,
    input: &CheckoutInput,
    key: &str,
) -> AppResult<CheckoutResult> {
    let s = env.settings;
    if !s.is_open() {
        return Err(AppError::NotFound);
    }
    if let Some(done) = replay(env.pool, key).await? {
        return Ok(done);
    }
    let email = input.email.trim().to_lowercase();
    let mut errors: Vec<(&str, &str)> = Vec::new();
    if !valid_email(&email) {
        errors.push(("email", "invalid"));
    }
    if !input.accept_terms {
        errors.push(("accept_terms", "required"));
    }
    if !s
        .enabled_methods(env.payments.card_available())
        .contains(&input.payment_method)
    {
        errors.push(("payment_method", "unavailable"));
    }
    if !errors.is_empty() {
        return Err(cart::field_errors(&errors));
    }

    let mut tx = env.pool.begin().await?;
    let cart = cart::lock(&mut tx, cart_id).await?;
    if let Some(done) = replay(env.pool, key).await? {
        return Ok(done);
    }
    let lines = cart::items(&mut tx, cart.id).await?;
    let fulfillment: Fulfillment = cart.fulfillment();
    if matches!(
        fulfillment.method,
        Some(FulfillmentMethod::Shipping | FulfillmentMethod::RelayPoint)
    ) && cart.shipping_address.is_none()
    {
        return Err(cart::field_errors(&[("shipping_address", "required")]));
    }
    if fulfillment.method == Some(FulfillmentMethod::RelayPoint) && cart.relay_point.is_none() {
        return Err(cart::field_errors(&[("relay_point_id", "required")]));
    }
    // Terms of sale: the accepted revision is recorded on the order (proof of consent).
    let terms_revision = current_terms_revision(&mut tx, s)
        .await?
        .ok_or_else(|| unprocessable("terms_unavailable", "the terms of sale are not published"))?;
    if input.terms_revision_id.is_some_and(|r| r != terms_revision) {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "terms_changed",
            "the terms of sale changed: read and accept them again",
        )
        .with_details(json!({ "terms_revision_id": terms_revision })));
    }
    let buyer = Buyer {
        email: Some(email.clone()),
        ..cart.buyer()
    };
    let loaded = quote::load(
        &mut tx,
        s,
        &lines,
        &fulfillment,
        &cart.promotion_codes,
        &buyer,
        Some(input.payment_method),
        &cart.locale,
        env.now,
    )
    .await?;
    let q = loaded.price();
    let blocking = q.blocking_issues();
    if !blocking.is_empty() {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "cart_has_issues",
            "the cart must be fixed before ordering",
        )
        .with_details(json!({ "issues": blocking })));
    }
    if input.payment_method == ShopPaymentMethod::CashOnDelivery
        && s.payments
            .cod_max_order_minor
            .is_some_and(|max| q.totals.total_minor > max)
    {
        return Err(unprocessable(
            "cod_limit",
            "this amount cannot be paid cash on delivery",
        ));
    }
    if !cart::available_methods(s, env.payments.card_available(), &cart, &q)
        .contains(&input.payment_method)
    {
        return Err(cart::field_errors(&[("payment_method", "unavailable")]));
    }
    let (first, last) = match (&cart.shipping_address, &input.first_name) {
        (_, Some(f)) if !f.trim().is_empty() => (f.trim().to_string(), input.last_name.clone()),
        (Some(a), _) => (a.first_name.clone(), Some(a.last_name.clone())),
        _ => return Err(cart::field_errors(&[("first_name", "required")])),
    };
    let phone = input
        .phone
        .clone()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| cart.shipping_address.as_ref().map(|a| a.phone.clone()));
    if input.payment_method == ShopPaymentMethod::CashOnDelivery && phone.is_none() {
        // Cash on delivery is confirmed by phone.
        return Err(cart::field_errors(&[("phone", "required")]));
    }
    let customer_id = find_or_create_customer(
        &mut tx,
        signed_in_customer.or(cart.customer_id),
        &email,
        &first,
        last.as_deref(),
        phone.as_deref(),
        cart.shipping_address
            .as_ref()
            .map(|a| a.country_code.as_str()),
        &cart.locale,
    )
    .await?;
    if input.marketing_consent {
        sqlx::query!(
            "UPDATE customers SET marketing_consent = true, marketing_consent_at = COALESCE(marketing_consent_at, $2) WHERE id = $1",
            customer_id,
            env.now
        )
        .execute(&mut *tx)
        .await?;
    }
    let method = fulfillment
        .method
        .ok_or_else(|| cart::field_errors(&[("fulfillment_method", "required")]))?;
    let offline_open = matches!(input.payment_method, ShopPaymentMethod::PayAtPickup);
    let order = NewOrder {
        channel: OrderChannel::Website,
        status: if offline_open { "open" } else { "pending" },
        customer_id,
        cart_id: Some(cart.id),
        booking_id: (method == FulfillmentMethod::StayDelivery)
            .then_some(cart.booking_id)
            .flatten(),
        email: &email,
        phone: phone.as_deref(),
        locale: &cart.locale,
        currency: &cart.currency,
        method,
        shipping_address: cart.shipping_address.as_ref(),
        billing_address: cart
            .billing_address
            .as_ref()
            .or(cart.shipping_address.as_ref()),
        pickup_location_id: (method == FulfillmentMethod::Pickup)
            .then_some(cart.pickup_location_id)
            .flatten(),
        payment_method: Some(input.payment_method),
        requires_confirmation: input.payment_method == ShopPaymentMethod::CashOnDelivery,
        codes: &cart.promotion_codes,
        customer_note: input
            .customer_note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty()),
        checkout_key: Some(key),
        created_by: None,
        terms_accepted: true,
        terms_revision_id: Some(terms_revision),
        marketing_consent: input.marketing_consent,
        relay_point: (method == FulfillmentMethod::RelayPoint)
            .then_some(cart.relay_point.as_ref())
            .flatten(),
        ip: env.ip,
        user_agent: env.user_agent.as_deref(),
    };
    let (order_id, number, items) =
        insert_order(&mut tx, &order, &q, &loaded.facts, env.public_url, env.now).await?;

    let hold = match input.payment_method {
        ShopPaymentMethod::Stripe => Some(env.now + Duration::minutes(s.checkout_hold_minutes)),
        ShopPaymentMethod::CashOnDelivery => None,
        ShopPaymentMethod::BankTransfer | ShopPaymentMethod::PayAtPickup => {
            Some(env.now + Duration::days(s.payments.bank_transfer_hold_days))
        }
    };
    let reserve_lines: Vec<ReserveLine> = items
        .iter()
        .map(|(item, variant, q)| ReserveLine {
            order_item_id: *item,
            variant_id: *variant,
            quantity: *q,
        })
        .collect();
    inventory::reserve(
        &mut tx,
        order_id,
        &reserve_lines,
        order.pickup_location_id,
        hold,
    )
    .await?;

    let payment_id = Uuid::now_v7();
    let provider = match input.payment_method {
        ShopPaymentMethod::Stripe => env.payments.card().map_or("stripe", |p| p.name()),
        m => m.as_str(),
    };
    if q.totals.total_minor > 0 {
        sqlx::query!(
            "INSERT INTO payments (id, order_id, provider, purpose, status, amount_minor, currency, idempotency_key, legal_entity_id)
             VALUES ($1, $2, $3, 'full', 'pending', $4, $5, $6, shop_entity_id())",
            payment_id,
            order_id,
            provider,
            q.totals.total_minor,
            cart.currency,
            format!("order-{order_id}")
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        "UPDATE carts SET status = 'converted', converted_order_id = $2 WHERE id = $1",
        cart.id,
        order_id
    )
    .execute(&mut *tx)
    .await?;
    let actor = ActorRef::customer(audit::Actor::customer().with_request(
        env.ip,
        env.user_agent.clone(),
        None,
    ));
    orders::log_event(
        &mut tx,
        order_id,
        "placed",
        None,
        None,
        json!({ "payment_method": input.payment_method, "total_minor": q.totals.total_minor }),
        &actor,
    )
    .await?;
    orders::recompute(&mut tx, order_id, env.now, &actor).await?;
    audit::record(
        &mut tx,
        &actor.audit,
        "order.placed",
        "order",
        &order_id.to_string(),
        json!({ "number": number }),
    )
    .await?;
    if input.payment_method != ShopPaymentMethod::Stripe {
        events::enqueue(&mut tx, ShopEvent::OrderPlaced { order_id }).await?;
    }
    tx.commit().await?;

    if input.payment_method != ShopPaymentMethod::Stripe || q.totals.total_minor == 0 {
        return Ok(CheckoutResult {
            order_id,
            number,
            payment_method: input.payment_method,
            redirect_url: None,
            client_secret: None,
        });
    }
    start_card_payment(
        env,
        order_id,
        payment_id,
        &number,
        &email,
        &cart,
        &q,
        &loaded.facts,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_card_payment(
    env: &CheckoutEnv<'_>,
    order_id: Uuid,
    payment_id: Uuid,
    number: &str,
    email: &str,
    cart: &CartRow,
    q: &Quote,
    facts: &std::collections::HashMap<Uuid, VariantFacts>,
) -> AppResult<CheckoutResult> {
    let s = env.settings;
    let provider = env
        .payments
        .card()
        .ok_or_else(|| unprocessable("payment_unavailable", "card payment is unavailable"))?;
    let order_url = super::links::order_url(
        env.site_url,
        &cart.locale,
        env.default_locale,
        &s.route_prefix,
        number,
        env.secret,
    );
    let shipping_net = q.totals.shipping_minor - q.totals.shipping_discount_minor;
    let req = CheckoutRequest {
        order_id,
        payment_id,
        order_number: number.to_string(),
        email: email.to_string(),
        locale: cart.locale.clone(),
        currency: cart.currency.clone(),
        lines: q
            .lines
            .iter()
            .map(|l| {
                let f = facts.get(&l.variant_id);
                let name = match f {
                    Some(f) if !f.options_label.is_empty() => {
                        format!("{} ({})", f.name, f.options_label)
                    }
                    Some(f) => f.name.clone(),
                    None => "Article".into(),
                };
                // The VAT rate is shown on the payment page (prices incl. VAT).
                let mut rates: Vec<String> = l
                    .tax_lines
                    .iter()
                    .map(|t| format!("{} %", t.rate.normalize().to_string().replace('.', ",")))
                    .collect();
                rates.dedup();
                let name = if rates.is_empty() {
                    name
                } else {
                    format!("{name} — TVA {}", rates.join(" + "))
                };
                CheckoutLine {
                    name,
                    unit_amount_minor: l.unit_price_minor,
                    quantity: l.quantity,
                }
            })
            .collect(),
        shipping: (shipping_net > 0 || !q.shipping_lines.is_empty()).then(|| {
            (
                q.shipping_lines
                    .iter()
                    .map(|l| l.name.clone())
                    .collect::<Vec<_>>()
                    .join(" + "),
                shipping_net,
            )
        }),
        discount_minor: q.totals.discount_minor,
        exclusive_tax_minor: if q.totals.taxes_included {
            0
        } else {
            q.totals.tax_minor
        },
        total_minor: q.totals.total_minor,
        idempotency_key: format!("order-{order_id}"),
        success_url: format!("{order_url}&paid=1"),
        cancel_url: format!("{order_url}&cancelled=1"),
        expires_at: env.now + Duration::minutes(s.checkout_hold_minutes),
        statement_descriptor: Some(s.payments.statement_descriptor.clone())
            .filter(|d| !d.trim().is_empty()),
        embedded: s.payments.stripe_checkout_mode == "embedded",
    };
    let fail = |reason: &str| -> AppError {
        tracing::error!(%order_id, reason, alert = true, "card checkout could not start");
        AppError::domain(
            StatusCode::BAD_GATEWAY,
            "payment_unavailable",
            "the payment could not start, try again",
        )
    };
    let session = match provider.create_checkout(&req).await {
        Ok(s) => s,
        Err(e) => {
            abandon(env, order_id, &format!("provider refused the session: {e}")).await?;
            return Err(fail("provider error"));
        }
    };
    if session.amount_total != q.totals.total_minor {
        // Bug guard: never let the customer pay another amount than the order.
        let _ = provider.expire(&session.session_id).await;
        abandon(
            env,
            order_id,
            &format!(
                "session amount {} differs from the order total {}",
                session.amount_total, q.totals.total_minor
            ),
        )
        .await?;
        return Err(fail("amount mismatch"));
    }
    let mut tx = env.pool.begin().await?;
    sqlx::query!(
        "UPDATE payments SET provider_session_id = $2, provider_payload = $3 WHERE id = $1",
        payment_id,
        session.session_id,
        json!({ "url": session.url, "client_secret": session.client_secret, "session_id": session.session_id })
    )
    .execute(&mut *tx)
    .await?;
    events::enqueue(&mut tx, ShopEvent::OrderPlaced { order_id }).await?;
    tx.commit().await?;
    Ok(CheckoutResult {
        order_id,
        number: number.to_string(),
        payment_method: ShopPaymentMethod::Stripe,
        redirect_url: session.url,
        client_secret: session.client_secret,
    })
}

/// The payment could not start: the order is cancelled and its stock released.
async fn abandon(env: &CheckoutEnv<'_>, order_id: Uuid, why: &str) -> AppResult<()> {
    let mut tx = env.pool.begin().await?;
    let _ = orders::cancel(
        &mut tx,
        order_id,
        OrderCancelReason::PaymentFailed,
        "system",
        false,
        Some(why),
        &ActorRef::system(),
        env.now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Issues of a quote that stop a checkout (for error details).
pub fn blocking_codes(q: &Quote) -> Vec<IssueCode> {
    q.blocking_issues().iter().map(|i| i.code).collect()
}

/// Lines of an order to re-price (order edits): `(order_item_id, variant_id, quantity)`.
pub fn wanted_from(items: &[(Uuid, Uuid, i32)]) -> Vec<WantedLine> {
    items
        .iter()
        .map(|(id, v, q)| WantedLine {
            line_id: *id,
            variant_id: *v,
            quantity: *q,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_checked() {
        assert!(valid_email("amina@example.ma"));
        assert!(!valid_email("amina@example"));
        assert!(!valid_email("a b@example.ma"));
        assert!(!valid_email("@example.ma"));
    }
}
