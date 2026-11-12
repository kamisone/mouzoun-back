//! Privacy rights (GDPR, Moroccan law 09-08): data export and account deletion.
//!
//! - Export: requested from the account, built by the `accounts` job, emailed as a link valid 7
//!   days (downloading needs the same signed-in account).
//! - Deletion: scheduled after a grace period (`account.deletion_grace_days`, 14), cancellable by
//!   link or from the account. Refused while a stay is upcoming or a balance is unpaid. When due,
//!   the account is **anonymised**, not erased: bookings, payments and invoices stay (accounting
//!   obligations) without contact details; the guest register keeps its own legal retention.

use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::tokens::{random_token, sha256};
use crate::{
    camp_time,
    db::identity::DeletionRequestedBy,
    error::{AppError, AppResult},
    services::{
        audit::{self, Actor},
        outbox::{self, Event},
    },
};

pub const EXPORT_DAYS: i64 = 7;

/// Everything we hold about an account, in one JSON document.
pub async fn export(pool: &PgPool, user_id: Uuid) -> AppResult<Value> {
    let user = sqlx::query!(
        r#"SELECT email::text AS "email!", full_name, phone, preferred_locale, date_of_birth, display_currency,
                  created_at, last_login_at, email_verified_at, password_hash IS NOT NULL AS "has_password!"
           FROM users WHERE id = $1"#,
        user_id
    )
    .fetch_one(pool)
    .await?;
    let customer = sqlx::query!(
        r#"SELECT id, first_name, last_name, email::text AS "email?", phone, country_code, preferred_locale,
                  marketing_consent, marketing_consent_at, created_at
           FROM customers WHERE user_id = $1 AND deleted_at IS NULL"#,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    let customer_id = customer.as_ref().map(|c| c.id);
    let bookings = sqlx::query!(
        r#"SELECT b.id, b.reference, b.status, b.arrival_date, b.departure_date, b.adults, b.children, b.infants,
                  b.total_minor, b.amount_paid_minor, b.currency::text AS "currency!", b.special_requests, b.created_at,
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('first_name', g.first_name, 'last_name', g.last_name,
                              'nationality', g.nationality, 'date_of_birth', g.date_of_birth,
                              'id_document_type', g.id_document_type,
                              'id_document_stored', g.id_document_number_enc IS NOT NULL))
                            FROM booking_guests g WHERE g.booking_id = b.id), '[]') AS "guests!",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('amount_minor', p.amount_minor, 'status', p.status,
                              'provider', p.provider, 'purpose', p.purpose, 'paid_at', p.paid_at, 'created_at', p.created_at))
                            FROM payments p WHERE p.booking_id = b.id), '[]') AS "payments!",
                  COALESCE((SELECT jsonb_agg(i.number) FROM invoices i WHERE i.booking_id = b.id), '[]') AS "invoices!"
           FROM bookings b WHERE b.customer_id = $1 ORDER BY b.created_at"#,
        customer_id
    )
    .fetch_all(pool)
    .await?;
    let reviews = sqlx::query!(
        "SELECT rating, title, body, status, created_at FROM reviews WHERE customer_id = $1",
        customer_id
    )
    .fetch_all(pool)
    .await?;
    let travellers = sqlx::query!(
        r#"SELECT first_name, last_name, date_of_birth, nationality::text AS "nationality?", id_document_type,
                  id_document_number_enc IS NOT NULL AS "id_document_stored!", id_consent_at
           FROM saved_travellers WHERE user_id = $1"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let wishlists = sqlx::query!(
        r#"SELECT w.name, COALESCE(array_agg(i.product_id) FILTER (WHERE i.product_id IS NOT NULL), '{}') AS "products!"
           FROM wishlists w LEFT JOIN wishlist_items i ON i.wishlist_id = w.id WHERE w.user_id = $1 GROUP BY w.id"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let notifications = sqlx::query!(
        "SELECT category, channel, enabled, updated_at FROM notification_preferences WHERE user_id = $1",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let identities = sqlx::query!(
        r#"SELECT provider, email::text AS "email?", created_at, last_used_at FROM user_identities WHERE user_id = $1"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let sessions = sqlx::query!(
        "SELECT user_agent, created_at, last_used_at, revoked_at FROM user_sessions WHERE user_id = $1 ORDER BY created_at",
        user_id
    )
    .fetch_all(pool)
    .await?;
    Ok(json!({
        "generated_at": Utc::now(),
        "account": {
            "email": user.email, "full_name": user.full_name, "phone": user.phone, "locale": user.preferred_locale,
            "date_of_birth": user.date_of_birth, "display_currency": user.display_currency,
            "created_at": user.created_at, "last_login_at": user.last_login_at,
            "email_verified_at": user.email_verified_at, "has_password": user.has_password,
        },
        "customer": customer.map(|c| json!({
            "first_name": c.first_name, "last_name": c.last_name, "email": c.email, "phone": c.phone,
            "country": c.country_code, "locale": c.preferred_locale, "marketing_consent": c.marketing_consent,
            "marketing_consent_at": c.marketing_consent_at, "created_at": c.created_at,
        })),
        "bookings": bookings.into_iter().map(|b| json!({
            "reference": b.reference, "status": b.status, "arrival": b.arrival_date, "departure": b.departure_date,
            "adults": b.adults, "children": b.children, "infants": b.infants, "total_minor": b.total_minor,
            "paid_minor": b.amount_paid_minor, "currency": b.currency, "special_requests": b.special_requests,
            "created_at": b.created_at, "guests": b.guests, "payments": b.payments, "invoices": b.invoices,
        })).collect::<Vec<_>>(),
        "reviews": reviews.into_iter().map(|r| json!({
            "rating": r.rating, "title": r.title, "body": r.body, "status": r.status, "created_at": r.created_at,
        })).collect::<Vec<_>>(),
        "saved_travellers": travellers.into_iter().map(|t| json!({
            "first_name": t.first_name, "last_name": t.last_name, "date_of_birth": t.date_of_birth,
            "nationality": t.nationality, "id_document_type": t.id_document_type,
            "id_document_stored": t.id_document_stored, "id_consent_at": t.id_consent_at,
        })).collect::<Vec<_>>(),
        "wishlists": wishlists.into_iter().map(|w| json!({ "name": w.name, "products": w.products })).collect::<Vec<_>>(),
        "notification_preferences": notifications.into_iter().map(|n| json!({
            "category": n.category, "channel": n.channel, "enabled": n.enabled, "updated_at": n.updated_at,
        })).collect::<Vec<_>>(),
        "sign_in_methods": identities.into_iter().map(|i| json!({
            "provider": i.provider, "email": i.email, "linked_at": i.created_at, "last_used_at": i.last_used_at,
        })).collect::<Vec<_>>(),
        "sessions": sessions.into_iter().map(|s| json!({
            "device": s.user_agent, "created_at": s.created_at, "last_used_at": s.last_used_at, "revoked_at": s.revoked_at,
        })).collect::<Vec<_>>(),
        "kept_for_legal_reasons": "Bookings, payments and invoices are kept for accounting obligations.",
    }))
}

/// Queues an export (one per day).
pub async fn request_export(pool: &PgPool, user_id: Uuid, now: DateTime<Utc>) -> AppResult<Uuid> {
    let recent = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM account_exports WHERE user_id = $1 AND requested_at > $2) AS "r!""#,
        user_id,
        now - Duration::hours(24)
    )
    .fetch_one(pool)
    .await?;
    if recent {
        return Err(AppError::domain(
            StatusCode::TOO_MANY_REQUESTS,
            "export_recent",
            "an export was already requested today: check your email",
        ));
    }
    Ok(sqlx::query_scalar!(
        "INSERT INTO account_exports (user_id, requested_at) VALUES ($1, $2) RETURNING id",
        user_id,
        now
    )
    .fetch_one(pool)
    .await?)
}

/// Job: builds pending exports and emails their link.
pub async fn build_pending_exports(pool: &PgPool, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let pending = sqlx::query!(
        "SELECT id, user_id FROM account_exports WHERE ready_at IS NULL ORDER BY requested_at LIMIT 20"
    )
    .fetch_all(pool)
    .await?;
    for p in &pending {
        let data = export(pool, p.user_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let token = random_token();
        let mut tx = pool.begin().await?;
        let updated = sqlx::query!(
            "UPDATE account_exports SET data = $2, token_hash = $3, ready_at = $4, expires_at = $5
             WHERE id = $1 AND ready_at IS NULL",
            p.id,
            data,
            sha256(&token),
            now,
            now + Duration::days(EXPORT_DAYS)
        )
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 1 {
            outbox::enqueue(
                &mut tx,
                &Event::AccountExportReady {
                    user_id: p.user_id,
                    export_id: p.id,
                    token,
                },
            )
            .await?;
        }
        tx.commit().await?;
    }
    Ok(pending.len())
}

/// The export of a link, for its own (signed-in) account only.
pub async fn download(
    pool: &PgPool,
    user_id: Uuid,
    token: &str,
    now: DateTime<Utc>,
) -> AppResult<Value> {
    sqlx::query_scalar!(
        r#"UPDATE account_exports SET downloaded_at = COALESCE(downloaded_at, $3)
           WHERE token_hash = $1 AND user_id = $2 AND expires_at > $3 AND data IS NOT NULL
           RETURNING data AS "data!""#,
        sha256(token),
        user_id,
        now
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        AppError::domain(
            StatusCode::GONE,
            "link_expired",
            "this link expired: ask for a new export",
        )
    })
}

/// What prevents deleting the account now.
pub async fn deletion_blockers(
    pool: &PgPool,
    user_id: Uuid,
    today: NaiveDate,
) -> AppResult<Vec<&'static str>> {
    let b = sqlx::query!(
        r#"SELECT
             EXISTS (SELECT 1 FROM bookings b JOIN customers c ON c.id = b.customer_id
                     WHERE c.user_id = $1 AND b.status IN ('pending', 'awaiting_payment', 'confirmed', 'checked_in')
                       AND COALESCE(b.departure_date, b.arrival_date, $2) >= $2) AS "upcoming!",
             EXISTS (SELECT 1 FROM bookings b JOIN customers c ON c.id = b.customer_id
                     WHERE c.user_id = $1 AND b.status IN ('confirmed', 'checked_in', 'completed', 'no_show')
                       AND b.total_minor > b.amount_paid_minor) AS "unpaid!""#,
        user_id,
        today
    )
    .fetch_one(pool)
    .await?;
    let mut out = vec![];
    if b.upcoming {
        out.push("upcoming_stay");
    }
    if b.unpaid {
        out.push("unpaid_balance");
    }
    Ok(out)
}

pub async fn grace_days(pool: &PgPool) -> AppResult<i64> {
    Ok(
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'account.deletion_grace_days'")
            .fetch_optional(pool)
            .await?
            .and_then(|v| v.as_i64())
            .unwrap_or(14)
            .clamp(0, 90),
    )
}

/// Schedules the deletion of an account. Returns when it will happen.
pub async fn schedule_deletion(
    pool: &PgPool,
    user_id: Uuid,
    by: DeletionRequestedBy,
    actor: &Actor,
    now: DateTime<Utc>,
) -> AppResult<DateTime<Utc>> {
    let blockers = deletion_blockers(pool, user_id, camp_time::today(now)).await?;
    if !blockers.is_empty() {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "deletion_blocked",
            "the account cannot be deleted yet",
        )
        .with_details(json!({ "blockers": blockers })));
    }
    let when = now + Duration::days(grace_days(pool).await?);
    let token = random_token();
    let mut tx = pool.begin().await?;
    let customer = sqlx::query_scalar!(
        "SELECT id FROM customers WHERE user_id = $1 AND deleted_at IS NULL",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let id = sqlx::query_scalar!(
        "INSERT INTO account_deletion_requests (user_id, customer_id, requested_at, scheduled_for, cancel_token_hash, requested_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (user_id) WHERE cancelled_at IS NULL AND completed_at IS NULL DO NOTHING
         RETURNING id",
        user_id,
        customer,
        now,
        when,
        sha256(&token),
        by.as_str()
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        // Already scheduled: same answer.
        let at = sqlx::query_scalar!(
            "SELECT scheduled_for FROM account_deletion_requests WHERE user_id = $1 AND cancelled_at IS NULL AND completed_at IS NULL",
            user_id
        )
        .fetch_one(&mut *tx)
        .await?;
        return Ok(at);
    };
    audit::record(
        &mut tx,
        actor,
        "account.deletion_scheduled",
        "user",
        &user_id.to_string(),
        json!({ "scheduled_for": when }),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::AccountDeletionScheduled {
            user_id,
            request_id: id,
            token,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(when)
}

/// Cancels a scheduled deletion, by the account itself or with the emailed link.
pub async fn cancel_deletion(
    pool: &PgPool,
    user_id: Option<Uuid>,
    token: Option<&str>,
    actor: &Actor,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let cancelled = sqlx::query_scalar!(
        "UPDATE account_deletion_requests SET cancelled_at = $3
         WHERE cancelled_at IS NULL AND completed_at IS NULL
           AND (($1::uuid IS NOT NULL AND user_id = $1) OR ($2::bytea IS NOT NULL AND cancel_token_hash = $2))
         RETURNING user_id",
        user_id,
        token.map(sha256),
        now
    )
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound)?;
    let mut conn = pool.acquire().await?;
    audit::record(
        &mut conn,
        actor,
        "account.deletion_cancelled",
        "user",
        &cancelled.map(|u| u.to_string()).unwrap_or_default(),
        json!({}),
    )
    .await?;
    Ok(())
}

/// Job: carries out due deletions (postponed a day while a blocker appeared meanwhile).
pub async fn run_due_deletions(pool: &PgPool, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let due = sqlx::query!(
        "SELECT id, user_id FROM account_deletion_requests
         WHERE cancelled_at IS NULL AND completed_at IS NULL AND scheduled_for <= $1 LIMIT 20",
        now
    )
    .fetch_all(pool)
    .await?;
    let mut done = 0;
    for d in due {
        let Some(user_id) = d.user_id else { continue };
        let blockers = deletion_blockers(pool, user_id, camp_time::today(now))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if !blockers.is_empty() {
            sqlx::query!(
                "UPDATE account_deletion_requests SET scheduled_for = $2 WHERE id = $1",
                d.id,
                now + Duration::days(1)
            )
            .execute(pool)
            .await?;
            continue;
        }
        let mut tx = pool.begin().await?;
        anonymize(&mut tx, user_id, now)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        sqlx::query!("UPDATE account_deletion_requests SET completed_at = $2, cancel_token_hash = NULL WHERE id = $1", d.id, now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        done += 1;
    }
    Ok(done)
}

/// Removes personal data of an account and its customer record; keeps bookings, payments and
/// invoices (amounts, dates, invoice billing details) for accounting.
pub async fn anonymize(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let u = sqlx::query!(
        r#"SELECT email::text AS "email!", COALESCE(preferred_locale, 'fr') AS "locale!" FROM users WHERE id = $1 FOR UPDATE"#,
        user_id
    )
    .fetch_one(&mut *conn)
    .await?;
    let customers = sqlx::query_scalar!("SELECT id FROM customers WHERE user_id = $1", user_id)
        .fetch_all(&mut *conn)
        .await?;
    sqlx::query!(
        "UPDATE customers SET first_name = 'Deleted', last_name = NULL, email = NULL, phone = NULL, internal_notes = NULL,
                marketing_consent = false, marketing_consent_at = NULL, stripe_customer_id = NULL,
                anonymized_at = $2, user_id = NULL
         WHERE id = ANY($1)",
        &customers,
        now
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE reviews SET author_name = 'Former guest' WHERE customer_id = ANY($1)",
        &customers
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM newsletter_subscribers WHERE email = $1::text::citext",
        u.email
    )
    .execute(&mut *conn)
    .await?;
    for q in [
        "DELETE FROM user_identities WHERE user_id = $1",
        "DELETE FROM saved_travellers WHERE user_id = $1",
        "DELETE FROM wishlists WHERE user_id = $1",
        "DELETE FROM notification_preferences WHERE user_id = $1",
        "DELETE FROM account_exports WHERE user_id = $1",
        "DELETE FROM auth_tokens WHERE user_id = $1",
    ] {
        sqlx::query(q).bind(user_id).execute(&mut *conn).await?;
    }
    sqlx::query!(
        "UPDATE user_sessions SET revoked_at = COALESCE(revoked_at, $2), revoked_reason = COALESCE(revoked_reason, 'account_deleted'),
                user_agent = NULL, ip_address = NULL
         WHERE user_id = $1",
        user_id,
        now
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE users SET email = ('deleted-' || id || '@deleted.invalid')::citext, full_name = '', phone = NULL,
                password_hash = NULL, totp_secret_enc = NULL, date_of_birth = NULL, status = 'suspended', deleted_at = $2
         WHERE id = $1",
        user_id,
        now
    )
    .execute(&mut *conn)
    .await?;
    audit::record(
        conn,
        &Actor::system(),
        "account.deleted",
        "user",
        &user_id.to_string(),
        json!({ "customers": customers }),
    )
    .await?;
    outbox::enqueue(
        conn,
        &Event::AccountDeleted {
            email: u.email,
            locale: u.locale,
        },
    )
    .await?;
    Ok(())
}

/// Job: housekeeping of expired tokens, sessions and exports.
pub async fn cleanup(pool: &PgPool, now: DateTime<Utc>) -> anyhow::Result<()> {
    sqlx::query!(
        "DELETE FROM user_sessions WHERE (expires_at < $1 OR revoked_at < $1) AND kind = 'guest'",
        now - Duration::days(30)
    )
    .execute(pool)
    .await?;
    sqlx::query!(
        "DELETE FROM auth_tokens WHERE (expires_at < $1 OR used_at < $1)",
        now - Duration::days(7)
    )
    .execute(pool)
    .await?;
    sqlx::query!("UPDATE account_exports SET data = NULL, token_hash = NULL WHERE expires_at < $1 AND data IS NOT NULL", now)
        .execute(pool)
        .await?;
    Ok(())
}
