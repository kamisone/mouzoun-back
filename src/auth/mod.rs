mod extractor;
mod handlers;
pub mod jwt;
pub mod password;

use axum::{
    Router,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

pub use extractor::{AdminUser, AuthUser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "user_role", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Staff,
    Customer,
}

impl Role {
    pub fn is_staff(self) -> bool {
        matches!(self, Role::Admin | Role::Staff)
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", post(handlers::login))
        .route("/me", get(handlers::me))
}
