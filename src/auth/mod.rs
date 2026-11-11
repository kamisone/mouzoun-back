mod extractor;
mod handlers;
pub mod jwt;
pub mod password;
pub mod session;

use axum::{
    Router,
    routing::{get, post},
};

use crate::state::AppState;

pub use extractor::{AuthUser, require_staff};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", post(handlers::login))
        .route("/logout", post(handlers::logout))
        .route("/me", get(handlers::me))
}
