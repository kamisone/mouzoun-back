//! Shared settings of the catalogue: seasons, rate plans, guest categories, cancellation
//! policies (Tarifs, `pricing.write`) and resources (`inventory.write`), plus the reference
//! lists the editors need. A setting still used (prices, bookings, rate plans, assignments)
//! is never deleted: deactivate it or remove its uses first.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Fields, field_error, settings_changed};
use crate::{
    auth::Staff,
    db::types::DateRange,
    error::{AppError, AppResult},
    state::AppState,
};

const LOCALES: [&str; 3] = ["fr", "en", "ar"];

fn in_use(what: &str, count: i64) -> AppError {
    AppError::domain(
        StatusCode::CONFLICT,
        "in_use",
        format!("still used by {count} {what}"),
    )
    .with_details(json!({ "what": what, "count": count }))
}

fn code_ok(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 40
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.is_unique_violation())
}

fn names_ok(names: &BTreeMap<String, String>, f: &mut Fields) {
    if names.get("fr").is_none_or(|n| n.trim().is_empty()) {
        f.add("names.fr", "required");
    }
}

async fn write_names(
    conn: &mut PgConnection,
    table: &str,
    fk: &str,
    id: Uuid,
    names: &BTreeMap<String, String>,
    descriptions: Option<&BTreeMap<String, String>>,
) -> sqlx::Result<()> {
    sqlx::query(&format!("DELETE FROM {table} WHERE {fk} = $1"))
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for (l, n) in names {
        if n.trim().is_empty() || !LOCALES.contains(&l.as_str()) {
            continue;
        }
        let description = descriptions
            .and_then(|d| d.get(l))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        if descriptions.is_some() {
            sqlx::query(&format!(
                "INSERT INTO {table} ({fk}, locale, name, description) VALUES ($1, $2, $3, $4)"
            ))
            .bind(id)
            .bind(l)
            .bind(n.trim())
            .bind(description)
            .execute(&mut *conn)
            .await?;
        } else {
            sqlx::query(&format!(
                "INSERT INTO {table} ({fk}, locale, name) VALUES ($1, $2, $3)"
            ))
            .bind(id)
            .bind(l)
            .bind(n.trim())
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

// ---- References (selects of the editors) ----------------------------------------------------------

pub async fn references(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("products.read")?;
    let db = &state.db;
    let seasons = sqlx::query!(
        r#"SELECT s.id, s.code::text AS "code!", s.color, s.priority,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM season_translations WHERE season_id = s.id), '{}') AS "names!"
           FROM seasons s ORDER BY s.priority DESC, s.code"#
    )
    .fetch_all(db)
    .await?;
    let plans = sqlx::query!(
        r#"SELECT r.id, r.code::text AS "code!", r.is_public, r.is_active, r.cancellation_policy_id, r.deposit_type, r.payment_mode,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM rate_plan_translations WHERE rate_plan_id = r.id), '{}') AS "names!"
           FROM rate_plans r ORDER BY r.sort_order, r.code"#
    )
    .fetch_all(db)
    .await?;
    let categories = sqlx::query!(
        r#"SELECT g.id, g.code::text AS "code!", g.min_age, g.max_age, g.is_active, g.counts_toward_capacity,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM guest_category_translations WHERE guest_category_id = g.id), '{}') AS "names!"
           FROM guest_categories g ORDER BY g.sort_order, g.code"#
    )
    .fetch_all(db)
    .await?;
    let policies = sqlx::query!(
        r#"SELECT p.id, p.code::text AS "code!", p.is_default, p.rules,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM cancellation_policy_translations WHERE cancellation_policy_id = p.id), '{}') AS "names!"
           FROM cancellation_policies p ORDER BY p.code"#
    )
    .fetch_all(db)
    .await?;
    let taxes = sqlx::query!(
        r#"SELECT t.id, t.code::text AS "code!", t.calc_type, t.rate_percent, t.amount_minor, t.is_inclusive, t.is_active,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM tax_translations WHERE tax_id = t.id), '{}') AS "names!"
           FROM taxes t ORDER BY t.code"#
    )
    .fetch_all(db)
    .await?;
    Ok(Json(json!({
        "seasons": seasons.iter().map(|s| json!({ "id": s.id, "code": s.code, "color": s.color, "priority": s.priority, "names": s.names })).collect::<Vec<_>>(),
        "rate_plans": plans.iter().map(|r| json!({
            "id": r.id, "code": r.code, "is_public": r.is_public, "is_active": r.is_active, "names": r.names,
            "cancellation_policy_id": r.cancellation_policy_id, "deposit_type": r.deposit_type, "payment_mode": r.payment_mode,
        })).collect::<Vec<_>>(),
        "guest_categories": categories.iter().map(|g| json!({
            "id": g.id, "code": g.code, "min_age": g.min_age, "max_age": g.max_age, "is_active": g.is_active,
            "counts_toward_capacity": g.counts_toward_capacity, "names": g.names,
        })).collect::<Vec<_>>(),
        "cancellation_policies": policies.iter().map(|p| json!({ "id": p.id, "code": p.code, "is_default": p.is_default, "rules": p.rules, "names": p.names })).collect::<Vec<_>>(),
        "taxes": taxes.iter().map(|t| json!({
            "id": t.id, "code": t.code, "calc_type": t.calc_type, "rate_percent": t.rate_percent.map(|d| d.normalize()),
            "amount_minor": t.amount_minor, "is_inclusive": t.is_inclusive, "is_active": t.is_active, "names": t.names,
        })).collect::<Vec<_>>(),
        "locales": LOCALES,
        "defaults": { "check_in_from": crate::services::booking::trip::pricing::DEFAULT_CHECK_IN.format("%H:%M").to_string() },
    })))
}

// ---- Seasons --------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Period {
    pub from: NaiveDate,
    /// Exclusive.
    pub to: NaiveDate,
}

#[derive(Debug, Deserialize)]
pub struct SeasonBody {
    pub code: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub periods: Vec<Period>,
}

pub async fn seasons(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("pricing.read")?;
    let rows = sqlx::query!(
        r#"SELECT s.id, s.code::text AS "code!", s.color, s.priority,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM season_translations WHERE season_id = s.id), '{}') AS "names!",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('from', lower(period), 'to', upper(period)) ORDER BY lower(period))
                            FROM season_periods WHERE season_id = s.id), '[]') AS "periods!",
                  (SELECT count(*) FROM prices WHERE season_id = s.id) AS "prices!"
           FROM seasons s ORDER BY s.priority DESC, s.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|s| json!({
        "id": s.id, "code": s.code, "color": s.color, "priority": s.priority, "names": s.names, "periods": s.periods,
        "price_rules": s.prices,
    })).collect::<Vec<_>>() })))
}

async fn save_season(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    b: SeasonBody,
) -> AppResult<Value> {
    staff.require("pricing.write")?;
    let mut f = Fields::default();
    if !code_ok(b.code.trim()) {
        f.add("code", "format");
    }
    if let Some(c) = &b.color
        && !(c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit()))
    {
        f.add("color", "format");
    }
    names_ok(&b.names, &mut f);
    for (i, p) in b.periods.iter().enumerate() {
        if p.to <= p.from {
            f.add(format!("periods[{i}].to"), "before_from");
        }
        for (j, q) in b.periods.iter().enumerate().skip(i + 1) {
            if p.from < q.to && q.from < p.to {
                f.add(format!("periods[{j}]"), "overlap");
            }
        }
    }
    f.check()?;
    let mut tx = state.db.begin().await?;
    let sid = id.unwrap_or_else(Uuid::now_v7);
    let res = match id {
        None => {
            sqlx::query!(
                "INSERT INTO seasons (id, code, color, priority) VALUES ($1, $2, $3, $4)",
                sid,
                b.code.trim(),
                b.color,
                b.priority
            )
            .execute(&mut *tx)
            .await
        }
        Some(_) => {
            sqlx::query!(
                "UPDATE seasons SET code = $2, color = $3, priority = $4 WHERE id = $1",
                sid,
                b.code.trim(),
                b.color,
                b.priority
            )
            .execute(&mut *tx)
            .await
        }
    };
    match res {
        Err(e) if unique_violation(&e) => return Err(field_error("code", "taken")),
        Err(e) => return Err(e.into()),
        Ok(r) if r.rows_affected() == 0 => return Err(AppError::NotFound),
        Ok(_) => {}
    }
    write_names(
        &mut tx,
        "season_translations",
        "season_id",
        sid,
        &b.names,
        None,
    )
    .await?;
    sqlx::query!("DELETE FROM season_periods WHERE season_id = $1", sid)
        .execute(&mut *tx)
        .await?;
    for p in &b.periods {
        sqlx::query!(
            "INSERT INTO season_periods (season_id, period) VALUES ($1, $2)",
            sid,
            DateRange::new(p.from, p.to) as DateRange
        )
        .execute(&mut *tx)
        .await?;
    }
    settings_changed(
        &mut tx,
        &staff.actor(),
        "season.saved",
        "season",
        sid,
        json!({ "code": b.code.trim(), "periods": b.periods.len() }),
    )
    .await?;
    tx.commit().await?;
    Ok(json!({ "id": sid }))
}

pub async fn create_season(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<SeasonBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(save_season(&state, &staff, None, b).await?),
    ))
}

pub async fn update_season(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<SeasonBody>,
) -> AppResult<Json<Value>> {
    Ok(Json(save_season(&state, &staff, Some(id), b).await?))
}

pub async fn delete_season(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("pricing.write")?;
    let mut tx = state.db.begin().await?;
    let used = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM prices WHERE season_id = $1"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if used > 0 {
        return Err(in_use("price_rules", used));
    }
    if sqlx::query!("DELETE FROM seasons WHERE id = $1", id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Err(AppError::NotFound);
    }
    settings_changed(
        &mut tx,
        &staff.actor(),
        "season.deleted",
        "season",
        id,
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Rate plans -----------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RatePlanBody {
    pub code: String,
    #[serde(default)]
    pub cancellation_policy_id: Option<Uuid>,
    #[serde(default = "none")]
    pub meal_plan: String,
    #[serde(default = "none")]
    pub deposit_type: String,
    #[serde(default, deserialize_with = "crate::web::percent::option")]
    pub deposit_percent: Option<Decimal>,
    #[serde(default)]
    pub deposit_amount_minor: Option<i64>,
    #[serde(default = "pay_on_site")]
    pub payment_mode: String,
    #[serde(default = "yes")]
    pub is_public: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub descriptions: BTreeMap<String, String>,
}

fn none() -> String {
    "none".into()
}

fn pay_on_site() -> String {
    "pay_on_site".into()
}

fn yes() -> bool {
    true
}

pub async fn rate_plans(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("pricing.read")?;
    let rows = sqlx::query!(
        r#"SELECT r.id, r.code::text AS "code!", r.cancellation_policy_id, r.meal_plan, r.deposit_type, r.deposit_percent,
                  r.deposit_amount_minor, r.payment_mode, r.is_public, r.is_active, r.sort_order,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM rate_plan_translations WHERE rate_plan_id = r.id), '{}') AS "names!",
                  COALESCE((SELECT jsonb_object_agg(locale, description) FROM rate_plan_translations WHERE rate_plan_id = r.id
                            AND description IS NOT NULL), '{}') AS "descriptions!",
                  (SELECT count(*) FROM prices WHERE rate_plan_id = r.id) AS "prices!"
           FROM rate_plans r ORDER BY r.sort_order, r.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|r| json!({
        "id": r.id, "code": r.code, "cancellation_policy_id": r.cancellation_policy_id, "meal_plan": r.meal_plan,
        "deposit_type": r.deposit_type, "deposit_percent": r.deposit_percent.map(|d| d.normalize()),
        "deposit_amount_minor": r.deposit_amount_minor, "payment_mode": r.payment_mode, "is_public": r.is_public,
        "is_active": r.is_active, "sort_order": r.sort_order, "names": r.names, "descriptions": r.descriptions,
        "price_rules": r.prices,
    })).collect::<Vec<_>>() })))
}

async fn save_rate_plan(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    b: RatePlanBody,
) -> AppResult<Value> {
    staff.require("pricing.write")?;
    let mut f = Fields::default();
    if !code_ok(b.code.trim()) {
        f.add("code", "format");
    }
    names_ok(&b.names, &mut f);
    if !["none", "breakfast", "half_board", "full_board"].contains(&b.meal_plan.as_str()) {
        f.add("meal_plan", "invalid");
    }
    if !["pay_on_site", "optional_online", "online_required"].contains(&b.payment_mode.as_str()) {
        f.add("payment_mode", "invalid");
    }
    match b.deposit_type.as_str() {
        "none" | "full" => {}
        "percent" => match b.deposit_percent {
            Some(p) if p > Decimal::ZERO && p <= Decimal::ONE_HUNDRED => {}
            Some(_) => f.add("deposit_percent", "range"),
            None => f.add("deposit_percent", "required"),
        },
        "fixed" => match b.deposit_amount_minor {
            Some(a) if a > 0 => {}
            _ => f.add("deposit_amount_minor", "required"),
        },
        _ => f.add("deposit_type", "invalid"),
    }
    f.check()?;
    let percent = (b.deposit_type == "percent")
        .then_some(b.deposit_percent)
        .flatten();
    let amount = (b.deposit_type == "fixed")
        .then_some(b.deposit_amount_minor)
        .flatten();
    let mut tx = state.db.begin().await?;
    let currency = crate::db::foundation::default_currency(&mut *tx)
        .await?
        .code;
    let rid = id.unwrap_or_else(Uuid::now_v7);
    let res = sqlx::query!(
        "INSERT INTO rate_plans (id, code, cancellation_policy_id, meal_plan, deposit_type, deposit_percent, deposit_amount_minor,
                                 deposit_currency, payment_mode, is_public, is_active, sort_order)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT (id) DO UPDATE SET code = $2, cancellation_policy_id = $3, meal_plan = $4, deposit_type = $5,
             deposit_percent = $6, deposit_amount_minor = $7, deposit_currency = $8, payment_mode = $9, is_public = $10,
             is_active = $11, sort_order = $12",
        rid,
        b.code.trim(),
        b.cancellation_policy_id,
        b.meal_plan,
        b.deposit_type,
        percent,
        amount,
        amount.map(|_| currency.as_str().to_string()),
        b.payment_mode,
        b.is_public,
        b.is_active,
        b.sort_order
    )
    .execute(&mut *tx)
    .await;
    match res {
        Err(e) if unique_violation(&e) => return Err(field_error("code", "taken")),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    write_names(
        &mut tx,
        "rate_plan_translations",
        "rate_plan_id",
        rid,
        &b.names,
        Some(&b.descriptions),
    )
    .await?;
    settings_changed(
        &mut tx,
        &staff.actor(),
        "rate_plan.saved",
        "rate_plan",
        rid,
        json!({ "code": b.code.trim() }),
    )
    .await?;
    tx.commit().await?;
    Ok(json!({ "id": rid }))
}

pub async fn create_rate_plan(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<RatePlanBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(save_rate_plan(&state, &staff, None, b).await?),
    ))
}

pub async fn update_rate_plan(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<RatePlanBody>,
) -> AppResult<Json<Value>> {
    let mut conn = state.db.acquire().await?;
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM rate_plans WHERE id = $1) AS "x!""#,
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    drop(conn);
    if !exists {
        return Err(AppError::NotFound);
    }
    Ok(Json(save_rate_plan(&state, &staff, Some(id), b).await?))
}

pub async fn delete_rate_plan(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("pricing.write")?;
    let mut tx = state.db.begin().await?;
    let used = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM prices WHERE rate_plan_id = $1) AS "prices!",
                  (SELECT count(*) FROM bookings WHERE rate_plan_id = $1) + (SELECT count(*) FROM booking_items WHERE rate_plan_id = $1) AS "bookings!""#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if used.bookings > 0 {
        return Err(in_use("bookings", used.bookings));
    }
    if used.prices > 0 {
        return Err(in_use("price_rules", used.prices));
    }
    if sqlx::query!("DELETE FROM rate_plans WHERE id = $1", id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Err(AppError::NotFound);
    }
    settings_changed(
        &mut tx,
        &staff.actor(),
        "rate_plan.deleted",
        "rate_plan",
        id,
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Guest categories -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GuestCategoryBody {
    pub code: String,
    #[serde(default)]
    pub min_age: Option<i16>,
    #[serde(default)]
    pub max_age: Option<i16>,
    #[serde(default = "yes")]
    pub counts_toward_capacity: bool,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub names: BTreeMap<String, String>,
}

pub async fn guest_categories(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<Json<Value>> {
    staff.require("pricing.read")?;
    let rows = sqlx::query!(
        r#"SELECT g.id, g.code::text AS "code!", g.min_age, g.max_age, g.counts_toward_capacity, g.sort_order, g.is_active,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM guest_category_translations WHERE guest_category_id = g.id), '{}') AS "names!",
                  (SELECT count(*) FROM prices WHERE guest_category_id = g.id) AS "prices!"
           FROM guest_categories g ORDER BY g.sort_order, g.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|g| json!({
        "id": g.id, "code": g.code, "min_age": g.min_age, "max_age": g.max_age, "counts_toward_capacity": g.counts_toward_capacity,
        "sort_order": g.sort_order, "is_active": g.is_active, "names": g.names, "price_rules": g.prices,
    })).collect::<Vec<_>>() })))
}

async fn save_guest_category(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    b: GuestCategoryBody,
) -> AppResult<Value> {
    staff.require("pricing.write")?;
    let mut f = Fields::default();
    if !code_ok(b.code.trim()) {
        f.add("code", "format");
    }
    names_ok(&b.names, &mut f);
    if b.min_age.is_some_and(|a| a < 0) {
        f.add("min_age", "min");
    }
    if let (Some(a), Some(z)) = (b.min_age, b.max_age)
        && z < a
    {
        f.add("max_age", "below_min");
    }
    f.check()?;
    let mut tx = state.db.begin().await?;
    let gid = id.unwrap_or_else(Uuid::now_v7);
    let res = sqlx::query!(
        "INSERT INTO guest_categories (id, code, min_age, max_age, counts_toward_capacity, sort_order, is_active)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (id) DO UPDATE SET code = $2, min_age = $3, max_age = $4, counts_toward_capacity = $5, sort_order = $6,
             is_active = $7",
        gid,
        b.code.trim(),
        b.min_age,
        b.max_age,
        b.counts_toward_capacity,
        b.sort_order,
        b.is_active
    )
    .execute(&mut *tx)
    .await;
    match res {
        Err(e) if unique_violation(&e) => return Err(field_error("code", "taken")),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    write_names(
        &mut tx,
        "guest_category_translations",
        "guest_category_id",
        gid,
        &b.names,
        Some(&BTreeMap::new()),
    )
    .await?;
    settings_changed(
        &mut tx,
        &staff.actor(),
        "guest_category.saved",
        "guest_category",
        gid,
        json!({ "code": b.code.trim() }),
    )
    .await?;
    tx.commit().await?;
    Ok(json!({ "id": gid }))
}

pub async fn create_guest_category(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<GuestCategoryBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(save_guest_category(&state, &staff, None, b).await?),
    ))
}

pub async fn update_guest_category(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<GuestCategoryBody>,
) -> AppResult<Json<Value>> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM guest_categories WHERE id = $1) AS "x!""#,
        id
    )
    .fetch_one(&state.db)
    .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    Ok(Json(
        save_guest_category(&state, &staff, Some(id), b).await?,
    ))
}

pub async fn delete_guest_category(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("pricing.write")?;
    let mut tx = state.db.begin().await?;
    let used = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM prices WHERE guest_category_id = $1) AS "prices!",
                  (SELECT count(*) FROM booking_item_guests WHERE guest_category_id = $1) AS "bookings!""#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if used.bookings > 0 {
        return Err(in_use("bookings", used.bookings));
    }
    if used.prices > 0 {
        return Err(in_use("price_rules", used.prices));
    }
    if sqlx::query!("DELETE FROM guest_categories WHERE id = $1", id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Err(AppError::NotFound);
    }
    settings_changed(
        &mut tx,
        &staff.actor(),
        "guest_category.deleted",
        "guest_category",
        id,
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Cancellation policies ------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct PolicyRule {
    pub min_hours_before: i64,
    pub refund_percent: i64,
}

#[derive(Debug, Deserialize)]
pub struct PolicyBody {
    pub code: String,
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub descriptions: BTreeMap<String, String>,
}

pub async fn policies(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("pricing.read")?;
    let rows = sqlx::query!(
        r#"SELECT p.id, p.code::text AS "code!", p.rules, p.is_default,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM cancellation_policy_translations WHERE cancellation_policy_id = p.id), '{}') AS "names!",
                  COALESCE((SELECT jsonb_object_agg(locale, description) FROM cancellation_policy_translations
                            WHERE cancellation_policy_id = p.id AND description IS NOT NULL), '{}') AS "descriptions!",
                  (SELECT count(*) FROM rate_plans WHERE cancellation_policy_id = p.id) AS "plans!"
           FROM cancellation_policies p ORDER BY p.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|p| json!({
        "id": p.id, "code": p.code, "rules": p.rules, "is_default": p.is_default, "names": p.names,
        "descriptions": p.descriptions, "rate_plans": p.plans,
    })).collect::<Vec<_>>() })))
}

async fn save_policy(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    b: PolicyBody,
) -> AppResult<Value> {
    staff.require("pricing.write")?;
    let mut f = Fields::default();
    if !code_ok(b.code.trim()) {
        f.add("code", "format");
    }
    names_ok(&b.names, &mut f);
    if b.rules.is_empty() {
        f.add("rules", "required");
    }
    for (i, r) in b.rules.iter().enumerate() {
        if r.min_hours_before < 0 {
            f.add(format!("rules[{i}].min_hours_before"), "min");
        }
        if !(0..=100).contains(&r.refund_percent) {
            f.add(format!("rules[{i}].refund_percent"), "range");
        }
        if b.rules
            .iter()
            .filter(|x| x.min_hours_before == r.min_hours_before)
            .count()
            > 1
        {
            f.add(format!("rules[{i}].min_hours_before"), "duplicate");
        }
    }
    f.check()?;
    let mut rules = b.rules.clone();
    rules.sort_by_key(|r| std::cmp::Reverse(r.min_hours_before));
    let mut tx = state.db.begin().await?;
    let pid = id.unwrap_or_else(Uuid::now_v7);
    if b.is_default {
        sqlx::query!(
            "UPDATE cancellation_policies SET is_default = false WHERE is_default AND id <> $1",
            pid
        )
        .execute(&mut *tx)
        .await?;
    }
    let res = sqlx::query!(
        "INSERT INTO cancellation_policies (id, code, rules, is_default) VALUES ($1, $2, $3, $4)
         ON CONFLICT (id) DO UPDATE SET code = $2, rules = $3, is_default = $4",
        pid,
        b.code.trim(),
        json!(rules),
        b.is_default
    )
    .execute(&mut *tx)
    .await;
    match res {
        Err(e) if unique_violation(&e) => return Err(field_error("code", "taken")),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    write_names(
        &mut tx,
        "cancellation_policy_translations",
        "cancellation_policy_id",
        pid,
        &b.names,
        Some(&b.descriptions),
    )
    .await?;
    settings_changed(
        &mut tx,
        &staff.actor(),
        "cancellation_policy.saved",
        "cancellation_policy",
        pid,
        json!({ "code": b.code.trim() }),
    )
    .await?;
    tx.commit().await?;
    Ok(json!({ "id": pid }))
}

pub async fn create_policy(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<PolicyBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(save_policy(&state, &staff, None, b).await?),
    ))
}

pub async fn update_policy(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<PolicyBody>,
) -> AppResult<Json<Value>> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM cancellation_policies WHERE id = $1) AS "x!""#,
        id
    )
    .fetch_one(&state.db)
    .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    Ok(Json(save_policy(&state, &staff, Some(id), b).await?))
}

pub async fn delete_policy(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("pricing.write")?;
    let mut tx = state.db.begin().await?;
    let used = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM rate_plans WHERE cancellation_policy_id = $1"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if used > 0 {
        return Err(in_use("rate_plans", used));
    }
    if sqlx::query!("DELETE FROM cancellation_policies WHERE id = $1", id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Err(AppError::NotFound);
    }
    settings_changed(
        &mut tx,
        &staff.actor(),
        "cancellation_policy.deleted",
        "cancellation_policy",
        id,
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Resources ------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ResourceBody {
    pub kind: String,
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub seats: Option<i32>,
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default = "active")]
    pub status: String,
    #[serde(default)]
    pub notes: Option<String>,
}

fn active() -> String {
    "active".into()
}

pub async fn resources(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    let rows = sqlx::query!(
        r#"SELECT r.id, r.kind, r.code::text AS "code!", r.name, r.seats, r.user_id, r.status, r.notes, u.full_name AS "user_name?"
           FROM resources r LEFT JOIN users u ON u.id = r.user_id ORDER BY r.kind, r.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.iter().map(|r| json!({
        "id": r.id, "kind": r.kind, "code": r.code, "name": r.name, "seats": r.seats, "user_id": r.user_id,
        "user_name": r.user_name, "status": r.status, "notes": r.notes,
    })).collect::<Vec<_>>() })))
}

async fn save_resource(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    b: ResourceBody,
) -> AppResult<Value> {
    staff.require("inventory.write")?;
    let mut f = Fields::default();
    if !["vehicle", "equipment", "animal", "guide", "room", "other"].contains(&b.kind.as_str()) {
        f.add("kind", "invalid");
    }
    if b.code.trim().is_empty() || b.code.trim().len() > 30 {
        f.add("code", "format");
    }
    if b.name.trim().is_empty() {
        f.add("name", "required");
    }
    if b.seats.is_some_and(|s| s < 1) {
        f.add("seats", "min");
    }
    if !["active", "maintenance", "retired"].contains(&b.status.as_str()) {
        f.add("status", "invalid");
    }
    f.check()?;
    let mut tx = state.db.begin().await?;
    let rid = id.unwrap_or_else(Uuid::now_v7);
    let res = sqlx::query!(
        "INSERT INTO resources (id, kind, code, name, seats, user_id, status, notes) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (id) DO UPDATE SET kind = $2, code = $3, name = $4, seats = $5, user_id = $6, status = $7, notes = $8",
        rid,
        b.kind,
        b.code.trim(),
        b.name.trim(),
        b.seats,
        b.user_id,
        b.status,
        b.notes.as_deref().map(str::trim).filter(|s| !s.is_empty())
    )
    .execute(&mut *tx)
    .await;
    match res {
        Err(e) if unique_violation(&e) => return Err(field_error("code", "taken")),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    crate::services::audit::record(
        &mut tx,
        &staff.actor(),
        "resource.saved",
        "resource",
        &rid.to_string(),
        json!({ "code": b.code.trim(), "status": b.status }),
    )
    .await?;
    tx.commit().await?;
    Ok(json!({ "id": rid }))
}

pub async fn create_resource(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<ResourceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(save_resource(&state, &staff, None, b).await?),
    ))
}

pub async fn update_resource(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<ResourceBody>,
) -> AppResult<Json<Value>> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM resources WHERE id = $1) AS "x!""#,
        id
    )
    .fetch_one(&state.db)
    .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    Ok(Json(save_resource(&state, &staff, Some(id), b).await?))
}

pub async fn delete_resource(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("inventory.write")?;
    let mut tx = state.db.begin().await?;
    let used = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM resource_assignments WHERE resource_id = $1"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if used > 0 {
        return Err(in_use("bookings", used));
    }
    if sqlx::query!("DELETE FROM resources WHERE id = $1", id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Err(AppError::NotFound);
    }
    crate::services::audit::record(
        &mut tx,
        &staff.actor(),
        "resource.deleted",
        "resource",
        &id.to_string(),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
