//! Availability checks shared by quotes (read-only) and checkout (`lock = true`: the rows are
//! locked `FOR UPDATE` in a fixed order — nights by (product, day), then slots by id, then
//! extras by id — so concurrent checkouts serialise instead of overselling or deadlocking).
//!
//! Each check returns `Ok(Err(issue))` when the item cannot be sold (a business answer shown to
//! the guest) and `Err(_)` only for database failures.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{catalog::Product, model::ItemIssue};
use crate::{
    camp_time,
    db::{
        catalog::InventoryMode,
        inventory::{ResourceKind, SlotStatus},
    },
    services::booking::BookingError,
};

pub type Check<T> = Result<Result<T, ItemIssue>, BookingError>;

/// Quantities already taken by earlier items of the same trip.
#[derive(Debug, Default)]
pub struct TripUsage {
    pub nights: HashMap<(Uuid, NaiveDate), i32>,
    pub units: HashSet<Uuid>,
    pub slots: HashMap<Uuid, i32>,
    pub resources: HashSet<Uuid>,
    pub extras: HashMap<(Uuid, Option<NaiveDate>), i32>,
}

#[derive(Debug, Clone)]
pub struct StayOk {
    /// Fewest units left on any night after this item.
    pub remaining: i32,
    pub unit_ids: Vec<Uuid>,
}

struct DayRow {
    day: NaiveDate,
    capacity: i32,
    reserved: i32,
    stop_sell: bool,
    min_nights_override: Option<i16>,
    closed_to_arrival: bool,
    closed_to_departure: bool,
}

#[allow(clippy::too_many_arguments)]
pub async fn stay(
    conn: &mut PgConnection,
    product: &Product,
    arrival: NaiveDate,
    departure: NaiveDate,
    quantity: i32,
    requested_units: &[Uuid],
    usage: &mut TripUsage,
    lock: bool,
    today: NaiveDate,
    max_days_ahead: i64,
) -> Check<StayOk> {
    let Some(acc) = &product.accommodation else {
        return Ok(Err(ItemIssue::new("invalid_item", "not an accommodation")));
    };
    if departure <= arrival {
        return Ok(Err(ItemIssue::new(
            "invalid_dates",
            "departure must be after arrival",
        )));
    }
    if arrival < today {
        return Ok(Err(ItemIssue::new("past_date", "arrival is in the past")));
    }
    if (arrival - today).num_days() > max_days_ahead {
        return Ok(Err(ItemIssue::new(
            "too_far_ahead",
            "these dates are not open yet",
        )
        .with(json!({ "max_days_ahead": max_days_ahead }))));
    }
    if quantity < 1 {
        return Ok(Err(ItemIssue::new(
            "invalid_quantity",
            "quantity must be at least 1",
        )));
    }
    let nights = (departure - arrival).num_days();
    if nights < i64::from(acc.min_nights) {
        return Ok(Err(ItemIssue::new(
            "min_nights",
            format!("minimum {} nights", acc.min_nights),
        )
        .with(json!({ "min_nights": acc.min_nights }))));
    }
    if let Some(max) = acc.max_nights
        && nights > i64::from(max)
    {
        return Ok(Err(ItemIssue::new(
            "max_nights",
            format!("maximum {max} nights"),
        )
        .with(json!({ "max_nights": max }))));
    }

    let days = if lock {
        sqlx::query_as!(
            DayRow,
            "SELECT day, capacity, reserved, stop_sell, min_nights_override, closed_to_arrival, closed_to_departure
             FROM inventory_calendar
             WHERE product_id = $1 AND day >= $2 AND day <= $3
             ORDER BY day
             FOR UPDATE",
            product.id,
            arrival,
            departure
        )
        .fetch_all(&mut *conn)
        .await?
    } else {
        sqlx::query_as!(
            DayRow,
            "SELECT day, capacity, reserved, stop_sell, min_nights_override, closed_to_arrival, closed_to_departure
             FROM inventory_calendar
             WHERE product_id = $1 AND day >= $2 AND day <= $3
             ORDER BY day",
            product.id,
            arrival,
            departure
        )
        .fetch_all(&mut *conn)
        .await?
    };
    let stay_nights: Vec<&DayRow> = days.iter().filter(|d| d.day < departure).collect();
    if i64::try_from(stay_nights.len()).unwrap_or_default() != nights {
        return Ok(Err(ItemIssue::new(
            "not_open",
            "these dates are not open for booking yet",
        )));
    }
    if stay_nights.iter().any(|d| d.stop_sell) {
        return Ok(Err(ItemIssue::new(
            "sold_out",
            "not available on some of these nights",
        )));
    }
    if stay_nights[0].closed_to_arrival {
        return Ok(Err(ItemIssue::new(
            "closed_to_arrival",
            "arrivals are not possible on this date",
        )));
    }
    if days
        .iter()
        .any(|d| d.day == departure && d.closed_to_departure)
    {
        return Ok(Err(ItemIssue::new(
            "closed_to_departure",
            "departures are not possible on this date",
        )));
    }
    if let Some(min) = stay_nights[0].min_nights_override
        && nights < i64::from(min)
    {
        return Ok(Err(ItemIssue::new(
            "min_nights",
            format!("minimum {min} nights from this date"),
        )
        .with(json!({ "min_nights": min }))));
    }
    let mut remaining = i32::MAX;
    for d in &stay_nights {
        let taken = usage.nights.get(&(product.id, d.day)).copied().unwrap_or(0);
        let left = d.capacity - d.reserved - taken - quantity;
        if left < 0 {
            let available = (d.capacity - d.reserved - taken).max(0);
            return Ok(Err(if available == 0 {
                ItemIssue::new("sold_out", "fully booked on some of these nights")
            } else {
                ItemIssue::new("not_enough_units", format!("only {available} left"))
                    .with(json!({ "left": available }))
            }));
        }
        remaining = remaining.min(left);
    }

    let closed = sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM closures
               WHERE period && daterange($2, $3)
                 AND (scope = 'site' OR (scope = 'product' AND product_id = $1))
           ) AS "closed!""#,
        product.id,
        arrival,
        departure
    )
    .fetch_one(&mut *conn)
    .await?;
    if closed {
        return Ok(Err(ItemIssue::new(
            "closed",
            "the camp is closed during these dates",
        )));
    }

    let unit_ids = if acc.inventory_mode == InventoryMode::Unit || !requested_units.is_empty() {
        let free = sqlx::query_scalar!(
            "SELECT u.id
             FROM accommodation_units u
             WHERE u.product_id = $1 AND u.status = 'active'
               AND NOT EXISTS (SELECT 1 FROM unit_allocations a
                               WHERE a.unit_id = u.id AND a.released_at IS NULL AND a.stay && daterange($2, $3))
               AND NOT EXISTS (SELECT 1 FROM closures c
                               WHERE c.scope = 'unit' AND c.unit_id = u.id AND c.period && daterange($2, $3))
             ORDER BY u.sort_order, u.code",
            product.id,
            arrival,
            departure
        )
        .fetch_all(&mut *conn)
        .await?;
        let free: Vec<Uuid> = free
            .into_iter()
            .filter(|u| !usage.units.contains(u))
            .collect();
        let wanted = usize::try_from(quantity).unwrap_or_default();
        let chosen = if requested_units.is_empty() {
            free.into_iter().take(wanted).collect::<Vec<_>>()
        } else {
            if requested_units.len() != wanted {
                return Ok(Err(ItemIssue::new(
                    "invalid_units",
                    "pick one pitch per unit booked",
                )));
            }
            if let Some(taken) = requested_units.iter().find(|u| !free.contains(u)) {
                return Ok(Err(ItemIssue::new(
                    "unit_taken",
                    "this pitch is no longer free",
                )
                .with(json!({ "unit_id": taken }))));
            }
            requested_units.to_vec()
        };
        if chosen.len() < wanted {
            return Ok(Err(ItemIssue::new(
                "sold_out",
                "no unit free for the whole stay",
            )));
        }
        chosen
    } else {
        vec![]
    };

    for d in &stay_nights {
        *usage.nights.entry((product.id, d.day)).or_default() += quantity;
    }
    usage.units.extend(unit_ids.iter().copied());
    Ok(Ok(StayOk {
        remaining,
        unit_ids,
    }))
}

#[derive(Debug, Clone)]
pub struct SlotOk {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub remaining: i32,
    pub language: Option<String>,
}

struct SlotRow {
    product_id: Uuid,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    capacity: i32,
    reserved: i32,
    status: SlotStatus,
    guide_language: Option<String>,
}

pub async fn slot(
    conn: &mut PgConnection,
    product: &Product,
    slot_id: Uuid,
    participants: i32,
    usage: &mut TripUsage,
    lock: bool,
    now: DateTime<Utc>,
) -> Check<SlotOk> {
    let Some(activity) = &product.activity else {
        return Ok(Err(ItemIssue::new("invalid_item", "not an activity")));
    };
    let row = if lock {
        sqlx::query_as!(
            SlotRow,
            r#"SELECT product_id, starts_at, ends_at, capacity, reserved, status AS "status: SlotStatus",
                      guide_language
               FROM activity_slots WHERE id = $1 FOR UPDATE"#,
            slot_id
        )
        .fetch_optional(&mut *conn)
        .await?
    } else {
        sqlx::query_as!(
            SlotRow,
            r#"SELECT product_id, starts_at, ends_at, capacity, reserved, status AS "status: SlotStatus",
                      guide_language
               FROM activity_slots WHERE id = $1"#,
            slot_id
        )
        .fetch_optional(&mut *conn)
        .await?
    };
    let Some(row) = row.filter(|r| r.product_id == product.id) else {
        return Ok(Err(ItemIssue::new(
            "unknown_slot",
            "this departure does not exist",
        )));
    };
    if row.starts_at - Duration::hours(i64::from(activity.booking_cutoff_hours)) < now {
        return Ok(Err(ItemIssue::new(
            "too_late",
            "too late to book this departure",
        )
        .with(json!({ "cutoff_hours": activity.booking_cutoff_hours }))));
    }
    if matches!(row.status, SlotStatus::Closed | SlotStatus::Cancelled) {
        return Ok(Err(ItemIssue::new(
            "slot_closed",
            "this departure is not running",
        )));
    }
    let taken = usage.slots.get(&slot_id).copied().unwrap_or(0);
    let left = row.capacity - row.reserved - taken;
    if row.status == SlotStatus::Full || left <= 0 {
        return Ok(Err(ItemIssue::new("full", "this departure is full")));
    }
    if participants > left {
        return Ok(Err(ItemIssue::new(
            "not_enough_seats",
            format!("only {left} place(s) left"),
        )
        .with(json!({ "left": left }))));
    }
    if closed_on(conn, product.id, camp_time::today(row.starts_at)).await? {
        return Ok(Err(ItemIssue::new("closed", "closed on this date")));
    }
    *usage.slots.entry(slot_id).or_default() += participants;
    Ok(Ok(SlotOk {
        starts_at: row.starts_at,
        ends_at: row.ends_at,
        remaining: left - participants,
        language: row.guide_language,
    }))
}

/// The first open slot of `product` on `date` with room for `participants` (package parts).
pub async fn first_slot_on(
    conn: &mut PgConnection,
    product_id: Uuid,
    date: NaiveDate,
    participants: i32,
) -> Result<Option<Uuid>, BookingError> {
    let from = camp_time::to_utc(date.and_hms_opt(0, 0, 0).unwrap_or_default());
    let to = from + Duration::days(1);
    Ok(sqlx::query_scalar!(
        "SELECT id FROM activity_slots
         WHERE product_id = $1 AND starts_at >= $2 AND starts_at < $3 AND status = 'open'
           AND capacity - reserved >= $4
         ORDER BY starts_at LIMIT 1",
        product_id,
        from,
        to,
        participants
    )
    .fetch_optional(conn)
    .await?)
}

pub async fn closed_on(
    conn: &mut PgConnection,
    product_id: Uuid,
    date: NaiveDate,
) -> Result<bool, BookingError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM closures
               WHERE period @> $2::date AND (scope = 'site' OR (scope = 'product' AND product_id = $1))
           ) AS "closed!""#,
        product_id,
        date
    )
    .fetch_one(conn)
    .await?)
}

/// Free resources (quads, guides...) for `participants` during `[starts_at, ends_at)`.
pub async fn resources(
    conn: &mut PgConnection,
    product_id: Uuid,
    participants: i32,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    usage: &mut TripUsage,
    lock: bool,
) -> Check<Vec<Uuid>> {
    let requirements = sqlx::query!(
        r#"SELECT resource_kind AS "resource_kind: ResourceKind", units_per_group, participants_per_group
           FROM product_resource_requirements WHERE product_id = $1 ORDER BY resource_kind"#,
        product_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let local_date = camp_time::today(starts_at);
    let mut chosen = Vec::new();
    for req in requirements {
        let groups = (participants + req.participants_per_group - 1) / req.participants_per_group;
        let needed = usize::try_from(groups * req.units_per_group).unwrap_or_default();
        let free = if lock {
            sqlx::query_scalar!(
                "SELECT r.id FROM resources r
                 WHERE r.kind = $1 AND r.status = 'active'
                   AND NOT EXISTS (SELECT 1 FROM resource_assignments a
                                   WHERE a.resource_id = r.id AND a.released_at IS NULL
                                     AND a.period && tstzrange($2, $3))
                   AND NOT EXISTS (SELECT 1 FROM closures c
                                   WHERE c.scope = 'resource' AND c.resource_id = r.id AND c.period @> $4::date)
                 ORDER BY r.code
                 FOR UPDATE OF r SKIP LOCKED",
                req.resource_kind.as_str(),
                starts_at,
                ends_at,
                local_date
            )
            .fetch_all(&mut *conn)
            .await?
        } else {
            sqlx::query_scalar!(
                "SELECT r.id FROM resources r
                 WHERE r.kind = $1 AND r.status = 'active'
                   AND NOT EXISTS (SELECT 1 FROM resource_assignments a
                                   WHERE a.resource_id = r.id AND a.released_at IS NULL
                                     AND a.period && tstzrange($2, $3))
                   AND NOT EXISTS (SELECT 1 FROM closures c
                                   WHERE c.scope = 'resource' AND c.resource_id = r.id AND c.period @> $4::date)
                 ORDER BY r.code",
                req.resource_kind.as_str(),
                starts_at,
                ends_at,
                local_date
            )
            .fetch_all(&mut *conn)
            .await?
        };
        let free: Vec<Uuid> = free
            .into_iter()
            .filter(|r| !usage.resources.contains(r) && !chosen.contains(r))
            .take(needed)
            .collect();
        if free.len() < needed {
            return Ok(Err(ItemIssue::new(
                "no_resource",
                format!("not enough {} available", req.resource_kind),
            )
            .with(json!({ "resource": req.resource_kind }))));
        }
        chosen.extend(free);
    }
    usage.resources.extend(chosen.iter().copied());
    Ok(Ok(chosen))
}

/// Stock of a limited extra on `date` (or overall when the extra has no date).
pub async fn extra_stock(
    conn: &mut PgConnection,
    product: &Product,
    date: Option<NaiveDate>,
    quantity: i32,
    usage: &mut TripUsage,
    lock: bool,
) -> Check<Option<i32>> {
    let Some(extra) = &product.extra else {
        return Ok(Err(ItemIssue::new("invalid_item", "not an extra")));
    };
    if quantity < 1 {
        return Ok(Err(ItemIssue::new(
            "invalid_quantity",
            "quantity must be at least 1",
        )));
    }
    if let Some(max) = extra.max_quantity
        && quantity > max
    {
        return Ok(Err(ItemIssue::new(
            "max_quantity",
            format!("at most {max}"),
        )
        .with(json!({ "max": max }))));
    }
    let Some(stock) = extra.stock_quantity else {
        return Ok(Ok(None));
    };
    if lock {
        sqlx::query!(
            "SELECT product_id FROM extras WHERE product_id = $1 FOR UPDATE",
            product.id
        )
        .fetch_one(&mut *conn)
        .await?;
    }
    let used = sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(i.quantity), 0)::int AS "used!"
           FROM booking_items i JOIN bookings b ON b.id = i.booking_id
           WHERE i.product_id = $1 AND i.status = 'active'
             AND b.status IN ('pending', 'awaiting_payment', 'confirmed', 'checked_in')
             AND ($2::date IS NULL OR i.service_period @> $2::date)"#,
        product.id,
        date
    )
    .fetch_one(&mut *conn)
    .await?;
    let key = (product.id, date);
    let taken = usage.extras.get(&key).copied().unwrap_or(0);
    let left = stock - used - taken;
    if quantity > left {
        return Ok(Err(if left <= 0 {
            ItemIssue::new("sold_out", "no longer available")
        } else {
            ItemIssue::new("not_enough_units", format!("only {left} left"))
                .with(json!({ "left": left }))
        }));
    }
    *usage.extras.entry(key).or_default() += quantity;
    Ok(Ok(Some(left - quantity)))
}
