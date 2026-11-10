use axum::extract::{FromRequest, rejection::JsonRejection};

use crate::error::AppError;

/// JSON body extractor whose rejections use the standard API error format.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(AppError))]
pub struct ApiJson<T>(pub T);

impl From<JsonRejection> for AppError {
    fn from(rejection: JsonRejection) -> Self {
        AppError::BadRequest(rejection.body_text())
    }
}
