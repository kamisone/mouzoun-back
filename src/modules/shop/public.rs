//! Public shop API (`/v1/shop/...`). Catalog reads work while `shop.status` is `catalog_only`;
//! the cart, checkout and order endpoints answer 404 unless the shop is `open` (order pages of
//! existing orders stay readable). Every cart response is the full re-priced cart.

use std::time::Duration as StdDuration;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::GuestUser,
    error::{AppError, AppResult},
    services::{
        audit::Actor,
        shop::categories as shop_categories,
        shop::{
            cart::{self, CartRow, FulfillmentChoice},
            catalog::{self, ListQuery},
            checkout::{self, CheckoutEnv, CheckoutInput},
            links,
            orders::{self, ActorRef, ReturnRequest},
            settings::{self, ShopSettings, ShopStatus},
            webhooks,
        },
    },
    state::AppState,
    web::{
        client::{ClientInfo, RequestLocale},
        limits::{idempotency_key, idempotent, rate_limit},
    },
};

const CATALOG_TTL: StdDuration = StdDuration::from_secs(600);
pub const CATALOG_CACHE_PREFIX: &str = crate::services::cache_invalidation::SHOP_CATALOG_PREFIX;

pub async fn shop_settings(state: &AppState) -> AppResult<ShopSettings> {
    if let Some(s) = state.cache.get_json::<ShopSettings>("settings:shop").await {
        return Ok(s);
    }
    let s = settings::load(&state.db).await?;
    state
        .cache
        .set_json("settings:shop", &s, StdDuration::from_secs(60))
        .await;
    Ok(s)
}

fn require_visible(s: &ShopSettings) -> AppResult<()> {
    if s.is_visible() {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

fn require_open(s: &ShopSettings) -> AppResult<()> {
    if s.is_open() {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

fn public_url(state: &AppState) -> impl Fn(&str) -> String + Sync + '_ {
    move |key: &str| state.media.public_url(key)
}

async fn default_locale(state: &AppState) -> AppResult<String> {
    Ok(
        sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
            .fetch_optional(&state.db)
            .await?
            .unwrap_or_else(|| "fr".into()),
    )
}

// ---- Config and catalog -------------------------------------------------------------------------

pub async fn config(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    let pickup_points = sqlx::query!(
        r#"SELECT s.id, COALESCE(t.name, d.name, s.code::text) AS "name!", COALESCE(t.pickup_instructions, d.pickup_instructions) AS instructions
           FROM stock_locations s
           LEFT JOIN stock_location_translations t ON t.location_id = s.id AND t.locale = $1
           LEFT JOIN stock_location_translations d ON d.location_id = s.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE s.is_active AND s.is_pickup_point ORDER BY s.sort_order"#,
        locale.as_str()
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|p| json!({ "id": p.id, "name": p.name, "instructions": p.instructions }))
    .collect::<Vec<_>>();
    let embedded = s.payments.stripe_checkout_mode == "embedded";
    let mut conn = state.db.acquire().await?;
    let entity_country = cart::entity_country(&mut conn, &s).await?;
    let terms_revision =
        crate::services::shop::checkout::current_terms_revision(&mut conn, &s).await?;
    let seller = sqlx::query!(
        r#"SELECT legal_name, legal_form, registration->>'siren' AS siren, address FROM legal_entities WHERE id = shop_entity_id()"#
    )
    .fetch_optional(&mut *conn)
    .await?
    .map(|e| json!({ "legal_name": e.legal_name, "legal_form": e.legal_form, "siren": e.siren,
                     "address": crate::services::shop::invoices::address_line(&e.address) }));
    // Legal pages: path in the request's language (default language as fallback).
    let mut legal_pages = serde_json::Map::new();
    for (key, page) in &s.legal_pages {
        let slug = sqlx::query_scalar!(
            "SELECT t.slug FROM pages p JOIN page_translations t ON t.page_id = p.id
             WHERE p.id = $1 AND p.status = 'published' AND p.deleted_at IS NULL
             ORDER BY (t.locale = $2) DESC, (t.locale = (SELECT code FROM locales WHERE is_default)) DESC LIMIT 1",
            page,
            locale.as_str()
        )
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(slug) = slug {
            legal_pages.insert(key.clone(), json!(slug));
        }
    }
    Ok(Json(json!({
        "fulfillment_methods": cart::methods_for(&entity_country, &s),
        "checkout_button_label": ShopSettings::localized(&s.checkout_button_label, locale.as_str()),
        "terms_revision_id": terms_revision,
        "seller": seller,
        "legal_pages": legal_pages,
        "consumer_mediator": s.consumer_mediator,
        "refund_deadline_days": s.refund_deadline_days,
        "legal_guarantee_years": s.legal_guarantee_years,
        "relay_carriers": ["mondial_relay", "colissimo"],
        "status": s.status,
        "currency": s.currency,
        "prices_include_tax": s.prices_include_tax,
        "guest_checkout": s.guest_checkout,
        "payment_methods": s.enabled_methods(state.shop_payments.card_available()),
        "stripe_checkout_mode": s.payments.stripe_checkout_mode,
        "stripe_publishable_key": if embedded { state.config.booking.stripe.as_ref().map(|c| c.publishable_key.clone()) } else { None },
        "ships_to_countries": s.ships_to_countries,
        "stay_delivery_enabled": s.stay_delivery_enabled && entity_country != "FR",
        "stay_delivery_fee_minor": s.stay_delivery_fee_minor,
        "withdrawal_days": s.withdrawal_days,
        "min_order_minor": s.min_order_minor,
        "max_quantity_per_line": s.max_quantity_per_line,
        "route_prefix": s.route_prefix,
        "pickup_points": if entity_country == "FR" { vec![] } else { pickup_points },
        "fake_payments": state.shop_payments.get("fake").is_some(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct RelayQuery {
    pub postal_code: String,
    #[serde(default = "fr")]
    pub country: String,
    #[serde(default = "mondial_relay")]
    pub carrier: String,
}

fn fr() -> String {
    "FR".into()
}

fn mondial_relay() -> String {
    "mondial_relay".into()
}

/// Relay points near a postal code (the picker of the checkout).
pub async fn relay_points(Query(q): Query<RelayQuery>) -> AppResult<Json<Value>> {
    if !matches!(q.carrier.as_str(), "mondial_relay" | "colissimo") {
        return Err(AppError::BadRequest("unknown carrier".into()));
    }
    let points = crate::services::shop::relay::provider()
        .search(&q.carrier, &q.country, &q.postal_code)
        .await
        .map_err(AppError::Internal)?;
    Ok(Json(json!({ "data": points })))
}

pub async fn products(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Query(q): Query<ListQuery>,
    uri: axum::http::Uri,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_visible(&s)?;
    let key = format!(
        "{CATALOG_CACHE_PREFIX}list:{}:{}",
        locale.as_str(),
        uri.query().unwrap_or("")
    );
    if let Some(cached) = state.cache.get_json::<Value>(&key).await {
        return Ok(Json(cached));
    }
    let list = catalog::listing(&state, &s, &q, locale.as_str()).await?;
    let value = serde_json::to_value(&list).unwrap_or_default();
    state.cache.set_json(&key, &value, CATALOG_TTL).await;
    Ok(Json(value))
}

pub async fn product(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Path(slug): Path<String>,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_visible(&s)?;
    let key = format!("{CATALOG_CACHE_PREFIX}product:{}:{slug}", locale.as_str());
    if let Some(cached) = state.cache.get_json::<Value>(&key).await {
        return Ok(Json(cached));
    }
    let mut conn = state.db.acquire().await?;
    let p = catalog::detail(
        &mut conn,
        &slug,
        locale.as_str(),
        s.low_stock_threshold,
        s.max_quantity_per_line,
        &public_url(&state),
    )
    .await?
    .ok_or(AppError::NotFound)?;
    state.cache.set_json(&key, &p, CATALOG_TTL).await;
    Ok(Json(p))
}

/// Visible shop categories (active, under active parents) in order, with their published
/// product counts including subcategories. `in_menu` is false for categories hidden from the
/// shop sidebar and menus (their page still works).
pub async fn categories(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
) -> AppResult<Json<Value>> {
    require_visible(&shop_settings(&state).await?)?;
    let key = format!("{CATALOG_CACHE_PREFIX}categories:{}", locale.as_str());
    if let Some(cached) = state.cache.get_json::<Value>(&key).await {
        return Ok(Json(cached));
    }
    let rows = sqlx::query!(
        r#"WITH vis AS (SELECT * FROM shop_visible_categories()),
           counts AS (
               SELECT a.id, count(DISTINCT pc.product_id) AS n
               FROM vis a JOIN vis d ON a.id = ANY(d.path)
               JOIN product_categories pc ON pc.category_id = d.id
               JOIN products p ON p.id = pc.product_id AND p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL
               GROUP BY a.id)
           SELECT c.id, c.parent_id, c.icon, COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!",
                  COALESCE(t.description, d.description) AS description, m.object_key AS "image_key?",
                  COALESCE(t.seo_title, d.seo_title) AS seo_title, COALESCE(t.seo_description, d.seo_description) AS seo_description,
                  (SELECT jsonb_object_agg(locale, slug) FROM category_translations WHERE category_id = c.id) AS slugs,
                  v.in_menu AS "in_menu!", v.depth AS "depth!", COALESCE(n.n, 0) AS "product_count!"
           FROM vis v JOIN categories c ON c.id = v.id
           LEFT JOIN counts n ON n.id = c.id
           LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $1
           LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN media_assets m ON m.id = c.cover_media_id AND m.deleted_at IS NULL
           ORDER BY c.sort_order, 4"#,
        locale.as_str()
    )
    .fetch_all(&state.db)
    .await?;
    let value = json!({ "data": rows.iter().map(|c| json!({
        "id": c.id, "parent_id": c.parent_id, "icon": c.icon, "name": c.name, "slug": c.slug, "description": c.description,
        "image_url": c.image_key.as_deref().map(|k| state.media.public_url(k)), "seo_title": c.seo_title,
        "seo_description": c.seo_description, "slugs": c.slugs, "in_menu": c.in_menu, "depth": c.depth,
        "product_count": c.product_count,
    })).collect::<Vec<_>>() });
    state.cache.set_json(&key, &value, CATALOG_TTL).await;
    Ok(Json(value))
}

/// A category page: `{category}` (cover, description, breadcrumbs, subcategories, SEO), or
/// `{redirect}` with the path to send the visitor to (301): an inactive category goes to its
/// nearest visible parent or the shop home. Old addresses (`redirects`) never reach this page:
/// the Nuxt server answers them first (`modules::seo`).
#[allow(clippy::too_many_lines)]
pub async fn category(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Path(slug): Path<String>,
) -> AppResult<Json<Value>> {
    require_visible(&shop_settings(&state).await?)?;
    let locale = locale.as_str();
    let key = format!("{CATALOG_CACHE_PREFIX}category:{locale}:{slug}");
    if let Some(cached) = state.cache.get_json::<Value>(&key).await {
        return Ok(Json(cached));
    }
    let mut conn = state.db.acquire().await?;
    let default_locale = shop_categories::default_locale(&mut conn).await?;
    let found = sqlx::query!(
        "SELECT c.id, c.parent_id FROM categories c JOIN category_translations t ON t.category_id = c.id
         WHERE c.scope = 'shop' AND t.slug = $1 ORDER BY (t.locale = $2) DESC LIMIT 1",
        slug,
        locale
    )
    .fetch_optional(&mut *conn)
    .await?;
    // Old addresses are answered by the site-wide redirect layer before this page renders.
    let found = found.ok_or(AppError::NotFound)?;
    let visible: Option<(Vec<Uuid>, bool)> = sqlx::query!(
        r#"SELECT path AS "path!", in_menu AS "in_menu!" FROM shop_visible_categories() WHERE id = $1"#,
        found.id
    )
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| (r.path, r.in_menu));
    let Some((path, in_menu)) = visible else {
        // Hidden: the nearest visible ancestor, else the shop home.
        let target = sqlx::query_scalar!(
            r#"WITH RECURSIVE up AS (
                   SELECT parent_id, 1 AS n FROM categories WHERE id = $1
                   UNION ALL
                   SELECT c.parent_id, up.n + 1 FROM categories c JOIN up ON c.id = up.parent_id WHERE up.n < 20)
               SELECT COALESCE(t.slug, d.slug) AS "slug!" FROM up
               JOIN shop_visible_categories() v ON v.id = up.parent_id
               LEFT JOIN category_translations t ON t.category_id = v.id AND t.locale = $2
               LEFT JOIN category_translations d ON d.category_id = v.id AND d.locale = $3
               ORDER BY up.n LIMIT 1"#,
            found.id,
            locale,
            default_locale
        )
        .fetch_optional(&mut *conn)
        .await?;
        let value = json!({ "redirect": shop_categories::shop_path(locale, &default_locale, target.as_deref()) });
        state.cache.set_json(&key, &value, CATALOG_TTL).await;
        return Ok(Json(value));
    };
    let c = sqlx::query!(
        r#"SELECT c.id, c.icon, c.default_sort, COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!",
                  COALESCE(t.description, d.description) AS description,
                  COALESCE(t.seo_title, d.seo_title) AS seo_title, COALESCE(t.seo_description, d.seo_description) AS seo_description,
                  (SELECT jsonb_object_agg(locale, slug) FROM category_translations WHERE category_id = c.id) AS "slugs!",
                  c.cover_media_id
           FROM categories c
           LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $2
           LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = $3
           WHERE c.id = $1"#,
        found.id,
        locale,
        default_locale
    )
    .fetch_one(&mut *conn)
    .await?;
    let crumbs = sqlx::query!(
        r#"SELECT c.id, COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!"
           FROM unnest($1::uuid[]) WITH ORDINALITY AS u(id, n) JOIN categories c ON c.id = u.id
           LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $2
           LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = $3
           ORDER BY u.n"#,
        &path,
        locale,
        default_locale
    )
    .fetch_all(&mut *conn)
    .await?;
    let children = sqlx::query!(
        r#"WITH vis AS (SELECT * FROM shop_visible_categories())
           SELECT c.id, COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!",
                  (SELECT count(DISTINCT pc.product_id) FROM vis x JOIN product_categories pc ON pc.category_id = x.id
                   JOIN products p ON p.id = pc.product_id AND p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL
                   WHERE c.id = ANY(x.path)) AS "count!"
           FROM vis v JOIN categories c ON c.id = v.id
           LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $2
           LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = $3
           WHERE v.parent_id = $1 AND v.in_menu ORDER BY c.sort_order, 2"#,
        found.id,
        locale,
        default_locale
    )
    .fetch_all(&mut *conn)
    .await?;
    let count = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT pc.product_id) AS "n!" FROM shop_visible_categories() x
           JOIN product_categories pc ON pc.category_id = x.id
           JOIN products p ON p.id = pc.product_id AND p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL
           WHERE $1 = ANY(x.path)"#,
        found.id
    )
    .fetch_one(&mut *conn)
    .await?;
    // The cover as page sections get media: variants (srcset), blur-up, focal point, alt.
    let cover = match c.cover_media_id {
        Some(m) => {
            let all: Vec<String> = sqlx::query_scalar!("SELECT code FROM locales")
                .fetch_all(&mut *conn)
                .await?;
            let lang = crate::modules::pages::public::Lang {
                locale,
                default: &default_locale,
                all: &all,
            };
            crate::modules::pages::public::media_views(&state, &[m], &lang)
                .await?
                .remove(&m)
        }
        None => None,
    };
    let value = json!({ "category": {
        "id": c.id, "name": c.name, "slug": c.slug, "slugs": c.slugs, "icon": c.icon, "description": c.description,
        "seo_title": c.seo_title, "seo_description": c.seo_description, "default_sort": c.default_sort, "in_menu": in_menu,
        "cover": cover,
        "breadcrumbs": crumbs.iter().map(|b| json!({ "id": b.id, "name": b.name, "slug": b.slug })).collect::<Vec<_>>(),
        "children": children.iter().map(|b| json!({ "id": b.id, "name": b.name, "slug": b.slug, "product_count": b.count })).collect::<Vec<_>>(),
        "product_count": count,
        "indexable": count > 0,
    }});
    state.cache.set_json(&key, &value, CATALOG_TTL).await;
    Ok(Json(value))
}

// ---- Cart ------------------------------------------------------------------------------------------

fn cookie_name(secure: bool) -> &'static str {
    if secure {
        "__Host-mz_shop_cart"
    } else {
        "mz_shop_cart"
    }
}

fn cart_cookie(state: &AppState, token: String, days: i64) -> Cookie<'static> {
    Cookie::build((cookie_name(state.config.cookie_secure), token))
        .http_only(true)
        .secure(state.config.cookie_secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(days))
        .build()
}

async fn signed_in_customer(
    state: &AppState,
    guest: Option<&GuestUser>,
) -> AppResult<Option<Uuid>> {
    let Some(g) = guest else { return Ok(None) };
    Ok(sqlx::query_scalar!(
        "SELECT id FROM customers WHERE user_id = $1 AND deleted_at IS NULL",
        g.user_id
    )
    .fetch_optional(&state.db)
    .await?)
}

/// The cart of this visitor, if any.
async fn current_cart(state: &AppState, jar: &CookieJar) -> AppResult<Option<CartRow>> {
    let Some(token) = jar
        .get(cookie_name(state.config.cookie_secure))
        .map(|c| c.value().to_string())
    else {
        return Ok(None);
    };
    if token.len() != 43 {
        return Ok(None);
    }
    let mut conn = state.db.acquire().await?;
    Ok(cart::find(&mut conn, &token, Utc::now()).await?)
}

/// The cart, created (and its cookie set) when missing.
async fn cart_or_create(
    state: &AppState,
    jar: CookieJar,
    s: &ShopSettings,
    locale: &str,
    guest: Option<&GuestUser>,
) -> AppResult<(CartRow, CookieJar)> {
    if let Some(c) = current_cart(state, &jar).await? {
        return Ok((c, jar));
    }
    let customer = signed_in_customer(state, guest).await?;
    let mut conn = state.db.acquire().await?;
    let (c, token) = cart::create(&mut conn, s, locale, customer, Utc::now()).await?;
    Ok((c, jar.add(cart_cookie(state, token, s.cart_ttl_days))))
}

async fn cart_view(state: &AppState, s: &ShopSettings, c: &CartRow) -> AppResult<Value> {
    let mut conn = state.db.acquire().await?;
    let (view, _) = cart::view(
        &mut conn,
        s,
        c,
        None,
        state.shop_payments.card_available(),
        &public_url(state),
        Utc::now(),
    )
    .await?;
    Ok(serde_json::to_value(view).unwrap_or_default())
}

fn empty_cart(s: &ShopSettings) -> Value {
    json!({ "id": null, "currency": s.currency, "items": [], "item_count": 0, "promotion_codes": [], "promotions": [],
            "shipping_lines": [], "issues": [], "can_checkout": false, "payment_methods": [],
            "totals": { "subtotal_minor": 0, "discount_minor": 0, "shipping_minor": 0, "shipping_discount_minor": 0,
                        "tax_minor": 0, "cod_fee_minor": 0, "total_minor": 0, "taxes_included": s.prices_include_tax } })
}

pub async fn get_cart(State(state): State<AppState>, jar: CookieJar) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    match current_cart(&state, &jar).await? {
        Some(c) => Ok(Json(cart_view(&state, &s, &c).await?)),
        None => Ok(Json(empty_cart(&s))),
    }
}

pub async fn create_cart(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    guest: Option<Extension<GuestUser>>,
    jar: CookieJar,
) -> AppResult<(CookieJar, Json<Value>)> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    let (c, jar) = cart_or_create(&state, jar, &s, locale.as_str(), guest.as_deref()).await?;
    let view = cart_view(&state, &s, &c).await?;
    Ok((jar, Json(view)))
}

#[derive(Debug, Deserialize)]
pub struct AddItem {
    pub variant_id: Uuid,
    #[serde(default = "one")]
    pub quantity: i32,
}

fn one() -> i32 {
    1
}

pub async fn add_item(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    guest: Option<Extension<GuestUser>>,
    client: ClientInfo,
    jar: CookieJar,
    Json(body): Json<AddItem>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    rate_limit(
        &state.cache,
        "shop_cart",
        &client.key(),
        120,
        StdDuration::from_secs(60),
    )
    .await?;
    let (c, jar) = cart_or_create(&state, jar, &s, locale.as_str(), guest.as_deref()).await?;
    let mut conn = state.db.acquire().await?;
    cart::add_item(
        &mut conn,
        c.id,
        body.variant_id,
        body.quantity,
        s.max_quantity_per_line,
    )
    .await?;
    cart::touch(&mut conn, c.id, &s, Utc::now()).await?;
    drop(conn);
    Ok((jar, Json(cart_view(&state, &s, &c).await?)))
}

#[derive(Debug, Deserialize)]
pub struct SetQuantity {
    pub quantity: i32,
}

pub async fn update_item(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
    Json(body): Json<SetQuantity>,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    let c = current_cart(&state, &jar)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut conn = state.db.acquire().await?;
    cart::set_quantity(&mut conn, c.id, id, body.quantity, s.max_quantity_per_line).await?;
    cart::touch(&mut conn, c.id, &s, Utc::now()).await?;
    drop(conn);
    Ok(Json(cart_view(&state, &s, &c).await?))
}

pub async fn remove_item(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    let c = current_cart(&state, &jar)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut conn = state.db.acquire().await?;
    cart::set_quantity(&mut conn, c.id, id, 0, s.max_quantity_per_line).await?;
    drop(conn);
    Ok(Json(cart_view(&state, &s, &c).await?))
}

pub async fn set_fulfillment(
    State(state): State<AppState>,
    client: ClientInfo,
    jar: CookieJar,
    Json(body): Json<FulfillmentChoice>,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    let c = current_cart(&state, &jar)
        .await?
        .ok_or(AppError::NotFound)?;
    if body.booking_reference.is_some() {
        // Booking reference + last name: a guessable pair, rate limited.
        rate_limit(
            &state.cache,
            "shop_stay",
            &client.key(),
            10,
            StdDuration::from_secs(600),
        )
        .await?;
    }
    let mut conn = state.db.acquire().await?;
    cart::set_fulfillment(&mut conn, &c, &s, &body, Utc::now()).await?;
    let c = cart::load(&mut conn, c.id).await?;
    drop(conn);
    Ok(Json(cart_view(&state, &s, &c).await?))
}

#[derive(Debug, Deserialize)]
pub struct PromoBody {
    pub code: String,
}

pub async fn add_promotion(
    State(state): State<AppState>,
    client: ClientInfo,
    jar: CookieJar,
    Json(body): Json<PromoBody>,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    rate_limit(
        &state.cache,
        "shop_promo",
        &client.key(),
        20,
        StdDuration::from_secs(600),
    )
    .await?;
    let c = current_cart(&state, &jar)
        .await?
        .ok_or(AppError::NotFound)?;
    let code = body.code.trim().to_string();
    if code.is_empty() || code.len() > 64 {
        return Err(AppError::BadRequest("invalid code".into()));
    }
    let mut codes = c.promotion_codes.clone();
    if !codes.iter().any(|x| x.eq_ignore_ascii_case(&code)) {
        codes.push(code);
    }
    codes.truncate(5);
    let mut conn = state.db.acquire().await?;
    cart::set_codes(&mut conn, c.id, &codes).await?;
    let c = cart::load(&mut conn, c.id).await?;
    drop(conn);
    Ok(Json(cart_view(&state, &s, &c).await?))
}

pub async fn remove_promotion(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(code): Path<String>,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    let c = current_cart(&state, &jar)
        .await?
        .ok_or(AppError::NotFound)?;
    let codes: Vec<String> = c
        .promotion_codes
        .iter()
        .filter(|x| !x.eq_ignore_ascii_case(&code))
        .cloned()
        .collect();
    let mut conn = state.db.acquire().await?;
    cart::set_codes(&mut conn, c.id, &codes).await?;
    let c = cart::load(&mut conn, c.id).await?;
    drop(conn);
    Ok(Json(cart_view(&state, &s, &c).await?))
}

pub async fn shipping_options(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    let c = current_cart(&state, &jar)
        .await?
        .ok_or(AppError::NotFound)?;
    let view = cart_view(&state, &s, &c).await?;
    Ok(Json(
        json!({ "shipping_lines": view["shipping_lines"], "issues": view["issues"] }),
    ))
}

// ---- Checkout -------------------------------------------------------------------------------

pub async fn checkout_handler(
    State(state): State<AppState>,
    guest: Option<Extension<GuestUser>>,
    client: ClientInfo,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<CheckoutInput>,
) -> AppResult<Json<Value>> {
    let s = shop_settings(&state).await?;
    require_open(&s)?;
    let key = idempotency_key(&headers)
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key header required".into()))?;
    rate_limit(
        &state.cache,
        "shop_checkout",
        &client.key(),
        10,
        StdDuration::from_secs(60),
    )
    .await?;
    rate_limit(
        &state.cache,
        "shop_checkout_email",
        &body.email.trim().to_lowercase(),
        10,
        StdDuration::from_secs(600),
    )
    .await?;
    let c = current_cart(&state, &jar).await?;
    let replayed = sqlx::query_scalar!("SELECT cart_id FROM orders WHERE checkout_key = $1", key)
        .fetch_optional(&state.db)
        .await?
        .flatten();
    let cart_id = c.map(|c| c.id).or(replayed).ok_or_else(|| {
        AppError::domain(
            StatusCode::CONFLICT,
            "cart_empty",
            "the cart is empty or expired",
        )
    })?;
    let customer = signed_in_customer(&state, guest.as_deref()).await?;
    let default_locale = default_locale(&state).await?;
    let pu = public_url(&state);
    let env = CheckoutEnv {
        pool: &state.db,
        payments: &state.shop_payments,
        settings: &s,
        site_url: &state.config.email.public_site_url,
        secret: &state.config.jwt_secret,
        default_locale: &default_locale,
        public_url: &pu,
        ip: client.ip_network(),
        user_agent: client.user_agent.clone(),
        now: Utc::now(),
    };
    let result = idempotent(&state.cache, "shop_checkout", Some(key.clone()), || async {
        let r = checkout::checkout(&env, cart_id, customer, &body, &key).await?;
        let token = links::order_token(&state.config.jwt_secret, &r.number);
        Ok(json!({ "order": r, "access_token": token }))
    })
    .await?;
    Ok(Json(result))
}

// ---- Orders (guest) -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct OrderAccess {
    pub email: Option<String>,
    pub t: Option<String>,
}

/// The order of `number` if the caller may see it: signed link token, email, or the signed-in
/// customer who placed it. Unknown and forbidden look the same (404).
async fn authorize_order(
    state: &AppState,
    number: &str,
    access: &OrderAccess,
    guest: Option<&GuestUser>,
    client: &ClientInfo,
) -> AppResult<Uuid> {
    rate_limit(
        &state.cache,
        "shop_order_lookup",
        &client.key(),
        30,
        StdDuration::from_secs(60),
    )
    .await?;
    let row = sqlx::query!(
        r#"SELECT o.id, o.email::text AS "email!", c.user_id FROM orders o JOIN customers c ON c.id = o.customer_id
           WHERE o.number = $1 AND o.status <> 'draft'"#,
        number
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let by_token = access
        .t
        .as_deref()
        .is_some_and(|t| links::order_token_valid(&state.config.jwt_secret, number, t));
    let by_email = access.email.as_deref().is_some_and(|e| {
        crate::services::accounts::tokens::constant_time_eq(
            e.trim().to_lowercase().as_bytes(),
            row.email.to_lowercase().as_bytes(),
        )
    });
    let by_account = guest.is_some_and(|g| row.user_id == Some(g.user_id));
    if by_token || by_email || by_account {
        Ok(row.id)
    } else {
        Err(AppError::NotFound)
    }
}

/// The customer-facing view of an order (no internal notes, costs or risk).
pub async fn order_view(
    state: &AppState,
    order_id: Uuid,
    withdrawal_days: i64,
) -> AppResult<Value> {
    let o = sqlx::query!(
        r#"SELECT o.id, o.number, o.status, o.payment_status, o.fulfillment_status, o.payment_method, o.fulfillment_method,
                  o.requires_confirmation, o.confirmed_at, o.email::text AS "email!", o.phone, o.locale, o.currency::text AS "currency!",
                  o.shipping_address, o.pickup_location_id, o.shipping_lines, o.subtotal_minor, o.discount_minor, o.shipping_minor,
                  o.shipping_discount_minor, o.tax_minor, o.cod_fee_minor, o.total_minor, o.taxes_included, o.amount_paid_minor,
                  o.amount_refunded_minor, o.placed_at, o.paid_at, o.cancelled_at, o.cancel_reason, o.created_at,
                  (SELECT max(COALESCE(delivered_at, shipped_at)) FROM fulfillments f WHERE f.order_id = o.id AND f.status <> 'cancelled') AS delivered_at,
                  (SELECT number FROM invoices WHERE order_id = o.id AND kind = 'invoice') AS invoice_number,
                  (SELECT max(expires_at) FROM inventory_reservations r WHERE r.order_id = o.id AND released_at IS NULL AND consumed_at IS NULL) AS hold_until
           FROM orders o WHERE o.id = $1"#,
        order_id
    )
    .fetch_one(&state.db)
    .await?;
    let items = sqlx::query!(
        r#"SELECT id, sku, quantity, unit_price_minor, subtotal_minor, discount_minor, total_minor, product_snapshot,
                  fulfilled_quantity, returned_quantity, refunded_quantity
           FROM order_items WHERE order_id = $1 ORDER BY sort_order"#,
        order_id
    )
    .fetch_all(&state.db)
    .await?;
    let secret = links::pickup_secret(&state.config.jwt_secret);
    let fulfillments = sqlx::query!(
        r#"SELECT id, method, status, carrier, tracking_number, tracking_url, shipped_at, delivered_at, created_at
           FROM fulfillments WHERE order_id = $1 AND status <> 'cancelled' ORDER BY created_at"#,
        order_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|f| {
        let code = (f.method == "pickup" && !matches!(f.status.as_str(), "picked_up" | "failed"))
            .then(|| orders::pickup_code(&secret, f.id));
        json!({ "id": f.id, "method": f.method, "status": f.status, "carrier": f.carrier, "tracking_number": f.tracking_number,
                "tracking_url": f.tracking_url, "shipped_at": f.shipped_at, "delivered_at": f.delivered_at, "pickup_code": code })
    })
    .collect::<Vec<_>>();
    let events = sqlx::query!(
        "SELECT kind, to_status, created_at FROM order_events WHERE order_id = $1
         AND kind IN ('placed', 'payment_succeeded', 'confirmed', 'fulfillment_created', 'fulfillment_updated', 'cancelled',
                      'refund_created', 'return_requested', 'return_updated')
         ORDER BY created_at, id",
        order_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|e| json!({ "kind": e.kind, "status": e.to_status, "at": e.created_at }))
    .collect::<Vec<_>>();
    let returns = sqlx::query!(
        "SELECT id, number, status, reason, requested_at FROM order_returns WHERE order_id = $1 ORDER BY requested_at",
        order_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| json!({ "id": r.id, "number": r.number, "status": r.status, "reason": r.reason, "requested_at": r.requested_at }))
    .collect::<Vec<_>>();
    let pickup = match o.pickup_location_id {
        Some(id) => sqlx::query!(
            r#"SELECT COALESCE(t.name, s.code::text) AS "name!", t.pickup_instructions FROM stock_locations s
               LEFT JOIN stock_location_translations t ON t.location_id = s.id AND t.locale = $2 WHERE s.id = $1"#,
            id,
            o.locale
        )
        .fetch_optional(&state.db)
        .await?
        .map(|p| json!({ "name": p.name, "instructions": p.pickup_instructions })),
        None => None,
    };
    let return_until = o
        .delivered_at
        .map(|d| d + chrono::Duration::days(withdrawal_days));
    let returnable = return_until.is_some_and(|u| Utc::now() <= u)
        && items
            .iter()
            .any(|i| i.fulfilled_quantity > i.returned_quantity);
    let bank = if o.payment_method.as_deref() == Some("bank_transfer") && o.payment_status != "paid"
    {
        let s = shop_settings(state).await?;
        Some(ShopSettings::localized(
            &s.payments.bank_transfer_details,
            &o.locale,
        ))
    } else {
        None
    };
    let payment_pending_url = if o.payment_method.as_deref() == Some("stripe")
        && o.status == "pending"
    {
        sqlx::query_scalar!(
            "SELECT provider_payload->>'url' FROM payments WHERE order_id = $1 AND status = 'pending' ORDER BY created_at DESC LIMIT 1",
            order_id
        )
        .fetch_optional(&state.db)
        .await?
        .flatten()
    } else {
        None
    };
    Ok(json!({
        "id": o.id, "number": o.number, "status": o.status, "payment_status": o.payment_status,
        "fulfillment_status": o.fulfillment_status, "payment_method": o.payment_method, "fulfillment_method": o.fulfillment_method,
        "requires_confirmation": o.requires_confirmation, "confirmed": o.confirmed_at.is_some(),
        "email": o.email, "phone": o.phone, "locale": o.locale, "currency": o.currency,
        "shipping_address": o.shipping_address, "pickup_location": pickup, "shipping_lines": o.shipping_lines,
        "totals": { "subtotal_minor": o.subtotal_minor, "discount_minor": o.discount_minor, "shipping_minor": o.shipping_minor,
                    "shipping_discount_minor": o.shipping_discount_minor, "tax_minor": o.tax_minor, "cod_fee_minor": o.cod_fee_minor,
                    "total_minor": o.total_minor, "taxes_included": o.taxes_included },
        "amount_paid_minor": o.amount_paid_minor, "amount_refunded_minor": o.amount_refunded_minor,
        "placed_at": o.placed_at, "paid_at": o.paid_at, "cancelled_at": o.cancelled_at, "cancel_reason": o.cancel_reason,
        "hold_until": o.hold_until,
        "items": items.iter().map(|i| json!({
            "id": i.id, "sku": i.sku, "quantity": i.quantity, "unit_price_minor": i.unit_price_minor,
            "subtotal_minor": i.subtotal_minor, "discount_minor": i.discount_minor, "total_minor": i.total_minor,
            "name": i.product_snapshot["name"], "options_label": i.product_snapshot["options_label"],
            "image_url": i.product_snapshot["image_url"], "slug": i.product_snapshot["slug"],
            "withdrawal_exemption": i.product_snapshot["withdrawal_exemption"].as_str().unwrap_or("none"),
            "returnable": withdrawable(&i.product_snapshot),
            "fulfilled_quantity": i.fulfilled_quantity, "returned_quantity": i.returned_quantity, "refunded_quantity": i.refunded_quantity,
        })).collect::<Vec<_>>(),
        "fulfillments": fulfillments,
        "timeline": events,
        "returns": returns,
        "can_request_return": returnable,
        "return_until": return_until,
        "invoice_number": o.invoice_number,
        "bank_transfer_details": bank,
        "payment_url": payment_pending_url,
    }))
}

pub async fn get_order(
    State(state): State<AppState>,
    guest: Option<Extension<GuestUser>>,
    client: ClientInfo,
    Path(number): Path<String>,
    Query(access): Query<OrderAccess>,
) -> AppResult<Json<Value>> {
    let id = authorize_order(&state, &number, &access, guest.as_deref(), &client).await?;
    let s = shop_settings(&state).await?;
    Ok(Json(order_view(&state, id, s.withdrawal_days).await?))
}

/// Polled by the success page until the webhook marks the order paid (the redirect itself
/// never changes anything).
pub async fn order_status(
    State(state): State<AppState>,
    guest: Option<Extension<GuestUser>>,
    client: ClientInfo,
    Path(number): Path<String>,
    Query(access): Query<OrderAccess>,
) -> AppResult<Json<Value>> {
    let id = authorize_order(&state, &number, &access, guest.as_deref(), &client).await?;
    let o = sqlx::query!(
        "SELECT status, payment_status, fulfillment_status FROM orders WHERE id = $1",
        id
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(
        json!({ "status": o.status, "payment_status": o.payment_status, "fulfillment_status": o.fulfillment_status }),
    ))
}

/// Whether the right of withdrawal can apply (hygiene goods: while sealed).
fn withdrawable(snapshot: &Value) -> bool {
    matches!(
        snapshot["withdrawal_exemption"].as_str(),
        None | Some("none" | "unsealed_hygiene")
    )
}

pub async fn request_return(
    State(state): State<AppState>,
    guest: Option<Extension<GuestUser>>,
    client: ClientInfo,
    Path(number): Path<String>,
    Query(access): Query<OrderAccess>,
    Json(body): Json<ReturnRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = authorize_order(&state, &number, &access, guest.as_deref(), &client).await?;
    let s = shop_settings(&state).await?;
    let actor = ActorRef::customer(client.actor(Actor::customer()));
    let mut tx = state.db.begin().await?;
    let return_id = orders::request_return(
        &mut tx,
        id,
        &body,
        Some(orders::ReturnPolicy {
            withdrawal_days: s.withdrawal_days,
            refund_deadline_days: s.refund_deadline_days,
            guarantee_years: s.legal_guarantee_years,
        }),
        &actor,
        Utc::now(),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "return_id": return_id }))))
}

/// Printable invoice (HTML) of the order.
pub async fn invoice(
    State(state): State<AppState>,
    guest: Option<Extension<GuestUser>>,
    client: ClientInfo,
    Path(number): Path<String>,
    Query(access): Query<OrderAccess>,
) -> AppResult<axum::response::Html<String>> {
    let id = authorize_order(&state, &number, &access, guest.as_deref(), &client).await?;
    super::documents::invoice_html(&state, id)
        .await
        .map(axum::response::Html)
}

// ---- Notifications ------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StockNotificationBody {
    pub variant_id: Uuid,
    pub email: String,
}

pub async fn stock_notification(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    guest: Option<Extension<GuestUser>>,
    client: ClientInfo,
    Json(body): Json<StockNotificationBody>,
) -> AppResult<StatusCode> {
    require_visible(&shop_settings(&state).await?)?;
    rate_limit(
        &state.cache,
        "shop_notify",
        &client.key(),
        10,
        StdDuration::from_secs(600),
    )
    .await?;
    let email = body.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return Err(cart::field_errors(&[("email", "invalid")]));
    }
    let customer = signed_in_customer(&state, guest.as_deref()).await?;
    let exists = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM product_variants WHERE id = $1 AND deleted_at IS NULL) AS \"x!\"",
        body.variant_id
    )
    .fetch_one(&state.db)
    .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    sqlx::query!(
        "INSERT INTO stock_notifications (id, variant_id, email, locale, customer_id) VALUES ($1, $2, $3::text::citext, $4, $5)
         ON CONFLICT (variant_id, email) WHERE notified_at IS NULL DO NOTHING",
        Uuid::now_v7(),
        body.variant_id,
        email,
        locale.as_str(),
        customer
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct NotifyMeBody {
    pub email: String,
}

/// "Shop opens soon" or "tell me when this category has products": the address joins the
/// newsletter list (source `shop`), confirmed by email (double opt-in through the existing
/// newsletter flow).
pub async fn notify_me(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    Json(body): Json<NotifyMeBody>,
) -> AppResult<StatusCode> {
    let s = shop_settings(&state).await?;
    // "Opens soon" (catalog only) and "tell me when this category has products" (open shop).
    if s.status == ShopStatus::Closed {
        return Err(AppError::NotFound);
    }
    rate_limit(
        &state.cache,
        "shop_notify",
        &client.key(),
        10,
        StdDuration::from_secs(600),
    )
    .await?;
    let email = body.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return Err(cart::field_errors(&[("email", "invalid")]));
    }
    let (raw, hash) = cart::new_token();
    let mut tx = state.db.begin().await?;
    let id = sqlx::query_scalar!(
        "INSERT INTO newsletter_subscribers (id, email, locale, status, source, confirm_token_hash)
         VALUES ($1, $2::text::citext, $3, 'pending', 'shop', $4)
         ON CONFLICT (email) DO NOTHING RETURNING id",
        Uuid::now_v7(),
        email,
        locale.as_str(),
        hash
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(subscriber_id) = id {
        crate::services::outbox::enqueue(
            &mut tx,
            &crate::services::outbox::Event::NewsletterSubscribed {
                subscriber_id,
                token: raw,
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Webhooks and the fake provider --------------------------------------------------------------

/// `PAYMENTS_FAKE=true` only: plays Stripe's part for a fake checkout session (`paid` |
/// `failed` | `expired`), through the same webhook pipeline (stored, queued, applied).
pub async fn fake_outcome(
    State(state): State<AppState>,
    Path((session, outcome)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    if state.shop_payments.get("fake").is_none() || !session.starts_with("fake_cs_") {
        return Err(AppError::NotFound);
    }
    let p = sqlx::query!(
        r#"SELECT p.id, p.amount_minor, p.currency::text AS "currency!", p.order_id AS "order_id!" FROM payments p
           WHERE p.provider_session_id = $1 AND p.order_id IS NOT NULL"#,
        session
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let (kind, extra) = match outcome.as_str() {
        "paid" => (
            "checkout.session.completed",
            json!({ "payment_status": "paid" }),
        ),
        "failed" => (
            "checkout.session.async_payment_failed",
            json!({ "payment_status": "unpaid" }),
        ),
        "expired" => (
            "checkout.session.expired",
            json!({ "payment_status": "unpaid" }),
        ),
        _ => return Err(AppError::BadRequest("paid, failed or expired".into())),
    };
    let mut object = json!({
        "id": session, "object": "checkout.session", "amount_total": p.amount_minor, "currency": p.currency.to_lowercase(),
        "payment_intent": format!("fake_pi_{}", p.id.simple()),
        "metadata": { "order_id": p.order_id, "payment_id": p.id },
    });
    if let (Some(o), Some(e)) = (object.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    let event = json!({ "id": format!("evt_fake_{}_{outcome}", p.id.simple()), "type": kind, "data": { "object": object } });
    webhooks::ingest(&state.db, "fake", &event).await?;
    // Development convenience: apply it right away (the worker would too).
    let _ = webhooks::process(
        &state.db,
        &state.shop_payments,
        "fake",
        event["id"].as_str().unwrap_or_default(),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

/// Stripe events of shop orders, received on the shared Stripe endpoint (see
/// `modules::booking::webhooks::receive`): stored and queued, answered 200 immediately.
pub async fn receive_stripe_shop_event(state: &AppState, event: &Value) -> AppResult<bool> {
    if !webhooks::is_shop_event(&state.db, event).await? {
        return Ok(false);
    }
    let fresh = webhooks::ingest(&state.db, "stripe", event).await?;
    if !fresh {
        tracing::info!(event_id = %event["id"], "duplicate shop webhook ignored");
    }
    Ok(true)
}
