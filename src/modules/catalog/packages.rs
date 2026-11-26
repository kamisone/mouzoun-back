//! Packages: their items (stays, activities, extras) and the availability check. A package has
//! no availability of its own: "Vérifier une date" asks the booking funnel (`trip::quote`) and
//! shows which item blocks.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Fields, Kind, changed, field_error, pricing, product_of_kind};
use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::{
        booking::trip::{self, TripItem, TripRequest},
        pricing::GuestCount,
    },
    state::AppState,
};

#[derive(Debug, Clone, Deserialize)]
pub struct ItemBody {
    pub item_product_id: Uuid,
    #[serde(default = "one")]
    pub quantity: i32,
    #[serde(default)]
    pub day_number: Option<i32>,
    #[serde(default)]
    pub is_optional: bool,
}

fn one() -> i32 {
    1
}

pub struct Summary {
    pub pricing_mode: String,
    pub items: i64,
    /// Non-optional items not published.
    pub unpublished_required: i64,
    /// Items without any active price rule (sum pricing modes need one each).
    pub items_without_price: i64,
}

pub async fn summary(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Summary> {
    let r = sqlx::query!(
        r#"SELECT pk.pricing_mode,
                  (SELECT count(*) FROM package_items i WHERE i.package_product_id = pk.product_id) AS "items!",
                  (SELECT count(*) FROM package_items i JOIN products p ON p.id = i.item_product_id
                   WHERE i.package_product_id = pk.product_id AND NOT i.is_optional
                     AND (p.status <> 'published' OR p.deleted_at IS NOT NULL)) AS "unpublished!",
                  (SELECT count(*) FROM package_items i
                   WHERE i.package_product_id = pk.product_id
                     AND NOT EXISTS (SELECT 1 FROM prices pr WHERE pr.product_id = i.item_product_id AND pr.is_active)) AS "no_price!"
           FROM packages pk WHERE pk.product_id = $1"#,
        id
    )
    .fetch_one(conn)
    .await?;
    Ok(Summary {
        pricing_mode: r.pricing_mode,
        items: r.items,
        unpublished_required: r.unpublished,
        items_without_price: r.no_price,
    })
}

/// The rules of a package's content (checked by the API, whatever the editor sends).
pub async fn write_items(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    items: &[ItemBody],
) -> AppResult<()> {
    if kind != Kind::Packages {
        return Err(field_error("items", "not_allowed"));
    }
    let pk = sqlx::query!(
        "SELECT duration_days, duration_nights FROM packages WHERE product_id = $1",
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    let ids: Vec<Uuid> = items.iter().map(|i| i.item_product_id).collect();
    let kinds: HashMap<Uuid, (String, bool)> = sqlx::query!(
        "SELECT id, kind, deleted_at IS NOT NULL AS \"deleted!\" FROM products WHERE id = ANY($1)",
        &ids
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| (r.id, (r.kind, r.deleted)))
    .collect();
    let mut f = Fields::default();
    let mut stays = 0;
    for (i, it) in items.iter().enumerate() {
        let path = |field: &str| format!("items[{i}].{field}");
        match kinds.get(&it.item_product_id) {
            None => f.add(path("item_product_id"), "unknown"),
            Some((_, true)) => f.add(path("item_product_id"), "deleted"),
            Some((k, _)) if k == "package" => f.add(path("item_product_id"), "package_in_package"),
            Some((k, _)) if !["accommodation", "activity", "extra"].contains(&k.as_str()) => {
                f.add(path("item_product_id"), "not_bookable_kind");
            }
            Some((k, _)) if k == "accommodation" => {
                stays += 1;
                if stays > 1 {
                    f.add(path("item_product_id"), "one_stay_only");
                }
                match pk.duration_nights {
                    None | Some(0) => f.add("details.duration_nights", "required_with_stay"),
                    Some(n) if it.quantity != n => f.add(path("quantity"), "nights_mismatch"),
                    Some(_) => {}
                }
            }
            Some(_) => {}
        }
        if it.item_product_id == id {
            f.add(path("item_product_id"), "package_in_package");
        }
        if it.quantity < 1 {
            f.add(path("quantity"), "min");
        }
        match (it.day_number, pk.duration_days) {
            (Some(d), _) if d < 1 => f.add(path("day_number"), "range"),
            (Some(d), Some(max)) if d > max => f.add(path("day_number"), "range"),
            _ => {}
        }
    }
    f.check()?;
    sqlx::query!(
        "DELETE FROM package_items WHERE package_product_id = $1",
        id
    )
    .execute(&mut *conn)
    .await?;
    for (n, it) in items.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO package_items (package_product_id, item_product_id, quantity, day_number, is_optional, sort_order)
             VALUES ($1, $2, $3, $4, $5, $6)",
            id,
            it.item_product_id,
            it.quantity,
            it.day_number,
            it.is_optional,
            n as i32
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub async fn put_items(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(items): Json<Vec<ItemBody>>,
) -> AppResult<StatusCode> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.write")?;
    let mut tx = state.db.begin().await?;
    product_of_kind(&mut tx, kind, id).await?;
    write_items(&mut tx, kind, id, &items).await?;
    let published = sqlx::query_scalar!("SELECT status FROM products WHERE id = $1", id)
        .fetch_one(&mut *tx)
        .await?
        == "published";
    if published {
        let s = summary(&mut tx, id).await?;
        if s.unpublished_required > 0 || s.items == 0 {
            return Err(AppError::domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "publish_blocked",
                "a published package needs published items",
            ));
        }
    }
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "package.items_updated",
        json!({ "items": items.len() }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Items with their status and the price of one adult on a sample date (the funnel's engine).
pub async fn items_json(
    conn: &mut PgConnection,
    state: &AppState,
    id: Uuid,
    sample: NaiveDate,
) -> AppResult<Value> {
    let rows = sqlx::query!(
        r#"SELECT i.item_product_id, i.quantity, i.day_number, i.is_optional, i.sort_order, p.kind, p.status,
                  p.deleted_at IS NOT NULL AS "deleted!", COALESCE(t.name, '') AS "name!"
           FROM package_items i JOIN products p ON p.id = i.item_product_id
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = 'fr'
           WHERE i.package_product_id = $1 ORDER BY i.sort_order"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let day = sample + Duration::days(i64::from(r.day_number.unwrap_or(1) - 1));
        let kind = Kind::from_product_kind(&r.kind);
        let price = match kind {
            Some(k) => {
                pricing::sample_price(conn, state, k, r.item_product_id, day, r.quantity).await?
            }
            None => None,
        };
        out.push(json!({
            "item_product_id": r.item_product_id, "quantity": r.quantity, "day_number": r.day_number,
            "is_optional": r.is_optional, "kind": r.kind, "segment": kind.map(Kind::segment), "status": r.status,
            "deleted": r.deleted, "name": r.name, "sample_price_minor": price,
        }));
    }
    Ok(json!(out))
}

#[derive(Debug, Deserialize)]
pub struct SampleQuery {
    /// Sample start date (default: in 30 days).
    pub date: Option<NaiveDate>,
}

pub async fn items(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Query(q): Query<SampleQuery>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    if kind != Kind::Packages {
        return Err(AppError::NotFound);
    }
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    product_of_kind(&mut conn, kind, id).await?;
    let date = q
        .date
        .unwrap_or_else(|| crate::camp_time::today(Utc::now()) + Duration::days(30));
    Ok(Json(
        json!({ "date": date, "data": items_json(&mut conn, &state, id, date).await? }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct AvailabilityQuery {
    pub start: NaiveDate,
    /// `adult:2,child:1` (guest category codes).
    #[serde(default)]
    pub party: Option<String>,
}

/// Parses `adult:2,child:1` into guest counts (unknown codes are refused).
pub async fn parse_party(
    conn: &mut PgConnection,
    party: Option<&str>,
) -> AppResult<Vec<GuestCount>> {
    let cats: HashMap<String, Uuid> =
        sqlx::query!(r#"SELECT code::text AS "code!", id FROM guest_categories WHERE is_active"#)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|r| (r.code, r.id))
            .collect();
    let raw = party.unwrap_or("adult:2");
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (code, n) = part
            .split_once(':')
            .ok_or_else(|| field_error("party", "invalid"))?;
        let id = cats
            .get(code.trim())
            .ok_or_else(|| field_error("party", "unknown_category"))?;
        let count: i32 = n
            .trim()
            .parse()
            .map_err(|_| field_error("party", "invalid"))?;
        out.push(GuestCount {
            guest_category_id: *id,
            count,
        });
    }
    Ok(out)
}

pub async fn availability(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Query(q): Query<AvailabilityQuery>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    if kind != Kind::Packages {
        return Err(AppError::NotFound);
    }
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    product_of_kind(&mut conn, kind, id).await?;
    let guests = parse_party(&mut conn, q.party.as_deref()).await?;
    let request = TripRequest {
        items: vec![TripItem::Package {
            product_id: id,
            arrival: q.start,
            guests,
        }],
        ..TripRequest::default()
    };
    let ctx = pricing::staff_context(&state).await?;
    let quote = trip::quote(&mut conn, &request, &ctx).await?;
    let blocking: Vec<Value> = quote
        .items
        .iter()
        .filter_map(|i| i.issue.as_ref().map(|issue| json!({
            "product_id": i.product_id, "name": i.name, "kind": i.kind, "date": i.date.or(i.arrival),
            "code": issue.code, "message": issue.message, "details": issue.details,
        })))
        .collect();
    Ok(Json(json!({
        "available": !quote.has_issues, "blocking": blocking, "total_minor": quote.total_minor,
        "checked_at": Utc::now(),
    })))
}
