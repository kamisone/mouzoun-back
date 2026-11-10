//! SQL for the `media` table. Functions take any executor so they compose in transactions.

use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, Postgres, Transaction};
use uuid::Uuid;

use super::model::{Media, MediaStatus, MediaVisibility};

const COLUMNS: &str = "id, visibility, status, object_key, content_type, size_bytes, \
                       original_filename, uploaded_by, created_at, updated_at, completed_at";

pub struct NewMedia<'a> {
    pub id: Uuid,
    pub visibility: MediaVisibility,
    pub object_key: &'a str,
    pub content_type: &'a str,
    pub size_bytes: i64,
    pub original_filename: &'a str,
    pub uploaded_by: Option<Uuid>,
}

pub async fn insert_pending(db: impl PgExecutor<'_>, m: NewMedia<'_>) -> sqlx::Result<Media> {
    sqlx::query_as::<_, Media>(&format!(
        "INSERT INTO media (id, visibility, status, object_key, content_type, size_bytes,
                            original_filename, uploaded_by)
         VALUES ($1, $2, 'pending', $3, $4, $5, $6, $7)
         RETURNING {COLUMNS}"
    ))
    .bind(m.id)
    .bind(m.visibility)
    .bind(m.object_key)
    .bind(m.content_type)
    .bind(m.size_bytes)
    .bind(m.original_filename)
    .bind(m.uploaded_by)
    .fetch_one(db)
    .await
}

pub async fn find(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<Media>> {
    sqlx::query_as::<_, Media>(&format!("SELECT {COLUMNS} FROM media WHERE id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn find_for_update(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> sqlx::Result<Option<Media>> {
    sqlx::query_as::<_, Media>(&format!(
        "SELECT {COLUMNS} FROM media WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
}

/// Transition pending → ready. Returns `None` if the row is no longer pending (race lost).
pub async fn mark_ready(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<Media>> {
    sqlx::query_as::<_, Media>(&format!(
        "UPDATE media SET status = 'ready', completed_at = NOW(), updated_at = NOW()
         WHERE id = $1 AND status = 'pending'
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn delete(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<u64> {
    Ok(sqlx::query("DELETE FROM media WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?
        .rows_affected())
}

pub async fn list(
    db: impl PgExecutor<'_>,
    status: MediaStatus,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<Media>> {
    sqlx::query_as::<_, Media>(&format!(
        "SELECT {COLUMNS} FROM media WHERE status = $1
         ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3"
    ))
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
}

pub async fn count(db: impl PgExecutor<'_>, status: MediaStatus) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM media WHERE status = $1")
        .bind(status)
        .fetch_one(db)
        .await
}

/// Locks a batch of abandoned uploads. SKIP LOCKED lets several instances run cleanup
/// concurrently without blocking or double-processing.
pub async fn lock_abandoned(
    tx: &mut Transaction<'_, Postgres>,
    created_before: DateTime<Utc>,
    limit: i64,
) -> sqlx::Result<Vec<Media>> {
    sqlx::query_as::<_, Media>(&format!(
        "SELECT {COLUMNS} FROM media
         WHERE status = 'pending' AND created_at < $1
         ORDER BY created_at
         LIMIT $2
         FOR UPDATE SKIP LOCKED"
    ))
    .bind(created_before)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await
}

pub async fn delete_many(tx: &mut Transaction<'_, Postgres>, ids: &[Uuid]) -> sqlx::Result<u64> {
    Ok(sqlx::query("DELETE FROM media WHERE id = ANY($1)")
        .bind(ids)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}
