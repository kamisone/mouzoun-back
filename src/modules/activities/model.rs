use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

/// Database row (activity joined with its cover media key).
#[derive(Debug, sqlx::FromRow)]
pub struct ActivityRow {
    pub id: Uuid,
    pub slug: String,
    pub title_fr: String,
    pub title_en: String,
    pub description_fr: String,
    pub description_en: String,
    pub price_mad: i32,
    pub duration_minutes: i32,
    pub cover_media_id: Option<Uuid>,
    pub cover_object_key: Option<String>,
    pub is_published: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// API representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: Uuid,
    pub slug: String,
    pub title_fr: String,
    pub title_en: String,
    pub description_fr: String,
    pub description_en: String,
    pub price_mad: i32,
    pub duration_minutes: i32,
    pub cover_media_id: Option<Uuid>,
    /// Resolved public URL of the cover image (CDN-ready, immutable).
    pub cover_image_url: Option<String>,
    pub is_published: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Validate)]
pub struct ActivityInput {
    #[validate(length(min = 2, max = 120), custom(function = "validate_slug"))]
    pub slug: String,
    #[validate(length(min = 2, max = 200))]
    pub title_fr: String,
    #[validate(length(min = 2, max = 200))]
    pub title_en: String,
    #[validate(length(max = 10000))]
    pub description_fr: String,
    #[validate(length(max = 10000))]
    pub description_en: String,
    #[validate(range(min = 0))]
    pub price_mad: i32,
    #[validate(range(min = 0))]
    pub duration_minutes: i32,
    /// Must reference a completed public image from the media library.
    pub cover_media_id: Option<Uuid>,
    #[serde(default)]
    pub is_published: bool,
}

fn validate_slug(slug: &str) -> Result<(), validator::ValidationError> {
    let valid = slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(validator::ValidationError::new("slug"))
    }
}
