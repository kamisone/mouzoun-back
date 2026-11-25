//! Admin pricing (`shop.pricing`; reads `shop.read`): shop promotions, shipping profiles,
//! zones and rates, taxes (percent rates used by shop products).

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::Staff,
    db::{
        pricing::DiscountType,
        shop::{CustomerEligibility, PromotionScope, ShippingRateKind},
    },
    error::{AppError, AppResult},
    services::{
        audit,
        shop::events::{self, ShopEvent},
    },
    state::AppState,
};

fn invalid(message: &str) -> AppError {
    AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message)
}

fn db_error(e: sqlx::Error) -> AppError {
    match e.as_database_error() {
        Some(d) if d.is_unique_violation() => AppError::domain(
            StatusCode::CONFLICT,
            "code_taken",
            "this code is already used",
        ),
        Some(d) if d.code().is_some_and(|c| c == "23514") => {
            invalid("a value is out of range or inconsistent")
        }
        Some(d) if d.is_foreign_key_violation() => AppError::domain(
            StatusCode::CONFLICT,
            "in_use",
            "this item is used elsewhere",
        ),
        _ => e.into(),
    }
}

// ---- Promotions -------------------------------------------------------------------------------

pub async fn promotions(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT p.id, p.code::text AS code, p.scope, p.discount_type, p.discount_percent, p.discount_amount_minor, p.min_subtotal_minor,
                  p.min_quantity, lower(p.booking_window) AS starts_at, upper(p.booking_window) AS ends_at, p.applies_to_all,
                  p.applies_to_shipping, p.is_stackable, p.is_active, p.customer_eligibility, p.max_redemptions, p.max_per_customer,
                  COALESCE((SELECT array_agg(product_id) FROM promotion_products WHERE promotion_id = p.id), '{}') AS "product_ids!",
                  COALESCE((SELECT array_agg(category_id) FROM promotion_categories WHERE promotion_id = p.id), '{}') AS "category_ids!",
                  COALESCE((SELECT jsonb_object_agg(locale, jsonb_build_object('name', name, 'description', description))
                            FROM promotion_translations WHERE promotion_id = p.id), '{}') AS "translations!",
                  (SELECT count(*) FROM promotion_redemptions r WHERE r.promotion_id = p.id AND r.order_id IS NOT NULL) AS "redemptions!"
           FROM promotions p WHERE p.scope IN ('shop', 'all') ORDER BY p.is_active DESC, p.created_at DESC"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|p| json!({
        "id": p.id, "code": p.code, "scope": p.scope, "discount_type": p.discount_type, "percent": p.discount_percent,
        "amount_minor": p.discount_amount_minor, "min_subtotal_minor": p.min_subtotal_minor, "min_quantity": p.min_quantity,
        "starts_at": p.starts_at, "ends_at": p.ends_at, "applies_to_all": p.applies_to_all, "applies_to_shipping": p.applies_to_shipping,
        "is_stackable": p.is_stackable, "is_active": p.is_active, "customer_eligibility": p.customer_eligibility,
        "max_redemptions": p.max_redemptions, "max_per_customer": p.max_per_customer, "product_ids": p.product_ids,
        "category_ids": p.category_ids, "translations": p.translations, "redemptions": p.redemptions,
    })).collect::<Vec<_>>() })))
}

#[derive(Debug, Deserialize)]
pub struct PromotionInput {
    pub code: Option<String>,
    #[serde(default = "shop_scope")]
    pub scope: PromotionScope,
    pub discount_type: DiscountType,
    pub percent: Option<Decimal>,
    pub amount_minor: Option<i64>,
    pub min_subtotal_minor: Option<i64>,
    pub min_quantity: Option<i32>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    #[serde(default = "yes")]
    pub applies_to_all: bool,
    #[serde(default)]
    pub applies_to_shipping: bool,
    #[serde(default)]
    pub is_stackable: bool,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default = "all_customers")]
    pub customer_eligibility: CustomerEligibility,
    pub max_redemptions: Option<i32>,
    pub max_per_customer: Option<i32>,
    #[serde(default)]
    pub product_ids: Vec<Uuid>,
    #[serde(default)]
    pub category_ids: Vec<Uuid>,
    /// `{locale: {name, description}}`.
    #[serde(default)]
    pub translations: HashMap<String, Value>,
}

fn shop_scope() -> PromotionScope {
    PromotionScope::Shop
}
fn yes() -> bool {
    true
}
fn all_customers() -> CustomerEligibility {
    CustomerEligibility::All
}

pub async fn save_promotion(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    p: &PromotionInput,
) -> AppResult<Uuid> {
    staff.require("shop.pricing")?;
    if p.scope == PromotionScope::Booking {
        return Err(invalid(
            "booking promotions are managed in the booking back office",
        ));
    }
    if p.discount_type == DiscountType::FreeNights {
        return Err(invalid("free nights do not apply to the shop"));
    }
    let id = id.unwrap_or_else(Uuid::now_v7);
    let window = match (p.starts_at, p.ends_at) {
        (None, None) => None,
        (a, b) => Some(crate::db::types::TsRange { start: a, end: b }),
    };
    let currency = (p.amount_minor.is_some() || p.min_subtotal_minor.is_some()).then_some("EUR");
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "INSERT INTO promotions (id, code, scope, discount_type, discount_percent, discount_amount_minor, currency, min_subtotal_minor,
                                 min_quantity, booking_window, applies_to_all, applies_to_shipping, is_stackable, is_active,
                                 customer_eligibility, max_redemptions, max_per_customer)
         VALUES ($1, $2::text::citext, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
         ON CONFLICT (id) DO UPDATE SET code = $2::text::citext, scope = $3, discount_type = $4, discount_percent = $5,
             discount_amount_minor = $6, currency = $7, min_subtotal_minor = $8, min_quantity = $9, booking_window = $10,
             applies_to_all = $11, applies_to_shipping = $12, is_stackable = $13, is_active = $14, customer_eligibility = $15,
             max_redemptions = $16, max_per_customer = $17",
        id,
        p.code.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(str::to_uppercase),
        p.scope.as_str(),
        p.discount_type.as_str(),
        (p.discount_type == DiscountType::Percent).then_some(p.percent).flatten(),
        (p.discount_type == DiscountType::FixedAmount).then_some(p.amount_minor).flatten(),
        currency,
        p.min_subtotal_minor,
        p.min_quantity,
        window as Option<crate::db::types::TsRange>,
        p.applies_to_all,
        p.applies_to_shipping,
        p.is_stackable,
        p.is_active,
        p.customer_eligibility.as_str(),
        p.max_redemptions,
        p.max_per_customer
    )
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    sqlx::query!("DELETE FROM promotion_products WHERE promotion_id = $1", id)
        .execute(&mut *tx)
        .await?;
    for pid in &p.product_ids {
        sqlx::query!("INSERT INTO promotion_products (promotion_id, product_id) VALUES ($1, $2) ON CONFLICT DO NOTHING", id, pid)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query!(
        "DELETE FROM promotion_categories WHERE promotion_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;
    for c in &p.category_ids {
        sqlx::query!("INSERT INTO promotion_categories (promotion_id, category_id) VALUES ($1, $2) ON CONFLICT DO NOTHING", id, c)
            .execute(&mut *tx)
            .await?;
    }
    for (locale, t) in &p.translations {
        let Some(name) = t["name"].as_str().filter(|n| !n.trim().is_empty()) else {
            continue;
        };
        sqlx::query!(
            "INSERT INTO promotion_translations (promotion_id, locale, name, description) VALUES ($1, $2, $3, $4)
             ON CONFLICT (promotion_id, locale) DO UPDATE SET name = $3, description = $4",
            id,
            locale,
            name,
            t["description"].as_str()
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "promotion.saved",
        "promotion",
        &id.to_string(),
        json!({ "code": p.code, "active": p.is_active }),
    )
    .await?;
    events::enqueue(&mut tx, ShopEvent::PromotionUpdated { promotion_id: id }).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn create_promotion(
    State(state): State<AppState>,
    staff: Staff,
    Json(p): Json<PromotionInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = save_promotion(&state, &staff, None, &p).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn update_promotion(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(p): Json<PromotionInput>,
) -> AppResult<Json<Value>> {
    let exists = sqlx::query_scalar!("SELECT EXISTS (SELECT 1 FROM promotions WHERE id = $1 AND scope IN ('shop', 'all')) AS \"x!\"", id)
        .fetch_one(&state.db)
        .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    save_promotion(&state, &staff, Some(id), &p).await?;
    Ok(Json(json!({ "id": id })))
}

// ---- Shipping -------------------------------------------------------------------------------

pub async fn shipping(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let profiles = sqlx::query!(
        r#"SELECT id, code::text AS "code!", name, is_default,
                  (SELECT count(*) FROM shop_products WHERE shipping_profile_id = shipping_profiles.id) AS "products!"
           FROM shipping_profiles ORDER BY is_default DESC, code"#
    )
    .fetch_all(&state.db)
    .await?;
    let zones = sqlx::query!(
        r#"SELECT z.id, z.code::text AS "code!", z.country_codes::text[] AS "country_codes!", z.region_codes, z.postal_code_patterns,
                  z.is_export, z.is_active, z.sort_order,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM shipping_zone_translations WHERE zone_id = z.id), '{}') AS "names!"
           FROM shipping_zones z ORDER BY z.sort_order, z.code"#
    )
    .fetch_all(&state.db)
    .await?;
    let rates = sqlx::query!(
        r#"SELECT r.id, r.zone_id, r.profile_id, r.code::text AS "code!", r.kind, r.carrier, r.amount_minor, r.per_kg_minor,
                  r.min_weight_grams, r.max_weight_grams, r.min_subtotal_minor, r.max_subtotal_minor, r.free_over_minor,
                  r.delivery_days_min, r.delivery_days_max, r.allows_cash_on_delivery, r.cod_fee_minor, r.is_active, r.sort_order,
                  r.is_placeholder, r.delivery_type,
                  COALESCE((SELECT jsonb_object_agg(locale, jsonb_build_object('name', name, 'description', description))
                            FROM shipping_rate_translations WHERE rate_id = r.id), '{}') AS "translations!"
           FROM shipping_rates r ORDER BY r.sort_order, r.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "profiles": profiles.iter().map(|p| json!({ "id": p.id, "code": p.code, "name": p.name, "is_default": p.is_default, "product_count": p.products })).collect::<Vec<_>>(),
        "zones": zones.iter().map(|z| json!({ "id": z.id, "code": z.code, "country_codes": z.country_codes.iter().map(|c| c.trim()).collect::<Vec<_>>(),
            "region_codes": z.region_codes, "postal_code_patterns": z.postal_code_patterns, "is_export": z.is_export,
            "is_active": z.is_active, "sort_order": z.sort_order, "names": z.names })).collect::<Vec<_>>(),
        "rates": rates.iter().map(|r| json!({ "id": r.id, "zone_id": r.zone_id, "profile_id": r.profile_id, "code": r.code, "kind": r.kind,
            "carrier": r.carrier, "amount_minor": r.amount_minor, "per_kg_minor": r.per_kg_minor, "min_weight_grams": r.min_weight_grams,
            "max_weight_grams": r.max_weight_grams, "min_subtotal_minor": r.min_subtotal_minor, "max_subtotal_minor": r.max_subtotal_minor,
            "free_over_minor": r.free_over_minor, "delivery_days_min": r.delivery_days_min, "delivery_days_max": r.delivery_days_max,
            "allows_cash_on_delivery": r.allows_cash_on_delivery, "cod_fee_minor": r.cod_fee_minor, "is_active": r.is_active,
            "sort_order": r.sort_order, "is_placeholder": r.is_placeholder, "delivery_type": r.delivery_type,
            "translations": r.translations })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct ProfileInput {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
}

async fn save_profile(
    state: AppState,
    staff: Staff,
    id: Option<Uuid>,
    Json(p): Json<ProfileInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.pricing")?;
    let id = id.unwrap_or_else(Uuid::now_v7);
    let mut tx = state.db.begin().await?;
    if p.is_default {
        sqlx::query!(
            "UPDATE shipping_profiles SET is_default = false WHERE id <> $1",
            id
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        "INSERT INTO shipping_profiles (id, code, name, is_default) VALUES ($1, $2::text::citext, $3, $4)
         ON CONFLICT (id) DO UPDATE SET code = $2::text::citext, name = $3, is_default = $4",
        id,
        p.code.trim(),
        p.name.trim(),
        p.is_default
    )
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "shipping_profile.saved",
        "shipping_profile",
        &id.to_string(),
        json!({ "code": p.code }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Debug, Deserialize)]
pub struct ZoneInput {
    pub code: String,
    pub country_codes: Vec<String>,
    pub region_codes: Option<Vec<String>>,
    pub postal_code_patterns: Option<Vec<String>>,
    #[serde(default)]
    pub is_export: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub names: HashMap<String, String>,
}

/// A country belongs to at most one active zone without region / postal restrictions.
async fn save_zone(
    state: AppState,
    staff: Staff,
    id: Option<Uuid>,
    Json(z): Json<ZoneInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.pricing")?;
    let id = id.unwrap_or_else(Uuid::now_v7);
    let countries: Vec<String> = z
        .country_codes
        .iter()
        .map(|c| c.trim().to_uppercase())
        .filter(|c| c.len() == 2)
        .collect();
    if countries.is_empty() {
        return Err(invalid("list at least one ISO country code"));
    }
    let restricted = z.region_codes.as_ref().is_some_and(|r| !r.is_empty())
        || z.postal_code_patterns
            .as_ref()
            .is_some_and(|p| !p.is_empty());
    let mut tx = state.db.begin().await?;
    if z.is_active && !restricted {
        let clash = sqlx::query_scalar!(
            r#"SELECT code::text AS "code!" FROM shipping_zones
               WHERE is_active AND id <> $1 AND country_codes::text[] && $2::text[]
                 AND COALESCE(cardinality(region_codes), 0) = 0 AND COALESCE(cardinality(postal_code_patterns), 0) = 0
               LIMIT 1"#,
            id,
            &countries
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(other) = clash {
            return Err(AppError::domain(
                StatusCode::CONFLICT,
                "country_in_other_zone",
                "a country is already in another active zone",
            )
            .with_details(json!({ "zone": other })));
        }
    }
    sqlx::query!(
        "INSERT INTO shipping_zones (id, code, country_codes, region_codes, postal_code_patterns, is_export, is_active, sort_order)
         VALUES ($1, $2::text::citext, $3::text[]::char(2)[], $4, $5, $6, $7, $8)
         ON CONFLICT (id) DO UPDATE SET code = $2::text::citext, country_codes = $3::text[]::char(2)[], region_codes = $4,
             postal_code_patterns = $5, is_export = $6, is_active = $7, sort_order = $8",
        id,
        z.code.trim(),
        &countries,
        z.region_codes.as_deref(),
        z.postal_code_patterns.as_deref(),
        z.is_export,
        z.is_active,
        z.sort_order
    )
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    for (locale, name) in &z.names {
        sqlx::query!(
            "INSERT INTO shipping_zone_translations (zone_id, locale, name) VALUES ($1, $2, $3)
             ON CONFLICT (zone_id, locale) DO UPDATE SET name = $3",
            id,
            locale,
            name
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "shipping_zone.saved",
        "shipping_zone",
        &id.to_string(),
        json!({ "countries": countries }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Debug, Deserialize)]
pub struct RateInput {
    pub zone_id: Uuid,
    pub profile_id: Uuid,
    pub code: String,
    pub kind: ShippingRateKind,
    pub carrier: Option<String>,
    pub amount_minor: Option<i64>,
    pub per_kg_minor: Option<i64>,
    pub min_weight_grams: Option<i32>,
    pub max_weight_grams: Option<i32>,
    pub min_subtotal_minor: Option<i64>,
    pub max_subtotal_minor: Option<i64>,
    pub free_over_minor: Option<i64>,
    pub delivery_days_min: Option<i32>,
    pub delivery_days_max: Option<i32>,
    #[serde(default)]
    pub allows_cash_on_delivery: bool,
    pub cod_fee_minor: Option<i64>,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub sort_order: i32,
    /// `{locale: {name, description}}`.
    #[serde(default)]
    pub translations: HashMap<String, Value>,
}

async fn save_rate(
    state: AppState,
    staff: Staff,
    id: Option<Uuid>,
    Json(r): Json<RateInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.pricing")?;
    let id = id.unwrap_or_else(Uuid::now_v7);
    let carriers = crate::services::shop::settings::load(&state.db)
        .await?
        .carriers;
    if let Some(c) = r.carrier.as_deref().filter(|c| !c.is_empty())
        && !carriers.iter().any(|x| x == c)
    {
        return Err(invalid("choose a carrier from the shop settings list"));
    }
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "INSERT INTO shipping_rates (id, zone_id, profile_id, code, kind, carrier, amount_minor, per_kg_minor, min_weight_grams,
                                     max_weight_grams, min_subtotal_minor, max_subtotal_minor, free_over_minor, delivery_days_min,
                                     delivery_days_max, allows_cash_on_delivery, cod_fee_minor, is_active, sort_order)
         VALUES ($1, $2, $3, $4::text::citext, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
         ON CONFLICT (id) DO UPDATE SET zone_id = $2, profile_id = $3, code = $4::text::citext, kind = $5, carrier = $6,
             amount_minor = $7, per_kg_minor = $8, min_weight_grams = $9, max_weight_grams = $10, min_subtotal_minor = $11,
             max_subtotal_minor = $12, free_over_minor = $13, delivery_days_min = $14, delivery_days_max = $15,
             allows_cash_on_delivery = $16, cod_fee_minor = $17, is_active = $18, sort_order = $19,
             is_placeholder = false",
        id,
        r.zone_id,
        r.profile_id,
        r.code.trim(),
        r.kind.as_str(),
        r.carrier.as_deref().filter(|c| !c.is_empty()),
        r.amount_minor,
        r.per_kg_minor,
        r.min_weight_grams,
        r.max_weight_grams,
        r.min_subtotal_minor,
        r.max_subtotal_minor,
        r.free_over_minor,
        r.delivery_days_min,
        r.delivery_days_max,
        r.allows_cash_on_delivery,
        r.cod_fee_minor,
        r.is_active,
        r.sort_order
    )
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    for (locale, t) in &r.translations {
        let Some(name) = t["name"].as_str().filter(|n| !n.trim().is_empty()) else {
            continue;
        };
        sqlx::query!(
            "INSERT INTO shipping_rate_translations (rate_id, locale, name, description) VALUES ($1, $2, $3, $4)
             ON CONFLICT (rate_id, locale) DO UPDATE SET name = $3, description = $4",
            id,
            locale,
            name,
            t["description"].as_str()
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "shipping_rate.saved",
        "shipping_rate",
        &id.to_string(),
        json!({ "code": r.code, "kind": r.kind }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_rate(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("shop.pricing")?;
    let mut tx = state.db.begin().await?;
    let n = sqlx::query!("DELETE FROM shipping_rates WHERE id = $1", id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "shipping_rate.deleted",
        "shipping_rate",
        &id.to_string(),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Taxes --------------------------------------------------------------------------------------

pub async fn taxes(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT t.id, t.code::text AS "code!", t.rate_percent, t.is_active, t.is_inclusive,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM tax_translations WHERE tax_id = t.id), '{}') AS "names!",
                  (SELECT count(*) FROM product_taxes pt JOIN products p ON p.id = pt.product_id WHERE pt.tax_id = t.id AND p.kind = 'shop_product') AS "products!"
           FROM taxes t WHERE t.calc_type = 'percent' ORDER BY t.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "data": rows.iter().map(|t| json!({ "id": t.id, "code": t.code, "rate_percent": t.rate_percent,
        "is_active": t.is_active, "is_inclusive": t.is_inclusive, "names": t.names, "product_count": t.products })).collect::<Vec<_>>() }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct TaxInput {
    pub code: String,
    pub rate_percent: Decimal,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub names: HashMap<String, String>,
}

async fn save_tax(
    state: AppState,
    staff: Staff,
    id: Option<Uuid>,
    Json(t): Json<TaxInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.pricing")?;
    if t.rate_percent < Decimal::ZERO || t.rate_percent > Decimal::from(100) {
        return Err(invalid("a rate between 0 and 100 %"));
    }
    let id = id.unwrap_or_else(Uuid::now_v7);
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "INSERT INTO taxes (id, code, calc_type, rate_percent, is_inclusive, is_active) VALUES ($1, $2::text::citext, 'percent', $3, true, $4)
         ON CONFLICT (id) DO UPDATE SET code = $2::text::citext, rate_percent = $3, is_active = $4",
        id,
        t.code.trim(),
        t.rate_percent,
        t.is_active
    )
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    for (locale, name) in &t.names {
        sqlx::query!(
            "INSERT INTO tax_translations (tax_id, locale, name) VALUES ($1, $2, $3) ON CONFLICT (tax_id, locale) DO UPDATE SET name = $3",
            id,
            locale,
            name
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "tax.saved",
        "tax",
        &id.to_string(),
        json!({ "code": t.code, "rate": t.rate_percent }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id })))
}

pub async fn create_profile(
    State(state): State<AppState>,
    staff: Staff,
    body: Json<ProfileInput>,
) -> AppResult<Json<Value>> {
    save_profile(state, staff, None, body).await
}

pub async fn update_profile(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    body: Json<ProfileInput>,
) -> AppResult<Json<Value>> {
    save_profile(state, staff, Some(id), body).await
}

pub async fn create_zone(
    State(state): State<AppState>,
    staff: Staff,
    body: Json<ZoneInput>,
) -> AppResult<Json<Value>> {
    save_zone(state, staff, None, body).await
}

pub async fn update_zone(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    body: Json<ZoneInput>,
) -> AppResult<Json<Value>> {
    save_zone(state, staff, Some(id), body).await
}

pub async fn create_rate(
    State(state): State<AppState>,
    staff: Staff,
    body: Json<RateInput>,
) -> AppResult<Json<Value>> {
    save_rate(state, staff, None, body).await
}

pub async fn update_rate(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    body: Json<RateInput>,
) -> AppResult<Json<Value>> {
    save_rate(state, staff, Some(id), body).await
}

pub async fn create_tax(
    State(state): State<AppState>,
    staff: Staff,
    body: Json<TaxInput>,
) -> AppResult<Json<Value>> {
    save_tax(state, staff, None, body).await
}

pub async fn update_tax(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    body: Json<TaxInput>,
) -> AppResult<Json<Value>> {
    save_tax(state, staff, Some(id), body).await
}

#[derive(Debug, Deserialize)]
pub struct ConfirmRates {
    /// The placeholder rates the admin opened and checked (all of them are required).
    pub ids: Vec<Uuid>,
}

/// `POST /shipping/rates/confirm-placeholders`: the admin confirms the placeholder rates as real
/// prices. Every placeholder must be listed (the screen only enables the button once each row
/// was opened), so none is confirmed unseen.
pub async fn confirm_placeholders(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<ConfirmRates>,
) -> AppResult<Json<Value>> {
    staff.require("shop.pricing")?;
    let mut tx = state.db.begin().await?;
    let pending: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT id FROM shipping_rates WHERE is_placeholder ORDER BY id FOR UPDATE"
    )
    .fetch_all(&mut *tx)
    .await?;
    let unseen: Vec<&Uuid> = pending.iter().filter(|id| !body.ids.contains(id)).collect();
    if !unseen.is_empty() {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "placeholders_unseen",
            "open every placeholder rate before confirming them",
        )
        .with_details(json!({ "unseen": unseen })));
    }
    let n = sqlx::query!(
        "UPDATE shipping_rates SET is_placeholder = false WHERE is_placeholder AND id = ANY($1)",
        &body.ids
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit::record(
        &mut tx,
        &staff.actor(),
        "shipping_rates.placeholders_confirmed",
        "shipping_rate",
        "*",
        json!({ "count": n }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "confirmed": n })))
}
