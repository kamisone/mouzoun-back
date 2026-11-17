use axum::{
    Extension, Json,
    extract::{Multipart, Path, Query, State, multipart::MultipartError},
    http::StatusCode,
};
use uuid::Uuid;

use super::{
    model::{MediaDto, MediaFilter, MediaVisibility, NewUpload},
    service::MediaService,
};
use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    web::pagination::{PageParams, Paginated},
};

/// GET /v1/admin/media?status=&page=&per_page=
pub async fn list(
    State(media): State<MediaService>,
    Query(params): Query<PageParams>,
    Query(filter): Query<MediaFilter>,
) -> AppResult<Json<Paginated<MediaDto>>> {
    Ok(Json(media.list(params, filter).await?))
}

/// POST /v1/admin/media (multipart: `file`, optional `visibility` = public | private).
///
/// The API receives the bytes and writes the object itself, like a server-side SDK upload: the
/// browser only talks to the API, so the bucket needs no CORS rule.
pub async fn upload(
    State(media): State<MediaService>,
    Extension(user): Extension<AuthUser>,
    mut multipart: Multipart,
) -> AppResult<(StatusCode, Json<MediaDto>)> {
    let mut file = None;
    let mut visibility = MediaVisibility::default();
    while let Some(field) = multipart.next_field().await.map_err(bad_multipart)? {
        match field.name() {
            Some("file") => {
                let filename = field.file_name().unwrap_or_default().to_string();
                let content_type = field.content_type().unwrap_or_default().to_string();
                let bytes = field.bytes().await.map_err(bad_multipart)?;
                file = Some((filename, content_type, bytes));
            }
            Some("visibility") => {
                visibility = match field.text().await.map_err(bad_multipart)?.as_str() {
                    "public" => MediaVisibility::Public,
                    "private" => MediaVisibility::Private,
                    _ => {
                        return Err(AppError::BadRequest(
                            "visibility must be public or private".into(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let (filename, content_type, bytes) =
        file.ok_or_else(|| AppError::BadRequest("missing file field".into()))?;
    let dto = media
        .upload(
            NewUpload {
                filename,
                content_type,
                bytes,
                visibility,
            },
            user.id,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(dto)))
}

/// Malformed body, or a file over the route's body limit (413).
fn bad_multipart(e: MultipartError) -> AppError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::domain(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "file too large",
        )
    } else {
        AppError::BadRequest(e.body_text())
    }
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
