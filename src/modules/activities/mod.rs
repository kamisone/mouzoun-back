//! Tourism activities (camel trek, oasis tour, desert night...): public catalogue + back-office CRUD.

mod handlers;
pub mod model;
mod repository;
mod service;

use axum::{Router, routing::get};

pub use service::ActivityService;

use crate::state::AppState;

/// `/v1/activities`: published activities only.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(handlers::list_published))
        .route("/{slug}", get(handlers::get_published))
}

/// `/v1/admin/activities`: drafts included (staff guard applied by the admin router).
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(handlers::list).post(handlers::create))
        .route(
            "/{id}",
            get(handlers::get)
                .put(handlers::update)
                .delete(handlers::delete),
        )
}
