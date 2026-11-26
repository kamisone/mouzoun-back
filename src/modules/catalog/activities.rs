//! Activities: recurring schedules (fixed departures), the departures they generate, one-off
//! departures, guides and resource requirements ("1 quad pour 2 participants").
//!
//! Slots come only from `services::inventory::sync_activity_slots` (the daily job uses the same
//! planner). A departure with bookings is never deleted, shrunk below its bookings or cancelled.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Fields, Kind, blocked_by_bookings, changed, field_error, product_of_kind};
use crate::{
    auth::Staff,
    db::types::DateRange,
    error::{AppError, AppResult},
    services::{
        inventory::{self, PlannedSlot, ScheduleRule, SlotSync},
        settings,
    },
    state::AppState,
};

const RESOURCE_KINDS: [&str; 6] = ["vehicle", "equipment", "animal", "guide", "room", "other"];

async fn activity(conn: &mut PgConnection, kind: &str, id: Uuid) -> AppResult<()> {
    let kind = Kind::parse(kind)?;
    if kind != Kind::Activities {
        return Err(AppError::NotFound);
    }
    product_of_kind(conn, kind, id).await
}

async fn horizon(state: &AppState) -> AppResult<i64> {
    Ok(settings::number(&state.db, "booking.max_days_ahead", 365).await?)
}

/// `Some(ok)` for fixed-departure activities: an active schedule and a future open departure.
pub async fn departures_ok(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<bool>> {
    let r = sqlx::query!(
        r#"SELECT a.scheduling_mode,
                  EXISTS (SELECT 1 FROM activity_schedules s WHERE s.product_id = a.product_id AND s.is_active) AS "schedule!",
                  EXISTS (SELECT 1 FROM activity_slots s WHERE s.product_id = a.product_id AND s.starts_at > now()
                          AND s.status = 'open') AS "slots!"
           FROM activities a WHERE a.product_id = $1"#,
        id
    )
    .fetch_optional(conn)
    .await?;
    Ok(r.filter(|r| r.scheduling_mode == "fixed_slots")
        .map(|r| r.schedule && r.slots))
}

// ---- Schedules ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleBody {
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Mon=1 … Sun=64.
    pub days_of_week: i16,
    /// Camp time (Africa/Casablanca).
    pub start_time: NaiveTime,
    pub capacity: i32,
    pub valid_from: NaiveDate,
    /// Exclusive; `None` = open-ended.
    #[serde(default)]
    pub valid_to: Option<NaiveDate>,
    #[serde(default = "yes")]
    pub is_active: bool,
}

fn yes() -> bool {
    true
}

fn validate_schedules(s: &[ScheduleBody], f: &mut Fields) {
    for (i, x) in s.iter().enumerate() {
        if !(1..=127).contains(&x.days_of_week) {
            f.add(format!("schedules[{i}].days_of_week"), "required");
        }
        if x.capacity < 1 {
            f.add(format!("schedules[{i}].capacity"), "min");
        }
        if x.valid_to.is_some_and(|t| t <= x.valid_from) {
            f.add(format!("schedules[{i}].valid_to"), "before_from");
        }
    }
}

fn rules(product_id: Uuid, duration: Option<i32>, s: &[ScheduleBody]) -> Vec<ScheduleRule> {
    s.iter()
        .filter(|x| x.is_active)
        .map(|x| ScheduleRule {
            id: x.id.unwrap_or_else(Uuid::nil),
            product_id,
            days_of_week: x.days_of_week,
            start_time: x.start_time,
            capacity: x.capacity,
            valid_period: DateRange {
                start: Some(x.valid_from),
                end: x.valid_to,
            },
            duration_minutes: duration,
        })
        .collect()
}

/// Replaces the schedules and syncs the future departures (same transaction).
pub async fn write_schedules(
    conn: &mut PgConnection,
    state: &AppState,
    kind: Kind,
    id: Uuid,
    s: &[ScheduleBody],
) -> AppResult<SlotSync> {
    if kind != Kind::Activities {
        return Err(field_error("schedules", "not_allowed"));
    }
    let mut f = Fields::default();
    validate_schedules(s, &mut f);
    f.check()?;
    let keep: Vec<Uuid> = s.iter().filter_map(|x| x.id).collect();
    let now = Utc::now();
    // Departures of the schedules removed: unbooked future ones go, booked ones stay (their
    // schedule_id becomes NULL) and are reported.
    let orphans = sqlx::query!(
        "SELECT s.id, s.starts_at, s.reserved,
                EXISTS (SELECT 1 FROM booking_items i WHERE i.activity_slot_id = s.id) AS \"used!\"
         FROM activity_slots s JOIN activity_schedules sc ON sc.id = s.schedule_id
         WHERE sc.product_id = $1 AND NOT (sc.id = ANY($2)) AND s.starts_at > $3
         ORDER BY s.id FOR UPDATE OF s",
        id,
        &keep,
        now
    )
    .fetch_all(&mut *conn)
    .await?;
    let (free, booked): (Vec<_>, Vec<_>) = orphans
        .into_iter()
        .partition(|o| o.reserved == 0 && !o.used);
    let dropped = sqlx::query!(
        "DELETE FROM activity_slots WHERE id = ANY($1)",
        &free.iter().map(|o| o.id).collect::<Vec<_>>()
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query!(
        "DELETE FROM activity_schedules WHERE product_id = $1 AND NOT (id = ANY($2))",
        id,
        &keep
    )
    .execute(&mut *conn)
    .await?;
    for x in s {
        let period = DateRange {
            start: Some(x.valid_from),
            end: x.valid_to,
        };
        let sid = x.id.unwrap_or_else(Uuid::now_v7);
        sqlx::query!(
            "INSERT INTO activity_schedules (id, product_id, days_of_week, start_time, capacity, valid_period, is_active)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE SET days_of_week = $3, start_time = $4, capacity = $5, valid_period = $6, is_active = $7
             WHERE activity_schedules.product_id = $2",
            sid,
            id,
            x.days_of_week,
            x.start_time,
            x.capacity,
            period as DateRange,
            x.is_active
        )
        .execute(&mut *conn)
        .await?;
    }
    let days = horizon(state).await?;
    let mut sync = inventory::sync_activity_slots(conn, id, now, days).await?;
    sync.deleted += dropped;
    sync.kept_booked
        .extend(booked.into_iter().map(|o| inventory::KeptSlot {
            slot_id: o.id,
            starts_at: o.starts_at,
            reserved: o.reserved,
            reason: "no_longer_scheduled",
        }));
    Ok(sync)
}

pub async fn schedules(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    let mut conn = state.db.acquire().await?;
    activity(&mut conn, &kind, id).await?;
    let rows = sqlx::query!(
        r#"SELECT id, days_of_week, start_time, capacity, valid_period AS "valid_period: DateRange", is_active
           FROM activity_schedules WHERE product_id = $1 ORDER BY start_time, created_at"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(json!({
        "data": rows.iter().map(|s| json!({
            "id": s.id, "days_of_week": s.days_of_week, "start_time": s.start_time.format("%H:%M").to_string(),
            "capacity": s.capacity, "valid_from": s.valid_period.start, "valid_to": s.valid_period.end, "is_active": s.is_active,
        })).collect::<Vec<_>>(),
    })))
}

pub async fn put_schedules(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(s): Json<Vec<ScheduleBody>>,
) -> AppResult<Json<SlotSync>> {
    staff.require("inventory.write")?;
    let mut tx = state.db.begin().await?;
    activity(&mut tx, &kind, id).await?;
    let sync = write_schedules(&mut tx, &state, Kind::Activities, id, &s).await?;
    changed(&mut tx, &staff.actor(), Kind::Activities, id, "schedules.updated", json!({
        "schedules": s.len(), "created": sync.created, "updated": sync.updated, "deleted": sync.deleted,
        "kept_booked": sync.kept_booked.len(),
    })).await?;
    tx.commit().await?;
    Ok(Json(sync))
}

fn preview_of(planned: Vec<PlannedSlot>, now: DateTime<Utc>) -> Value {
    json!({
        "data": planned.into_iter().filter(|p| p.starts_at > now).take(14).map(|p| json!({
            "starts_at": p.starts_at, "ends_at": p.ends_at, "capacity": p.capacity,
            "local": crate::camp_time::local(p.starts_at).format("%Y-%m-%d %H:%M").to_string(),
        })).collect::<Vec<_>>(),
    })
}

/// The next 14 departures the (unsaved) schedules would create.
pub async fn preview(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(s): Json<Vec<ScheduleBody>>,
) -> AppResult<Json<Value>> {
    staff.require("products.read")?;
    let mut f = Fields::default();
    validate_schedules(&s, &mut f);
    f.check()?;
    let mut conn = state.db.acquire().await?;
    activity(&mut conn, &kind, id).await?;
    let duration = sqlx::query_scalar!(
        "SELECT duration_minutes FROM activities WHERE product_id = $1",
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    let now = Utc::now();
    let planned =
        inventory::plan_slots(&rules(id, duration, &s), crate::camp_time::today(now), 120);
    Ok(Json(preview_of(planned, now)))
}

pub async fn preview_saved(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    activity(&mut conn, &kind, id).await?;
    let saved = sqlx::query!(
        r#"SELECT s.id, s.days_of_week, s.start_time, s.capacity, s.valid_period AS "valid_period: DateRange", s.is_active,
                  a.duration_minutes
           FROM activity_schedules s JOIN activities a ON a.product_id = s.product_id WHERE s.product_id = $1 AND s.is_active"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    let rules: Vec<ScheduleRule> = saved
        .iter()
        .map(|s| ScheduleRule {
            id: s.id,
            product_id: id,
            days_of_week: s.days_of_week,
            start_time: s.start_time,
            capacity: s.capacity,
            valid_period: s.valid_period,
            duration_minutes: s.duration_minutes,
        })
        .collect();
    let now = Utc::now();
    Ok(Json(preview_of(
        inventory::plan_slots(&rules, crate::camp_time::today(now), 120),
        now,
    )))
}

// ---- Departures -----------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SlotRange {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

pub async fn slots(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Query(q): Query<SlotRange>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    let mut conn = state.db.acquire().await?;
    activity(&mut conn, &kind, id).await?;
    let from = q.from.unwrap_or_else(Utc::now);
    let to = q.to.unwrap_or(from + chrono::Duration::days(14));
    let rows = sqlx::query!(
        r#"SELECT s.id, s.schedule_id, s.starts_at, s.ends_at, s.capacity, s.reserved, s.status, s.guide_user_id,
                  s.guide_language, s.notes, u.full_name AS "guide_name?"
           FROM activity_slots s LEFT JOIN users u ON u.id = s.guide_user_id
           WHERE s.product_id = $1 AND s.starts_at >= $2 AND s.starts_at < $3
           ORDER BY s.starts_at LIMIT 500"#,
        id,
        from,
        to
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(json!({
        "data": rows.iter().map(|s| json!({
            "id": s.id, "schedule_id": s.schedule_id, "starts_at": s.starts_at, "ends_at": s.ends_at,
            "local": crate::camp_time::local(s.starts_at).format("%Y-%m-%d %H:%M").to_string(),
            "capacity": s.capacity, "reserved": s.reserved, "available": (s.capacity - s.reserved).max(0), "status": s.status,
            "guide_user_id": s.guide_user_id, "guide_name": s.guide_name, "guide_language": s.guide_language, "notes": s.notes,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct NewSlot {
    /// Camp date and time.
    pub date: NaiveDate,
    pub time: NaiveTime,
    pub capacity: i32,
    #[serde(default)]
    pub guide_user_id: Option<Uuid>,
    #[serde(default)]
    pub guide_language: Option<String>,
}

/// A one-off departure (no schedule).
pub async fn create_slot(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<NewSlot>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("inventory.write")?;
    if b.capacity < 1 {
        return Err(field_error("capacity", "min"));
    }
    let mut tx = state.db.begin().await?;
    activity(&mut tx, &kind, id).await?;
    let duration = sqlx::query_scalar!(
        "SELECT duration_minutes FROM activities WHERE product_id = $1",
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    let starts_at = crate::camp_time::to_utc(b.date.and_time(b.time));
    if starts_at <= Utc::now() {
        return Err(field_error("date", "past"));
    }
    let slot_id = Uuid::now_v7();
    let inserted = sqlx::query!(
        "INSERT INTO activity_slots (id, product_id, starts_at, ends_at, capacity, guide_user_id, guide_language)
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (product_id, starts_at) DO NOTHING",
        slot_id,
        id,
        starts_at,
        starts_at + chrono::Duration::minutes(i64::from(duration.unwrap_or(60))),
        b.capacity,
        b.guide_user_id,
        b.guide_language
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(field_error("time", "taken"));
    }
    inventory::apply_slot_closures(&mut tx, Some(id), Utc::now()).await?;
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Activities,
        id,
        "slot.created",
        json!({ "slot_id": slot_id, "starts_at": starts_at }),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": slot_id, "starts_at": starts_at })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct SlotPatch {
    #[serde(default)]
    pub capacity: Option<i32>,
    /// `open`, `closed` or `cancelled`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, with = "double_uuid")]
    pub guide_user_id: Option<Option<Uuid>>,
    #[serde(default, with = "double_string")]
    pub guide_language: Option<Option<String>>,
    #[serde(default, with = "double_string")]
    pub notes: Option<Option<String>>,
}

mod double_uuid {
    use serde::{Deserialize, Deserializer};
    use uuid::Uuid;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<Uuid>>, D::Error> {
        Ok(Some(Option::<Uuid>::deserialize(d)?))
    }
}

mod double_string {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::<String>::deserialize(d)?))
    }
}

/// Capacity (never below the bookings), close / reopen, cancel (only without bookings), guide.
pub async fn patch_slot(
    State(state): State<AppState>,
    staff: Staff,
    Path(slot_id): Path<Uuid>,
    Json(p): Json<SlotPatch>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.write")?;
    let mut tx = state.db.begin().await?;
    let s = sqlx::query!(
        "SELECT product_id, capacity, reserved, status, starts_at FROM activity_slots WHERE id = $1 FOR UPDATE",
        slot_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if let Some(c) = p.capacity {
        if c < 0 {
            return Err(field_error("capacity", "min"));
        }
        if c < s.reserved {
            let bookings = inventory::bookings_on_slots(&mut tx, &[slot_id]).await?;
            return Err(blocked_by_bookings(
                "below_reserved",
                "this departure already has more bookings",
                &bookings,
                json!({ "reserved": s.reserved }),
            ));
        }
    }
    let status = match p.status.as_deref() {
        None => None,
        Some("cancelled") => {
            if s.reserved > 0 {
                let bookings = inventory::bookings_on_slots(&mut tx, &[slot_id]).await?;
                return Err(blocked_by_bookings(
                    "slot_booked",
                    "this departure has bookings: cancel them from the booking screen first",
                    &bookings,
                    json!({ "reserved": s.reserved }),
                ));
            }
            Some("cancelled")
        }
        Some("closed") => Some("closed"),
        Some("open") => Some("open"),
        Some(_) => return Err(field_error("status", "invalid")),
    };
    if let Some(Some(lang)) = &p.guide_language
        && !(2..=8).contains(&lang.len())
    {
        return Err(field_error("guide_language", "invalid"));
    }
    let capacity = p.capacity.unwrap_or(s.capacity);
    // `open` means bookable: `full` when no place is left.
    let status = status.map(|st| {
        if st == "open" && s.reserved >= capacity {
            "full"
        } else {
            st
        }
    });
    let status = status.or_else(|| {
        p.capacity.and_then(|c| match s.status.as_str() {
            "open" if s.reserved >= c => Some("full"),
            "full" if s.reserved < c => Some("open"),
            _ => None,
        })
    });
    sqlx::query!(
        "UPDATE activity_slots SET capacity = $2, status = COALESCE($3, status),
                guide_user_id = CASE WHEN $4 THEN $5 ELSE guide_user_id END,
                guide_language = CASE WHEN $6 THEN $7 ELSE guide_language END,
                notes = CASE WHEN $8 THEN $9 ELSE notes END
         WHERE id = $1",
        slot_id,
        capacity,
        status,
        p.guide_user_id.is_some(),
        p.guide_user_id.flatten(),
        p.guide_language.is_some(),
        p.guide_language.clone().flatten(),
        p.notes.is_some(),
        p.notes.clone().flatten()
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if e.as_database_error()
            .is_some_and(|d| d.is_foreign_key_violation())
        {
            field_error("guide_language", "unknown")
        } else {
            e.into()
        }
    })?;
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Activities,
        s.product_id,
        "slot.updated",
        json!({
            "slot_id": slot_id, "starts_at": s.starts_at, "capacity": p.capacity, "status": status,
            "guide_user_id": p.guide_user_id,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({ "id": slot_id, "capacity": capacity, "status": status.unwrap_or(&s.status) }),
    ))
}

/// Staff who can guide: users with the `guide` role or linked to a `guide` resource.
pub async fn guides(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    let rows = sqlx::query!(
        r#"SELECT DISTINCT u.id, u.full_name, u.email::text AS "email!"
           FROM users u
           WHERE u.status = 'active' AND (
               EXISTS (SELECT 1 FROM user_roles ur JOIN roles r ON r.id = ur.role_id WHERE ur.user_id = u.id AND r.code = 'guide')
               OR EXISTS (SELECT 1 FROM resources x WHERE x.user_id = u.id AND x.kind = 'guide' AND x.status = 'active'))
           ORDER BY u.full_name"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "data": rows.iter().map(|g| json!({ "id": g.id, "name": g.full_name, "email": g.email })).collect::<Vec<_>>(),
    })))
}

// ---- Resource requirements ------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct RequirementBody {
    pub resource_kind: String,
    pub units_per_group: i32,
    pub participants_per_group: i32,
}

pub async fn write_requirements(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    r: &[RequirementBody],
) -> AppResult<()> {
    if kind != Kind::Activities {
        return Err(field_error("resource_requirements", "not_allowed"));
    }
    let mut f = Fields::default();
    for (i, x) in r.iter().enumerate() {
        if !RESOURCE_KINDS.contains(&x.resource_kind.as_str()) {
            f.add(
                format!("resource_requirements[{i}].resource_kind"),
                "invalid",
            );
        }
        if r.iter()
            .filter(|y| y.resource_kind == x.resource_kind)
            .count()
            > 1
        {
            f.add(
                format!("resource_requirements[{i}].resource_kind"),
                "duplicate",
            );
        }
        if x.units_per_group < 1 {
            f.add(format!("resource_requirements[{i}].units_per_group"), "min");
        }
        if x.participants_per_group < 1 {
            f.add(
                format!("resource_requirements[{i}].participants_per_group"),
                "min",
            );
        }
    }
    f.check()?;
    sqlx::query!(
        "DELETE FROM product_resource_requirements WHERE product_id = $1",
        id
    )
    .execute(&mut *conn)
    .await?;
    for x in r {
        sqlx::query!(
            "INSERT INTO product_resource_requirements (product_id, resource_kind, units_per_group, participants_per_group)
             VALUES ($1, $2, $3, $4)",
            id,
            x.resource_kind,
            x.units_per_group,
            x.participants_per_group
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Requirements with what the fleet allows: `max_participants` per departure (the smallest
/// over the requirements) and schedules whose capacity is above it.
pub async fn requirements(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    staff.require("inventory.read")?;
    let mut conn = state.db.acquire().await?;
    activity(&mut conn, &kind, id).await?;
    let rows = sqlx::query!(
        r#"SELECT r.resource_kind, r.units_per_group, r.participants_per_group,
                  (SELECT count(*) FROM resources x WHERE x.kind = r.resource_kind AND x.status = 'active') AS "available!"
           FROM product_resource_requirements r WHERE r.product_id = $1 ORDER BY r.resource_kind"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    let max = rows
        .iter()
        .map(|r| (r.available / i64::from(r.units_per_group)) * i64::from(r.participants_per_group))
        .min();
    let over = match max {
        Some(m) => sqlx::query!(
            "SELECT id, start_time, capacity FROM activity_schedules WHERE product_id = $1 AND is_active AND capacity > $2",
            id,
            i32::try_from(m).unwrap_or(i32::MAX)
        )
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(|s| json!({ "id": s.id, "start_time": s.start_time.format("%H:%M").to_string(), "capacity": s.capacity }))
        .collect::<Vec<_>>(),
        None => vec![],
    };
    Ok(Json(json!({
        "data": rows.iter().map(|r| json!({
            "resource_kind": r.resource_kind, "units_per_group": r.units_per_group,
            "participants_per_group": r.participants_per_group, "available": r.available,
            "max_participants": (r.available / i64::from(r.units_per_group)) * i64::from(r.participants_per_group),
        })).collect::<Vec<_>>(),
        "max_participants": max, "schedules_over": over,
    })))
}

pub async fn put_requirements(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(r): Json<Vec<RequirementBody>>,
) -> AppResult<StatusCode> {
    staff.require("inventory.write")?;
    let mut tx = state.db.begin().await?;
    activity(&mut tx, &kind, id).await?;
    write_requirements(&mut tx, Kind::Activities, id, &r).await?;
    changed(
        &mut tx,
        &staff.actor(),
        Kind::Activities,
        id,
        "resource_requirements.updated",
        json!({ "count": r.len() }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
