//! Shop carts, in Postgres (abandoned-cart recovery, cross-device for signed-in customers).
//! The browser holds a random token in an httpOnly cookie; the database stores only its hash.
//! A cart reserves nothing: stock is reserved when the order is created at checkout.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    pricing::{Destination, Issue, Quote, ShippingLine, Totals},
    quote::{self, Buyer, Fulfillment, WantedLine},
    settings::ShopSettings,
};
use crate::{
    db::shop::{FulfillmentMethod, ShopPaymentMethod},
    error::{AppError, AppResult},
};

pub const MAX_LINES: i64 = 50;

pub fn new_token() -> (String, Vec<u8>) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let raw = URL_SAFE_NO_PAD.encode(bytes);
    let hash = hash_token(&raw);
    (raw, hash)
}

pub fn hash_token(raw: &str) -> Vec<u8> {
    Sha256::digest(raw.as_bytes()).to_vec()
}

/// A shipping / billing address as stored on carts and orders.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Address {
    pub first_name: String,
    pub last_name: String,
    #[serde(default)]
    pub company: Option<String>,
    pub line1: String,
    #[serde(default)]
    pub line2: Option<String>,
    pub city: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    pub country_code: String,
    pub phone: String,
    #[serde(default)]
    pub delivery_notes: Option<String>,
}

impl Address {
    /// Field errors (`field → code`), empty when valid.
    pub fn errors(&self, ships_to: &[String]) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        let blank = |s: &str| s.trim().is_empty();
        for (field, value) in [
            ("first_name", &self.first_name),
            ("last_name", &self.last_name),
            ("line1", &self.line1),
            ("city", &self.city),
            ("phone", &self.phone),
        ] {
            if blank(value) {
                out.push((field, "required"));
            } else if value.chars().count() > 200 {
                out.push((field, "too_long"));
            }
        }
        let cc = self.country_code.trim().to_ascii_uppercase();
        if cc.len() != 2 || !cc.chars().all(|c| c.is_ascii_uppercase()) {
            out.push(("country_code", "invalid"));
        } else if !ships_to.is_empty() && !ships_to.iter().any(|c| c.eq_ignore_ascii_case(&cc)) {
            out.push(("country_code", "not_shipped"));
        }
        // France: 5-digit postal code, required.
        if cc == "FR" {
            match self.postal_code.as_deref().map(str::trim) {
                None | Some("") => out.push(("postal_code", "required")),
                Some(p) if !super::relay::valid_fr_postal(p) => {
                    out.push(("postal_code", "invalid"))
                }
                _ => {}
            }
        }
        // Morocco: 5-digit postal codes; others free-form.
        if cc == "MA"
            && self.postal_code.as_deref().is_some_and(|p| {
                !p.trim().is_empty() && !p.trim().chars().all(|c| c.is_ascii_digit())
            })
        {
            out.push(("postal_code", "invalid"));
        }
        if !blank(&self.phone) && e164(&self.phone, &cc).is_none() {
            out.push(("phone", "invalid"));
        }
        out
    }

    /// The address with its phone in E.164 (`+33612345678`), after validation.
    pub fn normalized(mut self) -> Self {
        if let Some(p) = e164(&self.phone, &self.country_code.trim().to_ascii_uppercase()) {
            self.phone = p;
        }
        self.country_code = self.country_code.trim().to_ascii_uppercase();
        self.postal_code = self.postal_code.map(|p| p.trim().to_string());
        self
    }

    pub fn destination(&self) -> Destination {
        Destination {
            country_code: Some(self.country_code.trim().to_ascii_uppercase()),
            region: self.region.clone().filter(|r| !r.trim().is_empty()),
            postal_code: self.postal_code.clone().filter(|p| !p.trim().is_empty()),
        }
    }
}

/// A phone number in E.164: `+` and 8 to 15 digits, or a national number of a known country
/// (France `0X XX XX XX XX` → `+33X...`, Morocco `0X...` → `+212X...`).
pub fn e164(phone: &str, country: &str) -> Option<String> {
    let compact: String = phone
        .chars()
        .filter(|c| !matches!(c, ' ' | '.' | '-' | '(' | ')'))
        .collect();
    let compact = compact
        .strip_prefix("00")
        .map(|r| format!("+{r}"))
        .unwrap_or(compact);
    if let Some(rest) = compact.strip_prefix('+') {
        let ok = (8..=15).contains(&rest.len())
            && rest.chars().all(|c| c.is_ascii_digit())
            && !rest.starts_with('0');
        return ok.then(|| format!("+{rest}"));
    }
    let national = compact.strip_prefix('0')?;
    if national.len() != 9 || !national.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    match country {
        "FR" => Some(format!("+33{national}")),
        "MA" => Some(format!("+212{national}")),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct CartRow {
    pub id: Uuid,
    pub customer_id: Option<Uuid>,
    pub email: Option<String>,
    pub locale: String,
    pub currency: String,
    pub fulfillment_method: Option<FulfillmentMethod>,
    pub shipping_address: Option<Address>,
    pub billing_address: Option<Address>,
    /// Country chosen for an estimate before the address is known.
    pub estimate_country: Option<String>,
    pub shipping_rate_ids: Vec<Uuid>,
    pub pickup_location_id: Option<Uuid>,
    pub booking_id: Option<Uuid>,
    pub promotion_codes: Vec<String>,
    pub relay_point: Option<super::relay::RelayPoint>,
}

impl CartRow {
    pub fn fulfillment(&self) -> Fulfillment {
        let relay = (self.fulfillment_method == Some(FulfillmentMethod::RelayPoint))
            .then_some(self.relay_point.as_ref())
            .flatten();
        let destination = match (relay, &self.shipping_address, &self.estimate_country) {
            // Goods are delivered at the relay point: its address decides the zone and the VAT.
            (Some(p), _, _) => Destination {
                country_code: Some(p.country_code.clone()),
                region: None,
                postal_code: Some(p.postal_code.clone()),
            },
            (None, Some(a), _) => a.destination(),
            (None, None, Some(c)) => Destination {
                country_code: Some(c.clone()),
                ..Destination::default()
            },
            _ => Destination::default(),
        };
        Fulfillment {
            method: self.fulfillment_method,
            destination,
            pickup_location_id: self.pickup_location_id,
            chosen_rate_ids: self.shipping_rate_ids.clone(),
        }
    }

    pub fn buyer(&self) -> Buyer {
        Buyer {
            customer_id: self.customer_id,
            email: self.email.clone(),
            booking_id: (self.fulfillment_method == Some(FulfillmentMethod::StayDelivery))
                .then_some(self.booking_id)
                .flatten(),
        }
    }
}

/// The active, unexpired cart of a token.
pub async fn find(
    conn: &mut PgConnection,
    token: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<Option<CartRow>> {
    let id = sqlx::query_scalar!(
        "SELECT id FROM carts WHERE token_hash = $1 AND status = 'active' AND expires_at > $2",
        hash_token(token),
        now
    )
    .fetch_optional(&mut *conn)
    .await?;
    match id {
        Some(id) => Ok(Some(load(conn, id).await?)),
        None => Ok(None),
    }
}

/// Locks the cart row (checkout, concurrent edits), then reads it.
pub async fn lock(conn: &mut PgConnection, cart_id: Uuid) -> sqlx::Result<CartRow> {
    sqlx::query!("SELECT id FROM carts WHERE id = $1 FOR UPDATE", cart_id)
        .fetch_one(&mut *conn)
        .await?;
    load(conn, cart_id).await
}

pub async fn load(conn: &mut PgConnection, cart_id: Uuid) -> sqlx::Result<CartRow> {
    let r = sqlx::query!(
        r#"SELECT id, customer_id, email::text AS email, locale, currency::text AS "currency!",
                  fulfillment_method AS "fulfillment_method: FulfillmentMethod", shipping_address, billing_address,
                  shipping_rate_ids, pickup_location_id, booking_id, promotion_codes::text[] AS "promotion_codes!", relay_point
           FROM carts WHERE id = $1"#,
        cart_id
    )
    .fetch_one(conn)
    .await?;
    let estimate_country = r
        .shipping_address
        .as_ref()
        .and_then(|a| a.get("estimate_country"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(CartRow {
        id: r.id,
        customer_id: r.customer_id,
        email: r.email,
        locale: r.locale,
        currency: r.currency,
        fulfillment_method: r.fulfillment_method,
        shipping_address: r
            .shipping_address
            .and_then(|a| serde_json::from_value(a).ok()),
        billing_address: r
            .billing_address
            .and_then(|a| serde_json::from_value(a).ok()),
        estimate_country,
        shipping_rate_ids: r.shipping_rate_ids,
        pickup_location_id: r.pickup_location_id,
        booking_id: r.booking_id,
        promotion_codes: r.promotion_codes,
        relay_point: r.relay_point.and_then(|p| serde_json::from_value(p).ok()),
    })
}

pub async fn create(
    conn: &mut PgConnection,
    settings: &ShopSettings,
    locale: &str,
    customer_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> sqlx::Result<(CartRow, String)> {
    let (raw, hash) = new_token();
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO carts (id, token_hash, customer_id, locale, currency, expires_at, last_activity_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        id,
        hash,
        customer_id,
        locale,
        settings.currency,
        now + Duration::days(settings.cart_ttl_days),
        now
    )
    .execute(&mut *conn)
    .await?;
    Ok((lock(conn, id).await?, raw))
}

/// Activity: pushes the expiry, re-allows the abandoned-cart email.
pub async fn touch(
    conn: &mut PgConnection,
    cart_id: Uuid,
    settings: &ShopSettings,
    now: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE carts SET last_activity_at = $2, expires_at = $3 WHERE id = $1",
        cart_id,
        now,
        now + Duration::days(settings.cart_ttl_days)
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn items(conn: &mut PgConnection, cart_id: Uuid) -> sqlx::Result<Vec<WantedLine>> {
    Ok(sqlx::query!(
        "SELECT id, variant_id, quantity FROM cart_items WHERE cart_id = $1 ORDER BY created_at, id",
        cart_id
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|r| WantedLine {
        line_id: r.id,
        variant_id: r.variant_id,
        quantity: r.quantity,
    })
    .collect())
}

pub async fn add_item(
    conn: &mut PgConnection,
    cart_id: Uuid,
    variant_id: Uuid,
    quantity: i32,
    max_per_line: i64,
) -> AppResult<()> {
    if quantity <= 0 {
        return Err(AppError::BadRequest("quantity must be positive".into()));
    }
    let sellable = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM product_variants v JOIN products p ON p.id = v.product_id
                          WHERE v.id = $1 AND v.status = 'active' AND v.deleted_at IS NULL
                            AND p.status = 'published' AND p.deleted_at IS NULL) AS "x!""#,
        variant_id
    )
    .fetch_one(&mut *conn)
    .await?;
    if !sellable {
        return Err(AppError::domain(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "product_unavailable",
            "this product is not for sale",
        ));
    }
    let lines = sqlx::query_scalar!(
        "SELECT count(*) AS \"n!\" FROM cart_items WHERE cart_id = $1",
        cart_id
    )
    .fetch_one(&mut *conn)
    .await?;
    if lines >= MAX_LINES {
        return Err(AppError::BadRequest("the cart is full".into()));
    }
    let max = i32::try_from(max_per_line.min(999)).unwrap_or(999);
    sqlx::query!(
        "INSERT INTO cart_items (id, cart_id, variant_id, quantity) VALUES ($1, $2, $3, LEAST($4::int, $5::int))
         ON CONFLICT (cart_id, variant_id) DO UPDATE SET quantity = LEAST(cart_items.quantity + $4::int, $5::int)",
        Uuid::now_v7(),
        cart_id,
        variant_id,
        quantity,
        max
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_quantity(
    conn: &mut PgConnection,
    cart_id: Uuid,
    item_id: Uuid,
    quantity: i32,
    max_per_line: i64,
) -> AppResult<()> {
    let n = if quantity <= 0 {
        sqlx::query!(
            "DELETE FROM cart_items WHERE id = $1 AND cart_id = $2",
            item_id,
            cart_id
        )
        .execute(conn)
        .await?
        .rows_affected()
    } else {
        sqlx::query!(
            "UPDATE cart_items SET quantity = LEAST($3::int, $4::int) WHERE id = $1 AND cart_id = $2",
            item_id,
            cart_id,
            quantity,
            i32::try_from(max_per_line.min(999)).unwrap_or(999)
        )
        .execute(conn)
        .await?
        .rows_affected()
    };
    if n == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct FulfillmentChoice {
    pub method: FulfillmentMethod,
    pub shipping_address: Option<Address>,
    pub billing_address: Option<Address>,
    #[serde(default)]
    pub shipping_rate_ids: Vec<Uuid>,
    pub pickup_location_id: Option<Uuid>,
    /// Stay delivery: the booking reference and the guest's last name (checked together).
    pub booking_reference: Option<String>,
    pub last_name: Option<String>,
    /// Shipping estimate before the full address (cart page).
    pub estimate_country: Option<String>,
    pub email: Option<String>,
    /// Relay-point delivery: the point chosen in the picker (`<carrier>:<id>`).
    pub relay_point_id: Option<String>,
}

/// Country of the selling entity (`shop.legal_entity`).
pub async fn entity_country(
    conn: &mut PgConnection,
    settings: &ShopSettings,
) -> sqlx::Result<String> {
    Ok(sqlx::query_scalar!(
        r#"SELECT country_code::text AS "c!" FROM legal_entities WHERE code = $1::text::citext"#,
        settings.legal_entity
    )
    .fetch_optional(conn)
    .await?
    .unwrap_or_else(|| "FR".into()))
}

/// Fulfilment methods offered by the selling entity: a French company ships (home or relay
/// point) and does not hand over goods at the Moroccan camp (no French establishment there).
pub fn methods_for(entity_country: &str, settings: &ShopSettings) -> Vec<FulfillmentMethod> {
    let mut out = vec![FulfillmentMethod::Shipping, FulfillmentMethod::RelayPoint];
    if entity_country != "FR" {
        out.push(FulfillmentMethod::Pickup);
        if settings.stay_delivery_enabled {
            out.push(FulfillmentMethod::StayDelivery);
        }
    }
    out
}

/// Validates and stores the fulfilment choice. Stay delivery needs an active booking whose
/// guest has this last name; pickup a pickup point.
pub async fn set_fulfillment(
    conn: &mut PgConnection,
    cart: &CartRow,
    settings: &ShopSettings,
    choice: &FulfillmentChoice,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let mut booking_id = None;
    let mut pickup = None;
    let mut shipping_address: Option<Value> = None;
    let mut relay_point: Option<Value> = None;
    let country = entity_country(conn, settings).await?;
    if !methods_for(&country, settings).contains(&choice.method) {
        return Err(AppError::domain(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "fulfillment_unavailable",
            "this delivery method is not offered by the shop",
        ));
    }
    match choice.method {
        FulfillmentMethod::RelayPoint => {
            if let Some(a) = &choice.shipping_address {
                let errors = a.errors(&settings.ships_to_countries);
                if !errors.is_empty() {
                    return Err(field_errors(&errors));
                }
                shipping_address =
                    Some(serde_json::to_value(a.clone().normalized()).unwrap_or_default());
            }
            if let Some(id) = &choice.relay_point_id {
                let point = super::relay::provider()
                    .get(id)
                    .await
                    .map_err(AppError::Internal)?
                    .ok_or_else(|| field_errors(&[("relay_point_id", "invalid")]))?;
                if !settings.ships_to_countries.is_empty()
                    && !settings
                        .ships_to_countries
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(&point.country_code))
                {
                    return Err(field_errors(&[("relay_point_id", "not_shipped")]));
                }
                relay_point = Some(serde_json::to_value(point).unwrap_or_default());
            }
        }
        FulfillmentMethod::Shipping => match (&choice.shipping_address, &choice.estimate_country) {
            (Some(a), _) => {
                let errors = a.errors(&settings.ships_to_countries);
                if !errors.is_empty() {
                    return Err(field_errors(&errors));
                }
                shipping_address =
                    Some(serde_json::to_value(a.clone().normalized()).unwrap_or_default());
            }
            (None, Some(c)) => {
                let c = c.trim().to_ascii_uppercase();
                if c.len() != 2 {
                    return Err(AppError::BadRequest("invalid country".into()));
                }
                shipping_address = Some(serde_json::json!({ "estimate_country": c }));
            }
            (None, None) => {}
        },
        FulfillmentMethod::Pickup => {
            let id = match choice.pickup_location_id {
                Some(id) => id,
                None => sqlx::query_scalar!(
                    "SELECT id FROM stock_locations WHERE is_active AND is_pickup_point ORDER BY sort_order LIMIT 1"
                )
                .fetch_optional(&mut *conn)
                .await?
                .ok_or_else(|| AppError::domain(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "no_pickup_point", "no pickup point"))?,
            };
            let ok = sqlx::query_scalar!(
                "SELECT EXISTS (SELECT 1 FROM stock_locations WHERE id = $1 AND is_active AND is_pickup_point) AS \"x!\"",
                id
            )
            .fetch_one(&mut *conn)
            .await?;
            if !ok {
                return Err(AppError::BadRequest("not a pickup point".into()));
            }
            pickup = Some(id);
        }
        FulfillmentMethod::StayDelivery => {
            if !settings.stay_delivery_enabled {
                return Err(AppError::domain(
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    "stay_delivery_disabled",
                    "delivery to the tent is not offered",
                ));
            }
            let (Some(reference), Some(last_name)) = (&choice.booking_reference, &choice.last_name)
            else {
                return Err(field_errors(&[
                    ("booking_reference", "required"),
                    ("last_name", "required"),
                ]));
            };
            booking_id = sqlx::query_scalar!(
                "SELECT b.id FROM bookings b JOIN customers c ON c.id = b.customer_id
                 WHERE b.reference = upper(trim($1)) AND lower(trim(c.last_name)) = lower(trim($2))
                   AND b.status IN ('confirmed', 'checked_in') AND b.departure_date >= $3",
                reference,
                last_name,
                crate::camp_time::today(now)
            )
            .fetch_optional(&mut *conn)
            .await?;
            if booking_id.is_none() {
                return Err(AppError::domain(
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    "booking_not_found",
                    "no current stay matches this reference and last name",
                ));
            }
        }
    }
    let email = choice
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .or_else(|| cart.email.clone());
    sqlx::query!(
        "UPDATE carts SET fulfillment_method = $2, shipping_address = $3, billing_address = $4, shipping_rate_ids = $5,
                pickup_location_id = $6, booking_id = $7, email = $8::text::citext, relay_point = $9
         WHERE id = $1",
        cart.id,
        choice.method.as_str(),
        shipping_address,
        choice.billing_address.as_ref().map(|a| serde_json::to_value(a.clone().normalized()).unwrap_or_default()),
        &choice.shipping_rate_ids,
        pickup,
        booking_id,
        email,
        relay_point
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub fn field_errors(errors: &[(&str, &str)]) -> AppError {
    let map: serde_json::Map<String, Value> = errors
        .iter()
        .map(|(f, c)| ((*f).to_string(), Value::String((*c).to_string())))
        .collect();
    AppError::domain(
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_fields",
        "some fields are invalid",
    )
    .with_details(serde_json::json!({ "fields": map }))
}

pub async fn set_codes(
    conn: &mut PgConnection,
    cart_id: Uuid,
    codes: &[String],
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE carts SET promotion_codes = $2::text[]::citext[] WHERE id = $1",
        cart_id,
        codes
    )
    .execute(conn)
    .await?;
    Ok(())
}

// ---- The re-priced view --------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct CartItemView {
    pub id: Uuid,
    pub variant_id: Uuid,
    pub product_id: Uuid,
    pub name: String,
    pub slug: String,
    pub options_label: String,
    pub sku: String,
    pub image_url: Option<String>,
    pub quantity: i32,
    pub unit_price_minor: i64,
    pub compare_at_price_minor: Option<i64>,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub total_minor: i64,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CartView {
    pub id: Uuid,
    pub currency: String,
    pub items: Vec<CartItemView>,
    pub item_count: i64,
    pub email: Option<String>,
    pub fulfillment_method: Option<FulfillmentMethod>,
    pub shipping_address: Option<Address>,
    pub estimate_country: Option<String>,
    pub pickup_location_id: Option<Uuid>,
    pub relay_point: Option<super::relay::RelayPoint>,
    pub stay_booking: bool,
    pub promotion_codes: Vec<String>,
    pub promotions: Vec<super::pricing::AppliedPromotion>,
    pub shipping_lines: Vec<ShippingLine>,
    pub totals: Totals,
    /// Issues not tied to a line (address, minimum, promotion codes...).
    pub issues: Vec<Issue>,
    pub can_checkout: bool,
    pub payment_methods: Vec<ShopPaymentMethod>,
}

/// Re-prices the cart (every response returns this).
pub async fn view(
    conn: &mut PgConnection,
    settings: &ShopSettings,
    cart: &CartRow,
    payment_method: Option<ShopPaymentMethod>,
    card_available: bool,
    public_url: &(dyn Fn(&str) -> String + Sync),
    now: DateTime<Utc>,
) -> sqlx::Result<(CartView, Quote)> {
    let lines = items(conn, cart.id).await?;
    let loaded = quote::load(
        conn,
        settings,
        &lines,
        &cart.fulfillment(),
        &cart.promotion_codes,
        &cart.buyer(),
        payment_method,
        &cart.locale,
        now,
    )
    .await?;
    let q = loaded.price();
    let items_view = q
        .lines
        .iter()
        .map(|l| {
            let f = loaded.facts.get(&l.variant_id);
            CartItemView {
                id: l.line_id,
                variant_id: l.variant_id,
                product_id: l.product_id,
                name: f.map(|f| f.name.clone()).unwrap_or_default(),
                slug: f.map(|f| f.slug.clone()).unwrap_or_default(),
                options_label: f.map(|f| f.options_label.clone()).unwrap_or_default(),
                sku: f.map(|f| f.sku.clone()).unwrap_or_default(),
                image_url: f.and_then(|f| f.image_key.as_deref()).map(public_url),
                quantity: l.quantity,
                unit_price_minor: l.unit_price_minor,
                compare_at_price_minor: l.compare_at_price_minor,
                subtotal_minor: l.subtotal_minor,
                discount_minor: l.discount_minor,
                total_minor: l.total_minor,
                issues: q
                    .issues
                    .iter()
                    .filter(|i| i.line_id == Some(l.line_id))
                    .cloned()
                    .collect(),
            }
        })
        .collect();
    // Checkout needs contact details too, but the cart can be "ready" without the address yet.
    let blocking: Vec<&Issue> = q
        .blocking_issues()
        .into_iter()
        .filter(|i| {
            !matches!(
                i.code,
                super::pricing::IssueCode::AddressRequired
                    | super::pricing::IssueCode::FulfillmentRequired
            )
        })
        .collect();
    let view = CartView {
        id: cart.id,
        currency: cart.currency.clone(),
        items: items_view,
        item_count: q.lines.iter().map(|l| i64::from(l.quantity)).sum(),
        email: cart.email.clone(),
        fulfillment_method: cart.fulfillment_method,
        shipping_address: cart.shipping_address.clone(),
        estimate_country: cart.estimate_country.clone(),
        pickup_location_id: cart.pickup_location_id,
        relay_point: cart.relay_point.clone(),
        stay_booking: cart.booking_id.is_some(),
        promotion_codes: cart.promotion_codes.clone(),
        promotions: q.promotions.clone(),
        shipping_lines: q.shipping_lines.clone(),
        totals: q.totals.clone(),
        issues: q
            .issues
            .iter()
            .filter(|i| i.line_id.is_none())
            .cloned()
            .collect(),
        can_checkout: settings.is_open() && !q.lines.is_empty() && blocking.is_empty(),
        payment_methods: available_methods(settings, card_available, cart, &q),
    };
    Ok((view, q))
}

/// Payment methods offered for this cart (enabled, and applicable to its fulfilment).
pub fn available_methods(
    settings: &ShopSettings,
    card_available: bool,
    cart: &CartRow,
    q: &Quote,
) -> Vec<ShopPaymentMethod> {
    settings
        .enabled_methods(card_available)
        .into_iter()
        .filter(|m| match m {
            ShopPaymentMethod::CashOnDelivery => {
                matches!(
                    cart.fulfillment_method,
                    Some(FulfillmentMethod::Shipping | FulfillmentMethod::RelayPoint)
                ) && !q.shipping_lines.is_empty()
                    && q.shipping_lines.iter().all(|l| l.allows_cash_on_delivery)
                    && settings
                        .payments
                        .cod_max_order_minor
                        .is_none_or(|max| q.totals.total_minor <= max)
            }
            ShopPaymentMethod::PayAtPickup => matches!(
                cart.fulfillment_method,
                Some(FulfillmentMethod::Pickup | FulfillmentMethod::StayDelivery)
            ),
            _ => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_validated() {
        let mut a = Address {
            first_name: "Amina".into(),
            last_name: "Ait".into(),
            line1: "Rue 1".into(),
            city: "Rissani".into(),
            country_code: "MA".into(),
            phone: "+212 600 000 000".into(),
            postal_code: Some("52450".into()),
            ..Address::default()
        };
        assert!(a.errors(&["MA".into()]).is_empty());
        a.country_code = "FR".into();
        assert_eq!(
            a.errors(&["MA".into()]),
            vec![("country_code", "not_shipped")]
        );
        a.country_code = "MA".into();
        a.phone = "12".into();
        a.city = " ".into();
        assert_eq!(
            a.errors(&[]),
            vec![("city", "required"), ("phone", "invalid")]
        );
    }

    #[test]
    fn tokens_are_random_and_only_their_hash_is_kept() {
        let (a, ha) = new_token();
        let (b, _) = new_token();
        assert_ne!(a, b);
        assert_eq!(ha, hash_token(&a));
        assert_eq!(a.len(), 43);
    }
}
