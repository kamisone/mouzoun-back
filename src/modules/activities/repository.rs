//! SQL for the `activities` table. Every read joins the cover media to resolve its key.

use sqlx::PgExecutor;
use uuid::Uuid;

use super::model::{ActivityInput, ActivityRow};

/// Selected columns, for a source aliased `a` joined with `media m`.
const COLUMNS: &str = "a.id, a.slug, a.title_fr, a.title_en, a.description_fr, a.description_en, \
                       a.price_mad, a.duration_minutes, a.cover_media_id, \
                       m.object_key AS cover_object_key, a.is_published, a.created_at, a.updated_at";
const FROM: &str = "FROM activities a LEFT JOIN media m ON m.id = a.cover_media_id";

pub async fn list_published(db: impl PgExecutor<'_>) -> sqlx::Result<Vec<ActivityRow>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM} WHERE a.is_published ORDER BY a.created_at DESC"
    ))
    .fetch_all(db)
    .await
}

pub async fn find_published_by_slug(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> sqlx::Result<Option<ActivityRow>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM} WHERE a.slug = $1 AND a.is_published"
    ))
    .bind(slug)
    .fetch_optional(db)
    .await
}

pub async fn find(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<ActivityRow>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} {FROM} WHERE a.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn list(
    db: impl PgExecutor<'_>,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<ActivityRow>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM} ORDER BY a.created_at DESC, a.id DESC LIMIT $1 OFFSET $2"
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
}

pub async fn count(db: impl PgExecutor<'_>) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM activities")
        .fetch_one(db)
        .await
}

pub async fn insert(db: impl PgExecutor<'_>, i: &ActivityInput) -> sqlx::Result<ActivityRow> {
    sqlx::query_as(&format!(
        "WITH a AS (
            INSERT INTO activities (slug, title_fr, title_en, description_fr, description_en,
                                    price_mad, duration_minutes, cover_media_id, is_published)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            RETURNING *
         )
         SELECT {COLUMNS} FROM a LEFT JOIN media m ON m.id = a.cover_media_id"
    ))
    .bind(&i.slug)
    .bind(&i.title_fr)
    .bind(&i.title_en)
    .bind(&i.description_fr)
    .bind(&i.description_en)
    .bind(i.price_mad)
    .bind(i.duration_minutes)
    .bind(i.cover_media_id)
    .bind(i.is_published)
    .fetch_one(db)
    .await
}

pub async fn update(
    db: impl PgExecutor<'_>,
    id: Uuid,
    i: &ActivityInput,
) -> sqlx::Result<Option<ActivityRow>> {
    sqlx::query_as(&format!(
        "WITH a AS (
            UPDATE activities SET slug = $2, title_fr = $3, title_en = $4, description_fr = $5,
                   description_en = $6, price_mad = $7, duration_minutes = $8,
                   cover_media_id = $9, is_published = $10, updated_at = NOW()
            WHERE id = $1
            RETURNING *
         )
         SELECT {COLUMNS} FROM a LEFT JOIN media m ON m.id = a.cover_media_id"
    ))
    .bind(id)
    .bind(&i.slug)
    .bind(&i.title_fr)
    .bind(&i.title_en)
    .bind(&i.description_fr)
    .bind(&i.description_en)
    .bind(i.price_mad)
    .bind(i.duration_minutes)
    .bind(i.cover_media_id)
    .bind(i.is_published)
    .fetch_optional(db)
    .await
}

/// Returns the deleted activity's slug (for cache invalidation).
pub async fn delete(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("DELETE FROM activities WHERE id = $1 RETURNING slug")
        .bind(id)
        .fetch_optional(db)
        .await
}
