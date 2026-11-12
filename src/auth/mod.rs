mod extractor;
pub mod guest;
mod handlers;
pub mod jwt;
pub mod mfa;
pub mod password;
pub mod session;
mod staff;

use axum::{
    Router,
    routing::{get, post},
};

use crate::state::AppState;

pub use extractor::{AuthUser, require_staff};
pub use guest::GuestUser;
pub use staff::Staff;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", post(handlers::login))
        .route("/mfa", post(mfa::verify))
        .route("/logout", post(handlers::logout))
        .route("/me", get(handlers::me))
}
