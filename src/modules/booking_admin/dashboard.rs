//! "Today" for the reception: arrivals, departures, guests in house, today's departures of
//! activities with their fill rate, requests waiting for an answer, balances due, failed
//! payments, missing pre-check-ins and alerts.

use axum::{Json, extract::State};
use chrono::{Duration, Utc};
use serde_json::{Value, json};

use crate::{auth::Staff, camp_time, error::AppResult, state::AppState};

/// id, reference, status, first name, last name, adults, children, total, paid, products.
type DayRow = (
    uuid::Uuid,
    String,
    String,
    Option<String>,
    Option<String>,
    i16,
    i16,
    i64,
    i64,
    Option<String>,
);

pub async fn today(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("dashboard.read")?;
    let db = &state.db;
    let now = Utc::now();
    let today = camp_time::today(now);
    let row = |r: DayRow| {
        json!({
            "id": r.0, "reference": r.1, "status": r.2, "first_name": r.3, "last_name": r.4,
            "adults": r.5, "children": r.6, "total_minor": r.7, "paid_minor": r.8, "products": r.9,
        })
    };
    let list = |sql: &'static str| sqlx::query_as::<_, DayRow>(sql);

    let arrivals: Vec<Value> = list(
        "SELECT b.id, b.reference, b.status, c.first_name, c.last_name, b.adults, b.children, b.total_minor,
                b.amount_paid_minor,
                (SELECT string_agg(COALESCE(i.product_snapshot->>'localized_name', i.product_snapshot->>'name'), ', ')
                 FROM booking_items i WHERE i.booking_id = b.id AND i.parent_item_id IS NULL AND i.status = 'active')
         FROM bookings b JOIN customers c ON c.id = b.customer_id
         WHERE b.arrival_date = $1 AND b.status IN ('confirmed', 'checked_in', 'pending', 'awaiting_payment')
         ORDER BY b.estimated_arrival_time NULLS LAST, b.reference",
    )
    .bind(today)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(row)
    .collect();
    let departures: Vec<Value> = list(
        "SELECT b.id, b.reference, b.status, c.first_name, c.last_name, b.adults, b.children, b.total_minor,
                b.amount_paid_minor,
                (SELECT string_agg(COALESCE(i.product_snapshot->>'localized_name', i.product_snapshot->>'name'), ', ')
                 FROM booking_items i WHERE i.booking_id = b.id AND i.parent_item_id IS NULL AND i.status = 'active')
         FROM bookings b JOIN customers c ON c.id = b.customer_id
         WHERE b.departure_date = $1 AND b.status IN ('confirmed', 'checked_in')
         ORDER BY b.reference",
    )
    .bind(today)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(row)
    .collect();
    let in_house: Vec<Value> = list(
        "SELECT b.id, b.reference, b.status, c.first_name, c.last_name, b.adults, b.children, b.total_minor,
                b.amount_paid_minor,
                (SELECT string_agg(COALESCE(i.product_snapshot->>'localized_name', i.product_snapshot->>'name'), ', ')
                 FROM booking_items i WHERE i.booking_id = b.id AND i.parent_item_id IS NULL AND i.status = 'active')
         FROM bookings b JOIN customers c ON c.id = b.customer_id
         WHERE b.status = 'checked_in' ORDER BY b.departure_date, b.reference",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(row)
    .collect();

    let day_start = camp_time::to_utc(today.and_hms_opt(0, 0, 0).unwrap_or_default());
    let slots: Vec<Value> = sqlx::query!(
        r#"SELECT s.id, s.starts_at, s.capacity, s.reserved, s.status, s.guide_language,
                  COALESCE(t.name, s.product_id::text) AS "name!", u.full_name AS "guide?"
           FROM activity_slots s
           LEFT JOIN product_translations t ON t.product_id = s.product_id AND t.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN users u ON u.id = s.guide_user_id
           WHERE s.starts_at >= $1 AND s.starts_at < $2 AND s.status <> 'cancelled'
           ORDER BY s.starts_at"#,
        day_start,
        day_start + Duration::days(1)
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|s| json!({
        "id": s.id, "starts_at": s.starts_at, "name": s.name, "capacity": s.capacity, "reserved": s.reserved,
        "fill_rate": if s.capacity > 0 { f64::from(s.reserved) / f64::from(s.capacity) } else { 0.0 },
        "status": s.status, "language": s.guide_language, "guide": s.guide,
    }))
    .collect();

    let requests: Vec<Value> = sqlx::query!(
        r#"SELECT b.id, b.reference, b.hold_expires_at, b.arrival_date, b.total_minor, c.first_name, c.last_name
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           WHERE b.status = 'pending' ORDER BY b.hold_expires_at NULLS LAST"#
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| json!({
        "id": r.id, "reference": r.reference, "answer_by": r.hold_expires_at, "arrival": r.arrival_date,
        "total_minor": r.total_minor, "first_name": r.first_name, "last_name": r.last_name,
    }))
    .collect();

    let balances: Vec<Value> = sqlx::query!(
        r#"SELECT b.id, b.reference, b.balance_due_on, b.total_minor - b.amount_paid_minor AS "due!",
                  b.balance_collection, c.first_name, c.last_name
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           WHERE b.status IN ('confirmed', 'checked_in') AND b.total_minor > b.amount_paid_minor
             AND (b.balance_due_on <= $1 OR b.arrival_date <= $1)
           ORDER BY b.balance_due_on NULLS LAST LIMIT 50"#,
        today + Duration::days(7)
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| json!({
        "id": r.id, "reference": r.reference, "due_on": r.balance_due_on, "due_minor": r.due,
        "collection": r.balance_collection, "first_name": r.first_name, "last_name": r.last_name,
    }))
    .collect();

    let failed_payments: Vec<Value> = sqlx::query!(
        r#"SELECT p.id, p.amount_minor, p.failure_reason, p.created_at, b.id AS booking_id, b.reference
           FROM payments p JOIN bookings b ON b.id = p.booking_id
           WHERE p.status = 'failed' AND p.created_at > $1 ORDER BY p.created_at DESC LIMIT 20"#,
        now - Duration::days(7)
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|p| json!({
        "id": p.id, "booking_id": p.booking_id, "reference": p.reference, "amount_minor": p.amount_minor,
        "reason": p.failure_reason, "at": p.created_at,
    }))
    .collect();

    let precheckin_missing: Vec<Value> = sqlx::query!(
        r#"SELECT b.id, b.reference, b.arrival_date, c.first_name, c.last_name
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           WHERE b.status = 'confirmed' AND b.precheckin_completed_at IS NULL
             AND b.arrival_date BETWEEN $1 AND $2
           ORDER BY b.arrival_date"#,
        today,
        today + Duration::days(3)
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| json!({ "id": r.id, "reference": r.reference, "arrival": r.arrival_date, "first_name": r.first_name, "last_name": r.last_name }))
    .collect();

    let alerts = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM staff_tasks WHERE status = 'open') AS "open_tasks!",
                  (SELECT count(*) FROM payment_webhook_events WHERE processed_at IS NULL AND error IS NOT NULL) AS "webhook_errors!",
                  (SELECT count(*) FROM outbox_events WHERE failed_at IS NOT NULL AND failed_at > $1) AS "dead_events!""#,
        now - Duration::days(7)
    )
    .fetch_one(db)
    .await?;

    Ok(Json(json!({
        "date": today,
        "arrivals": arrivals,
        "departures": departures,
        "in_house": in_house,
        "slots": slots,
        "requests": requests,
        "balances_due": balances,
        "failed_payments": failed_payments,
        "precheckin_missing": precheckin_missing,
        "alerts": {
            "open_tasks": alerts.open_tasks,
            "webhook_errors": alerts.webhook_errors,
            "dead_events": alerts.dead_events,
        },
    })))
}
