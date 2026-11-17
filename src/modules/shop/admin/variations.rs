//! The variation library (`/v1/admin/shop/variations`), modelled on vitecamio's variant
//! attributes: reusable dimensions ("Poids", "Conditionnement", "Couleur") with their values,
//! display type, colour swatches and price adjustments. Products link them from the product
//! editor (`catalog::set_options`); every edit here is copied onto the linked product options
//! (`catalog::sync_linked`). Codes never change once created (they appear in variant SKUs and
//! storefront URLs); deleting something a product uses is refused, deactivating is allowed.

use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::catalog::{DisplayType, changed, conflict, invalid, slugify, sync_linked, valid_color};
use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::audit,
    state::AppState,
};

async fn attribute_json(conn: &mut PgConnection, id: Option<Uuid>) -> sqlx::Result<Vec<Value>> {
    let rows = sqlx::query!(
        r#"SELECT a.id, a.code::text AS "code!", a.admin_label, a.display_type, a.category_id, a.sort_order, a.is_active,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM variation_attribute_translations WHERE attribute_id = a.id), '{}') AS "names!",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('id', v.id, 'code', v.code::text, 'swatch_color', v.swatch_color,
                            'swatch_media_id', v.swatch_media_id, 'price_adjustment_minor', v.price_adjustment_minor,
                            'sort_order', v.sort_order, 'is_active', v.is_active,
                            'product_count', (SELECT count(DISTINCT o.product_id) FROM product_option_values pv
                                              JOIN product_options o ON o.id = pv.option_id WHERE pv.variation_value_id = v.id),
                            'labels', COALESCE((SELECT jsonb_object_agg(locale, label) FROM variation_value_translations WHERE value_id = v.id), '{}'))
                            ORDER BY v.sort_order, v.code) FROM variation_values v WHERE v.attribute_id = a.id), '[]') AS "values!",
                  (SELECT count(*) FROM product_options o JOIN products p ON p.id = o.product_id
                   WHERE o.attribute_id = a.id AND p.deleted_at IS NULL) AS "products!"
           FROM variation_attributes a WHERE $1::uuid IS NULL OR a.id = $1
           ORDER BY a.sort_order, a.code"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|a| {
            json!({ "id": a.id, "code": a.code, "admin_label": a.admin_label, "display_type": a.display_type,
                    "category_id": a.category_id, "sort_order": a.sort_order, "is_active": a.is_active,
                    "names": a.names, "values": a.values, "product_count": a.products })
        })
        .collect())
}

pub async fn list(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    Ok(Json(
        json!({ "data": attribute_json(&mut conn, None).await? }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ValueInput {
    /// Existing value; without it a new value is created.
    pub id: Option<Uuid>,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    pub swatch_color: Option<String>,
    pub swatch_media_id: Option<Uuid>,
    pub price_adjustment_minor: Option<i64>,
    #[serde(default = "yes")]
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
pub struct AttributeInput {
    /// Set on creation only (default: from the French name).
    #[serde(default)]
    pub code: String,
    pub admin_label: Option<String>,
    #[serde(default = "button")]
    pub display_type: DisplayType,
    pub category_id: Option<Uuid>,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default = "yes")]
    pub is_active: bool,
    /// `{locale: name}`; French is required.
    pub names: HashMap<String, String>,
    /// In display order; values left out are deleted (refused while a product uses them).
    pub values: Vec<ValueInput>,
}

fn yes() -> bool {
    true
}

fn button() -> DisplayType {
    DisplayType::Button
}

fn code_of(raw: &str, fallback: &str) -> String {
    let source = if raw.trim().is_empty() { fallback } else { raw };
    slugify(source).replace('-', "_")
}

async fn default_locale(conn: &mut PgConnection) -> sqlx::Result<String> {
    sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
        .fetch_optional(conn)
        .await
        .map(|c| c.unwrap_or_else(|| "fr".into()))
}

fn unique_violation(e: sqlx::Error) -> AppError {
    match e
        .as_database_error()
        .and_then(|d| d.constraint().map(str::to_string))
    {
        Some(c) if c.contains("admin_label") => {
            conflict("admin_label_taken", "this internal label is already used")
        }
        Some(c) if c.contains("code") => conflict("code_taken", "this code is already used"),
        _ => e.into(),
    }
}

async fn save(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    a: &AttributeInput,
) -> AppResult<Value> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let main = default_locale(&mut tx).await?;
    let name = a.names.get(&main).map(|n| n.trim()).unwrap_or("");
    if name.is_empty() {
        return Err(invalid("the name in the default language is required"));
    }
    if a.values.is_empty() {
        return Err(invalid("a variation needs at least one value"));
    }
    let admin_label = a
        .admin_label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let (id, before) = match id {
        Some(id) => {
            let before = attribute_json(&mut tx, Some(id)).await?;
            if before.is_empty() {
                return Err(AppError::NotFound);
            }
            sqlx::query!(
                "UPDATE variation_attributes SET admin_label = $2, display_type = $3, category_id = $4, sort_order = $5, is_active = $6 WHERE id = $1",
                id,
                admin_label,
                a.display_type.as_str(),
                a.category_id,
                a.sort_order,
                a.is_active
            )
            .execute(&mut *tx)
            .await
            .map_err(unique_violation)?;
            (id, before.into_iter().next())
        }
        None => {
            let code = code_of(&a.code, name);
            if code.is_empty() || !code.starts_with(|c: char| c.is_ascii_lowercase()) {
                return Err(invalid("the code starts with a letter (a-z)"));
            }
            let id = Uuid::now_v7();
            sqlx::query!(
                "INSERT INTO variation_attributes (id, code, admin_label, display_type, category_id, sort_order, is_active)
                 VALUES ($1, $2::text::citext, $3, $4, $5, $6, $7)",
                id,
                code,
                admin_label,
                a.display_type.as_str(),
                a.category_id,
                a.sort_order,
                a.is_active
            )
            .execute(&mut *tx)
            .await
            .map_err(unique_violation)?;
            (id, None)
        }
    };
    sqlx::query!(
        "DELETE FROM variation_attribute_translations WHERE attribute_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;
    for (locale, n) in a.names.iter().filter(|(_, n)| !n.trim().is_empty()) {
        sqlx::query!(
            "INSERT INTO variation_attribute_translations (attribute_id, locale, name) VALUES ($1, $2, $3)",
            id,
            locale,
            n.trim()
        )
        .execute(&mut *tx)
        .await?;
    }

    // Values: removed ones first (refused while a product uses them), then upserts in order.
    let kept: Vec<Uuid> = a.values.iter().filter_map(|v| v.id).collect();
    let used = sqlx::query_scalar!(
        r#"SELECT v.code::text AS "code!" FROM variation_values v
           WHERE v.attribute_id = $1 AND NOT (v.id = ANY($2))
             AND EXISTS (SELECT 1 FROM product_option_values pv WHERE pv.variation_value_id = v.id)"#,
        id,
        &kept
    )
    .fetch_all(&mut *tx)
    .await?;
    if !used.is_empty() {
        return Err(conflict(
            "variation_in_use",
            &format!(
                "still used by products: {} (deactivate instead)",
                used.join(", ")
            ),
        ));
    }
    sqlx::query!(
        "DELETE FROM variation_values WHERE attribute_id = $1 AND NOT (id = ANY($2))",
        id,
        &kept
    )
    .execute(&mut *tx)
    .await?;
    let mut codes = HashSet::new();
    for (i, v) in a.values.iter().enumerate() {
        let label = v.labels.get(&main).map(|l| l.trim()).unwrap_or("");
        if label.is_empty() {
            return Err(invalid(
                "every value needs its label in the default language",
            ));
        }
        let color = valid_color(&v.swatch_color)?;
        let adjustment = v.price_adjustment_minor.filter(|x| *x != 0);
        let vid = match v.id {
            Some(vid) => {
                let code = sqlx::query_scalar!(
                    r#"UPDATE variation_values SET swatch_color = $3, swatch_media_id = $4, price_adjustment_minor = $5, sort_order = $6, is_active = $7
                       WHERE id = $1 AND attribute_id = $2 RETURNING code::text AS "code!""#,
                    vid,
                    id,
                    color,
                    v.swatch_media_id,
                    adjustment,
                    i as i32,
                    v.is_active
                )
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| invalid("this value does not belong to the variation"))?;
                codes.insert(code);
                vid
            }
            None => {
                let code = code_of(&v.code, label);
                if code.is_empty() || !codes.insert(code.clone()) {
                    return Err(invalid("value codes must be unique and not empty"));
                }
                let vid = Uuid::now_v7();
                sqlx::query!(
                    "INSERT INTO variation_values (id, attribute_id, code, swatch_color, swatch_media_id, price_adjustment_minor, sort_order, is_active)
                     VALUES ($1, $2, $3::text::citext, $4, $5, $6, $7, $8)",
                    vid,
                    id,
                    code,
                    color,
                    v.swatch_media_id,
                    adjustment,
                    i as i32,
                    v.is_active
                )
                .execute(&mut *tx)
                .await
                .map_err(unique_violation)?;
                vid
            }
        };
        sqlx::query!(
            "DELETE FROM variation_value_translations WHERE value_id = $1",
            vid
        )
        .execute(&mut *tx)
        .await?;
        for (locale, l) in v.labels.iter().filter(|(_, l)| !l.trim().is_empty()) {
            sqlx::query!(
                "INSERT INTO variation_value_translations (value_id, locale, label) VALUES ($1, $2, $3)",
                vid,
                locale,
                l.trim()
            )
            .execute(&mut *tx)
            .await?;
        }
    }

    let products = sync_linked(&mut tx, id).await?;
    for p in &products {
        changed(&mut tx, *p).await?;
    }
    let after = attribute_json(&mut tx, Some(id))
        .await?
        .into_iter()
        .next()
        .unwrap_or(Value::Null);
    audit::record(
        &mut tx,
        &staff.actor(),
        if before.is_some() {
            "variation.updated"
        } else {
            "variation.created"
        },
        "variation",
        &id.to_string(),
        json!({ "before": before, "after": after, "products_updated": products.len() }),
    )
    .await?;
    tx.commit().await?;
    Ok(after)
}

pub async fn create(
    State(state): State<AppState>,
    staff: Staff,
    Json(a): Json<AttributeInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(save(&state, &staff, None, &a).await?),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(a): Json<AttributeInput>,
) -> AppResult<Json<Value>> {
    Ok(Json(save(&state, &staff, Some(id), &a).await?))
}

pub async fn delete(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let before = attribute_json(&mut tx, Some(id)).await?;
    if before.is_empty() {
        return Err(AppError::NotFound);
    }
    let used = sqlx::query_scalar!(
        "SELECT count(DISTINCT product_id) FROM product_options WHERE attribute_id = $1",
        id
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(0);
    if used > 0 {
        return Err(conflict(
            "variation_in_use",
            &format!("still used by {used} product(s): unlink it there or deactivate it"),
        ));
    }
    sqlx::query!("DELETE FROM variation_attributes WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "variation.deleted",
        "variation",
        &id.to_string(),
        json!({ "before": before[0] }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
