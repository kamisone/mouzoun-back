//! "Sign in or create an account" by email: a 6-digit code (10 minutes, 5 attempts) and a magic
//! link in the same email. The answer is the same whether the address has an account or not.
//! Codes live in Redis (the address may not have an account yet: no users row is ever created
//! for an unverified address). On success the account is created if needed and linked to the
//! customer record of that verified email — which brings every booking made as a guest.

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::tokens::{
    constant_time_eq, normalize_email, random_token, sha256, sha256_hex, six_digit_code,
};
use crate::{
    db::types::Locale,
    error::{AppError, AppResult},
    infra::cache::Cache,
    services::{
        audit::{self, Actor},
        outbox::{self, Event},
    },
};

pub const CODE_MINUTES: i64 = 10;
pub const MAX_ATTEMPTS: u32 = 5;
const RESEND_AFTER_SECS: i64 = 30;

#[derive(Debug, Serialize, Deserialize)]
struct PendingCode {
    code_hash: String,
    link_hash: String,
    attempts: u32,
    sent_at: DateTime<Utc>,
}

fn key(cache: &Cache, email: &str) -> String {
    cache.key(&format!(
        "acct:code:{}",
        sha256_hex(&normalize_email(email))
    ))
}

/// Sends a sign-in code + link to `email` (unless one was sent less than 30 s ago). Never says
/// whether the address has an account.
pub async fn send_code(
    pool: &PgPool,
    cache: &Cache,
    email: &str,
    locale: &Locale,
    return_to: Option<&str>,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let email = normalize_email(email);
    let k = key(cache, &email);
    let mut conn = cache.connection();
    let existing: Option<String> = conn.get(&k).await?;
    if let Some(p) = existing.and_then(|raw| serde_json::from_str::<PendingCode>(&raw).ok())
        && (now - p.sent_at).num_seconds() < RESEND_AFTER_SECS
    {
        return Ok(());
    }
    let code = six_digit_code();
    let token = random_token();
    let pending = PendingCode {
        code_hash: sha256_hex(&format!("{email}:{code}")),
        link_hash: sha256_hex(&token),
        attempts: 0,
        sent_at: now,
    };
    let _: () = conn
        .set_ex(
            &k,
            serde_json::to_string(&pending).unwrap_or_default(),
            (CODE_MINUTES * 60) as u64,
        )
        .await?;
    let mut tx = pool.begin().await?;
    outbox::enqueue(
        &mut tx,
        &Event::AuthLoginCode {
            email,
            locale: locale.as_str().to_string(),
            code,
            token,
            return_to: return_to.map(str::to_string),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

fn invalid(attempts_left: u32) -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_code",
        "this code is not valid",
    )
    .with_details(serde_json::json!({ "attempts_left": attempts_left }))
}

fn expired() -> AppError {
    AppError::domain(
        StatusCode::GONE,
        "code_expired",
        "this code expired: ask for a new one",
    )
}

/// Checks a code typed by the user. Five wrong attempts burn the code (ask for a new one): an
/// attacker can never lock the account itself.
pub async fn check_code(cache: &Cache, email: &str, code: &str) -> AppResult<()> {
    let email = normalize_email(email);
    let k = key(cache, &email);
    let mut conn = cache.connection();
    let raw: Option<String> = conn.get(&k).await?;
    let Some(mut pending) = raw.and_then(|r| serde_json::from_str::<PendingCode>(&r).ok()) else {
        return Err(expired());
    };
    let given = sha256_hex(&format!("{email}:{}", code.trim()));
    if constant_time_eq(given.as_bytes(), pending.code_hash.as_bytes()) {
        let _: () = conn.del(&k).await?;
        return Ok(());
    }
    pending.attempts += 1;
    if pending.attempts >= MAX_ATTEMPTS {
        let _: () = conn.del(&k).await?;
        return Err(AppError::domain(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_attempts",
            "too many wrong codes: ask for a new one",
        ));
    }
    let ttl: i64 = conn.ttl(&k).await.unwrap_or(60);
    let _: () = conn
        .set_ex(
            &k,
            serde_json::to_string(&pending).unwrap_or_default(),
            ttl.max(1) as u64,
        )
        .await?;
    Err(invalid(MAX_ATTEMPTS - pending.attempts))
}

/// Checks the magic link of the same email (single use).
pub async fn check_link(cache: &Cache, email: &str, token: &str) -> AppResult<()> {
    let email = normalize_email(email);
    let k = key(cache, &email);
    let mut conn = cache.connection();
    let raw: Option<String> = conn.get(&k).await?;
    let Some(pending) = raw.and_then(|r| serde_json::from_str::<PendingCode>(&r).ok()) else {
        return Err(expired());
    };
    if !constant_time_eq(sha256_hex(token).as_bytes(), pending.link_hash.as_bytes()) {
        return Err(expired());
    }
    let _: () = conn.del(&k).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct SignedIn {
    pub user_id: Uuid,
    pub created: bool,
    /// First and last name still to ask (new account without a previous booking).
    pub needs_profile: bool,
}

/// The user of a verified email address: found, or created and linked to the customer record
/// of that address (and so to every booking made with it).
pub async fn user_for_verified_email(
    pool: &PgPool,
    email: &str,
    locale: &Locale,
    actor: &Actor,
    now: DateTime<Utc>,
) -> AppResult<SignedIn> {
    let email = normalize_email(email);
    let mut tx = pool.begin().await?;
    let existing = sqlx::query!(
        r#"SELECT id, status, email_verified_at FROM users
           WHERE email = $1::text::citext AND deleted_at IS NULL FOR UPDATE"#,
        email
    )
    .fetch_optional(&mut *tx)
    .await?;
    let (user_id, created) = match existing {
        Some(u) if u.status == "suspended" => {
            return Err(AppError::domain(
                StatusCode::FORBIDDEN,
                "account_suspended",
                "this account is suspended: contact the camp",
            ));
        }
        Some(u) => {
            sqlx::query!(
                "UPDATE users SET email_verified_at = COALESCE(email_verified_at, $2),
                        status = CASE WHEN status = 'invited' AND NOT is_staff THEN 'active' ELSE status END
                 WHERE id = $1",
                u.id,
                now
            )
            .execute(&mut *tx)
            .await?;
            (u.id, false)
        }
        None => {
            let id = Uuid::now_v7();
            sqlx::query!(
                "INSERT INTO users (id, email, full_name, status, is_staff, email_verified_at, preferred_locale)
                 VALUES ($1, $2::text::citext, '', 'active', false, $3, $4)",
                id,
                email,
                now,
                locale.as_str()
            )
            .execute(&mut *tx)
            .await?;
            (id, true)
        }
    };
    let customer = link_customer(&mut tx, user_id, &email, locale).await?;
    if created {
        audit::record(
            &mut tx,
            actor,
            "account.created",
            "user",
            &user_id.to_string(),
            serde_json::json!({}),
        )
        .await?;
        outbox::enqueue(&mut tx, &Event::AccountCreated { user_id }).await?;
    }
    // Names known from a booking fill the account at once.
    let needs_profile = sqlx::query_scalar!(
        r#"SELECT (btrim(first_name) = '' OR last_name IS NULL OR btrim(last_name) = '') AS "missing!"
           FROM customers WHERE id = $1"#,
        customer
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE users u SET full_name = btrim(c.first_name || ' ' || COALESCE(c.last_name, ''))
         FROM customers c WHERE u.id = $1 AND c.id = $2 AND u.full_name = ''",
        user_id,
        customer
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(SignedIn {
        user_id,
        created,
        needs_profile,
    })
}

/// The customer record of an account: the one already linked, else the (unlinked) customer
/// with the same verified email, else a new one. Returns its id.
pub async fn link_customer(
    conn: &mut PgConnection,
    user_id: Uuid,
    email: &str,
    locale: &Locale,
) -> AppResult<Uuid> {
    if let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM customers WHERE user_id = $1 AND deleted_at IS NULL",
        user_id
    )
    .fetch_optional(&mut *conn)
    .await?
    {
        return Ok(id);
    }
    let found = sqlx::query_scalar!(
        "SELECT id FROM customers WHERE email = $1::text::citext AND deleted_at IS NULL AND user_id IS NULL
         FOR UPDATE",
        email
    )
    .fetch_optional(&mut *conn)
    .await?;
    match found {
        Some(id) => {
            sqlx::query!(
                "UPDATE customers SET user_id = $2 WHERE id = $1",
                id,
                user_id
            )
            .execute(&mut *conn)
            .await?;
            Ok(id)
        }
        None => {
            let id = Uuid::now_v7();
            sqlx::query!(
                "INSERT INTO customers (id, user_id, email, first_name, preferred_locale)
                 VALUES ($1, $2, $3::text::citext, '', $4)",
                id,
                user_id,
                email,
                locale.as_str()
            )
            .execute(&mut *conn)
            .await?;
            Ok(id)
        }
    }
}

/// The customer id of an account (every account has one after sign-in).
pub async fn customer_of(pool: &PgPool, user_id: Uuid) -> AppResult<Uuid> {
    sqlx::query_scalar!(
        "SELECT id FROM customers WHERE user_id = $1 AND deleted_at IS NULL",
        user_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound)
}

/// Emails a security notice when this sign-in comes from a device never used before.
pub async fn notice_new_device(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    user_agent: Option<&str>,
) -> AppResult<()> {
    let known = sqlx::query!(
        r#"SELECT count(*) AS "total!", count(*) FILTER (WHERE user_agent IS NOT DISTINCT FROM $3) AS "same!"
           FROM user_sessions WHERE user_id = $1 AND id <> $2"#,
        user_id,
        session_id,
        user_agent
    )
    .fetch_one(pool)
    .await?;
    if known.total > 0 && known.same == 0 {
        let mut tx = pool.begin().await?;
        outbox::enqueue(
            &mut tx,
            &Event::AuthNewDevice {
                user_id,
                session_id,
            },
        )
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// For hashing a password reset or claim token stored in `auth_tokens`.
pub fn token_hash(token: &str) -> Vec<u8> {
    sha256(token)
}
