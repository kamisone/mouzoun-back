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
    error::AppResult,
    web::{
        extract::ApiJson,
        pagination::{PageParams, Paginated},
    },
};

// ---- Public -----------------------------------------------------------------------------------

/// GET /v1/activities
pub async fn list_published(State(svc): State<ActivityService>) -> AppResult<Json<Vec<Activity>>> {
    Ok(Json(svc.list_published().await?))
}

/// GET /v1/activities/{slug}
pub async fn get_published(
    State(svc): State<ActivityService>,
    Path(slug): Path<String>,
) -> AppResult<Json<Activity>> {
    Ok(Json(svc.get_published(&slug).await?))
}

// ---- Back-office ------------------------------------------------------------------------------

/// GET /v1/admin/activities
pub async fn list(
    State(svc): State<ActivityService>,
    Query(params): Query<PageParams>,
) -> AppResult<Json<Paginated<Activity>>> {
    Ok(Json(svc.list_all(params).await?))
}

/// GET /v1/admin/activities/{id}
pub async fn get(
    State(svc): State<ActivityService>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Activity>> {
    Ok(Json(svc.get(id).await?))
}

/// POST /v1/admin/activities
pub async fn create(
    State(svc): State<ActivityService>,
    ApiJson(input): ApiJson<ActivityInput>,
) -> AppResult<(StatusCode, Json<Activity>)> {
    Ok((StatusCode::CREATED, Json(svc.create(input).await?)))
}

/// PUT /v1/admin/activities/{id}
pub async fn update(
    State(svc): State<ActivityService>,
    Path(id): Path<Uuid>,
    ApiJson(input): ApiJson<ActivityInput>,
) -> AppResult<Json<Activity>> {
    Ok(Json(svc.update(id, input).await?))
}

/// DELETE /v1/admin/activities/{id}
pub async fn delete(
    State(svc): State<ActivityService>,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    svc.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
