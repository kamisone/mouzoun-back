//! 0003_media: metadata of the files stored in Google Cloud Storage.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{Locale, WithTranslations};
use crate::text_enum;

text_enum!(MediaKind {
    Image = "image",
    Video = "video",
    Audio = "audio",
    Document = "document",
    Other = "other",
});

impl MediaKind {
    pub fn from_mime(mime: &str) -> Self {
        match mime.split('/').next().unwrap_or_default() {
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            _ if mime == "application/pdf" => Self::Document,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MediaFolder {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MediaAssetRecord {
    pub id: Uuid,
    pub folder_id: Option<Uuid>,
    pub storage_provider: String,
    pub bucket: String,
    pub object_key: String,
    pub original_filename: String,
    pub mime_type: String,
    pub kind: MediaKind,
    pub size_bytes: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub duration_seconds: Option<i32>,
    pub checksum_sha256: Option<String>,
    pub blurhash: Option<String>,
    pub focal_x: Option<Decimal>,
    pub focal_y: Option<Decimal>,
    /// `{ "thumb": "...", "md": "...", "lg": "..." }`
    pub variants: serde_json::Value,
    pub uploaded_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MediaAssetTranslation {
    #[serde(skip_serializing)]
    pub media_asset_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub alt_text: Option<String>,
    pub title: Option<String>,
    pub caption: Option<String>,
}

pub type MediaAsset = WithTranslations<MediaAssetRecord, MediaAssetTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Gallery {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct GalleryItem {
    pub gallery_id: Uuid,
    pub media_id: Uuid,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
}
