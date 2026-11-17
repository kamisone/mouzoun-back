use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::media::MediaKind;

/// Who can read a file: anyone at its public URL (`public-read` ACL, CDN-cacheable), or only
/// through short-lived signed URLs (`private` ACL: files tied to a person).
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

/// A file received by `POST /v1/admin/media` (multipart), not yet checked.
#[derive(Debug)]
pub struct NewUpload {
    pub filename: String,
    /// Type announced by the browser; the bytes must match it.
    pub content_type: String,
    pub bytes: bytes::Bytes,
    pub visibility: MediaVisibility,
}

#[derive(Debug, Deserialize)]
pub struct MediaFilter {
    #[serde(default)]
    pub kind: Option<MediaKind>,
}
