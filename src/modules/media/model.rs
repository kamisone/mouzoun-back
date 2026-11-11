use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

use crate::{db::media::MediaKind, infra::storage::SignedRequest};

/// Which bucket a file goes to: the public one (served as-is, CDN-cacheable) or the private
/// one (only reachable through short-lived signed URLs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaVisibility {
    #[default]
    Public,
    Private,
}

/// API representation of a `media_assets` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDto {
    pub id: Uuid,
    pub kind: MediaKind,
    pub visibility: MediaVisibility,
    pub mime_type: String,
    pub size_bytes: i64,
    pub original_filename: String,
    /// Public URL, or a short-lived signed URL for private files.
    pub url: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub created_at: DateTime<Utc>,
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

/// An upload in progress. Lives in Redis until `complete` verifies the file and inserts the
/// `media_assets` row (the table never holds half-uploaded files).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUpload {
    pub id: Uuid,
    pub visibility: MediaVisibility,
    pub object_key: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub original_filename: String,
    pub uploaded_by: Uuid,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct UploadTicket {
    /// Id of the future media asset (pass it to `complete`).
    pub media_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub upload: SignedRequest,
}

#[derive(Debug, Deserialize)]
pub struct MediaFilter {
    #[serde(default)]
    pub kind: Option<MediaKind>,
}
