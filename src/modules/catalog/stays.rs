//! Stays: units (unit mode) or one capacity (pool mode), the night calendar and closures.
//! Every capacity change goes through `services::inventory` (`sync_unit_capacity`,
//! `set_pool_capacity`), in the same transaction, and is refused when a night would hold fewer
//! places than are booked; bookings are never moved or cancelled here.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Fields, Kind, blocked_by_bookings, changed, field_error, product_of_kind};
use crate::{
    auth::Staff,
    db::types::{DateRange, DaysOfWeek},
    error::{AppError, AppResult},
    services::inventory::{self, Shortfall},
    state::AppState,
};

fn today() -> NaiveDate {
    crate::camp_time::today(Utc::now())
}

async fn stay(conn: &mut PgConnection, kind: &str, id: Uuid) -> AppResult<String> {
    let kind = Kind::parse(kind)?;
    if kind != Kind::Stays {
        return Err(AppError::NotFound);
    }
    product_of_kind(conn, kind, id).await?;
    Ok(sqlx::query_scalar!(
        "SELECT inventory_mode FROM accommodation_types WHERE product_id = $1",
        id
    )
    .fetch_one(conn)
    .await?)
}

/// The refusal for nights that would go below their bookings, with the bookings concerned.
async fn shortfall(conn: &mut PgConnection, id: Uuid, short: Vec<Shortfall>) -> AppError {
    let days: Vec<NaiveDate> = short.iter().map(|s| s.day).collect();
    let bookings = inventory::bookings_on_nights(conn, id, &days)
        .await
        .unwrap_or_default();
    blocked_by_bookings(
        "below_reserved",
        "some nights would hold fewer places than are already booked",
        &bookings,
        json!({ "nights": short.iter().take(60).collect::<Vec<_>>() }),
    )
}

/// After units or closures changed: future capacity follows them (unit mode only).
async fn resync(conn: &mut PgConnection, id: Uuid, mode: &str) -> AppResult<()> {
    if mode != "unit" {
        return Ok(());
    }
    match inventory::sync_unit_capacity(conn, id, today()).await? {
        Ok(_) => Ok(()),
        Err(short) => Err(shortfall(conn, id, short).await),
    }
}

pub async fn has_inventory(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT CASE WHEN a.inventory_mode = 'pool' AND a.pool_capacity IS NOT NULL THEN a.pool_capacity > 0
                       ELSE EXISTS (SELECT 1 FROM accommodation_units u WHERE u.product_id = a.product_id AND u.status = 'active')
                  END AS "x!"
           FROM accommodation_types a WHERE a.product_id = $1"#,
        id
    )
    .fetch_optional(conn)
    .await
    .map(|x| x.unwrap_or(false))
}

/// The product form never switches the inventory mode (`PUT .../inventory` does).
pub async fn check_mode_unchanged(
    conn: &mut PgConnection,
    id: Uuid,
    asked: Option<&str>,
    f: &mut Fields,
) -> AppResult<()> {
    if let Some(asked) = asked {
        let mode = sqlx::query_scalar!(
            "SELECT inventory_mode FROM accommodation_types WHERE product_id = $1",
            id
        )
        .fetch_one(conn)
        .await?;
        if mode != asked {
            f.add("details.inventory_mode", "use_inventory_tab");
        }
    }
    Ok(())
}

// ---- Units ----------------------------------------------------------------------------------------

fn unit_json(u: &UnitRow) -> Value {
    json!({
        "id": u.id, "code": u.code, "name": u.name, "place_id": u.place_id, "status": u.status,
        "latitude": u.latitude.map(|d| d.normalize()), "longitude": u.longitude.map(|d| d.normalize()),
        "notes": u.notes, "sort_order": u.sort_order,
    })
}

struct UnitRow {
    id: Uuid,
    code: String,
    name: Option<String>,
    place_id: Option<Uuid>,
    status: String,
    latitude: Option<Decimal>,
    longitude: Option<Decimal>,
    notes: Option<String>,
    sort_order: i32,
}

async fn unit_rows(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Vec<UnitRow>> {
    sqlx::query_as!(
        UnitRow,
        r#"SELECT id, code::text AS "code!", name, place_id, status, latitude, longitude, notes, sort_order
           FROM accommodation_units WHERE product_id = $1 ORDER BY sort_order, code"#,
        id
    )
    .fetch_all(conn)
    .await
}

pub async fn units(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    let mut conn = state.db.acquire().await?;
    let mode = stay(&mut conn, &kind, id).await?;
    let pool_capacity = sqlx::query_scalar!(
        "SELECT pool_capacity FROM accommodation_types WHERE product_id = $1",
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    let rows = unit_rows(&mut conn, id).await?;
    Ok(Json(json!({
        "inventory_mode": mode, "pool_capacity": pool_capacity,
        "data": rows.iter().map(unit_json).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct UnitBody {
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub place_id: Option<Uuid>,
    #[serde(default = "active")]
    pub status: String,
    #[serde(default, deserialize_with = "crate::web::percent::option")]
    pub latitude: Option<Decimal>,
    #[serde(default, deserialize_with = "crate::web::percent::option")]
    pub longitude: Option<Decimal>,
    #[serde(default)]
    pub notes: Option<String>,
}

fn active() -> String {
    "active".into()
}

fn validate_unit(u: &UnitBody, prefix: &str, f: &mut Fields) {
    let code = u.code.trim();
    if code.is_empty() {
        f.add(format!("{prefix}code"), "required");
    } else if code.len() > 20 {
        f.add(format!("{prefix}code"), "too_long");
    }
    if !["active", "maintenance", "retired"].contains(&u.status.as_str()) {
        f.add(format!("{prefix}status"), "invalid");
    }
    if u.latitude.is_some_and(|x| x.abs() > Decimal::from(90)) {
        f.add(format!("{prefix}latitude"), "range");
    }
    if u.longitude.is_some_and(|x| x.abs() > Decimal::from(180)) {
        f.add(format!("{prefix}longitude"), "range");
    }
}

fn code_taken(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.constraint())
        == Some("accommodation_units_product_id_code_key")
}

pub async fn create_unit(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(u): Json<UnitBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("inventory.write")?;
    let mut f = Fields::default();
    validate_unit(&u, "", &mut f);
    f.check()?;
    let mut tx = state.db.begin().await?;
    let mode = stay(&mut tx, &kind, id).await?;
    let unit_id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO accommodation_units (id, product_id, code, name, place_id, status, latitude, longitude, notes, sort_order)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                 (SELECT COALESCE(max(sort_order) + 1, 0) FROM accommodation_units WHERE product_id = $2))",
        unit_id,
        id,
        u.code.trim(),
        u.name.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        u.place_id,
        u.status,
        u.latitude,
        u.longitude,
        u.notes.as_deref().map(str::trim).filter(|s| !s.is_empty())
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| if code_taken(&e) { field_error("code", "taken") } else { e.into() })?;
    resync(&mut tx, id, &mode).await?;
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Stays,
        id,
        "unit.created",
        json!({ "code": u.code.trim() }),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": unit_id }))))
}

#[derive(Debug, Deserialize)]
pub struct BulkUnits {
    /// `P-` in "Créer P-01 à P-12".
    pub prefix: String,
    pub from: i32,
    pub to: i32,
    /// Zero-padded width of the number (`2` → `01`).
    #[serde(default = "two")]
    pub width: usize,
}

fn two() -> usize {
    2
}

pub async fn bulk_units(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<BulkUnits>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.write")?;
    let mut f = Fields::default();
    if b.from < 0 || b.to < b.from {
        f.add("to", "below_from");
    }
    if b.to - b.from >= 200 {
        f.add("to", "too_many");
    }
    if b.prefix.trim().len() > 12 || b.width > 4 {
        f.add("prefix", "too_long");
    }
    f.check()?;
    let mut tx = state.db.begin().await?;
    let mode = stay(&mut tx, &kind, id).await?;
    let mut created = 0;
    for n in b.from..=b.to {
        let code = format!("{}{:0width$}", b.prefix.trim(), n, width = b.width);
        created += sqlx::query!(
            "INSERT INTO accommodation_units (product_id, code, sort_order)
             VALUES ($1, $2, (SELECT COALESCE(max(sort_order) + 1, 0) FROM accommodation_units WHERE product_id = $1))
             ON CONFLICT (product_id, code) DO NOTHING",
            id,
            code
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    resync(&mut tx, id, &mode).await?;
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Stays,
        id,
        "unit.bulk_created",
        json!({ "created": created }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "created": created })))
}

pub async fn update_unit(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id, unit_id)): Path<(String, Uuid, Uuid)>,
    Json(u): Json<UnitBody>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.write")?;
    let mut f = Fields::default();
    validate_unit(&u, "", &mut f);
    f.check()?;
    let mut tx = state.db.begin().await?;
    let mode = stay(&mut tx, &kind, id).await?;
    let before = sqlx::query!(
        "SELECT status FROM accommodation_units WHERE id = $1 AND product_id = $2 FOR UPDATE",
        unit_id,
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if before.status == "active" && u.status != "active" {
        // A unit already given to a guest cannot leave: move the booking first.
        let held = inventory::bookings_on_unit(&mut tx, unit_id, today()).await?;
        if !held.is_empty() {
            return Err(blocked_by_bookings(
                "unit_booked",
                "this unit is allocated to future bookings",
                &held,
                json!({}),
            ));
        }
    }
    sqlx::query!(
        "UPDATE accommodation_units SET code = $3, name = $4, place_id = $5, status = $6, latitude = $7, longitude = $8, notes = $9
         WHERE id = $1 AND product_id = $2",
        unit_id,
        id,
        u.code.trim(),
        u.name.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        u.place_id,
        u.status,
        u.latitude,
        u.longitude,
        u.notes.as_deref().map(str::trim).filter(|s| !s.is_empty())
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| if code_taken(&e) { field_error("code", "taken") } else { e.into() })?;
    resync(&mut tx, id, &mode).await?;
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Stays,
        id,
        "unit.updated",
        json!({ "unit_id": unit_id, "status": { "from": before.status, "to": u.status } }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": unit_id })))
}

/// Deleting a unit: only one never allocated (otherwise retire it).
pub async fn delete_unit(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id, unit_id)): Path<(String, Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    staff.require("inventory.write")?;
    let mut tx = state.db.begin().await?;
    let mode = stay(&mut tx, &kind, id).await?;
    let used = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM unit_allocations WHERE unit_id = $1) AS "x!""#,
        unit_id
    )
    .fetch_one(&mut *tx)
    .await?;
    if used {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "unit_has_history",
            "this unit was already booked: set it to retired instead",
        ));
    }
    let gone = sqlx::query!(
        "DELETE FROM accommodation_units WHERE id = $1 AND product_id = $2",
        unit_id,
        id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if gone == 0 {
        return Err(AppError::NotFound);
    }
    resync(&mut tx, id, &mode).await?;
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Stays,
        id,
        "unit.deleted",
        json!({ "unit_id": unit_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct UnitOrder {
    pub ids: Vec<Uuid>,
}

pub async fn reorder_units(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<UnitOrder>,
) -> AppResult<StatusCode> {
    staff.require("inventory.write")?;
    let mut tx = state.db.begin().await?;
    stay(&mut tx, &kind, id).await?;
    for (n, unit) in b.ids.iter().enumerate() {
        sqlx::query!(
            "UPDATE accommodation_units SET sort_order = $3 WHERE id = $1 AND product_id = $2",
            unit,
            id,
            n as i32
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct InventoryBody {
    pub inventory_mode: String,
    /// Pool mode: the number of interchangeable places.
    #[serde(default)]
    pub pool_capacity: Option<i32>,
}

/// Switches unit ↔ pool (refused with future bookings) and sets the pool capacity.
pub async fn set_inventory(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<InventoryBody>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.write")?;
    let mut f = Fields::default();
    if !["unit", "pool"].contains(&b.inventory_mode.as_str()) {
        f.add("inventory_mode", "invalid");
    }
    if b.inventory_mode == "pool" && b.pool_capacity.is_none_or(|c| c < 0) {
        f.add("pool_capacity", "required");
    }
    f.check()?;
    let mut tx = state.db.begin().await?;
    let mode = stay(&mut tx, &kind, id).await?;
    if mode != b.inventory_mode {
        let future = inventory::future_bookings(&mut tx, id, today()).await?;
        if !future.is_empty() {
            return Err(blocked_by_bookings(
                "mode_locked",
                "the inventory mode cannot change while future bookings exist",
                &future,
                json!({}),
            ));
        }
    }
    sqlx::query!(
        "UPDATE accommodation_types SET inventory_mode = $2, pool_capacity = $3 WHERE product_id = $1",
        id,
        b.inventory_mode,
        if b.inventory_mode == "pool" { b.pool_capacity } else { None }
    )
    .execute(&mut *tx)
    .await?;
    let result = if b.inventory_mode == "pool" {
        inventory::set_pool_capacity(
            &mut tx,
            id,
            today(),
            b.pool_capacity.unwrap_or(0),
            None,
            DaysOfWeek::ALL,
        )
        .await?
    } else {
        inventory::sync_unit_capacity(&mut tx, id, today()).await?
    };
    if let Err(short) = result {
        return Err(shortfall(&mut tx, id, short).await);
    }
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Stays,
        id,
        "inventory.mode_set",
        json!({ "from": mode, "to": b.inventory_mode, "pool_capacity": b.pool_capacity }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({ "inventory_mode": b.inventory_mode, "pool_capacity": b.pool_capacity }),
    ))
}

// ---- Calendar -------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

pub async fn calendar(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Query(q): Query<RangeQuery>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    if q.to <= q.from || (q.to - q.from).num_days() > 186 {
        return Err(field_error("to", "range"));
    }
    let mut conn = state.db.acquire().await?;
    let mode = stay(&mut conn, &kind, id).await?;
    let nights = sqlx::query!(
        "SELECT day, capacity, reserved, stop_sell, min_nights_override, closed_to_arrival, closed_to_departure
         FROM inventory_calendar WHERE product_id = $1 AND day >= $2 AND day < $3 ORDER BY day",
        id,
        q.from,
        q.to
    )
    .fetch_all(&mut *conn)
    .await?;
    let seasons = sqlx::query!(
        r#"SELECT s.id, s.code::text AS "code!", s.color, s.priority, sp.period AS "period: DateRange",
                  COALESCE((SELECT name FROM season_translations WHERE season_id = s.id AND locale = 'fr'), s.code::text) AS "name!"
           FROM seasons s JOIN season_periods sp ON sp.season_id = s.id WHERE sp.period && daterange($1, $2)"#,
        q.from,
        q.to
    )
    .fetch_all(&mut *conn)
    .await?;
    let season_rules: Vec<crate::services::pricing::engine::SeasonRule> = seasons
        .iter()
        .map(|s| crate::services::pricing::engine::SeasonRule {
            id: s.id,
            priority: s.priority,
            periods: vec![s.period],
        })
        .collect();
    let closures = closure_rows(&mut conn, id, Some((q.from, q.to))).await?;
    Ok(Json(json!({
        "inventory_mode": mode,
        "nights": nights.iter().map(|n| json!({
            "day": n.day, "capacity": n.capacity, "reserved": n.reserved, "available": (n.capacity - n.reserved).max(0),
            "stop_sell": n.stop_sell, "min_nights_override": n.min_nights_override, "closed_to_arrival": n.closed_to_arrival,
            "closed_to_departure": n.closed_to_departure,
            "season_id": crate::services::pricing::engine::season_of(n.day, &season_rules),
            "closed": closures.iter().any(|c| c.period.contains(n.day)),
        })).collect::<Vec<_>>(),
        "seasons": seasons.iter().map(|s| json!({ "id": s.id, "code": s.code, "name": s.name, "color": s.color }))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct CalendarPatch {
    pub from: NaiveDate,
    /// Exclusive.
    pub to: NaiveDate,
    /// Mon=1 … Sun=64 (default every day).
    #[serde(default)]
    pub days_of_week: Option<i16>,
    #[serde(default)]
    pub stop_sell: Option<bool>,
    /// `Some(None)` clears the override.
    #[serde(default, with = "double_option")]
    pub min_nights_override: Option<Option<i16>>,
    #[serde(default)]
    pub closed_to_arrival: Option<bool>,
    #[serde(default)]
    pub closed_to_departure: Option<bool>,
    /// Pool mode only.
    #[serde(default)]
    pub capacity: Option<i32>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<i16>>, D::Error> {
        Ok(Some(Option::<i16>::deserialize(d)?))
    }
}

pub async fn patch_calendar(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(p): Json<CalendarPatch>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.write")?;
    let mut f = Fields::default();
    if p.to <= p.from {
        f.add("to", "before_from");
    }
    if (p.to - p.from).num_days() > 548 {
        f.add("to", "range");
    }
    if p.days_of_week.is_some_and(|d| !(1..=127).contains(&d)) {
        f.add("days_of_week", "invalid");
    }
    if let Some(Some(n)) = p.min_nights_override
        && n < 1
    {
        f.add("min_nights_override", "min");
    }
    if p.capacity.is_some_and(|c| c < 0) {
        f.add("capacity", "min");
    }
    f.check()?;
    let weekdays = DaysOfWeek(p.days_of_week.unwrap_or(127));
    let mut tx = state.db.begin().await?;
    let mode = stay(&mut tx, &kind, id).await?;
    if p.capacity.is_some() && mode != "pool" {
        return Err(field_error("capacity", "unit_mode"));
    }
    let from = p.from.max(today());
    // Rows must exist for the range (inside the booking horizon they always do).
    inventory::generate_calendar_rows(&mut tx, Some(id), from, inventory::CALENDAR_HORIZON_MONTHS)
        .await?;
    if let Some(c) = p.capacity
        && let Err(short) =
            inventory::set_pool_capacity(&mut tx, id, today(), c, Some((from, p.to)), weekdays)
                .await?
    {
        return Err(shortfall(&mut tx, id, short).await);
    }
    let days: Vec<NaiveDate> = from
        .iter_days()
        .take_while(|d| *d < p.to)
        .filter(|d| weekdays.contains(*d))
        .collect();
    let set_min = p.min_nights_override.is_some();
    let updated = sqlx::query!(
        "UPDATE inventory_calendar SET
             stop_sell = COALESCE($3, stop_sell),
             min_nights_override = CASE WHEN $4 THEN $5 ELSE min_nights_override END,
             closed_to_arrival = COALESCE($6, closed_to_arrival),
             closed_to_departure = COALESCE($7, closed_to_departure)
         WHERE product_id = $1 AND day = ANY($2)",
        id,
        &days,
        p.stop_sell,
        set_min,
        p.min_nights_override.flatten(),
        p.closed_to_arrival,
        p.closed_to_departure
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    changed(&mut tx, &staff.actor(), Kind::Stays, id, "calendar.updated", json!({
        "from": from, "to": p.to, "days_of_week": weekdays.0, "nights": updated,
        "stop_sell": p.stop_sell, "min_nights_override": p.min_nights_override, "closed_to_arrival": p.closed_to_arrival,
        "closed_to_departure": p.closed_to_departure, "capacity": p.capacity,
    })).await?;
    tx.commit().await?;
    Ok(Json(json!({ "nights": updated })))
}

// ---- Closures -------------------------------------------------------------------------------------

struct ClosureRow {
    id: Uuid,
    scope: String,
    unit_id: Option<Uuid>,
    period: DateRange,
    reason: Option<String>,
    is_public: bool,
}

async fn closure_rows(
    conn: &mut PgConnection,
    id: Uuid,
    range: Option<(NaiveDate, NaiveDate)>,
) -> sqlx::Result<Vec<ClosureRow>> {
    let (from, to) = range.unwrap_or((today() - Duration::days(1), NaiveDate::MAX));
    sqlx::query_as!(
        ClosureRow,
        r#"SELECT c.id, c.scope, c.unit_id, c.period AS "period: DateRange", c.reason, c.is_public
           FROM closures c
           WHERE (c.product_id = $1 OR c.unit_id IN (SELECT u.id FROM accommodation_units u WHERE u.product_id = $1))
             AND c.period && daterange($2, $3)
           ORDER BY lower(c.period)"#,
        id,
        from,
        to
    )
    .fetch_all(conn)
    .await
}

pub async fn closures(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    let kind_ = Kind::parse(&kind)?;
    let mut conn = state.db.acquire().await?;
    product_of_kind(&mut conn, kind_, id).await?;
    let rows = closure_rows(&mut conn, id, None).await?;
    Ok(Json(json!({
        "data": rows.iter().map(|c| json!({
            "id": c.id, "scope": c.scope, "unit_id": c.unit_id, "from": c.period.start, "to": c.period.end,
            "reason": c.reason, "is_public": c.is_public,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct ClosureBody {
    pub from: NaiveDate,
    /// Exclusive (the first day open again).
    pub to: NaiveDate,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub is_public: bool,
    /// A closure of one unit (stays, unit mode) instead of the whole product.
    #[serde(default)]
    pub unit_id: Option<Uuid>,
}

/// Product (or unit) closure for stays and activities. A unit closure lowers the nights'
/// capacity (refused below bookings); a product closure stops sales (the booking flow checks it)
/// and closes the activity's departures it covers.
pub async fn create_closure(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<ClosureBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("inventory.write")?;
    let kind = Kind::parse(&kind)?;
    if b.to <= b.from {
        return Err(field_error("to", "before_from"));
    }
    let mut tx = state.db.begin().await?;
    product_of_kind(&mut tx, kind, id).await?;
    let closure_id = Uuid::now_v7();
    let period = DateRange::new(b.from, b.to);
    let reason = b.reason.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match b.unit_id {
        Some(unit) => {
            if kind != Kind::Stays {
                return Err(field_error("unit_id", "not_allowed"));
            }
            let mine = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM accommodation_units WHERE id = $1 AND product_id = $2) AS "x!""#,
                unit,
                id
            )
            .fetch_one(&mut *tx)
            .await?;
            if !mine {
                return Err(field_error("unit_id", "unknown"));
            }
            let held = inventory::bookings_on_unit(&mut tx, unit, today()).await?;
            let overlapping: Vec<_> = held
                .into_iter()
                .filter(|h| {
                    h.arrival.is_none_or(|a| a < b.to) && h.departure.is_none_or(|d| d > b.from)
                })
                .collect();
            if !overlapping.is_empty() {
                return Err(blocked_by_bookings(
                    "unit_booked",
                    "this unit is allocated during these dates",
                    &overlapping,
                    json!({}),
                ));
            }
            sqlx::query!(
                "INSERT INTO closures (id, scope, unit_id, period, reason, is_public, created_by)
                 VALUES ($1, 'unit', $2, $3, $4, $5, $6)",
                closure_id,
                unit,
                period as DateRange,
                reason,
                b.is_public,
                staff.id
            )
            .execute(&mut *tx)
            .await?;
            let mode = sqlx::query_scalar!(
                "SELECT inventory_mode FROM accommodation_types WHERE product_id = $1",
                id
            )
            .fetch_one(&mut *tx)
            .await?;
            resync(&mut tx, id, &mode).await?;
        }
        None => {
            sqlx::query!(
                "INSERT INTO closures (id, scope, product_id, period, reason, is_public, created_by)
                 VALUES ($1, 'product', $2, $3, $4, $5, $6)",
                closure_id,
                id,
                period as DateRange,
                reason,
                b.is_public,
                staff.id
            )
            .execute(&mut *tx)
            .await?;
            if kind == Kind::Activities {
                inventory::apply_slot_closures(&mut tx, Some(id), Utc::now()).await?;
            }
        }
    }
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "closure.created",
        json!({ "closure_id": closure_id, "from": b.from, "to": b.to, "unit_id": b.unit_id }),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": closure_id }))))
}

pub async fn delete_closure(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id, closure_id)): Path<(String, Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    staff.require("inventory.write")?;
    let kind = Kind::parse(&kind)?;
    let mut tx = state.db.begin().await?;
    product_of_kind(&mut tx, kind, id).await?;
    let gone = sqlx::query!(
        r#"DELETE FROM closures c WHERE c.id = $1
             AND (c.product_id = $2 OR c.unit_id IN (SELECT u.id FROM accommodation_units u WHERE u.product_id = $2))
           RETURNING c.scope, c.period AS "period: DateRange""#,
        closure_id,
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if gone.scope == "unit" {
        let mode = sqlx::query_scalar!(
            "SELECT inventory_mode FROM accommodation_types WHERE product_id = $1",
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        resync(&mut tx, id, &mode).await?;
    } else if kind == Kind::Activities {
        inventory::reopen_after_closure(&mut tx, Some(id), gone.period, Utc::now()).await?;
    }
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "closure.deleted",
        json!({ "closure_id": closure_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
