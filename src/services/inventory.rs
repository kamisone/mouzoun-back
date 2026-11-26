//! Inventory generation and synchronisation (invariants 5 and 6). The admin editors and the
//! daily job call these functions; the booking flow reads what they write. Nothing else creates
//! calendar rows or slots.
//!
//! - `generate_inventory_calendar`: creates the missing `inventory_calendar` rows up to 18 months
//!   ahead. Capacity: unit mode = active units not closed that night; pool mode =
//!   `pool_capacity` (or, for types created before it existed, the active units). Existing rows
//!   are never touched (`ON CONFLICT DO NOTHING`), so manual overrides and reservations stay.
//! - `sync_unit_capacity`: unit mode, after a unit or a closure changed: every future night's
//!   capacity becomes active units minus units closed that night, in the caller's transaction.
//!   Refused (nothing written) when a night would fall below its `reserved` count.
//! - `set_pool_capacity`: pool mode, the whole future (or a range) at once, same refusal.
//! - `generate_activity_slots` / `sync_activity_slots`: `activity_slots` from active
//!   `activity_schedules`, in the camp's local time (`crate::camp_time`, real tz database: the
//!   Ramadan clock changes are never a fixed offset). Idempotent through
//!   `UNIQUE (product_id, starts_at)`. A sync also updates or deletes future slots of the
//!   product that no schedule produces any more, **only when nobody booked them**; booked ones
//!   stay and are reported.
//! - `apply_slot_closures` / `reopen_after_closure`: slots covered by a site or product closure
//!   are `closed`; removing the closure reopens the closed slots of its period that no other
//!   closure covers.

use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::db::types::{DateRange, DaysOfWeek};

pub const CALENDAR_HORIZON_MONTHS: i32 = 18;

/// Returns the number of rows created.
pub async fn generate_inventory_calendar(
    pool: &PgPool,
    product_id: Option<Uuid>,
    from: NaiveDate,
    months: i32,
) -> sqlx::Result<u64> {
    let mut conn = pool.acquire().await?;
    generate_calendar_rows(&mut conn, product_id, from, months).await
}

/// `generate_inventory_calendar` inside the caller's transaction.
pub async fn generate_calendar_rows(
    conn: &mut PgConnection,
    product_id: Option<Uuid>,
    from: NaiveDate,
    months: i32,
) -> sqlx::Result<u64> {
    let result = sqlx::query!(
        "INSERT INTO inventory_calendar (product_id, day, capacity)
         SELECT a.product_id, day::date,
                CASE WHEN a.inventory_mode = 'pool' AND a.pool_capacity IS NOT NULL THEN a.pool_capacity
                     ELSE (SELECT count(*) FROM accommodation_units u
                           WHERE u.product_id = a.product_id AND u.status = 'active'
                             AND (a.inventory_mode = 'pool' OR NOT EXISTS (
                                 SELECT 1 FROM closures c WHERE c.scope = 'unit' AND c.unit_id = u.id
                                                            AND c.period @> day::date)))::int
                END
         FROM accommodation_types a
         JOIN products p ON p.id = a.product_id AND p.deleted_at IS NULL
         CROSS JOIN generate_series($2::date, ($2::date + make_interval(months => $3))::date - 1, interval '1 day') AS day
         WHERE $1::uuid IS NULL OR a.product_id = $1
         ON CONFLICT (product_id, day) DO NOTHING",
        product_id,
        from,
        months
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

// ---- Capacity --------------------------------------------------------------------------------

/// A night (or departure) that would hold fewer places than are already booked.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Shortfall {
    pub day: NaiveDate,
    pub capacity: i32,
    pub reserved: i32,
}

/// Unit mode: future capacity = active units not closed that night (rows from `today` on).
/// `Err` lists the nights that would go below `reserved`; nothing is written then.
pub async fn sync_unit_capacity(
    conn: &mut PgConnection,
    product_id: Uuid,
    today: NaiveDate,
) -> sqlx::Result<Result<u64, Vec<Shortfall>>> {
    // Same lock order as checkout: nights by (product, day).
    sqlx::query!(
        "SELECT day FROM inventory_calendar WHERE product_id = $1 AND day >= $2 ORDER BY day FOR UPDATE",
        product_id,
        today
    )
    .fetch_all(&mut *conn)
    .await?;
    let short = sqlx::query_as!(
        Shortfall,
        r#"SELECT c.day, t.cap AS "capacity!", c.reserved
           FROM inventory_calendar c
           CROSS JOIN LATERAL (
               SELECT count(*)::int AS cap FROM accommodation_units u
               WHERE u.product_id = c.product_id AND u.status = 'active'
                 AND NOT EXISTS (SELECT 1 FROM closures x WHERE x.scope = 'unit' AND x.unit_id = u.id AND x.period @> c.day)
           ) t
           WHERE c.product_id = $1 AND c.day >= $2 AND t.cap < c.reserved
           ORDER BY c.day"#,
        product_id,
        today
    )
    .fetch_all(&mut *conn)
    .await?;
    if !short.is_empty() {
        return Ok(Err(short));
    }
    let done = sqlx::query!(
        "UPDATE inventory_calendar c SET capacity = t.cap
         FROM (SELECT c2.day, (SELECT count(*) FROM accommodation_units u
                               WHERE u.product_id = $1 AND u.status = 'active'
                                 AND NOT EXISTS (SELECT 1 FROM closures x WHERE x.scope = 'unit' AND x.unit_id = u.id
                                                                         AND x.period @> c2.day))::int AS cap
               FROM inventory_calendar c2 WHERE c2.product_id = $1 AND c2.day >= $2) t
         WHERE c.product_id = $1 AND c.day = t.day AND c.capacity <> t.cap",
        product_id,
        today
    )
    .execute(&mut *conn)
    .await?;
    Ok(Ok(done.rows_affected()))
}

/// Pool mode: `capacity` for the nights of `range` (all future nights when `None`).
pub async fn set_pool_capacity(
    conn: &mut PgConnection,
    product_id: Uuid,
    today: NaiveDate,
    capacity: i32,
    range: Option<(NaiveDate, NaiveDate)>,
    weekdays: DaysOfWeek,
) -> sqlx::Result<Result<u64, Vec<Shortfall>>> {
    let (from, to) = range.unwrap_or((today, NaiveDate::MAX));
    let from = from.max(today);
    let rows = sqlx::query!(
        "SELECT day, reserved FROM inventory_calendar WHERE product_id = $1 AND day >= $2 AND day < $3
         ORDER BY day FOR UPDATE",
        product_id,
        from,
        to
    )
    .fetch_all(&mut *conn)
    .await?;
    let days: Vec<NaiveDate> = rows
        .iter()
        .filter(|r| weekdays.contains(r.day))
        .map(|r| r.day)
        .collect();
    let short: Vec<Shortfall> = rows
        .iter()
        .filter(|r| weekdays.contains(r.day) && r.reserved > capacity)
        .map(|r| Shortfall {
            day: r.day,
            capacity,
            reserved: r.reserved,
        })
        .collect();
    if !short.is_empty() {
        return Ok(Err(short));
    }
    let done = sqlx::query!(
        "UPDATE inventory_calendar SET capacity = $3 WHERE product_id = $1 AND day = ANY($2) AND capacity <> $3",
        product_id,
        &days,
        capacity
    )
    .execute(&mut *conn)
    .await?;
    Ok(Ok(done.rows_affected()))
}

// ---- Bookings concerned (shown with every refusal) ------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BookingRef {
    pub booking_id: Uuid,
    pub reference: String,
    pub status: String,
    pub arrival: Option<NaiveDate>,
    pub departure: Option<NaiveDate>,
    pub starts_at: Option<DateTime<Utc>>,
}

/// Live bookings of a stay product touching one of `days`.
pub async fn bookings_on_nights(
    conn: &mut PgConnection,
    product_id: Uuid,
    days: &[NaiveDate],
) -> sqlx::Result<Vec<BookingRef>> {
    sqlx::query_as!(
        BookingRef,
        r#"SELECT DISTINCT b.id AS booking_id, b.reference, b.status, lower(i.service_period) AS arrival,
                  upper(i.service_period) AS departure, NULL::timestamptz AS starts_at
           FROM booking_items i JOIN bookings b ON b.id = i.booking_id
           WHERE i.product_id = $1 AND i.status = 'active'
             AND b.status NOT IN ('cancelled', 'expired', 'no_show', 'completed')
             AND EXISTS (SELECT 1 FROM unnest($2::date[]) d WHERE i.service_period @> d)
           ORDER BY arrival, b.reference"#,
        product_id,
        days
    )
    .fetch_all(conn)
    .await
}

/// Live bookings holding `unit_id` from `today` on.
pub async fn bookings_on_unit(
    conn: &mut PgConnection,
    unit_id: Uuid,
    today: NaiveDate,
) -> sqlx::Result<Vec<BookingRef>> {
    sqlx::query_as!(
        BookingRef,
        r#"SELECT DISTINCT b.id AS booking_id, b.reference, b.status, lower(a.stay) AS arrival, upper(a.stay) AS departure,
                  NULL::timestamptz AS starts_at
           FROM unit_allocations a
           JOIN booking_items i ON i.id = a.booking_item_id
           JOIN bookings b ON b.id = i.booking_id
           WHERE a.unit_id = $1 AND a.released_at IS NULL AND upper(a.stay) > $2
             AND b.status NOT IN ('cancelled', 'expired', 'no_show', 'completed')
           ORDER BY arrival, b.reference"#,
        unit_id,
        today
    )
    .fetch_all(conn)
    .await
}

/// Live bookings of the given slots.
pub async fn bookings_on_slots(
    conn: &mut PgConnection,
    slot_ids: &[Uuid],
) -> sqlx::Result<Vec<BookingRef>> {
    sqlx::query_as!(
        BookingRef,
        r#"SELECT DISTINCT b.id AS booking_id, b.reference, b.status, NULL::date AS arrival, NULL::date AS departure,
                  s.starts_at AS "starts_at?"
           FROM booking_items i JOIN bookings b ON b.id = i.booking_id
           JOIN activity_slots s ON s.id = i.activity_slot_id
           WHERE i.activity_slot_id = ANY($1) AND i.status = 'active'
             AND b.status NOT IN ('cancelled', 'expired', 'no_show', 'completed')
           ORDER BY "starts_at?", b.reference"#,
        slot_ids
    )
    .fetch_all(conn)
    .await
}

/// Any live future booking of the product (inventory mode switch, delete...).
pub async fn future_bookings(
    conn: &mut PgConnection,
    product_id: Uuid,
    today: NaiveDate,
) -> sqlx::Result<Vec<BookingRef>> {
    sqlx::query_as!(
        BookingRef,
        r#"SELECT DISTINCT b.id AS booking_id, b.reference, b.status, lower(i.service_period) AS arrival,
                  upper(i.service_period) AS departure, i.starts_at
           FROM booking_items i JOIN bookings b ON b.id = i.booking_id
           WHERE i.product_id = $1 AND i.status = 'active'
             AND b.status NOT IN ('cancelled', 'expired', 'no_show', 'completed')
             AND (upper(i.service_period) > $2 OR i.starts_at >= $2::date::timestamptz
                  OR (i.service_period IS NULL AND i.starts_at IS NULL))
           ORDER BY arrival, b.reference
           LIMIT 50"#,
        product_id,
        today
    )
    .fetch_all(conn)
    .await
}

// ---- Activity slots --------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScheduleRule {
    pub id: Uuid,
    pub product_id: Uuid,
    pub days_of_week: i16,
    pub start_time: NaiveTime,
    pub capacity: i32,
    pub valid_period: DateRange,
    pub duration_minutes: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedSlot {
    pub schedule_id: Uuid,
    pub product_id: Uuid,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub capacity: i32,
}

/// The departures `schedules` give over `[from, from + days)` (pure: the preview uses it too).
/// `starts_at` = local start time in Africa/Casablanca → UTC with the tz database.
pub fn plan_slots(schedules: &[ScheduleRule], from: NaiveDate, days: i64) -> Vec<PlannedSlot> {
    let mut out = Vec::new();
    for s in schedules {
        let weekdays = DaysOfWeek(s.days_of_week);
        let duration = Duration::minutes(i64::from(s.duration_minutes.unwrap_or(60)));
        for date in from
            .iter_days()
            .take(usize::try_from(days).unwrap_or_default())
        {
            if !weekdays.contains(date) || !s.valid_period.contains(date) {
                continue;
            }
            let starts_at = crate::camp_time::to_utc(date.and_time(s.start_time));
            out.push(PlannedSlot {
                schedule_id: s.id,
                product_id: s.product_id,
                starts_at,
                ends_at: starts_at + duration,
                capacity: s.capacity,
            });
        }
    }
    out.sort_by_key(|p| (p.product_id, p.starts_at));
    // Two schedules at the same time: one departure, the larger capacity.
    out.dedup_by(|b, a| {
        if a.product_id == b.product_id && a.starts_at == b.starts_at {
            a.capacity = a.capacity.max(b.capacity);
            true
        } else {
            false
        }
    });
    out
}

async fn schedules(
    conn: &mut PgConnection,
    product_id: Option<Uuid>,
) -> sqlx::Result<Vec<ScheduleRule>> {
    sqlx::query_as!(
        ScheduleRule,
        r#"SELECT s.id, s.product_id, s.days_of_week, s.start_time, s.capacity,
                  s.valid_period AS "valid_period: DateRange", a.duration_minutes
           FROM activity_schedules s
           JOIN activities a ON a.product_id = s.product_id
           JOIN products p ON p.id = s.product_id AND p.deleted_at IS NULL
           WHERE s.is_active AND a.scheduling_mode = 'fixed_slots' AND ($1::uuid IS NULL OR s.product_id = $1)"#,
        product_id
    )
    .fetch_all(conn)
    .await
}

async fn insert_planned(conn: &mut PgConnection, planned: &[PlannedSlot]) -> sqlx::Result<u64> {
    if planned.is_empty() {
        return Ok(0);
    }
    let ids: Vec<Uuid> = planned.iter().map(|_| Uuid::now_v7()).collect();
    let products: Vec<Uuid> = planned.iter().map(|p| p.product_id).collect();
    let schedule_ids: Vec<Uuid> = planned.iter().map(|p| p.schedule_id).collect();
    let starts: Vec<DateTime<Utc>> = planned.iter().map(|p| p.starts_at).collect();
    let ends: Vec<DateTime<Utc>> = planned.iter().map(|p| p.ends_at).collect();
    let capacities: Vec<i32> = planned.iter().map(|p| p.capacity).collect();
    let result = sqlx::query!(
        "INSERT INTO activity_slots (id, product_id, schedule_id, starts_at, ends_at, capacity)
         SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::timestamptz[], $5::timestamptz[], $6::int4[])
         ON CONFLICT (product_id, starts_at) DO NOTHING",
        &ids,
        &products,
        &schedule_ids,
        &starts,
        &ends,
        &capacities
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

/// Creates the slots of `[from, from + days)` for every product (daily job). Returns the number
/// of slots created. New slots inside a closure are closed at once.
pub async fn generate_activity_slots(
    pool: &PgPool,
    from: NaiveDate,
    days: i64,
) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let rules = schedules(&mut tx, None).await?;
    let created = insert_planned(&mut tx, &plan_slots(&rules, from, days)).await?;
    apply_slot_closures(&mut tx, None, Utc::now()).await?;
    tx.commit().await?;
    Ok(created)
}

/// What a schedule save did to the product's future departures.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SlotSync {
    pub created: u64,
    pub updated: u64,
    pub deleted: u64,
    /// Departures a schedule no longer produces (or with another capacity) that have bookings:
    /// kept as they are, for staff to handle.
    pub kept_booked: Vec<KeptSlot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeptSlot {
    pub slot_id: Uuid,
    pub starts_at: DateTime<Utc>,
    pub reserved: i32,
    pub reason: &'static str,
}

/// Brings one product's future slots in line with its active schedules, inside the caller's
/// transaction. Never deletes or changes a slot with bookings (`reserved > 0`), never touches
/// one-off departures (no schedule) or past ones.
pub async fn sync_activity_slots(
    conn: &mut PgConnection,
    product_id: Uuid,
    now: DateTime<Utc>,
    days: i64,
) -> sqlx::Result<SlotSync> {
    let today = crate::camp_time::today(now);
    let rules = schedules(&mut *conn, Some(product_id)).await?;
    let planned: Vec<PlannedSlot> = plan_slots(&rules, today, days)
        .into_iter()
        .filter(|p| p.starts_at > now)
        .collect();
    let by_start: HashMap<DateTime<Utc>, &PlannedSlot> =
        planned.iter().map(|p| (p.starts_at, p)).collect();
    let existing = sqlx::query!(
        "SELECT id, schedule_id, starts_at, ends_at, capacity, reserved FROM activity_slots
         WHERE product_id = $1 AND starts_at > $2 AND schedule_id IS NOT NULL
         ORDER BY id FOR UPDATE",
        product_id,
        now
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut out = SlotSync::default();
    let mut drop_ids = Vec::new();
    for s in &existing {
        match by_start.get(&s.starts_at) {
            None if s.reserved == 0 => drop_ids.push(s.id),
            None => out.kept_booked.push(KeptSlot {
                slot_id: s.id,
                starts_at: s.starts_at,
                reserved: s.reserved,
                reason: "no_longer_scheduled",
            }),
            Some(p)
                if p.capacity != s.capacity
                    || p.ends_at != s.ends_at
                    || Some(p.schedule_id) != s.schedule_id =>
            {
                if s.reserved == 0 {
                    sqlx::query!(
                        "UPDATE activity_slots SET capacity = $2, ends_at = $3, schedule_id = $4 WHERE id = $1",
                        s.id,
                        p.capacity,
                        p.ends_at,
                        p.schedule_id
                    )
                    .execute(&mut *conn)
                    .await?;
                    out.updated += 1;
                } else if p.capacity != s.capacity {
                    out.kept_booked.push(KeptSlot {
                        slot_id: s.id,
                        starts_at: s.starts_at,
                        reserved: s.reserved,
                        reason: "capacity_changed",
                    });
                }
            }
            Some(_) => {}
        }
    }
    if !drop_ids.is_empty() {
        out.deleted = sqlx::query!(
            "DELETE FROM activity_slots s WHERE id = ANY($1) AND reserved = 0
               AND NOT EXISTS (SELECT 1 FROM booking_items i WHERE i.activity_slot_id = s.id)",
            &drop_ids
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    out.created = insert_planned(&mut *conn, &planned).await?;
    apply_slot_closures(&mut *conn, Some(product_id), now).await?;
    Ok(out)
}

/// Future slots covered by a site or product closure become `closed` (bookings stay valid:
/// staff see them on the departure).
pub async fn apply_slot_closures(
    conn: &mut PgConnection,
    product_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> sqlx::Result<u64> {
    let slots = sqlx::query!(
        "SELECT s.id, s.product_id, s.starts_at FROM activity_slots s
         WHERE s.starts_at > $1 AND s.status IN ('open', 'full') AND ($2::uuid IS NULL OR s.product_id = $2)",
        now,
        product_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let closures = open_closures(&mut *conn, now).await?;
    let close: Vec<Uuid> = slots
        .iter()
        .filter(|s| covered(&closures, s.product_id, s.starts_at))
        .map(|s| s.id)
        .collect();
    Ok(sqlx::query!(
        "UPDATE activity_slots SET status = 'closed' WHERE id = ANY($1)",
        &close
    )
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// A closure over `period` was removed: the closed slots of that period that no other closure
/// covers open again (`full` when no place is left).
pub async fn reopen_after_closure(
    conn: &mut PgConnection,
    product_id: Option<Uuid>,
    period: DateRange,
    now: DateTime<Utc>,
) -> sqlx::Result<u64> {
    let slots = sqlx::query!(
        "SELECT s.id, s.product_id, s.starts_at FROM activity_slots s
         WHERE s.starts_at > $1 AND s.status = 'closed' AND ($2::uuid IS NULL OR s.product_id = $2)",
        now,
        product_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let closures = open_closures(&mut *conn, now).await?;
    let reopen: Vec<Uuid> = slots
        .iter()
        .filter(|s| {
            period.contains(crate::camp_time::today(s.starts_at))
                && !covered(&closures, s.product_id, s.starts_at)
        })
        .map(|s| s.id)
        .collect();
    Ok(sqlx::query!(
        "UPDATE activity_slots SET status = CASE WHEN reserved >= capacity THEN 'full' ELSE 'open' END
         WHERE id = ANY($1)",
        &reopen
    )
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

struct OpenClosure {
    product_id: Option<Uuid>,
    period: DateRange,
}

async fn open_closures(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
) -> sqlx::Result<Vec<OpenClosure>> {
    sqlx::query_as!(
        OpenClosure,
        r#"SELECT product_id, period AS "period: DateRange" FROM closures
           WHERE scope IN ('site', 'product') AND (upper(period) IS NULL OR upper(period) > $1::date)"#,
        crate::camp_time::today(now)
    )
    .fetch_all(conn)
    .await
}

fn covered(closures: &[OpenClosure], product_id: Uuid, at: DateTime<Utc>) -> bool {
    let day = crate::camp_time::today(at);
    closures
        .iter()
        .any(|c| c.period.contains(day) && c.product_id.is_none_or(|p| p == product_id))
}
