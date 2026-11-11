use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;
use validator::Validate;

use super::{
    model::{
        CreateUploadRequest, Media, MediaDto, MediaFilter, MediaStatus, MediaVisibility,
        UploadTicket,
    },
    policy,
    repository::{self, NewMedia},
};
use crate::{
    config::MediaConfig,
    error::{AppError, AppResult},
    infra::storage::{Buckets, ObjectStorage, UploadConstraints},
    web::pagination::{PageParams, Paginated},
};

#[derive(Clone)]
pub struct MediaService {
    db: PgPool,
    buckets: Buckets,
    config: MediaConfig,
}

impl MediaService {
    pub fn new(db: PgPool, buckets: Buckets, config: MediaConfig) -> Self {
        Self {
            db,
            buckets,
            config,
        }
    }

    /// Step 1: validate the request, reserve a storage key and hand out a signed upload URL.
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
        let storage = self.bucket(req.visibility)?;

        let id = Uuid::new_v4();
        let object_key = policy::object_key(id, kind, Utc::now());
        let constraints = UploadConstraints {
            content_type: kind.content_type.to_string(),
            size_bytes: req.size_bytes as u64,
            cache_control: (req.visibility == MediaVisibility::Public)
                .then(|| policy::PUBLIC_CACHE_CONTROL.to_string()),
            ttl: self.config.upload_url_ttl,
        };
        // Sign before inserting: a signing failure leaves nothing behind.
        let upload = storage.signed_upload(&object_key, &constraints).await?;

        let media = repository::insert_pending(
            &self.db,
            NewMedia {
                id,
                visibility: req.visibility,
                object_key: &object_key,
                content_type: kind.content_type,
                size_bytes: req.size_bytes,
                original_filename: &policy::sanitize_filename(&req.filename),
                uploaded_by: Some(uploaded_by),
            },
        )
        .await?;

        Ok(UploadTicket {
            media: self.to_dto(media).await?,
            upload,
        })
    }

    /// Step 3: verify what actually landed in the bucket, then publish it.
    /// Idempotent: completing an already-ready media returns it unchanged.
    pub async fn complete(&self, id: Uuid) -> AppResult<MediaDto> {
        let media = repository::find(&self.db, id)
            .await?
            .ok_or(AppError::NotFound)?;
        if media.status == MediaStatus::Ready {
            return self.to_dto(media).await;
        }

        let storage = self.bucket(media.visibility)?;
        let meta = storage
            .head(&media.object_key)
            .await?
            .ok_or_else(|| AppError::BadRequest("file has not been uploaded yet".into()))?;
        let kind = policy::kind_for(&media.content_type)
            .ok_or_else(|| anyhow::anyhow!("media {id} has a non-allowed content type"))?;

        let declared_matches = meta.size == media.size_bytes as u64
            && meta.content_type.eq_ignore_ascii_case(&media.content_type);
        let content_matches = declared_matches
            && kind.matches(
                &storage
                    .read_prefix(&media.object_key, policy::SNIFF_LEN)
                    .await?,
            );

        if !content_matches {
            tracing::warn!(media_id = %id, "upload rejected: content does not match declaration");
            storage.delete(&media.object_key).await?;
            repository::delete(&self.db, id).await?;
            return Err(AppError::BadRequest(
                "uploaded file does not match the declared type or size".into(),
            ));
        }

        let media = match repository::mark_ready(&self.db, id).await? {
            Some(m) => m,
            // Another request completed it concurrently: return the current state.
            None => repository::find(&self.db, id)
                .await?
                .filter(|m| m.status == MediaStatus::Ready)
                .ok_or(AppError::NotFound)?,
        };
        self.to_dto(media).await
    }

    pub async fn get(&self, id: Uuid) -> AppResult<MediaDto> {
        let media = repository::find(&self.db, id)
            .await?
            .ok_or(AppError::NotFound)?;
        self.to_dto(media).await
    }

    pub async fn list(
        &self,
        params: PageParams,
        filter: MediaFilter,
    ) -> AppResult<Paginated<MediaDto>> {
        let status = filter.status.unwrap_or(MediaStatus::Ready);
        let (rows, total) = tokio::try_join!(
            repository::list(&self.db, status, params.limit(), params.offset()),
            repository::count(&self.db, status),
        )?;
        let mut data = Vec::with_capacity(rows.len());
        for media in rows {
            data.push(self.to_dto(media).await?);
        }
        Ok(Paginated::new(data, params, total))
    }

    /// Deletes the row and the object atomically from the caller's point of view:
    /// the row stays (transaction rolled back) if the object cannot be deleted.
    /// Media still referenced (e.g. an activity cover) is refused with 409.
    pub async fn delete(&self, id: Uuid) -> AppResult<()> {
        let mut tx = self.db.begin().await?;
        let media = repository::find_for_update(&mut tx, id)
            .await?
            .ok_or(AppError::NotFound)?;
        repository::delete(&mut *tx, id)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                    AppError::Conflict("media is in use and cannot be deleted".into())
                }
                _ => e.into(),
            })?;
        self.bucket(media.visibility)?
            .delete(&media.object_key)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes uploads started more than `older_than` ago and never completed.
    pub async fn purge_abandoned(&self, older_than: Duration, batch: i64) -> AppResult<usize> {
        let cutoff =
            Utc::now() - chrono::Duration::from_std(older_than).map_err(|e| anyhow::anyhow!(e))?;
        let mut tx = self.db.begin().await?;
        let abandoned = repository::lock_abandoned(&mut tx, cutoff, batch).await?;
        let mut purged = Vec::with_capacity(abandoned.len());
        for media in abandoned {
            let removed = match self.bucket(media.visibility) {
                Ok(storage) => storage.delete(&media.object_key).await,
                Err(_) => Ok(()), // bucket no longer configured: nothing to delete
            };
            match removed {
                Ok(()) => purged.push(media.id),
                // Keep the row: it will be retried on the next run.
                Err(e) => {
                    tracing::warn!(media_id = %media.id, error = %e, "cleanup: delete failed")
                }
            }
        }
        repository::delete_many(&mut tx, &purged).await?;
        tx.commit().await?;
        Ok(purged.len())
    }

    /// Guards references from other modules (e.g. activity covers).
    pub async fn ensure_public_image(&self, id: Uuid) -> AppResult<()> {
        let usable = repository::find(&self.db, id).await?.is_some_and(|m| {
            m.status == MediaStatus::Ready
                && m.visibility == MediaVisibility::Public
                && policy::kind_for(&m.content_type).is_some_and(|k| k.is_image)
        });
        if usable {
            Ok(())
        } else {
            Err(AppError::BadRequest(
                "media must be a completed public image".into(),
            ))
        }
    }

    /// Public URL for an object key of the public bucket.
    pub fn public_url(&self, object_key: &str) -> String {
        self.buckets.public.public_url(object_key)
    }

    fn bucket(&self, visibility: MediaVisibility) -> AppResult<&dyn ObjectStorage> {
        match visibility {
            MediaVisibility::Public => Ok(self.buckets.public.as_ref()),
            MediaVisibility::Private => self
                .buckets
                .private
                .as_deref()
                .ok_or_else(|| AppError::BadRequest("private storage is not configured".into())),
        }
    }

    async fn to_dto(&self, m: Media) -> AppResult<MediaDto> {
        let url = match (m.status, m.visibility) {
            (MediaStatus::Pending, _) => None,
            (MediaStatus::Ready, MediaVisibility::Public) => Some(self.public_url(&m.object_key)),
            (MediaStatus::Ready, MediaVisibility::Private) => Some(
                self.bucket(m.visibility)?
                    .signed_download_url(
                        &m.object_key,
                        self.config.download_url_ttl,
                        Some(&m.original_filename),
                    )
                    .await?,
            ),
        };
        Ok(MediaDto {
            id: m.id,
            status: m.status,
            visibility: m.visibility,
            content_type: m.content_type,
            size_bytes: m.size_bytes,
            original_filename: m.original_filename,
            url,
            created_at: m.created_at,
            completed_at: m.completed_at,
        })
    }
}
