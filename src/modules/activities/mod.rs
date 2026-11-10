//! Tourism activities (camel trek, oasis tour, desert night...): public catalogue + admin CRUD.

mod handlers;
pub mod model;
mod repository;
mod service;

use axum::{Router, routing::get};

pub use model::{Activity, ActivityInput};
pub use service::ActivityService;

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(handlers::list_published).post(handlers::create))
        .route("/admin", get(handlers::list_all))
        .route("/{slug}", get(handlers::get_by_slug))
        .route(
            "/id/{id}",
            get(handlers::get_by_id)
                .put(handlers::update)
                .delete(handlers::delete),
        )
}
