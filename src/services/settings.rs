//! Typed reads of the `settings` table.

use sqlx::PgPool;

pub async fn number(pool: &PgPool, key: &str, default: i64) -> sqlx::Result<i64> {
    let value = sqlx::query_scalar!("SELECT value FROM settings WHERE key = $1", key)
        .fetch_optional(pool)
        .await?;
    Ok(value.and_then(|v| v.as_i64()).unwrap_or(default))
}
