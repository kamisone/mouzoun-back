//! Admin stock (`shop.stock`; reads `shop.read`): stock locations, the inventory grid
//! (variant × location), receive / adjust / count / transfer, lots, the movement ledger, CSV
//! export, low-stock and expiring-soon lists. Every change goes through the inventory service
//! (one ledger movement per change of `on_hand`).

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::Staff,
    db::shop::MovementReason,
    error::{AppError, AppResult},
    services::{
        audit,
        shop::inventory::{self, MovementRefs, NewLot},
    },
    state::AppState,
    web::pagination::{PageParams, Paginated},
};

fn invalid(message: &str) -> AppError {
    AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message)
}

// ---- Locations -----------------------------------------------------------------------------------

pub async fn locations(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT s.id, s.code::text AS "code!", s.place_id, s.address, s.is_active, s.fulfills_online_orders, s.is_pickup_point, s.sort_order,
                  s.country_code::text AS "country_code!", (SELECT code::text FROM legal_entities e WHERE e.id = s.legal_entity_id) AS legal_entity,
                  COALESCE((SELECT jsonb_object_agg(locale, jsonb_build_object('name', name, 'pickup_instructions', pickup_instructions))
                            FROM stock_location_translations WHERE location_id = s.id), '{}') AS "translations!"
           FROM stock_locations s ORDER BY s.sort_order, s.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|r| json!({
        "id": r.id, "code": r.code, "place_id": r.place_id, "address": r.address, "is_active": r.is_active,
        "fulfills_online_orders": r.fulfills_online_orders, "is_pickup_point": r.is_pickup_point, "sort_order": r.sort_order,
        "translations": r.translations, "country_code": r.country_code, "legal_entity": r.legal_entity,
    })).collect::<Vec<_>>() })))
}

fn france() -> String {
    "FR".into()
}

#[derive(Debug, Deserialize)]
pub struct LocationInput {
    pub code: String,
    pub place_id: Option<Uuid>,
    #[serde(default)]
    pub address: Value,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default = "yes")]
    pub fulfills_online_orders: bool,
    #[serde(default)]
    pub is_pickup_point: bool,
    #[serde(default)]
    pub sort_order: i32,
    /// Where the stock physically is (customs: stock outside the EU needs declarations).
    #[serde(default = "france")]
    pub country_code: String,
    /// Owner of the stock (`legal_entities.code`); the shop sells only its entity's stock.
    pub legal_entity: Option<String>,
    /// `{locale: {name, pickup_instructions}}`.
    pub translations: HashMap<String, LocationTranslation>,
}

#[derive(Debug, Deserialize)]
pub struct LocationTranslation {
    pub name: String,
    pub pickup_instructions: Option<String>,
}

fn yes() -> bool {
    true
}

async fn save_location(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    b: &LocationInput,
) -> AppResult<Uuid> {
    staff.require("shop.stock")?;
    if b.code.trim().is_empty() || b.translations.values().all(|t| t.name.trim().is_empty()) {
        return Err(invalid("a code and a name are required"));
    }
    let address = if b.address.is_object() {
        b.address.clone()
    } else {
        json!({})
    };
    let country = b.country_code.trim().to_ascii_uppercase();
    if country.len() != 2 || !country.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "country_code: 2 letters",
        ));
    }
    let mut tx = state.db.begin().await?;
    let id = match id {
        Some(id) => {
            let n = sqlx::query!(
                "UPDATE stock_locations SET code = $2::text::citext, place_id = $3, address = $4, is_active = $5, fulfills_online_orders = $6,
                        is_pickup_point = $7, sort_order = $8, country_code = $9,
                        legal_entity_id = COALESCE((SELECT id FROM legal_entities WHERE code = $10::text::citext), shop_entity_id())
                 WHERE id = $1",
                id,
                b.code.trim(),
                b.place_id,
                address,
                b.is_active,
                b.fulfills_online_orders,
                b.is_pickup_point,
                b.sort_order,
                country,
                b.legal_entity
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if n == 0 {
                return Err(AppError::NotFound);
            }
            id
        }
        None => {
            let id = Uuid::now_v7();
            sqlx::query!(
                "INSERT INTO stock_locations (id, code, place_id, address, is_active, fulfills_online_orders, is_pickup_point, sort_order,
                                              country_code, legal_entity_id)
                 VALUES ($1, $2::text::citext, $3, $4, $5, $6, $7, $8, $9,
                         COALESCE((SELECT id FROM legal_entities WHERE code = $10::text::citext), shop_entity_id()))",
                id,
                b.code.trim(),
                b.place_id,
                address,
                b.is_active,
                b.fulfills_online_orders,
                b.is_pickup_point,
                b.sort_order,
                country,
                b.legal_entity
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
                    AppError::domain(StatusCode::CONFLICT, "code_taken", "this code is already used")
                } else {
                    e.into()
                }
            })?;
            id
        }
    };
    for (locale, t) in &b.translations {
        if t.name.trim().is_empty() {
            continue;
        }
        sqlx::query!(
            "INSERT INTO stock_location_translations (location_id, locale, name, pickup_instructions) VALUES ($1, $2, $3, $4)
             ON CONFLICT (location_id, locale) DO UPDATE SET name = $3, pickup_instructions = $4",
            id,
            locale,
            t.name.trim(),
            t.pickup_instructions.as_deref()
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "stock_location.saved",
        "stock_location",
        &id.to_string(),
        json!({ "code": b.code }),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn create_location(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<LocationInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = save_location(&state, &staff, None, &b).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn update_location(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<LocationInput>,
) -> AppResult<Json<Value>> {
    save_location(&state, &staff, Some(id), &b).await?;
    Ok(Json(json!({ "id": id })))
}

// ---- Inventory grid ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GridFilter {
    pub q: Option<String>,
    pub location_id: Option<Uuid>,
    pub variant: Option<Uuid>,
    #[serde(default)]
    pub low_only: bool,
}

pub async fn grid(
    State(state): State<AppState>,
    staff: Staff,
    Query(page): Query<PageParams>,
    Query(f): Query<GridFilter>,
) -> AppResult<Json<Paginated<Value>>> {
    staff.require("shop.read")?;
    let threshold =
        crate::services::settings::number(&state.db, "shop.low_stock_threshold", 5).await?;
    let q = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let rows = sqlx::query!(
        r#"SELECT v.id AS variant_id, v.sku::text AS "sku!", v.track_inventory, v.cost_minor, COALESCE(v.low_stock_threshold, $5::int) AS "threshold!",
                  COALESCE(t.name, '') AS "name!", sp.track_lots,
                  COALESCE((SELECT string_agg(COALESCE(vt.label, ov.code::text), ' · ' ORDER BY o.sort_order)
                            FROM product_variant_option_values pvo JOIN product_options o ON o.id = pvo.option_id
                            JOIN product_option_values ov ON ov.id = pvo.option_value_id
                            LEFT JOIN product_option_value_translations vt ON vt.option_value_id = ov.id
                                 AND vt.locale = (SELECT code FROM locales WHERE is_default)
                            WHERE pvo.variant_id = v.id), '') AS "options!",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('location_id', l.location_id, 'on_hand', l.on_hand, 'reserved', l.reserved,
                            'available', l.on_hand - l.reserved, 'backorder_limit', l.backorder_limit, 'reorder_point', l.reorder_point,
                            'bin_location', l.bin_location))
                            FROM inventory_levels l WHERE l.variant_id = v.id AND ($2::uuid IS NULL OR l.location_id = $2)), '[]') AS "levels!",
                  COALESCE((SELECT sum(l.on_hand - l.reserved) FROM inventory_levels l WHERE l.variant_id = v.id), 0)::bigint AS "available!",
                  count(*) OVER () AS "total!"
           FROM product_variants v
           JOIN shop_products sp ON sp.product_id = v.product_id
           JOIN products p ON p.id = v.product_id AND p.deleted_at IS NULL
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
           WHERE v.deleted_at IS NULL AND ($1::text IS NULL OR v.sku::text ILIKE '%' || $1 || '%' OR t.name ILIKE '%' || $1 || '%')
             AND ($3::uuid IS NULL OR v.id = $3)
             AND (NOT $4 OR (v.track_inventory AND
                  COALESCE((SELECT sum(l.on_hand - l.reserved) FROM inventory_levels l WHERE l.variant_id = v.id), 0)
                  <= COALESCE(v.low_stock_threshold, $5::int)))
           ORDER BY t.name, v.sort_order, v.sku
           LIMIT $6 OFFSET $7"#,
        q,
        f.location_id,
        f.variant,
        f.low_only,
        i32::try_from(threshold).unwrap_or(5),
        page.limit(),
        page.offset()
    )
    .fetch_all(&state.db)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let show_cost = staff.can("shop.pricing");
    let data = rows
        .into_iter()
        .map(|r| {
            json!({ "variant_id": r.variant_id, "sku": r.sku, "name": r.name, "options": r.options, "track_inventory": r.track_inventory,
                    "track_lots": r.track_lots, "threshold": r.threshold, "levels": r.levels, "available": r.available,
                    "cost_minor": if show_cost { r.cost_minor } else { None } })
        })
        .collect();
    Ok(Json(Paginated::new(data, page, total)))
}

// ---- Stock operations ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum StockOperation {
    Receive {
        variant_id: Uuid,
        location_id: Uuid,
        quantity: i32,
        lot: Option<NewLot>,
        unit_cost_minor: Option<i64>,
        note: Option<String>,
    },
    /// A signed change with its reason (damage, expired, correction, adjustment).
    Adjust {
        variant_id: Uuid,
        location_id: Uuid,
        lot_id: Option<Uuid>,
        delta: i32,
        reason: MovementReason,
        note: Option<String>,
    },
    Count {
        variant_id: Uuid,
        location_id: Uuid,
        lot_id: Option<Uuid>,
        counted: i32,
        note: Option<String>,
    },
    Transfer {
        variant_id: Uuid,
        from_location_id: Uuid,
        to_location_id: Uuid,
        quantity: i32,
        note: Option<String>,
    },
    /// Level settings (no stock change).
    Settings {
        variant_id: Uuid,
        location_id: Uuid,
        backorder_limit: Option<i32>,
        reorder_point: Option<Option<i32>>,
        bin_location: Option<Option<String>>,
    },
}

pub async fn operate(
    State(state): State<AppState>,
    staff: Staff,
    Json(op): Json<StockOperation>,
) -> AppResult<Json<Value>> {
    staff.require("shop.stock")?;
    let mut tx = state.db.begin().await?;
    let refs = |note: &Option<String>| MovementRefs {
        note: note.clone(),
        created_by: Some(staff.id),
        ..MovementRefs::default()
    };
    let (variant, result) = match &op {
        StockOperation::Receive {
            variant_id,
            location_id,
            quantity,
            lot,
            unit_cost_minor,
            note,
        } => {
            let r = MovementRefs {
                unit_cost_minor: *unit_cost_minor,
                ..refs(note)
            };
            let on_hand = inventory::receive(
                &mut tx,
                *variant_id,
                *location_id,
                *quantity,
                lot.as_ref(),
                &r,
            )
            .await?;
            (*variant_id, json!({ "on_hand": on_hand }))
        }
        StockOperation::Adjust {
            variant_id,
            location_id,
            lot_id,
            delta,
            reason,
            note,
        } => {
            if !matches!(
                reason,
                MovementReason::Adjustment
                    | MovementReason::Damage
                    | MovementReason::Expired
                    | MovementReason::Correction
            ) {
                return Err(invalid("use receive, count or transfer for that reason"));
            }
            if note.as_deref().is_none_or(|n| n.trim().is_empty()) {
                return Err(invalid("a note explains every adjustment"));
            }
            let lot = match lot_id {
                Some(l) => Some(*l),
                None if *delta < 0 => {
                    // Lot-tracked stock leaves FEFO.
                    let taken = inventory::take_out(
                        &mut tx,
                        *variant_id,
                        *location_id,
                        -delta,
                        *reason,
                        &refs(note),
                    )
                    .await?;
                    tx.commit().await?;
                    return Ok(Json(
                        json!({ "taken": taken.iter().map(|(l, q)| json!({ "lot_id": l, "quantity": q })).collect::<Vec<_>>() }),
                    ));
                }
                None => None,
            };
            let on_hand = inventory::apply_movement(
                &mut tx,
                *variant_id,
                *location_id,
                lot,
                *delta,
                *reason,
                &refs(note),
            )
            .await?;
            (*variant_id, json!({ "on_hand": on_hand }))
        }
        StockOperation::Count {
            variant_id,
            location_id,
            lot_id,
            counted,
            note,
        } => {
            let on_hand = inventory::count(
                &mut tx,
                *variant_id,
                *location_id,
                *lot_id,
                *counted,
                &refs(note),
            )
            .await?;
            (*variant_id, json!({ "on_hand": on_hand }))
        }
        StockOperation::Transfer {
            variant_id,
            from_location_id,
            to_location_id,
            quantity,
            note,
        } => {
            let id = inventory::transfer(
                &mut tx,
                *variant_id,
                *from_location_id,
                *to_location_id,
                *quantity,
                &refs(note),
            )
            .await?;
            (*variant_id, json!({ "transfer_id": id }))
        }
        StockOperation::Settings {
            variant_id,
            location_id,
            backorder_limit,
            reorder_point,
            bin_location,
        } => {
            inventory::ensure_level(&mut tx, *variant_id, *location_id).await?;
            let lots = sqlx::query_scalar!(
                "SELECT sp.track_lots FROM product_variants v JOIN shop_products sp ON sp.product_id = v.product_id WHERE v.id = $1",
                variant_id
            )
            .fetch_one(&mut *tx)
            .await?;
            if lots && backorder_limit.is_some_and(|b| b > 0) {
                return Err(invalid("lot-tracked products cannot be sold on backorder"));
            }
            sqlx::query!(
                "UPDATE inventory_levels SET backorder_limit = COALESCE($3, backorder_limit),
                        reorder_point = CASE WHEN $4 THEN $5 ELSE reorder_point END,
                        bin_location = CASE WHEN $6 THEN $7 ELSE bin_location END
                 WHERE variant_id = $1 AND location_id = $2",
                variant_id,
                location_id,
                *backorder_limit,
                reorder_point.is_some(),
                reorder_point.flatten(),
                bin_location.is_some(),
                bin_location.clone().flatten()
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if e.as_database_error()
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == "23514")
                {
                    invalid("the backorder limit cannot be below what is already reserved")
                } else {
                    e.into()
                }
            })?;
            (*variant_id, json!({ "ok": true }))
        }
    };
    audit::record(
        &mut tx,
        &staff.actor(),
        "stock.changed",
        "variant",
        &variant.to_string(),
        serde_json::to_value(StockLog(&op)).unwrap_or_default(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

/// Audit payload of an operation.
struct StockLog<'a>(&'a StockOperation);

impl serde::Serialize for StockLog<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v = match self.0 {
            StockOperation::Receive {
                location_id,
                quantity,
                lot,
                ..
            } => {
                json!({ "op": "receive", "location_id": location_id, "quantity": quantity, "lot": lot.as_ref().map(|l| &l.lot_code) })
            }
            StockOperation::Adjust {
                location_id,
                delta,
                reason,
                note,
                ..
            } => {
                json!({ "op": "adjust", "location_id": location_id, "delta": delta, "reason": reason, "note": note })
            }
            StockOperation::Count {
                location_id,
                counted,
                ..
            } => json!({ "op": "count", "location_id": location_id, "counted": counted }),
            StockOperation::Transfer {
                from_location_id,
                to_location_id,
                quantity,
                ..
            } => {
                json!({ "op": "transfer", "from": from_location_id, "to": to_location_id, "quantity": quantity })
            }
            StockOperation::Settings {
                location_id,
                backorder_limit,
                ..
            } => {
                json!({ "op": "settings", "location_id": location_id, "backorder_limit": backorder_limit })
            }
        };
        v.serialize(s)
    }
}

// ---- Lots, movements, lists -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LotFilter {
    pub variant_id: Option<Uuid>,
    /// Lots whose best-before is within this many days.
    pub expiring_within_days: Option<i64>,
}

pub async fn lots(
    State(state): State<AppState>,
    staff: Staff,
    Query(f): Query<LotFilter>,
) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let until: Option<NaiveDate> = f
        .expiring_within_days
        .map(|d| crate::camp_time::today(Utc::now()) + Duration::days(d));
    let rows = sqlx::query!(
        r#"SELECT l.id, l.variant_id, l.location_id, l.lot_code::text AS "lot_code!", l.produced_on, l.harvested_on, l.best_before,
                  l.quantity, l.notes, v.sku::text AS "sku!", COALESCE(t.name, '') AS "name!", s.code::text AS "location!"
           FROM inventory_lots l JOIN product_variants v ON v.id = l.variant_id JOIN stock_locations s ON s.id = l.location_id
           LEFT JOIN product_translations t ON t.product_id = v.product_id AND t.locale = (SELECT code FROM locales WHERE is_default)
           WHERE ($1::uuid IS NULL OR l.variant_id = $1) AND ($2::date IS NULL OR (l.best_before <= $2 AND l.quantity > 0))
           ORDER BY l.best_before NULLS LAST, l.lot_code LIMIT 500"#,
        f.variant_id,
        until
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|l| json!({
        "id": l.id, "variant_id": l.variant_id, "location_id": l.location_id, "lot_code": l.lot_code, "produced_on": l.produced_on,
        "harvested_on": l.harvested_on, "best_before": l.best_before, "quantity": l.quantity, "notes": l.notes, "sku": l.sku,
        "name": l.name, "location": l.location,
    })).collect::<Vec<_>>() })))
}

#[derive(Debug, Deserialize)]
pub struct MovementFilter {
    pub variant_id: Option<Uuid>,
    pub location_id: Option<Uuid>,
    pub reason: Option<String>,
    pub order_id: Option<Uuid>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

pub async fn movements(
    State(state): State<AppState>,
    staff: Staff,
    Query(page): Query<PageParams>,
    Query(f): Query<MovementFilter>,
) -> AppResult<Json<Paginated<Value>>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT m.id, m.variant_id, m.location_id, m.lot_id, m.delta, m.on_hand_after, m.reason, m.order_id, m.fulfillment_id,
                  m.return_id, m.transfer_id, m.unit_cost_minor, m.note, m.created_at, v.sku::text AS "sku!", s.code::text AS "location!",
                  u.full_name AS "user?", o.number AS "order_number?", l.lot_code::text AS "lot_code?",
                  count(*) OVER () AS "total!"
           FROM inventory_movements m JOIN product_variants v ON v.id = m.variant_id JOIN stock_locations s ON s.id = m.location_id
           LEFT JOIN users u ON u.id = m.created_by LEFT JOIN orders o ON o.id = m.order_id LEFT JOIN inventory_lots l ON l.id = m.lot_id
           WHERE ($1::uuid IS NULL OR m.variant_id = $1) AND ($2::uuid IS NULL OR m.location_id = $2) AND ($3::text IS NULL OR m.reason = $3)
             AND ($4::uuid IS NULL OR m.order_id = $4)
             AND ($5::date IS NULL OR m.created_at >= ($5::date::timestamp AT TIME ZONE 'Africa/Casablanca'))
             AND ($6::date IS NULL OR m.created_at < (($6::date + 1)::timestamp AT TIME ZONE 'Africa/Casablanca'))
           ORDER BY m.id DESC LIMIT $7 OFFSET $8"#,
        f.variant_id,
        f.location_id,
        f.reason.as_deref(),
        f.order_id,
        f.from,
        f.to,
        page.limit(),
        page.offset()
    )
    .fetch_all(&state.db)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let data = rows
        .into_iter()
        .map(|m| json!({ "id": m.id, "variant_id": m.variant_id, "location_id": m.location_id, "lot_id": m.lot_id, "lot_code": m.lot_code,
                         "delta": m.delta, "on_hand_after": m.on_hand_after, "reason": m.reason, "order_id": m.order_id,
                         "order_number": m.order_number, "fulfillment_id": m.fulfillment_id, "return_id": m.return_id,
                         "transfer_id": m.transfer_id, "unit_cost_minor": m.unit_cost_minor, "note": m.note, "created_at": m.created_at,
                         "sku": m.sku, "location": m.location, "user": m.user }))
        .collect();
    Ok(Json(Paginated::new(data, page, total)))
}

pub async fn export_csv(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<impl IntoResponse> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT v.sku::text AS "sku!", COALESCE(t.name, '') AS "name!", s.code::text AS "location!", l.on_hand, l.reserved,
                  l.backorder_limit, l.bin_location, v.cost_minor
           FROM inventory_levels l JOIN product_variants v ON v.id = l.variant_id AND v.deleted_at IS NULL
           JOIN stock_locations s ON s.id = l.location_id
           LEFT JOIN product_translations t ON t.product_id = v.product_id AND t.locale = (SELECT code FROM locales WHERE is_default)
           ORDER BY t.name, v.sku, s.sort_order"#
    )
    .fetch_all(&state.db)
    .await?;
    let cost = staff.can("shop.pricing");
    let mut out = String::from(
        "sku,product,location,on_hand,reserved,available,backorder_limit,bin,unit_cost_minor\n",
    );
    for r in rows {
        let name = if r.name.starts_with(['=', '+', '-', '@']) {
            format!("'{}", r.name)
        } else {
            r.name
        };
        out.push_str(&format!(
            "{},\"{}\",{},{},{},{},{},{},{}\n",
            r.sku,
            name.replace('"', "\"\""),
            r.location,
            r.on_hand,
            r.reserved,
            r.on_hand - r.reserved,
            r.backorder_limit,
            r.bin_location.unwrap_or_default().replace(',', " "),
            if cost {
                r.cost_minor.map(|c| c.to_string()).unwrap_or_default()
            } else {
                String::new()
            }
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"inventory.csv\"",
            ),
        ],
        out,
    ))
}
