//! "Privacy": download my data, delete my account.

use axum::{
    Json,
    extract::{Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::GuestUser,
    camp_time,
    db::identity::DeletionRequestedBy,
    error::{AppError, AppResult},
    services::{
        accounts::{privacy, sessions},
        audit::{self, Actor},
    },
    state::AppState,
    web::client::ClientInfo,
};

pub async fn request_export(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
) -> AppResult<(StatusCode, Json<Value>)> {
    let now = Utc::now();
    sessions::require_recent_auth(&state.db, guest.session_id, now).await?;
    let id = privacy::request_export(&state.db, guest.user_id, now).await?;
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "account.export_requested",
        "user",
        &guest.user_id.to_string(),
        json!({ "export_id": id }),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "requested": true, "days": privacy::EXPORT_DAYS })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub token: String,
}

pub async fn download_export(
    State(state): State<AppState>,
    guest: GuestUser,
    Query(q): Query<TokenQuery>,
) -> AppResult<impl IntoResponse> {
    let data = privacy::download(&state.db, guest.user_id, &q.token, Utc::now()).await?;
    let body = serde_json::to_vec_pretty(&data).map_err(|e| AppError::Internal(e.into()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"mouzoun-my-data.json\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    ))
}

pub async fn deletion_status(
    State(state): State<AppState>,
    guest: GuestUser,
) -> AppResult<Json<Value>> {
    let now = Utc::now();
    let scheduled = sqlx::query_scalar!(
        "SELECT scheduled_for FROM account_deletion_requests WHERE user_id = $1 AND cancelled_at IS NULL AND completed_at IS NULL",
        guest.user_id
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(json!({
        "scheduled_for": scheduled,
        "blockers": privacy::deletion_blockers(&state.db, guest.user_id, camp_time::today(now)).await?,
        "grace_days": privacy::grace_days(&state.db).await?,
    })))
}

pub async fn request_deletion(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
) -> AppResult<(StatusCode, Json<Value>)> {
    let now = Utc::now();
    sessions::require_recent_auth(&state.db, guest.session_id, now).await?;
    let when = privacy::schedule_deletion(
        &state.db,
        guest.user_id,
        DeletionRequestedBy::Customer,
        &client.actor(Actor::customer()),
        now,
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "scheduled_for": when }))))
}

pub async fn cancel_deletion(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
) -> AppResult<StatusCode> {
    privacy::cancel_deletion(
        &state.db,
        Some(guest.user_id),
        None,
        &client.actor(Actor::customer()),
        Utc::now(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct TokenBody {
    pub token: String,
}

/// The "Keep my account" link of the email (no session needed).
pub async fn cancel_deletion_by_link(
    State(state): State<AppState>,
    client: ClientInfo,
    Json(body): Json<TokenBody>,
) -> AppResult<StatusCode> {
    privacy::cancel_deletion(
        &state.db,
        None,
        Some(&body.token),
        &client.actor(Actor::customer()),
        Utc::now(),
    )
    .await
    .map_err(|_| {
        AppError::domain(
            StatusCode::GONE,
            "link_expired",
            "this link expired or was already used",
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}
