//! The guest's booking pages: view (with the signed token from the confirmation email or a
//! magic link), status polling, paying what is due, cancelling with the refund shown first,
//! arrival details, the guest register, calendar file, "find my booking".

use std::{collections::HashMap, time::Duration as StdDuration};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{NaiveTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::booking_settings;
use crate::{
    camp_time,
    db::bookings::{BookingStatus, CancelledBy, PaymentPurpose},
    error::{AppError, AppResult},
    services::{
        audit::Actor,
        booking::{
            manage::{self, RefundChoice},
            register::{self, GuestEntry},
            trip::pricing::cancellation_steps,
        },
        payments, turnstile,
    },
    state::AppState,
    web::{
        client::{ClientInfo, RequestLocale},
        limits::{idempotency_key, idempotent, rate_limit},
    },
};

/// The booking a request may see: `?t=` or `X-Booking-Token` must be its signed token.
/// Unknown reference and wrong token both answer 404 (no enumeration).
pub(crate) async fn access(
    state: &AppState,
    reference: &str,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> AppResult<Uuid> {
    let token = query
        .get("t")
        .cloned()
        .or_else(|| {
            headers
                .get("x-booking-token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .ok_or(AppError::NotFound)?;
    manage::resolve_access(&state.db, &state.config.jwt_secret, reference, &token)
        .await?
        .ok_or(AppError::NotFound)
}

/// Everything the guest sees about their booking.
pub(crate) async fn booking_view(state: &AppState, booking_id: Uuid) -> AppResult<Value> {
    let b = sqlx::query!(
        r#"SELECT b.id, b.reference, b.status AS "status: BookingStatus", b.locale, b.currency::text AS "currency!",
                  b.arrival_date, b.departure_date, b.adults, b.children, b.infants,
                  b.subtotal_minor, b.discount_minor, b.tax_minor, b.total_minor, b.amount_paid_minor,
                  b.deposit_due_minor, b.balance_due_on, b.payment_option, b.balance_collection,
                  b.hold_expires_at, b.estimated_arrival_time, b.special_requests, b.created_at, b.confirmed_at,
                  b.cancelled_at, b.cancellation_reason, b.cancellation_policy_snapshot, b.precheckin_completed_at,
                  b.saved_payment_method_id IS NOT NULL AS "card_saved!",
                  c.first_name, c.last_name, c.email::text AS "email?", c.phone, c.country_code,
                  rp.code::text AS "rate_plan_code?", rp.meal_plan AS "meal_plan?"
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           LEFT JOIN rate_plans rp ON rp.id = b.rate_plan_id
           WHERE b.id = $1"#,
        booking_id
    )
    .fetch_one(&state.db)
    .await?;
    let items: Vec<Value> = sqlx::query!(
        r#"SELECT i.id, i.parent_item_id, i.product_id, i.product_kind, i.status, i.quantity,
                  lower(i.service_period) AS "first_day?", upper(i.service_period) AS "last_day?",
                  i.starts_at, i.subtotal_minor, i.discount_minor, i.tax_minor, i.total_minor,
                  i.product_snapshot, i.guide_language, i.requested_time, i.price_breakdown,
                  s.ends_at AS "ends_at?",
                  (SELECT COALESCE(array_agg(u.code::text ORDER BY u.code), '{}') FROM unit_allocations ua
                   JOIN accommodation_units u ON u.id = ua.unit_id
                   WHERE ua.booking_item_id = i.id AND ua.released_at IS NULL) AS "units!",
                  (SELECT COALESCE(jsonb_object_agg(g.guest_category_id::text, g.guest_count), '{}')
                   FROM booking_item_guests g WHERE g.booking_item_id = i.id) AS "guests!"
           FROM booking_items i LEFT JOIN activity_slots s ON s.id = i.activity_slot_id
           WHERE i.booking_id = $1 ORDER BY i.created_at, i.id"#,
        booking_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|i| {
        json!({
            "id": i.id, "parent_id": i.parent_item_id, "product_id": i.product_id, "kind": i.product_kind,
            "status": i.status, "quantity": i.quantity, "arrival": i.first_day, "departure": i.last_day,
            "starts_at": i.starts_at, "ends_at": i.ends_at,
            "name": i.product_snapshot["localized_name"].as_str().or(i.product_snapshot["name"].as_str()),
            "slug": i.product_snapshot["slug"], "mandatory": i.product_snapshot["mandatory"],
            "in_package": i.product_snapshot["in_package"],
            "check_in_from": i.product_snapshot["check_in_from"], "check_out_until": i.product_snapshot["check_out_until"],
            "subtotal_minor": i.subtotal_minor, "discount_minor": i.discount_minor, "tax_minor": i.tax_minor,
            "total_minor": i.total_minor, "discounts": i.product_snapshot["discounts"], "taxes": i.product_snapshot["taxes"],
            "lines": i.price_breakdown, "language": i.guide_language, "requested_time": i.requested_time,
            "units": i.units, "guests": i.guests,
        })
    })
    .collect();
    let payments: Vec<Value> = sqlx::query!(
        r#"SELECT p.id, p.provider, p.purpose, p.status, p.amount_minor, p.paid_at, p.created_at,
                  COALESCE((SELECT sum(r.amount_minor) FROM refunds r WHERE r.payment_id = p.id AND r.status = 'succeeded'), 0)::bigint AS "refunded!"
           FROM payments p WHERE p.booking_id = $1 AND p.status NOT IN ('pending', 'cancelled')
           ORDER BY p.created_at"#,
        booking_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|p| json!({
        "id": p.id, "provider": p.provider, "purpose": p.purpose, "status": p.status, "amount_minor": p.amount_minor,
        "refunded_minor": p.refunded, "paid_at": p.paid_at, "created_at": p.created_at,
    }))
    .collect();
    let settings = booking_settings(state).await?;
    let start = trip_start(state, booking_id).await?;
    let balance = (b.total_minor - b.amount_paid_minor).max(0);
    let due_now = match b.status {
        BookingStatus::AwaitingPayment => (b.deposit_due_minor - b.amount_paid_minor).max(0),
        _ => 0,
    };
    let started = start.is_some_and(|s| s <= Utc::now());
    let open_for_guest = matches!(
        b.status,
        BookingStatus::Pending | BookingStatus::AwaitingPayment | BookingStatus::Confirmed
    );
    Ok(json!({
        "reference": b.reference,
        "status": b.status,
        "locale": b.locale,
        "currency": b.currency,
        "arrival": b.arrival_date,
        "departure": b.departure_date,
        "party": { "adults": b.adults, "children": b.children, "infants": b.infants },
        "created_at": b.created_at,
        "confirmed_at": b.confirmed_at,
        "cancelled_at": b.cancelled_at,
        "cancellation_reason": b.cancellation_reason,
        "hold_expires_at": b.hold_expires_at,
        "customer": { "first_name": b.first_name, "last_name": b.last_name, "email": b.email, "phone": b.phone, "country_code": b.country_code },
        "estimated_arrival_time": b.estimated_arrival_time,
        "special_requests": b.special_requests,
        "rate_plan": { "code": b.rate_plan_code, "meal_plan": b.meal_plan },
        "items": items,
        "totals": {
            "subtotal_minor": b.subtotal_minor, "discount_minor": b.discount_minor, "tax_minor": b.tax_minor,
            "total_minor": b.total_minor, "paid_minor": b.amount_paid_minor, "balance_minor": balance,
            "deposit_due_minor": b.deposit_due_minor, "due_now_minor": due_now, "balance_due_on": b.balance_due_on,
        },
        "payment_option": b.payment_option,
        "balance_collection": b.balance_collection,
        "card_saved": b.card_saved,
        "payments": payments,
        "starts_at": start,
        "cancellation": cancellation_steps(b.cancellation_policy_snapshot.as_ref(), start),
        "precheckin_completed": b.precheckin_completed_at.is_some(),
        "can": {
            "pay": due_now > 0 || (b.status == BookingStatus::Confirmed && balance > 0),
            "cancel": settings.guest_cancellation && open_for_guest && !started,
            "edit": open_for_guest && !started,
            "precheckin": matches!(b.status, BookingStatus::Confirmed | BookingStatus::Pending | BookingStatus::AwaitingPayment),
        },
    }))
}

async fn trip_start(
    state: &AppState,
    booking_id: Uuid,
) -> AppResult<Option<chrono::DateTime<Utc>>> {
    let starts: Vec<Value> = sqlx::query_scalar!(
        "SELECT product_snapshot->'starts_at' FROM booking_items WHERE booking_id = $1 AND status = 'active'",
        booking_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .flatten()
    .collect();
    Ok(starts
        .into_iter()
        .filter_map(|v| serde_json::from_value::<chrono::DateTime<Utc>>(v).ok())
        .min())
}

pub async fn get(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let id = access(&state, &reference, &headers, &query).await?;
    Ok(Json(booking_view(&state, id).await?))
}

/// Light polling for the payment return page.
pub async fn status(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let id = access(&state, &reference, &headers, &query).await?;
    let s = sqlx::query!(
        r#"SELECT b.status AS "status: BookingStatus", b.amount_paid_minor, b.total_minor, b.hold_expires_at,
                  (SELECT p.status FROM payments p WHERE p.booking_id = b.id ORDER BY p.created_at DESC LIMIT 1) AS "payment_status?",
                  (SELECT p.failure_reason FROM payments p WHERE p.booking_id = b.id ORDER BY p.created_at DESC LIMIT 1) AS "failure_reason?"
           FROM bookings b WHERE b.id = $1"#,
        id
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({
        "status": s.status, "paid_minor": s.amount_paid_minor, "total_minor": s.total_minor,
        "hold_expires_at": s.hold_expires_at, "payment_status": s.payment_status, "failure_reason": s.failure_reason,
    })))
}

#[derive(Debug, Deserialize)]
pub struct PayBody {
    #[serde(default)]
    pub provider: Option<String>,
}

/// Starts the payment of what is due: the deposit / full amount of a held booking, or the
/// balance of a confirmed one.
pub async fn pay(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    client: ClientInfo,
    Json(body): Json<PayBody>,
) -> AppResult<Json<Value>> {
    rate_limit(
        &state.cache,
        "pay",
        &client.key(),
        20,
        StdDuration::from_secs(60),
    )
    .await?;
    let id = access(&state, &reference, &headers, &query).await?;
    let key = idempotency_key(&headers);
    let response = idempotent(&state.cache, "pay", key, || async {
        let b = sqlx::query!(
            r#"SELECT status AS "status: BookingStatus", total_minor, amount_paid_minor, deposit_due_minor, payment_option
               FROM bookings WHERE id = $1"#,
            id
        )
        .fetch_one(&state.db)
        .await?;
        let (purpose, amount) = match b.status {
            BookingStatus::AwaitingPayment => (
                if b.payment_option.as_deref() == Some("deposit") { PaymentPurpose::Deposit } else { PaymentPurpose::Full },
                (b.deposit_due_minor.max(1) - b.amount_paid_minor).max(0),
            ),
            BookingStatus::Confirmed | BookingStatus::CheckedIn => {
                (PaymentPurpose::Balance, (b.total_minor - b.amount_paid_minor).max(0))
            }
            other => return Err(payments::PaymentError::NotPayable(other).into()),
        };
        let started = payments::start(&state.db, &state.payments, body.provider.as_deref(), id, purpose, amount, false).await?;
        Ok(json!({ "payment": started, "payment_config": state.payments.public_config() }))
    })
    .await?;
    Ok(Json(response))
}

pub async fn cancellation_preview(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let id = access(&state, &reference, &headers, &query).await?;
    let mut conn = state.db.acquire().await?;
    let preview = manage::preview_cancellation(&mut conn, id, None, Utc::now()).await?;
    let settings = booking_settings(&state).await?;
    let started = trip_start(&state, id)
        .await?
        .is_some_and(|s| s <= Utc::now());
    let mut value = serde_json::to_value(&preview).unwrap_or_default();
    if !settings.guest_cancellation {
        value["allowed"] = json!(false);
        value["blocked_reason"] = json!("disabled");
    } else if started {
        value["allowed"] = json!(false);
        value["blocked_reason"] = json!("started");
    }
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
pub struct CancelBody {
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    client: ClientInfo,
    Json(body): Json<CancelBody>,
) -> AppResult<Json<Value>> {
    let id = access(&state, &reference, &headers, &query).await?;
    let settings = booking_settings(&state).await?;
    let started = trip_start(&state, id)
        .await?
        .is_some_and(|s| s <= Utc::now());
    if !settings.guest_cancellation || started {
        return Err(AppError::domain(
            StatusCode::FORBIDDEN,
            "cancellation_not_allowed",
            "this booking can no longer be cancelled online, contact the camp",
        ));
    }
    let reason = body
        .reason
        .map(|r| r.chars().take(500).collect::<String>())
        .filter(|r| !r.trim().is_empty());
    let outcome = manage::cancel(
        &state.db,
        &state.payments,
        id,
        CancelledBy::Customer,
        &client.actor(Actor::customer()),
        reason,
        RefundChoice::Policy,
        Utc::now(),
    )
    .await?;
    Ok(Json(
        json!({ "refund_minor": outcome.refund_minor, "refunds": outcome.refunds }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct DetailsBody {
    pub estimated_arrival_time: Option<NaiveTime>,
    pub special_requests: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    client: ClientInfo,
    Json(body): Json<DetailsBody>,
) -> AppResult<Json<Value>> {
    let id = access(&state, &reference, &headers, &query).await?;
    if body
        .special_requests
        .as_deref()
        .is_some_and(|s| s.chars().count() > 2000)
    {
        return Err(AppError::BadRequest("special requests too long".into()));
    }
    manage::update_details(
        &state.db,
        id,
        body.estimated_arrival_time,
        body.special_requests,
        &client.actor(Actor::customer()),
    )
    .await?;
    Ok(Json(booking_view(&state, id).await?))
}

pub async fn guests(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let id = access(&state, &reference, &headers, &query).await?;
    let guests = register::load(
        &state.db,
        state.config.booking.guest_data_key.as_ref(),
        id,
        false,
    )
    .await?;
    Ok(Json(
        json!({ "guests": guests, "id_numbers_enabled": state.config.booking.guest_data_key.is_some() }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct GuestsBody {
    pub guests: Vec<GuestEntry>,
}

pub async fn save_guests(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    client: ClientInfo,
    Json(body): Json<GuestsBody>,
) -> AppResult<Json<Value>> {
    let id = access(&state, &reference, &headers, &query).await?;
    let complete = register::save(
        &state.db,
        state.config.booking.guest_data_key.as_ref(),
        id,
        &body.guests,
        &client.actor(Actor::customer()),
        Utc::now(),
    )
    .await?;
    Ok(Json(json!({ "complete": complete })))
}

#[derive(Debug, Deserialize)]
pub struct FindBody {
    pub reference: String,
    pub email: String,
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// "Find my booking": always the same answer; the link goes to the booking's email address.
pub async fn find(
    State(state): State<AppState>,
    client: ClientInfo,
    Json(body): Json<FindBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    rate_limit(
        &state.cache,
        "find",
        &client.key(),
        5,
        StdDuration::from_secs(900),
    )
    .await?;
    let reference = body.reference.trim().to_ascii_uppercase();
    rate_limit(
        &state.cache,
        "find_ref",
        &reference,
        5,
        StdDuration::from_secs(3600),
    )
    .await?;
    turnstile::verify(
        &state.http,
        state.config.booking.turnstile_secret.as_deref(),
        body.captcha_token.as_deref(),
        client.ip,
    )
    .await?;
    let settings = booking_settings(&state).await?;
    manage::request_magic_link(
        &state.db,
        &reference,
        &body.email,
        client.ip_network(),
        settings.magic_link_minutes,
        Utc::now(),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "sent": true }))))
}

#[derive(Debug, Deserialize)]
pub struct AccessBody {
    pub token: String,
}

/// Exchanges the magic-link token for the booking's access token.
pub async fn redeem(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    client: ClientInfo,
    Json(body): Json<AccessBody>,
) -> AppResult<Json<Value>> {
    rate_limit(
        &state.cache,
        "access",
        &client.key(),
        20,
        StdDuration::from_secs(600),
    )
    .await?;
    let token = manage::redeem_magic_link(
        &state.db,
        &state.config.jwt_secret,
        &reference,
        &body.token,
        Utc::now(),
    )
    .await?
    .ok_or_else(|| {
        AppError::domain(
            StatusCode::GONE,
            "link_expired",
            "this link expired or was already used",
        )
    })?;
    Ok(Json(json!({ "access_token": token })))
}

fn ics_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

/// `.ics` with the stay (all-day, arrival → departure) and each timed activity.
pub async fn ics(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    RequestLocale(_locale): RequestLocale,
) -> AppResult<Response> {
    let id = access(&state, &reference, &headers, &query).await?;
    let view = booking_view(&state, id).await?;
    let site = sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'contact.address'")
        .fetch_optional(&state.db)
        .await?
        .and_then(|v| {
            v.get("fr")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| v.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "M'ssici, Morocco".into());
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut out = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Mouzoun//Booking//EN\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\n",
    );
    let mut n = 0;
    for item in view["items"].as_array().into_iter().flatten() {
        if item["status"] != "active" || !item["parent_id"].is_null() {
            continue;
        }
        let name = item["name"].as_str().unwrap_or("Mouzoun");
        let (start, end) = match (
            item["arrival"].as_str(),
            item["departure"].as_str(),
            item["starts_at"].as_str(),
        ) {
            (_, _, Some(starts)) => {
                let s: chrono::DateTime<Utc> = starts.parse().map_err(|_| AppError::NotFound)?;
                let e: chrono::DateTime<Utc> = item["ends_at"]
                    .as_str()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(s + chrono::Duration::hours(2));
                (
                    format!("DTSTART:{}", s.format("%Y%m%dT%H%M%SZ")),
                    format!("DTEND:{}", e.format("%Y%m%dT%H%M%SZ")),
                )
            }
            (Some(a), Some(d), None) => (
                format!("DTSTART;VALUE=DATE:{}", a.replace('-', "")),
                format!("DTEND;VALUE=DATE:{}", d.replace('-', "")),
            ),
            _ => continue,
        };
        n += 1;
        out.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:{}-{n}@mouzoun.com\r\nDTSTAMP:{stamp}\r\n{start}\r\n{end}\r\nSUMMARY:{}\r\nLOCATION:{}\r\nDESCRIPTION:{}\r\nEND:VEVENT\r\n",
            reference,
            ics_escape(&format!("{name} — Mouzoun")),
            ics_escape(&site),
            ics_escape(&format!("Booking {reference}")),
        ));
    }
    out.push_str("END:VCALENDAR\r\n");
    let _ = camp_time::today(Utc::now());
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/calendar; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{reference}.ics\""),
            ),
        ],
        out,
    )
        .into_response())
}
