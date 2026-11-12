//! Passkey endpoints: add / list / remove (Login & security), sign in (also from the browser's
//! autofill, "conditional UI"), and confirm-it-is-you before sensitive actions.

use std::time::Duration as StdDuration;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::GuestUser,
    error::{AppError, AppResult},
    services::{
        accounts::{
            passkeys::{self, AssertionResponse, RegistrationResponse, Rp},
            sessions,
            signin::SignedIn,
        },
        audit::{self, Actor},
        outbox::{self, Event},
    },
    state::AppState,
    web::{client::ClientInfo, limits::rate_limit},
};

fn rp(state: &AppState) -> Rp {
    Rp {
        id: state.config.booking.webauthn_rp_id.clone(),
        origin: state
            .config
            .booking
            .webauthn_origin
            .trim_end_matches('/')
            .to_string(),
    }
}

async fn credential_ids(state: &AppState, user_id: Uuid) -> AppResult<Vec<Vec<u8>>> {
    Ok(sqlx::query_scalar!(
        "SELECT credential_id FROM webauthn_credentials WHERE user_id = $1",
        user_id
    )
    .fetch_all(&state.db)
    .await?)
}

pub async fn list(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<Value>> {
    let rows = sqlx::query!(
        "SELECT id, name, backed_up, created_at, last_used_at FROM webauthn_credentials WHERE user_id = $1 ORDER BY created_at",
        guest.user_id
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "passkeys": rows.into_iter().map(|p| json!({
        "id": p.id, "name": p.name, "synced": p.backed_up, "created_at": p.created_at, "last_used_at": p.last_used_at,
    })).collect::<Vec<_>>() })))
}

/// Options for `navigator.credentials.create` (needs a recent proof of identity).
pub async fn registration_options(
    State(state): State<AppState>,
    guest: GuestUser,
) -> AppResult<Json<Value>> {
    sessions::require_recent_auth(&state.db, guest.session_id, Utc::now()).await?;
    let u = sqlx::query!(
        r#"SELECT email::text AS "email!", full_name FROM users WHERE id = $1"#,
        guest.user_id
    )
    .fetch_one(&state.db)
    .await?;
    let challenge =
        passkeys::new_challenge(&state.cache, &format!("reg:{}", guest.user_id)).await?;
    let exclude = credential_ids(&state, guest.user_id).await?;
    Ok(Json(passkeys::creation_options(
        &rp(&state),
        "Mouzoun",
        guest.user_id,
        &u.email,
        &u.full_name,
        &challenge,
        &exclude,
    )))
}

#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    #[serde(default)]
    pub name: Option<String>,
    pub credential: RegistrationResponse,
}

pub async fn register(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<RegisterBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    sessions::require_recent_auth(&state.db, guest.session_id, Utc::now()).await?;
    let cred =
        passkeys::verify_registration(&state.cache, &rp(&state), guest.user_id, &body.credential)
            .await?;
    let name = body
        .name
        .map(|n| n.trim().chars().take(80).collect::<String>())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            crate::services::email::account_jobs::describe_device(client.user_agent.as_deref())
        });
    let mut tx = state.db.begin().await?;
    let id = sqlx::query_scalar!(
        "INSERT INTO webauthn_credentials (user_id, credential_id, public_key, sign_count, transports, backed_up, name)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (credential_id) DO NOTHING RETURNING id",
        guest.user_id,
        cred.credential_id,
        cred.public_key,
        i64::from(cred.sign_count),
        &cred.transports,
        cred.backed_up,
        name
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::domain(StatusCode::CONFLICT, "passkey_exists", "this passkey is already registered"))?;
    audit::record(
        &mut tx,
        &client.actor(Actor::customer()),
        "account.passkey_added",
        "user",
        &guest.user_id.to_string(),
        json!({ "passkey_id": id }),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::AuthPasskeyAdded {
            user_id: guest.user_id,
        },
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "name": name }))))
}

pub async fn remove(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    sessions::require_recent_auth(&state.db, guest.session_id, Utc::now()).await?;
    let deleted = sqlx::query!(
        "DELETE FROM webauthn_credentials WHERE id = $1 AND user_id = $2",
        id,
        guest.user_id
    )
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "account.passkey_removed",
        "user",
        &guest.user_id.to_string(),
        json!({ "passkey_id": id }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Options for a username-less sign-in (also used by the autofill / conditional UI).
pub async fn sign_in_options(
    State(state): State<AppState>,
    client: ClientInfo,
) -> AppResult<Json<Value>> {
    rate_limit(
        &state.cache,
        "acct_passkey_ip",
        &client.key(),
        60,
        StdDuration::from_secs(900),
    )
    .await?;
    let challenge = passkeys::new_challenge(&state.cache, "auth").await?;
    Ok(Json(passkeys::request_options(
        &rp(&state),
        &challenge,
        &[],
    )))
}

#[derive(Debug, Deserialize)]
pub struct SignInBody {
    pub credential: AssertionResponse,
    #[serde(default)]
    pub shared_device: bool,
}

struct Stored {
    id: Uuid,
    user_id: Uuid,
    public_key: Vec<u8>,
    sign_count: i64,
}

async fn check(
    state: &AppState,
    purpose: &str,
    credential: &AssertionResponse,
    only_user: Option<Uuid>,
) -> AppResult<Uuid> {
    let cred_id = credential.credential_id()?;
    let stored = sqlx::query_as!(
        Stored,
        "SELECT c.id, c.user_id, c.public_key, c.sign_count FROM webauthn_credentials c JOIN users u ON u.id = c.user_id
         WHERE c.credential_id = $1 AND u.deleted_at IS NULL AND NOT u.is_staff",
        cred_id
    )
    .fetch_optional(&state.db)
    .await?
    .filter(|s| only_user.is_none_or(|u| u == s.user_id))
    .ok_or_else(|| AppError::domain(StatusCode::UNAUTHORIZED, "passkey_unknown", "this passkey is not linked to an account"))?;
    let (count, backed_up) = passkeys::verify_assertion(
        &state.cache,
        &rp(state),
        purpose,
        credential,
        &stored.public_key,
        stored.sign_count,
    )
    .await?;
    sqlx::query!(
        "UPDATE webauthn_credentials SET sign_count = $2, backed_up = $3, last_used_at = now() WHERE id = $1",
        stored.id,
        i64::from(count),
        backed_up
    )
    .execute(&state.db)
    .await?;
    Ok(stored.user_id)
}

pub async fn sign_in(
    State(state): State<AppState>,
    jar: CookieJar,
    client: ClientInfo,
    Json(body): Json<SignInBody>,
) -> AppResult<(CookieJar, Json<Value>)> {
    rate_limit(
        &state.cache,
        "acct_passkey_ip",
        &client.key(),
        60,
        StdDuration::from_secs(900),
    )
    .await?;
    let user_id = check(&state, "auth", &body.credential, None).await?;
    let status = sqlx::query_scalar!("SELECT status FROM users WHERE id = $1", user_id)
        .fetch_one(&state.db)
        .await?;
    if status == "suspended" {
        return Err(AppError::domain(
            StatusCode::FORBIDDEN,
            "account_suspended",
            "this account is suspended: contact the camp",
        ));
    }
    let signed = SignedIn {
        user_id,
        created: false,
        needs_profile: false,
    };
    super::auth::finish(&state, jar, &client, signed, !body.shared_device, "passkey").await
}

/// Options to confirm it is you with one of your passkeys.
pub async fn reauth_options(
    State(state): State<AppState>,
    guest: GuestUser,
) -> AppResult<Json<Value>> {
    let challenge =
        passkeys::new_challenge(&state.cache, &format!("reauth:{}", guest.user_id)).await?;
    let allow = credential_ids(&state, guest.user_id).await?;
    if allow.is_empty() {
        return Err(AppError::NotFound);
    }
    Ok(Json(passkeys::request_options(
        &rp(&state),
        &challenge,
        &allow,
    )))
}

#[derive(Debug, Deserialize)]
pub struct ReauthBody {
    pub credential: AssertionResponse,
}

pub async fn reauth(
    State(state): State<AppState>,
    guest: GuestUser,
    Json(body): Json<ReauthBody>,
) -> AppResult<StatusCode> {
    check(
        &state,
        &format!("reauth:{}", guest.user_id),
        &body.credential,
        Some(guest.user_id),
    )
    .await?;
    sessions::mark_reauthenticated(&state.db, guest.session_id, Utc::now()).await?;
    Ok(StatusCode::NO_CONTENT)
}
