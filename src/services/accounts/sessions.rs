//! Guest sessions: one `user_sessions` row per sign-in (a refresh-token family).
//!
//! - access token: JWT (15 min) in the `HttpOnly` cookie `__Host-mz_at`, carrying the session id;
//!   every request also checks that the session is still active (sign-out everywhere is
//!   immediate);
//! - refresh token: random, in `__Host-mz_rt`, stored hashed; each refresh replaces it
//!   (rotation) and keeps the previous hash: presenting a rotated token again revokes the whole
//!   session (theft detection). Two tabs refreshing at the same moment are tolerated for a few
//!   seconds;
//! - "This is a shared device": cookies without `Max-Age` (gone when the browser closes) and a
//!   12-hour server-side lifetime instead of 60 days.

use axum_extra::extract::cookie::{Cookie, SameSite};
use chrono::{DateTime, Duration, Utc};
use ipnetwork::IpNetwork;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::tokens::{random_token, sha256};
use crate::{
    auth::jwt,
    error::{AppError, AppResult},
    services::audit::{self, Actor},
};

pub const ACCESS_MINUTES: i64 = 15;
pub const PERSISTENT_DAYS: i64 = 60;
pub const SHARED_HOURS: i64 = 12;
/// A rotated token presented again within this window is a concurrent refresh, not a theft.
const ROTATION_GRACE_SECS: i64 = 10;

pub fn access_cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-mz_at" } else { "mz_at" }
}

pub fn refresh_cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-mz_rt" } else { "mz_rt" }
}

fn cookie(
    name: &'static str,
    value: String,
    max_age: Option<time::Duration>,
    secure: bool,
) -> Cookie<'static> {
    let mut c = Cookie::build((name, value))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .build();
    if let Some(age) = max_age {
        c.set_max_age(age);
    }
    c
}

#[derive(Debug, Clone)]
pub struct IssuedSession {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub access_token: String,
    pub refresh_token: String,
    pub persistent: bool,
    pub expires_at: DateTime<Utc>,
}

impl IssuedSession {
    /// Both cookies of the session.
    pub fn cookies(&self, secure: bool) -> [Cookie<'static>; 2] {
        let lifetime = (self.expires_at - Utc::now()).num_seconds().max(0);
        [
            cookie(
                access_cookie_name(secure),
                self.access_token.clone(),
                self.persistent
                    .then(|| time::Duration::minutes(ACCESS_MINUTES)),
                secure,
            ),
            cookie(
                refresh_cookie_name(secure),
                self.refresh_token.clone(),
                self.persistent.then(|| time::Duration::seconds(lifetime)),
                secure,
            ),
        ]
    }
}

pub fn removal_cookies(secure: bool) -> [Cookie<'static>; 2] {
    [
        cookie(
            access_cookie_name(secure),
            String::new(),
            Some(time::Duration::ZERO),
            secure,
        ),
        cookie(
            refresh_cookie_name(secure),
            String::new(),
            Some(time::Duration::ZERO),
            secure,
        ),
    ]
}

/// Opens a new session for a verified sign-in.
pub async fn create(
    pool: &PgPool,
    jwt_secret: &str,
    user_id: Uuid,
    persistent: bool,
    user_agent: Option<&str>,
    ip: Option<IpNetwork>,
    now: DateTime<Utc>,
) -> AppResult<IssuedSession> {
    let session_id = Uuid::now_v7();
    let refresh_token = random_token();
    let expires_at = if persistent {
        now + Duration::days(PERSISTENT_DAYS)
    } else {
        now + Duration::hours(SHARED_HOURS)
    };
    sqlx::query!(
        "INSERT INTO user_sessions (id, user_id, refresh_token_hash, user_agent, ip_address, last_used_at, expires_at,
                                    kind, persistent, reauthenticated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'guest', $8, $6)",
        session_id,
        user_id,
        sha256(&refresh_token),
        user_agent.map(|ua| ua.chars().take(400).collect::<String>()),
        ip,
        now,
        expires_at,
        persistent
    )
    .execute(pool)
    .await?;
    Ok(IssuedSession {
        session_id,
        user_id,
        access_token: jwt::issue_guest(jwt_secret, user_id, session_id, ACCESS_MINUTES)?,
        refresh_token,
        persistent,
        expires_at,
    })
}

/// Outcome of presenting a refresh token.
#[derive(Debug)]
pub enum Refreshed {
    /// Rotated: new cookies to send.
    Rotated(IssuedSession),
    /// A concurrent request just rotated it: the user is known, the other response carries the
    /// new cookies.
    Concurrent { user_id: Uuid, session_id: Uuid },
}

pub async fn refresh(
    pool: &PgPool,
    jwt_secret: &str,
    refresh_token: &str,
    now: DateTime<Utc>,
) -> AppResult<Refreshed> {
    let hash = sha256(refresh_token);
    let new_token = random_token();
    let rotated = sqlx::query!(
        r#"UPDATE user_sessions s
           SET previous_refresh_token_hash = s.refresh_token_hash, refresh_token_hash = $2, last_used_at = $3
           FROM users u
           WHERE s.refresh_token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > $3 AND s.kind = 'guest'
             AND u.id = s.user_id AND u.status = 'active' AND u.deleted_at IS NULL
           RETURNING s.id, s.user_id, s.persistent, s.expires_at"#,
        hash,
        sha256(&new_token),
        now
    )
    .fetch_optional(pool)
    .await?;
    if let Some(s) = rotated {
        return Ok(Refreshed::Rotated(IssuedSession {
            session_id: s.id,
            user_id: s.user_id,
            access_token: jwt::issue_guest(jwt_secret, s.user_id, s.id, ACCESS_MINUTES)?,
            refresh_token: new_token,
            persistent: s.persistent,
            expires_at: s.expires_at,
        }));
    }
    // Not the current token: a rotated one presented again?
    let reused = sqlx::query!(
        "SELECT id, user_id, last_used_at FROM user_sessions
         WHERE previous_refresh_token_hash = $1 AND revoked_at IS NULL",
        hash
    )
    .fetch_optional(pool)
    .await?;
    if let Some(s) = reused {
        let recent = s
            .last_used_at
            .is_some_and(|at| now - at < Duration::seconds(ROTATION_GRACE_SECS));
        if recent {
            return Ok(Refreshed::Concurrent {
                user_id: s.user_id,
                session_id: s.id,
            });
        }
        tracing::warn!(session = %s.id, alert = true, "refresh token reuse: session revoked");
        revoke(pool, s.id, "refresh_token_reuse", now).await?;
        let mut conn = pool.acquire().await?;
        audit::record(
            &mut conn,
            &Actor::system(),
            "session.reuse_detected",
            "user",
            &s.user_id.to_string(),
            serde_json::json!({ "session_id": s.id }),
        )
        .await?;
    }
    Err(AppError::Unauthorized)
}

pub async fn revoke(
    pool: &PgPool,
    session_id: Uuid,
    reason: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE user_sessions SET revoked_at = $2, revoked_reason = $3 WHERE id = $1 AND revoked_at IS NULL",
        session_id,
        now,
        reason
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Revokes every guest session of a user (optionally keeping one). Returns how many.
pub async fn revoke_all(
    pool: &PgPool,
    user_id: Uuid,
    except: Option<Uuid>,
    reason: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<u64> {
    Ok(sqlx::query!(
        "UPDATE user_sessions SET revoked_at = $3, revoked_reason = $4
         WHERE user_id = $1 AND revoked_at IS NULL AND ($2::uuid IS NULL OR id <> $2)",
        user_id,
        except,
        now,
        reason
    )
    .execute(pool)
    .await?
    .rows_affected())
}

/// Whether the session is still usable (checked on every request).
pub async fn is_active(pool: &PgPool, session_id: Uuid, now: DateTime<Utc>) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM user_sessions s JOIN users u ON u.id = s.user_id
                          WHERE s.id = $1 AND s.revoked_at IS NULL AND s.expires_at > $2
                            AND u.status = 'active' AND u.deleted_at IS NULL) AS "ok!""#,
        session_id,
        now
    )
    .fetch_one(pool)
    .await
}

pub async fn mark_reauthenticated(
    pool: &PgPool,
    session_id: Uuid,
    now: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE user_sessions SET reauthenticated_at = $2 WHERE id = $1",
        session_id,
        now
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Sensitive actions need a fresh proof of identity (code, password, passkey) on this session.
pub async fn require_recent_auth(
    pool: &PgPool,
    session_id: Uuid,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let at = sqlx::query_scalar!(
        "SELECT reauthenticated_at FROM user_sessions WHERE id = $1",
        session_id
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    if at.is_some_and(|at| now - at <= Duration::minutes(10)) {
        Ok(())
    } else {
        Err(AppError::domain(
            axum::http::StatusCode::FORBIDDEN,
            "reauthentication_required",
            "confirm it is you first",
        ))
    }
}

#[derive(Debug, Serialize)]
pub struct SessionView {
    pub id: Uuid,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub persistent: bool,
    pub current: bool,
}

pub async fn list(
    pool: &PgPool,
    user_id: Uuid,
    current: Uuid,
    now: DateTime<Utc>,
) -> sqlx::Result<Vec<SessionView>> {
    Ok(sqlx::query!(
        r#"SELECT id, user_agent, ip_address::text AS "ip?", created_at, last_used_at, persistent
           FROM user_sessions
           WHERE user_id = $1 AND kind = 'guest' AND revoked_at IS NULL AND expires_at > $2
           ORDER BY last_used_at DESC NULLS LAST"#,
        user_id,
        now
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|s| SessionView {
        id: s.id,
        user_agent: s.user_agent,
        // Approximate location: the network, never the full address.
        ip_address: s.ip.map(|ip| mask_ip(&ip)),
        created_at: s.created_at,
        last_used_at: s.last_used_at,
        persistent: s.persistent,
        current: s.id == current,
    })
    .collect())
}

fn mask_ip(ip: &str) -> String {
    let ip = ip.split('/').next().unwrap_or(ip);
    if ip.contains(':') {
        let parts: Vec<&str> = ip.split(':').take(3).collect();
        format!("{}::/48", parts.join(":"))
    } else {
        let parts: Vec<&str> = ip.split('.').take(3).collect();
        format!("{}.0/24", parts.join("."))
    }
}
