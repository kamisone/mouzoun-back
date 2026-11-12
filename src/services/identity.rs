//! Back-office identity: login lookup, profile with roles and permissions, first super admin.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{auth::password, db::identity::UserStatus};

pub struct LoginCandidate {
    pub id: Uuid,
    pub status: UserStatus,
    pub password_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserProfile {
    pub id: Uuid,
    pub email: String,
    pub full_name: String,
    pub is_staff: bool,
    pub roles: Vec<String>,
    pub permissions: Vec<String>,
    pub created_at: DateTime<Utc>,
}

pub async fn find_login(pool: &PgPool, email: &str) -> sqlx::Result<Option<LoginCandidate>> {
    sqlx::query_as!(
        LoginCandidate,
        r#"SELECT id, status AS "status: UserStatus", password_hash
           FROM users WHERE email = $1::text::citext AND deleted_at IS NULL"#,
        email.trim()
    )
    .fetch_optional(pool)
    .await
}

pub async fn record_login(pool: &PgPool, user_id: Uuid, now: DateTime<Utc>) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE users SET last_login_at = $2 WHERE id = $1",
        user_id,
        now
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Active user with their role codes and effective permissions.
pub async fn profile(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Option<UserProfile>> {
    sqlx::query_as!(
        UserProfile,
        r#"SELECT u.id, u.email::text AS "email!", u.full_name, u.is_staff, u.created_at,
                  COALESCE((SELECT array_agg(r.code::text ORDER BY r.code)
                            FROM user_roles ur JOIN roles r ON r.id = ur.role_id
                            WHERE ur.user_id = u.id), '{}') AS "roles!",
                  COALESCE((SELECT array_agg(DISTINCT rp.permission_code ORDER BY rp.permission_code)
                            FROM user_roles ur JOIN role_permissions rp ON rp.role_id = ur.role_id
                            WHERE ur.user_id = u.id), '{}') AS "permissions!"
           FROM users u
           WHERE u.id = $1 AND u.status = 'active' AND u.deleted_at IS NULL"#,
        user_id
    )
    .fetch_optional(pool)
    .await
}

/// Creates the first super admin (staff, active, `super_admin` role) unless one exists.
/// Returns whether a user was created or promoted.
pub async fn ensure_super_admin(
    pool: &PgPool,
    email: &str,
    pass: &str,
    full_name: &str,
) -> anyhow::Result<bool> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM user_roles ur JOIN roles r ON r.id = ur.role_id
                          JOIN users u ON u.id = ur.user_id
                          WHERE r.code = 'super_admin' AND u.deleted_at IS NULL) AS "exists!""#
    )
    .fetch_one(pool)
    .await?;
    if exists {
        return Ok(false);
    }
    upsert_staff(pool, email, pass, full_name, "super_admin").await?;
    if pass.chars().count() < 12 {
        tracing::warn!("ADMIN_PASSWORD is shorter than 12 characters: use a stronger password");
    }
    Ok(true)
}

/// Creates (or reactivates, with a new password) a staff account holding `role`. Returns its id.
pub async fn upsert_staff(
    pool: &PgPool,
    email: &str,
    pass: &str,
    full_name: &str,
    role: &str,
) -> anyhow::Result<Uuid> {
    let hash = password::hash(pass.to_owned())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut tx = pool.begin().await?;
    let user_id = sqlx::query_scalar!(
        "INSERT INTO users (id, email, password_hash, full_name, status, is_staff, email_verified_at)
         VALUES ($1, $2::text::citext, $3, $4, 'active', true, now())
         ON CONFLICT (email) DO UPDATE
            SET password_hash = EXCLUDED.password_hash, status = 'active', is_staff = true, deleted_at = NULL
         RETURNING id",
        Uuid::now_v7(),
        email.trim(),
        hash,
        full_name
    )
    .fetch_one(&mut *tx)
    .await?;
    let granted = sqlx::query!(
        "INSERT INTO user_roles (user_id, role_id)
         SELECT $1, id FROM roles WHERE code = $2::text::citext
         ON CONFLICT DO NOTHING",
        user_id,
        role
    )
    .execute(&mut *tx)
    .await?;
    let has_role = granted.rows_affected() > 0
        || sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM user_roles ur JOIN roles r ON r.id = ur.role_id
                              WHERE ur.user_id = $1 AND r.code = $2::text::citext) AS "x!""#,
            user_id,
            role
        )
        .fetch_one(&mut *tx)
        .await?;
    if !has_role {
        anyhow::bail!("unknown role {role}");
    }
    tx.commit().await?;
    Ok(user_id)
}
