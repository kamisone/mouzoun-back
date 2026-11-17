//! Shop categories (scope `shop`, a tree) and a customer's orders (customer page, orders tab).

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::catalog::slugify;
use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::{
        audit,
        outbox::{self, Event},
    },
    state::AppState,
};

pub async fn list(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT c.id, c.parent_id, c.icon, c.cover_media_id, c.sort_order, c.is_active,
                  COALESCE((SELECT jsonb_object_agg(locale, jsonb_build_object('name', name, 'slug', slug, 'description', description,
                            'seo_title', seo_title, 'seo_description', seo_description)) FROM category_translations WHERE category_id = c.id), '{}') AS "translations!",
                  (SELECT count(*) FROM product_categories pc JOIN products p ON p.id = pc.product_id
                   WHERE pc.category_id = c.id AND p.deleted_at IS NULL) AS "products!"
           FROM categories c WHERE c.scope = 'shop' ORDER BY c.sort_order"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "data": rows.iter().map(|c| json!({ "id": c.id, "parent_id": c.parent_id, "icon": c.icon,
        "cover_media_id": c.cover_media_id, "sort_order": c.sort_order, "is_active": c.is_active, "translations": c.translations,
        "product_count": c.products })).collect::<Vec<_>>() }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct CategoryInput {
    pub parent_id: Option<Uuid>,
    pub icon: Option<String>,
    pub cover_media_id: Option<Uuid>,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default = "yes")]
    pub is_active: bool,
    /// `{locale: {name, slug?, description?, seo_title?, seo_description?}}`.
    pub translations: HashMap<String, Value>,
}

fn yes() -> bool {
    true
}

async fn save(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    c: &CategoryInput,
) -> AppResult<Uuid> {
    staff.require("shop.catalog")?;
    let id = id.unwrap_or_else(Uuid::now_v7);
    if c.parent_id == Some(id) {
        return Err(AppError::BadRequest(
            "a category cannot be its own parent".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "INSERT INTO categories (id, scope, parent_id, icon, cover_media_id, sort_order, is_active) VALUES ($1, 'shop', $2, $3, $4, $5, $6)
         ON CONFLICT (id) DO UPDATE SET parent_id = $2, icon = $3, cover_media_id = $4, sort_order = $5, is_active = $6
         WHERE categories.scope = 'shop'",
        id,
        c.parent_id,
        c.icon.as_deref(),
        c.cover_media_id,
        c.sort_order,
        c.is_active
    )
    .execute(&mut *tx)
    .await?;
    for (locale, t) in &c.translations {
        let Some(name) = t["name"].as_str().map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        let slug = t["slug"]
            .as_str()
            .map(slugify)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| slugify(name));
        sqlx::query!(
            "INSERT INTO category_translations (category_id, locale, name, slug, description, seo_title, seo_description)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (category_id, locale) DO UPDATE SET name = $3, slug = $4, description = $5, seo_title = $6, seo_description = $7",
            id,
            locale,
            name,
            slug,
            t["description"].as_str(),
            t["seo_title"].as_str(),
            t["seo_description"].as_str()
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
                AppError::domain(StatusCode::CONFLICT, "slug_taken", "this address is already used")
            } else {
                e.into()
            }
        })?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "category.saved",
        "category",
        &id.to_string(),
        json!({ "scope": "shop" }),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::ContentChanged {
            ref_type: "category".into(),
            ref_id: id,
        },
    )
    .await?;
    tx.commit().await?;
    state
        .cache
        .delete_prefix(crate::services::cache_invalidation::SHOP_CATALOG_PREFIX)
        .await
        .ok();
    Ok(id)
}

pub async fn create(
    State(state): State<AppState>,
    staff: Staff,
    Json(c): Json<CategoryInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = save(&state, &staff, None, &c).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(c): Json<CategoryInput>,
) -> AppResult<Json<Value>> {
    save(&state, &staff, Some(id), &c).await?;
    Ok(Json(json!({ "id": id })))
}

pub async fn delete(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let n = sqlx::query!(
        "DELETE FROM categories WHERE id = $1 AND scope = 'shop'",
        id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "category.deleted",
        "category",
        &id.to_string(),
        json!({}),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::ContentChanged {
            ref_type: "category".into(),
            ref_id: id,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Orders of a customer (the customer page shows them next to the bookings).
pub async fn customer_orders(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    if !staff.can("shop.orders") && !staff.can("shop.refunds") {
        return Err(AppError::Forbidden);
    }
    let rows = sqlx::query!(
        r#"SELECT id, number, status, payment_status, fulfillment_status, total_minor, currency::text AS "currency!", placed_at
           FROM orders WHERE customer_id = $1 AND status <> 'draft' ORDER BY COALESCE(placed_at, created_at) DESC LIMIT 100"#,
        id
    )
    .fetch_all(&state.db)
    .await?;
    let addresses = sqlx::query!(
        "SELECT id, label, first_name, last_name, line1, line2, city, region, postal_code, country_code, phone
         FROM customer_addresses WHERE customer_id = $1 AND deleted_at IS NULL ORDER BY created_at",
        id
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "orders": rows.iter().map(|o| json!({ "id": o.id, "number": o.number, "status": o.status, "payment_status": o.payment_status,
            "fulfillment_status": o.fulfillment_status, "total_minor": o.total_minor, "currency": o.currency, "placed_at": o.placed_at })).collect::<Vec<_>>(),
        "addresses": addresses.iter().map(|a| json!({ "id": a.id, "label": a.label, "first_name": a.first_name, "last_name": a.last_name,
            "line1": a.line1, "line2": a.line2, "city": a.city, "region": a.region, "postal_code": a.postal_code,
            "country_code": a.country_code, "phone": a.phone })).collect::<Vec<_>>(),
    })))
}
