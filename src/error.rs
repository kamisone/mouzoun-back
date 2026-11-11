//! Application error type and its mapping to HTTP responses.
//!
//! Every error leaves the API as `{"error": {"code": "...", "details": ...}}`.
//! Internal details (SQL, storage messages) are logged, never sent to clients.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::infra::storage::StorageError;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("validation failed")]
    Validation(#[from] validator::ValidationErrors),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, details) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", None),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden", None),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "conflict", Some(json!(msg))),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", Some(json!(msg))),
            AppError::Validation(errs) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                Some(json!(errs)),
            ),
            AppError::Database(sqlx::Error::RowNotFound) => {
                (StatusCode::NOT_FOUND, "not_found", None)
            }
            AppError::Database(sqlx::Error::Database(e)) if e.is_unique_violation() => (
                StatusCode::CONFLICT,
                "conflict",
                Some(json!("resource already exists")),
            ),
            AppError::Database(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => (
                StatusCode::CONFLICT,
                "conflict",
                Some(json!("resource is referenced by other records")),
            ),
            AppError::Storage(_) => {
                tracing::error!(error = ?self, "storage error");
                (StatusCode::BAD_GATEWAY, "storage_unavailable", None)
            }
            AppError::Database(_) | AppError::Redis(_) | AppError::Internal(_) => {
                tracing::error!(error = ?self, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", None)
            }
        };

        let body = Json(json!({ "error": { "code": code, "details": details } }));
        (status, body).into_response()
    }
}
