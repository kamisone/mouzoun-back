use chrono::Utc;
use redis::AsyncCommands;
use sqlx::PgPool;
use uuid::Uuid;
use validator::Validate;

use super::{
    model::{
        CreateUploadRequest, MediaDto, MediaFilter, MediaVisibility, PendingUpload, UploadTicket,
    },
    policy,
};
use crate::{
    config::MediaConfig,
    db::media::{MediaAssetRecord, MediaKind},
    error::{AppError, AppResult},
    infra::{
        cache::Cache,
        storage::{ObjectAcl, SharedStorage, UploadConstraints},
    },
    web::pagination::{PageParams, Paginated},
};

const PENDING_INDEX: &str = "media:uploads:pending";

fn ticket_key(id: Uuid) -> String {
    format!("media:upload:{id}")
}

#[derive(Clone)]
pub struct MediaService {
    db: PgPool,
    storage: SharedStorage,
    redis: Cache,
    config: MediaConfig,
}

impl MediaService {
    pub fn new(db: PgPool, storage: SharedStorage, redis: Cache, config: MediaConfig) -> Self {
        Self {
            db,
            storage,
            redis,
            config,
        }
    }

    /// Reaches the bucket (readiness probe; network call to the storage provider).
    pub async fn check_storage(&self) -> bool {
        self.storage.check().await.is_ok()
    }

    /// Step 1: validate the request, reserve a storage key, hand out a signed upload URL (its
    /// signature fixes the object's ACL from the requested visibility) and remember the upload
    /// (Redis) until it is completed or abandoned.
    pub async fn create_upload(
        &self,
        req: CreateUploadRequest,
        uploaded_by: Uuid,
    ) -> AppResult<UploadTicket> {
        req.validate()?;
        let kind = policy::kind_for(&req.content_type).ok_or_else(|| {
            AppError::BadRequest(format!(
                "unsupported content type, allowed: {}",
                policy::allowed_content_types().join(", ")
            ))
        })?;
        if req.size_bytes > self.config.max_upload_bytes {
            return Err(AppError::BadRequest(format!(
                "file too large, maximum is {} bytes",
                self.config.max_upload_bytes
            )));
        }
        let id = Uuid::now_v7();
        let now = Utc::now();
        let object_key = policy::object_key(id, kind, now);
        let (acl, cache_control) = match req.visibility {
            MediaVisibility::Public => (ObjectAcl::PublicRead, policy::PUBLIC_CACHE_CONTROL),
            MediaVisibility::Private => (ObjectAcl::Private, policy::PRIVATE_CACHE_CONTROL),
        };
        let upload = self
            .storage
            .signed_upload(
                &object_key,
                &UploadConstraints {
                    content_type: kind.content_type.to_string(),
                    acl,
                    size_bytes: u64::try_from(req.size_bytes).unwrap_or_default(),
                    cache_control: Some(cache_control.to_string()),
                    ttl: self.config.upload_url_ttl,
                },
            )
            .await?;

        let expires_at = now
            + chrono::Duration::from_std(self.config.pending_ttl)
                .map_err(|e| anyhow::anyhow!(e))?;
        let pending = PendingUpload {
            id,
            visibility: req.visibility,
            object_key,
            content_type: kind.content_type.to_string(),
            size_bytes: req.size_bytes,
            original_filename: policy::sanitize_filename(&req.filename),
            uploaded_by,
            expires_at,
        };
        // The ticket outlives its deadline by a day so the cleanup job can still read the key.
        let ticket_ttl = self.config.pending_ttl.as_secs() + 86_400;
        let mut redis = self.redis.connection();
        redis
            .set_ex::<_, _, ()>(
                self.redis.key(&ticket_key(id)),
                serde_json::to_string(&pending).map_err(anyhow::Error::from)?,
                ticket_ttl,
            )
            .await?;
        redis
            .zadd::<_, _, _, ()>(
                self.redis.key(PENDING_INDEX),
                id.to_string(),
                expires_at.timestamp(),
            )
            .await?;
        Ok(UploadTicket {
            media_id: id,
            expires_at,
            upload,
        })
    }

    /// Step 3: verify what landed in the bucket (size, type, magic bytes), then insert the
    /// `media_assets` row. Idempotent: completing twice returns the same asset.
    pub async fn complete(&self, id: Uuid) -> AppResult<MediaDto> {
        if let Some(asset) = self.find(id).await? {
            return self.to_dto(asset).await;
        }
        let mut redis = self.redis.connection();
        let raw: Option<String> = redis.get(self.redis.key(&ticket_key(id))).await?;
        let pending: PendingUpload = raw
            .and_then(|r| serde_json::from_str(&r).ok())
            .ok_or(AppError::NotFound)?;
        let storage = self.storage.as_ref();
        let meta = storage
            .head(&pending.object_key)
            .await?
            .ok_or_else(|| AppError::BadRequest("file has not been uploaded yet".into()))?;
        let kind = policy::kind_for(&pending.content_type)
            .ok_or_else(|| anyhow::anyhow!("upload {id} has a non-allowed content type"))?;
        let declared_matches = i64::try_from(meta.size).ok() == Some(pending.size_bytes)
            && meta
                .content_type
                .eq_ignore_ascii_case(&pending.content_type);
        let content_matches = declared_matches
            && kind.matches(
                &storage
                    .read_prefix(&pending.object_key, policy::SNIFF_LEN)
                    .await?,
            );
        if !content_matches {
            tracing::warn!(media_id = %id, "upload rejected: content does not match declaration");
            storage.delete(&pending.object_key).await?;
            self.forget(id).await?;
            return Err(AppError::BadRequest(
                "uploaded file does not match the declared type or size".into(),
            ));
        }

        let asset = sqlx::query_as!(
            MediaAssetRecord,
            r#"INSERT INTO media_assets (id, bucket, object_key, original_filename, mime_type, kind, size_bytes,
                                       uploaded_by, visibility)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
               ON CONFLICT (bucket, object_key) DO UPDATE SET updated_at = media_assets.updated_at
               RETURNING id, folder_id, storage_provider, bucket, visibility, object_key, original_filename, mime_type,
                         kind AS "kind: MediaKind", size_bytes, width, height, duration_seconds, checksum_sha256,
                         blurhash, focal_x, focal_y, variants, uploaded_by, created_at, updated_at, deleted_at"#,
            id,
            storage.bucket_name(),
            pending.object_key,
            pending.original_filename,
            pending.content_type,
            MediaKind::from_mime(&pending.content_type).as_str(),
            pending.size_bytes,
            pending.uploaded_by,
            match pending.visibility {
                MediaVisibility::Public => "public",
                MediaVisibility::Private => "private",
            }
        )
        .fetch_one(&self.db)
        .await?;
        self.forget(id).await?;
        self.to_dto(asset).await
    }

    pub async fn get(&self, id: Uuid) -> AppResult<MediaDto> {
        let asset = self.find(id).await?.ok_or(AppError::NotFound)?;
        self.to_dto(asset).await
    }

    pub async fn list(
        &self,
        params: PageParams,
        filter: MediaFilter,
    ) -> AppResult<Paginated<MediaDto>> {
        let kind = filter.kind.map(MediaKind::as_str);
        let (rows, total) = tokio::try_join!(
            sqlx::query_as!(
                MediaAssetRecord,
                r#"SELECT id, folder_id, storage_provider, bucket, visibility, object_key, original_filename, mime_type,
                          kind AS "kind: MediaKind", size_bytes, width, height, duration_seconds, checksum_sha256,
                          blurhash, focal_x, focal_y, variants, uploaded_by, created_at, updated_at, deleted_at
                   FROM media_assets
                   WHERE deleted_at IS NULL AND ($1::text IS NULL OR kind = $1)
                   ORDER BY created_at DESC, id DESC
                   LIMIT $2 OFFSET $3"#,
                kind,
                params.limit(),
                params.offset()
            )
            .fetch_all(&self.db),
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM media_assets WHERE deleted_at IS NULL AND ($1::text IS NULL OR kind = $1)"#,
                kind
            )
            .fetch_one(&self.db),
        )?;
        let mut data = Vec::with_capacity(rows.len());
        for asset in rows {
            data.push(self.to_dto(asset).await?);
        }
        Ok(Paginated::new(data, params, total))
    }

    /// Soft delete (`deleted_at`): the file stays in the bucket and can be restored.
    /// Refused while a published (or scheduled) page shows it: `409 media_in_use` with the pages.
    pub async fn delete(&self, id: Uuid) -> AppResult<()> {
        let mut conn = self.db.acquire().await?;
        if crate::modules::pages::service::used_by_live_page(
            &mut conn,
            crate::modules::pages::model::RefType::Media,
            id,
        )
        .await?
        {
            let usages = crate::modules::pages::service::entity_usages(
                &self.db,
                crate::modules::pages::model::RefType::Media,
                id,
            )
            .await?;
            return Err(AppError::domain(
                axum::http::StatusCode::CONFLICT,
                "media_in_use",
                "this file is shown on a published page: replace it there and publish first",
            )
            .with_details(serde_json::json!({ "usages": usages })));
        }
        drop(conn);
        let deleted = sqlx::query!(
            "UPDATE media_assets SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
            id
        )
        .execute(&self.db)
        .await?
        .rows_affected();
        if deleted == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    /// Deletes the objects of uploads started and never completed (background job).
    pub async fn purge_abandoned(&self, batch: isize) -> AppResult<usize> {
        let mut redis = self.redis.connection();
        let due: Vec<String> = redis
            .zrangebyscore_limit(
                self.redis.key(PENDING_INDEX),
                "-inf",
                Utc::now().timestamp(),
                0,
                batch,
            )
            .await?;
        let mut purged = 0;
        for id in due {
            let Ok(id) = id.parse::<Uuid>() else { continue };
            let raw: Option<String> = redis.get(self.redis.key(&ticket_key(id))).await?;
            if let Some(pending) = raw.and_then(|r| serde_json::from_str::<PendingUpload>(&r).ok())
                && let Err(e) = self.storage.delete(&pending.object_key).await
            {
                // Kept in the index: retried on the next run.
                tracing::warn!(media_id = %id, error = %e, "cleanup: delete failed");
                continue;
            }
            self.forget(id).await?;
            purged += 1;
        }
        Ok(purged)
    }

    /// URL of a `public-read` object (never use it for private files).
    pub fn public_url(&self, object_key: &str) -> String {
        self.storage.public_url(object_key)
    }

    async fn find(&self, id: Uuid) -> AppResult<Option<MediaAssetRecord>> {
        Ok(sqlx::query_as!(
            MediaAssetRecord,
            r#"SELECT id, folder_id, storage_provider, bucket, visibility, object_key, original_filename, mime_type,
                      kind AS "kind: MediaKind", size_bytes, width, height, duration_seconds, checksum_sha256,
                      blurhash, focal_x, focal_y, variants, uploaded_by, created_at, updated_at, deleted_at
               FROM media_assets WHERE id = $1 AND deleted_at IS NULL"#,
            id
        )
        .fetch_optional(&self.db)
        .await?)
    }

    async fn forget(&self, id: Uuid) -> AppResult<()> {
        let mut redis = self.redis.connection();
        redis.del::<_, ()>(self.redis.key(&ticket_key(id))).await?;
        redis
            .zrem::<_, _, ()>(self.redis.key(PENDING_INDEX), id.to_string())
            .await?;
        Ok(())
    }

    /// Public files: their stable URL. Private files: a short-lived signed URL (downloaded
    /// as an attachment under the original name), created for each response.
    async fn to_dto(&self, asset: MediaAssetRecord) -> AppResult<MediaDto> {
        let (visibility, url) = if asset.visibility == "private" {
            (
                MediaVisibility::Private,
                self.storage
                    .signed_download_url(
                        &asset.object_key,
                        self.config.download_url_ttl,
                        Some(&asset.original_filename),
                    )
                    .await?,
            )
        } else {
            (MediaVisibility::Public, self.public_url(&asset.object_key))
        };
        Ok(MediaDto {
            id: asset.id,
            kind: asset.kind,
            visibility,
            mime_type: asset.mime_type,
            size_bytes: asset.size_bytes,
            original_filename: asset.original_filename,
            url,
            width: asset.width,
            height: asset.height,
            created_at: asset.created_at,
        })
    }
}
