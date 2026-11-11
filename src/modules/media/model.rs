use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

use crate::infra::storage::SignedRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "media_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum MediaStatus {
    Pending,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "media_visibility", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum MediaVisibility {
    #[default]
    Public,
    Private,
}

/// Database row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Media {
    pub id: Uuid,
    pub visibility: MediaVisibility,
    pub status: MediaStatus,
    pub object_key: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub original_filename: String,
    pub uploaded_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// API representation. `url` is public for public media, a short-lived signed URL for
/// private media, and absent until the upload is completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDto {
    pub id: Uuid,
    pub status: MediaStatus,
    pub visibility: MediaVisibility,
    pub content_type: String,
    pub size_bytes: i64,
    pub original_filename: String,
    pub url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Validate)]
pub struct CreateUploadRequest {
    #[validate(length(min = 1, max = 255))]
    pub filename: String,
    #[validate(length(min = 3, max = 100))]
    pub content_type: String,
    #[validate(range(min = 1))]
    pub size_bytes: i64,
    #[serde(default)]
    pub visibility: MediaVisibility,
}

#[derive(Debug, Serialize)]
pub struct UploadTicket {
    pub media: MediaDto,
    pub upload: SignedRequest,
}

/// Query filter of the media list (pagination comes from the shared `PageParams`).
#[derive(Debug, Deserialize)]
pub struct MediaFilter {
    /// Defaults to `ready`: pending uploads are an implementation detail.
    #[serde(default)]
    pub status: Option<MediaStatus>,
}
