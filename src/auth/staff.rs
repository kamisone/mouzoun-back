//! Staff with their effective permissions (roles → permissions), for per-endpoint checks inside
//! the back-office router (already guarded by `require_staff`).

use std::collections::HashSet;

use axum::{extract::FromRequestParts, http::request::Parts};
use uuid::Uuid;

use super::AuthUser;
use crate::{error::AppError, services::audit::Actor, state::AppState};

#[derive(Debug, Clone)]
pub struct Staff {
    pub id: Uuid,
    pub permissions: HashSet<String>,
}

impl Staff {
    pub fn can(&self, permission: &str) -> bool {
        self.permissions.contains(permission)
    }

    /// `403 forbidden` unless the user has `permission`.
    pub fn require(&self, permission: &str) -> Result<(), AppError> {
        if self.can(permission) {
            Ok(())
        } else {
            tracing::info!(user = %self.id, permission, "permission denied");
            Err(AppError::Forbidden)
        }
    }

    pub fn actor(&self) -> Actor {
        Actor::staff(self.id)
    }
}

impl FromRequestParts<AppState> for Staff {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = match parts.extensions.get::<AuthUser>() {
            Some(u) => *u,
            None => AuthUser::from_request_parts(parts, state).await?,
        };
        if !user.is_staff {
            return Err(AppError::Forbidden);
        }
        let permissions = sqlx::query_scalar!(
            "SELECT DISTINCT rp.permission_code FROM user_roles ur
             JOIN role_permissions rp ON rp.role_id = ur.role_id
             JOIN users u ON u.id = ur.user_id AND u.status = 'active'
             WHERE ur.user_id = $1",
            user.id
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect();
        Ok(Self {
            id: user.id,
            permissions,
        })
    }
}
