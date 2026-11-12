//! Customer CRM (`/v1/admin/crm/...`). Every action is audited.
//!
//! | endpoint | permission |
//! |---|---|
//! | GET crm/customers, GET crm/customers/{id}, GET crm/reports/accounts | customers.read |
//! | POST …/sign-in-link, …/suspend, …/reactivate, …/sign-out-all, POST crm/bookings/{id}/attach | customers.write |
//! | POST …/merge | customers.merge |
//! | GET …/view-as | customers.impersonate |
//! | GET …/export, POST …/deletion | customers.privacy |

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::Staff,
    db::{identity::DeletionRequestedBy, types::Locale},
    error::{AppError, AppResult},
    modules::account::profile::account_view,
    services::{
        accounts::{privacy, sessions, signin},
        audit,
    },
    state::AppState,
};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub q: String,
    /// `with` / `without` an account.
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    staff.require("customers.read")?;
    let term = format!("%{}%", q.q.trim().replace('%', "\\%").replace('_', "\\_"));
    let page = q.page.unwrap_or(1).max(1);
    let rows = sqlx::query!(
        r#"SELECT c.id, c.first_name, c.last_name, c.email::text AS "email?", c.phone, c.country_code, c.created_at,
                  c.marketing_consent, c.user_id, u.status AS "account_status?", u.last_login_at,
                  (SELECT count(*) FROM bookings b WHERE b.customer_id = c.id AND b.status NOT IN ('draft', 'expired')) AS "bookings!",
                  (SELECT COALESCE(sum(b.amount_paid_minor), 0) FROM bookings b WHERE b.customer_id = c.id)::bigint AS "paid_minor!",
                  (SELECT max(b.arrival_date) FROM bookings b WHERE b.customer_id = c.id) AS "last_stay?",
                  count(*) OVER () AS "total!"
           FROM customers c LEFT JOIN users u ON u.id = c.user_id
           WHERE c.deleted_at IS NULL AND c.anonymized_at IS NULL
             AND ($1 = '%%' OR c.email::text ILIKE $1 OR c.phone ILIKE $1
                  OR (c.first_name || ' ' || COALESCE(c.last_name, '')) ILIKE $1)
             AND ($2::text IS NULL OR ($2 = 'with') = (c.user_id IS NOT NULL))
           ORDER BY c.updated_at DESC LIMIT 50 OFFSET $3"#,
        term,
        q.account.filter(|a| a == "with" || a == "without"),
        (page - 1) * 50
    )
    .fetch_all(&state.db)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let customers: Vec<Value> = rows
        .into_iter()
        .map(|c| json!({
            "id": c.id, "first_name": c.first_name, "last_name": c.last_name, "email": c.email, "phone": c.phone,
            "country_code": c.country_code, "created_at": c.created_at, "marketing_consent": c.marketing_consent,
            "has_account": c.user_id.is_some(), "account_status": c.account_status, "last_login_at": c.last_login_at,
            "bookings": c.bookings, "paid_minor": c.paid_minor, "last_stay": c.last_stay,
        }))
        .collect();
    Ok(Json(
        json!({ "customers": customers, "total": total, "page": page, "per_page": 50 }),
    ))
}

struct Target {
    user_id: Option<Uuid>,
    email: Option<String>,
    locale: Option<String>,
}

async fn target(state: &AppState, id: Uuid) -> AppResult<Target> {
    sqlx::query_as!(
        Target,
        r#"SELECT user_id, email::text AS "email?", preferred_locale AS locale FROM customers
           WHERE id = $1 AND deleted_at IS NULL AND anonymized_at IS NULL"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

fn no_account() -> AppError {
    AppError::domain(
        StatusCode::CONFLICT,
        "no_account",
        "this customer has no account",
    )
}

pub async fn detail(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("customers.read")?;
    let c = sqlx::query!(
        r#"SELECT c.id, c.first_name, c.last_name, c.email::text AS "email?", c.phone, c.country_code, c.preferred_locale,
                  c.marketing_consent, c.marketing_consent_at, c.internal_notes, c.created_at, c.user_id,
                  u.status AS "account_status?", u.created_at AS "account_created_at?", u.last_login_at,
                  u.email_verified_at, u.password_hash IS NOT NULL AS "has_password?"
           FROM customers c LEFT JOIN users u ON u.id = c.user_id
           WHERE c.id = $1 AND c.deleted_at IS NULL"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let bookings = sqlx::query!(
        r#"SELECT id, reference, status, arrival_date, departure_date, total_minor, amount_paid_minor,
                  currency::text AS "currency!", source, created_at
           FROM bookings WHERE customer_id = $1 ORDER BY created_at DESC"#,
        id
    )
    .fetch_all(&state.db)
    .await?;
    let (identities, sessions_active, deletion) = match c.user_id {
        Some(user_id) => (
            sqlx::query!("SELECT provider, last_used_at FROM user_identities WHERE user_id = $1", user_id)
                .fetch_all(&state.db)
                .await?
                .into_iter()
                .map(|i| json!({ "provider": i.provider, "last_used_at": i.last_used_at }))
                .collect::<Vec<_>>(),
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM user_sessions WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()"#,
                user_id
            )
            .fetch_one(&state.db)
            .await?,
            sqlx::query_scalar!(
                "SELECT scheduled_for FROM account_deletion_requests WHERE user_id = $1 AND cancelled_at IS NULL AND completed_at IS NULL",
                user_id
            )
            .fetch_optional(&state.db)
            .await?,
        ),
        None => (vec![], 0, None),
    };
    let history = sqlx::query!(
        r#"SELECT action, actor_type, created_at, changes FROM audit_logs
           WHERE (entity_type = 'user' AND entity_id = $1) OR (entity_type = 'customer' AND entity_id = $2)
           ORDER BY created_at DESC LIMIT 50"#,
        c.user_id.map(|u| u.to_string()).unwrap_or_default(),
        id.to_string()
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "id": c.id, "first_name": c.first_name, "last_name": c.last_name, "email": c.email, "phone": c.phone,
        "country_code": c.country_code, "locale": c.preferred_locale, "marketing_consent": c.marketing_consent,
        "marketing_consent_at": c.marketing_consent_at, "internal_notes": c.internal_notes, "created_at": c.created_at,
        "account": c.user_id.map(|_| json!({
            "status": c.account_status, "created_at": c.account_created_at, "last_login_at": c.last_login_at,
            "email_verified_at": c.email_verified_at, "has_password": c.has_password,
            "identities": identities, "active_sessions": sessions_active, "deletion_scheduled_for": deletion,
        })),
        "bookings": bookings.into_iter().map(|b| json!({
            "id": b.id, "reference": b.reference, "status": b.status, "arrival_date": b.arrival_date,
            "departure_date": b.departure_date, "total_minor": b.total_minor, "paid_minor": b.amount_paid_minor,
            "currency": b.currency, "source": b.source, "created_at": b.created_at,
        })).collect::<Vec<_>>(),
        "history": history.into_iter().map(|h| json!({
            "action": h.action, "actor_type": h.actor_type, "at": h.created_at, "data": h.changes,
        })).collect::<Vec<_>>(),
    })))
}

async fn record(
    state: &AppState,
    staff: &Staff,
    action: &str,
    id: Uuid,
    data: Value,
) -> AppResult<()> {
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &staff.actor(),
        action,
        "customer",
        &id.to_string(),
        data,
    )
    .await?;
    Ok(())
}

/// Emails a sign-in code + link (also serves as "resend verification": signing in verifies).
pub async fn send_sign_in_link(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("customers.write")?;
    let t = target(&state, id).await?;
    let email = t.email.ok_or_else(|| {
        AppError::domain(
            StatusCode::CONFLICT,
            "no_email",
            "this customer has no email",
        )
    })?;
    let locale = Locale::new(t.locale.as_deref().unwrap_or("fr"));
    signin::send_code(
        &state.db,
        &state.cache,
        &email,
        &locale,
        Some("/account"),
        Utc::now(),
    )
    .await?;
    record(&state, &staff, "customer.sign_in_link_sent", id, json!({})).await?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn suspend(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("customers.write")?;
    let user = target(&state, id).await?.user_id.ok_or_else(no_account)?;
    let now = Utc::now();
    sqlx::query!(
        "UPDATE users SET status = 'suspended' WHERE id = $1 AND NOT is_staff",
        user
    )
    .execute(&state.db)
    .await?;
    sessions::revoke_all(&state.db, user, None, "suspended", now).await?;
    record(
        &state,
        &staff,
        "customer.suspended",
        id,
        json!({ "user_id": user }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn reactivate(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("customers.write")?;
    let user = target(&state, id).await?.user_id.ok_or_else(no_account)?;
    sqlx::query!("UPDATE users SET status = 'active' WHERE id = $1 AND status = 'suspended' AND deleted_at IS NULL", user)
        .execute(&state.db)
        .await?;
    record(
        &state,
        &staff,
        "customer.reactivated",
        id,
        json!({ "user_id": user }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn sign_out_all(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("customers.write")?;
    let user = target(&state, id).await?.user_id.ok_or_else(no_account)?;
    let n = sessions::revoke_all(&state.db, user, None, "staff_sign_out", Utc::now()).await?;
    record(
        &state,
        &staff,
        "customer.signed_out_everywhere",
        id,
        json!({ "sessions": n }),
    )
    .await?;
    Ok(Json(json!({ "revoked": n })))
}

#[derive(Debug, Deserialize)]
pub struct MergeBody {
    /// The customer kept; `{id}` is merged into it and removed.
    pub into: Uuid,
}

/// Merges a duplicate customer into another: bookings, reviews and the account (if only the
/// duplicate has one) move; the duplicate is soft-deleted. Two accounts are never merged.
pub async fn merge(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<MergeBody>,
) -> AppResult<Json<Value>> {
    staff.require("customers.merge")?;
    if id == body.into {
        return Err(AppError::BadRequest(
            "a customer cannot be merged into itself".into(),
        ));
    }
    let from = target(&state, id).await?;
    let into = target(&state, body.into).await?;
    if from.user_id.is_some() && into.user_id.is_some() {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "both_have_accounts",
            "both customers have an account: they cannot be merged",
        ));
    }
    let mut tx = state.db.begin().await?;
    let moved = sqlx::query_scalar!(
        "UPDATE bookings SET customer_id = $2 WHERE customer_id = $1 RETURNING reference",
        id,
        body.into
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE reviews SET customer_id = $2 WHERE customer_id = $1",
        id,
        body.into
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE customers SET deleted_at = now(), user_id = NULL, internal_notes = COALESCE(internal_notes || E'\\n', '') || 'Merged into ' || $2::uuid
         WHERE id = $1",
        id,
        body.into
    )
    .execute(&mut *tx)
    .await?;
    if let (Some(user), None) = (from.user_id, into.user_id) {
        sqlx::query!(
            "UPDATE customers SET user_id = $2 WHERE id = $1",
            body.into,
            user
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        "UPDATE customers t SET phone = COALESCE(t.phone, f.phone), country_code = COALESCE(t.country_code, f.country_code),
                email = COALESCE(t.email, f.email)
         FROM customers f WHERE t.id = $2 AND f.id = $1",
        id,
        body.into
    )
    .execute(&mut *tx)
    .await?;
    audit::record(&mut tx, &staff.actor(), "customer.merged", "customer", &body.into.to_string(),
        json!({ "merged": id, "bookings": moved, "account_moved": from.user_id.is_some() && into.user_id.is_none() })).await?;
    tx.commit().await?;
    Ok(Json(json!({ "into": body.into, "bookings": moved })))
}

#[derive(Debug, Deserialize)]
pub struct AttachBody {
    pub customer_id: Uuid,
}

pub async fn attach_booking(
    State(state): State<AppState>,
    staff: Staff,
    Path(booking): Path<Uuid>,
    Json(body): Json<AttachBody>,
) -> AppResult<StatusCode> {
    staff.require("customers.write")?;
    target(&state, body.customer_id).await?;
    let mut tx = state.db.begin().await?;
    let before = sqlx::query_scalar!(
        "SELECT customer_id FROM bookings WHERE id = $1 FOR UPDATE",
        booking
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    sqlx::query!(
        "UPDATE bookings SET customer_id = $2 WHERE id = $1",
        booking,
        body.customer_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(&mut tx, &staff.actor(), "booking.customer_changed", "booking", &booking.to_string(),
        json!({ "before": { "customer_id": before }, "after": { "customer_id": body.customer_id } })).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Read-only view of what the customer sees in their account (no session is created).
pub async fn view_as(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("customers.impersonate")?;
    let user = target(&state, id).await?.user_id.ok_or_else(no_account)?;
    let account = account_view(&state, user).await?;
    let trips = sqlx::query!(
        r#"SELECT reference, status, arrival_date, departure_date FROM bookings
           WHERE customer_id = $1 AND status NOT IN ('draft', 'expired') ORDER BY arrival_date DESC NULLS LAST"#,
        id
    )
    .fetch_all(&state.db)
    .await?;
    record(
        &state,
        &staff,
        "customer.viewed_as",
        id,
        json!({ "user_id": user }),
    )
    .await?;
    Ok(Json(json!({
        "read_only": true,
        "account": account,
        "trips": trips.into_iter().map(|t| json!({ "reference": t.reference, "status": t.status,
            "arrival_date": t.arrival_date, "departure_date": t.departure_date })).collect::<Vec<_>>(),
    })))
}

pub async fn export(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("customers.privacy")?;
    let user = target(&state, id).await?.user_id.ok_or_else(no_account)?;
    let data = privacy::export(&state.db, user).await?;
    record(&state, &staff, "customer.exported", id, json!({})).await?;
    Ok(Json(data))
}

pub async fn schedule_deletion(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("customers.privacy")?;
    let user = target(&state, id).await?.user_id.ok_or_else(no_account)?;
    let when = privacy::schedule_deletion(
        &state.db,
        user,
        DeletionRequestedBy::Staff,
        &staff.actor(),
        Utc::now(),
    )
    .await?;
    Ok(Json(json!({ "scheduled_for": when })))
}

/// Accounts report: sign-ups, share of bookings made signed in, sign-in methods, top wishlisted.
pub async fn report(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("customers.read")?;
    let since = Utc::now() - Duration::days(90);
    let weekly = sqlx::query!(
        r#"SELECT date_trunc('week', created_at)::date AS "week!", count(*) AS "n!"
           FROM users WHERE NOT is_staff AND created_at >= $1 GROUP BY 1 ORDER BY 1"#,
        since
    )
    .fetch_all(&state.db)
    .await?;
    let share = sqlx::query!(
        r#"SELECT count(*) AS "total!", count(*) FILTER (WHERE c.user_id IS NOT NULL) AS "with_account!"
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           WHERE b.created_at >= $1 AND b.status NOT IN ('draft', 'expired')"#,
        since
    )
    .fetch_one(&state.db)
    .await?;
    let methods = sqlx::query!(
        r#"SELECT changes->>'method' AS "method?", count(*) AS "n!" FROM audit_logs
           WHERE action = 'auth.signed_in' AND created_at >= $1 GROUP BY 1 ORDER BY 2 DESC"#,
        since
    )
    .fetch_all(&state.db)
    .await?;
    let wished = sqlx::query!(
        r#"SELECT i.product_id, COALESCE(t.name, '') AS "name!", count(*) AS "n!"
           FROM wishlist_items i
           LEFT JOIN product_translations t ON t.product_id = i.product_id AND t.locale = 'fr'
           GROUP BY 1, 2 ORDER BY 3 DESC LIMIT 10"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "since": since,
        "signups_per_week": weekly.into_iter().map(|w| json!({ "week": w.week, "count": w.n })).collect::<Vec<_>>(),
        "bookings": { "total": share.total, "with_account": share.with_account },
        "sign_in_methods": methods.into_iter().map(|m| json!({ "method": m.method, "count": m.n })).collect::<Vec<_>>(),
        "most_wished": wished.into_iter().map(|w| json!({ "product_id": w.product_id, "name": w.name, "count": w.n })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct ScanBody {
    pub token: String,
}

/// Scans a guest's QR ticket (`bookings.read`): the booking it proves, or 404.
pub async fn scan_ticket(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<ScanBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.read")?;
    let booking = crate::services::accounts::tickets::verify(
        &state.config.jwt_secret,
        &body.token,
        Utc::now(),
    )
    .ok_or_else(|| {
        AppError::domain(
            StatusCode::NOT_FOUND,
            "invalid_ticket",
            "this ticket is not valid",
        )
    })?;
    let b = sqlx::query!(
        r#"SELECT b.id, b.reference, b.status, b.arrival_date, b.departure_date, b.adults, b.children, b.infants,
                  b.total_minor - b.amount_paid_minor AS "balance_minor!", b.precheckin_completed_at,
                  c.first_name, c.last_name
           FROM bookings b JOIN customers c ON c.id = b.customer_id WHERE b.id = $1"#,
        booking
    )
    .fetch_one(&state.db)
    .await?;
    record(
        &state,
        &staff,
        "booking.ticket_scanned",
        booking,
        json!({ "reference": b.reference }),
    )
    .await?;
    Ok(Json(json!({
        "id": b.id, "reference": b.reference, "status": b.status, "arrival_date": b.arrival_date,
        "departure_date": b.departure_date, "adults": b.adults, "children": b.children, "infants": b.infants,
        "balance_minor": b.balance_minor, "precheckin_done": b.precheckin_completed_at.is_some(),
        "lead": format!("{} {}", b.first_name, b.last_name.unwrap_or_default()).trim(),
    })))
}
