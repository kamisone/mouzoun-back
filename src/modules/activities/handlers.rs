use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use uuid::Uuid;

use super::{
    model::{Activity, ActivityInput},
    service::ActivityService,
};
use crate::{
    auth::AdminUser,
    error::AppResult,
    web::{
        extract::ApiJson,
        pagination::{PageParams, Paginated},
    },
};

/// GET /v1/activities — public.
pub async fn list_published(State(svc): State<ActivityService>) -> AppResult<Json<Vec<Activity>>> {
    Ok(Json(svc.list_published().await?))
}

/// GET /v1/activities/{slug} — public.
pub async fn get_by_slug(
    State(svc): State<ActivityService>,
    Path(slug): Path<String>,
) -> AppResult<Json<Activity>> {
    Ok(Json(svc.get_published(&slug).await?))
}

/// GET /v1/activities/admin — staff only, includes drafts.
pub async fn list_all(
    State(svc): State<ActivityService>,
    _admin: AdminUser,
    Query(params): Query<PageParams>,
) -> AppResult<Json<Paginated<Activity>>> {
    Ok(Json(svc.list_all(params).await?))
}

/// GET /v1/activities/id/{id} — staff only.
pub async fn get_by_id(
    State(svc): State<ActivityService>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Activity>> {
    Ok(Json(svc.get(id).await?))
}

/// POST /v1/activities — staff only.
pub async fn create(
    State(svc): State<ActivityService>,
    _admin: AdminUser,
    ApiJson(input): ApiJson<ActivityInput>,
) -> AppResult<(StatusCode, Json<Activity>)> {
    Ok((StatusCode::CREATED, Json(svc.create(input).await?)))
}

/// PUT /v1/activities/id/{id} — staff only.
pub async fn update(
    State(svc): State<ActivityService>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
    ApiJson(input): ApiJson<ActivityInput>,
) -> AppResult<Json<Activity>> {
    Ok(Json(svc.update(id, input).await?))
}

/// DELETE /v1/activities/id/{id} — staff only.
pub async fn delete(
    State(svc): State<ActivityService>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    svc.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
