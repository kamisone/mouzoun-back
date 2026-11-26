use axum::http::StatusCode;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    model::{MediaDto, MediaFilter, MediaVisibility, NewUpload},
    policy,
};
use crate::{
    config::MediaConfig,
    db::media::{MediaAssetRecord, MediaKind},
    error::{AppError, AppResult},
    infra::storage::{ObjectAcl, PutOptions, SharedStorage},
    web::pagination::{PageParams, Paginated},
};

#[derive(Clone)]
pub struct MediaService {
    db: PgPool,
    storage: SharedStorage,
    config: MediaConfig,
}

impl MediaService {
    /// The bucket (private documents written by other services: invoices).
    pub fn storage(&self) -> SharedStorage {
        self.storage.clone()
    }

    pub fn new(db: PgPool, storage: SharedStorage, config: MediaConfig) -> Self {
        Self {
            db,
            storage,
            config,
        }
    }

    /// Reaches the bucket (readiness probe; network call to the storage provider).
    pub async fn check_storage(&self) -> bool {
        self.storage.check().await.is_ok()
    }

    /// Checks the file (allowed type, size, magic bytes), writes it to the bucket with the ACL of
    /// its visibility, then inserts the `media_assets` row. The bytes are checked before anything
    /// is stored, so a refused file never reaches the bucket.
    pub async fn upload(&self, file: NewUpload, uploaded_by: Uuid) -> AppResult<MediaDto> {
        let filename = policy::sanitize_filename(&file.filename);
        let size = i64::try_from(file.bytes.len()).unwrap_or(i64::MAX);
        if size == 0 {
            return Err(AppError::BadRequest("the file is empty".into()));
        }
        let declared_mov = file.filename.to_ascii_lowercase().ends_with(".mov");
        if declared_mov || policy::is_quicktime(&file.content_type, &file.bytes) {
            return Err(AppError::domain(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "video_mov",
                "QuickTime (.mov) videos are not accepted: export the video as MP4 (H.264)",
            ));
        }
        let kind = policy::kind_for(&file.content_type).ok_or_else(|| {
            AppError::domain(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_type",
                format!(
                    "unsupported content type, allowed: {}",
                    policy::allowed_content_types().join(", ")
                ),
            )
            .with_details(json!({ "allowed": policy::allowed_content_types() }))
        })?;
        let max = if kind.is_video {
            self.config.max_video_bytes
        } else {
            self.config.max_upload_bytes
        };
        if size > max {
            return Err(AppError::domain(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file_too_large",
                format!("file too large, maximum is {max} bytes"),
            )
            .with_details(json!({ "max_bytes": max, "video": kind.is_video })));
        }
        if !kind.matches(&file.bytes) {
            tracing::warn!(
                declared = kind.content_type,
                "upload rejected: content does not match its type"
            );
            return Err(AppError::BadRequest(
                "the file content does not match its type".into(),
            ));
        }

        let id = Uuid::now_v7();
        let object_key = policy::object_key(id, kind, Utc::now());
        let (acl, cache_control) = match file.visibility {
            MediaVisibility::Public => (ObjectAcl::PublicRead, policy::PUBLIC_CACHE_CONTROL),
            MediaVisibility::Private => (ObjectAcl::Private, policy::PRIVATE_CACHE_CONTROL),
        };
        self.storage
            .put(
                &object_key,
                file.bytes,
                &PutOptions {
                    content_type: kind.content_type.to_string(),
                    acl,
                    cache_control: Some(cache_control.to_string()),
                },
            )
            .await?;

        let inserted = sqlx::query_as!(
            MediaAssetRecord,
            r#"INSERT INTO media_assets (id, bucket, object_key, original_filename, mime_type, kind, size_bytes,
                                       uploaded_by, visibility)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
               RETURNING id, folder_id, storage_provider, bucket, visibility, object_key, original_filename, mime_type,
                         kind AS "kind: MediaKind", size_bytes, width, height, duration_seconds, checksum_sha256,
                         blurhash, focal_x, focal_y, variants, uploaded_by, created_at, updated_at, deleted_at"#,
            id,
            self.storage.bucket_name(),
            object_key,
            filename,
            kind.content_type,
            MediaKind::from_mime(kind.content_type).as_str(),
            size,
            uploaded_by,
            match file.visibility {
                MediaVisibility::Public => "public",
                MediaVisibility::Private => "private",
            }
        )
        .fetch_one(&self.db)
        .await;
        let asset = match inserted {
            Ok(asset) => asset,
            Err(e) => {
                // No row points at the object: remove it rather than leave an orphan.
                if let Err(err) = self.storage.delete(&object_key).await {
                    tracing::warn!(object_key, error = %err, "orphan object not deleted");
                }
                return Err(e.into());
            }
        };
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

    /// URL of a `public-read` object (never use it for private files).
    pub fn public_url(&self, object_key: &str) -> String {
        // Images shipped with the website (`storage_provider = 'site'`, demo content) are
        // root-relative paths served by the front, not bucket objects.
        if object_key.starts_with('/') {
            return object_key.to_string();
        }
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
