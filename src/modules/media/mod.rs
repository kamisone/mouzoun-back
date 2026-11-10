//! Media library backed by object storage.
//!
//! Upload protocol (bytes never transit through the API):
//! 1. `POST /v1/media/uploads`          → validates the request, returns a signed PUT URL
//! 2. client `PUT`s the file to that URL (straight to GCS)
//! 3. `POST /v1/media/{id}/complete`    → verifies size, type and magic bytes, marks it `ready`
//!
//! Uploads never completed are purged by `jobs::media_cleanup`.

mod handlers;
pub mod model;
pub mod policy;
mod repository;
mod service;

use axum::{
    Router,
    routing::{get, post},
};

pub use model::{Media, MediaDto, MediaStatus, MediaVisibility};
pub use service::MediaService;

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(handlers::list))
        .route("/uploads", post(handlers::create_upload))
        .route("/{id}", get(handlers::get).delete(handlers::delete))
        .route("/{id}/complete", post(handlers::complete))
}
