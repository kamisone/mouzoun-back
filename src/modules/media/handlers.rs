use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use uuid::Uuid;

use super::{
    model::{CreateUploadRequest, ListMediaQuery, MediaDto, UploadTicket},
    service::MediaService,
};
use crate::{
    auth::AdminUser,
    error::AppResult,
    web::{extract::ApiJson, pagination::Paginated},
};

/// POST /v1/media/uploads — staff only.
pub async fn create_upload(
    State(media): State<MediaService>,
    AdminUser(user): AdminUser,
    ApiJson(req): ApiJson<CreateUploadRequest>,
) -> AppResult<(StatusCode, Json<UploadTicket>)> {
    let ticket = media.create_upload(req, user.id).await?;
    Ok((StatusCode::CREATED, Json(ticket)))
}

/// POST /v1/media/{id}/complete — staff only.
pub async fn complete(
    State(media): State<MediaService>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<MediaDto>> {
    Ok(Json(media.complete(id).await?))
}

/// GET /v1/media — staff only.
pub async fn list(
    State(media): State<MediaService>,
    _admin: AdminUser,
    Query(q): Query<ListMediaQuery>,
) -> AppResult<Json<Paginated<MediaDto>>> {
    Ok(Json(media.list(q).await?))
}

/// GET /v1/media/{id} — staff only.
pub async fn get(
    State(media): State<MediaService>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<MediaDto>> {
    Ok(Json(media.get(id).await?))
}

/// DELETE /v1/media/{id} — staff only. 409 if still referenced.
pub async fn delete(
    State(media): State<MediaService>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    media.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
