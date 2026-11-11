use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use uuid::Uuid;

use super::{
    model::{CreateUploadRequest, MediaDto, MediaFilter, UploadTicket},
    service::MediaService,
};
use crate::{
    auth::AuthUser,
    error::AppResult,
    web::{
        extract::ApiJson,
        pagination::{PageParams, Paginated},
    },
};

/// GET /v1/admin/media?status=&page=&per_page=
pub async fn list(
    State(media): State<MediaService>,
    Query(params): Query<PageParams>,
    Query(filter): Query<MediaFilter>,
) -> AppResult<Json<Paginated<MediaDto>>> {
    Ok(Json(media.list(params, filter).await?))
}

/// POST /v1/admin/media/uploads
pub async fn create_upload(
    State(media): State<MediaService>,
    Extension(user): Extension<AuthUser>,
    ApiJson(req): ApiJson<CreateUploadRequest>,
) -> AppResult<(StatusCode, Json<UploadTicket>)> {
    let ticket = media.create_upload(req, user.id).await?;
    Ok((StatusCode::CREATED, Json(ticket)))
}

/// POST /v1/admin/media/{id}/complete
pub async fn complete(
    State(media): State<MediaService>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<MediaDto>> {
    Ok(Json(media.complete(id).await?))
}

/// GET /v1/admin/media/{id}
pub async fn get(
    State(media): State<MediaService>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<MediaDto>> {
    Ok(Json(media.get(id).await?))
}

/// DELETE /v1/admin/media/{id} — 409 if still referenced (e.g. an activity cover).
pub async fn delete(
    State(media): State<MediaService>,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    media.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
