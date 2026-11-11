//! 0004_taxonomy_places: categories, tags, amenities, places, farm crops.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{Locale, WithTranslations};
use crate::text_enum;

text_enum!(CategoryScope { Product = "product", Post = "post", Faq = "faq", Place = "place" });
text_enum!(AmenityGroup {
    Sanitary = "sanitary",
    WaterPower = "water_power",
    FoodDrink = "food_drink",
    Comfort = "comfort",
    Connectivity = "connectivity",
    Family = "family",
    Safety = "safety",
    Accessibility = "accessibility",
    ParkingTransport = "parking_transport",
    Other = "other",
});
text_enum!(PlaceKind {
    CampZone = "camp_zone",
    Facility = "facility",
    FarmField = "farm_field",
    MeetingPoint = "meeting_point",
    Village = "village",
    Town = "town",
    NaturalSite = "natural_site",
    HeritageSite = "heritage_site",
    Other = "other",
});

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CategoryRecord {
    pub id: Uuid,
    pub scope: CategoryScope,
    pub parent_id: Option<Uuid>,
    pub icon: Option<String>,
    pub cover_media_id: Option<Uuid>,
    pub sort_order: i32,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CategoryTranslation {
    #[serde(skip_serializing)]
    pub category_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
}

pub type Category = WithTranslations<CategoryRecord, CategoryTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TagRecord {
    pub id: Uuid,
    pub code: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TagTranslation {
    #[serde(skip_serializing)]
    pub tag_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub slug: String,
}

pub type Tag = WithTranslations<TagRecord, TagTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AmenityRecord {
    pub id: Uuid,
    pub code: String,
    pub amenity_group: AmenityGroup,
    pub icon: Option<String>,
    pub is_site_wide: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AmenityTranslation {
    #[serde(skip_serializing)]
    pub amenity_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub description: Option<String>,
}

pub type Amenity = WithTranslations<AmenityRecord, AmenityTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PlaceRecord {
    pub id: Uuid,
    pub kind: PlaceKind,
    pub parent_id: Option<Uuid>,
    pub category_id: Option<Uuid>,
    pub latitude: Option<Decimal>,
    pub longitude: Option<Decimal>,
    pub distance_km: Option<Decimal>,
    pub cover_media_id: Option<Uuid>,
    pub gallery_id: Option<Uuid>,
    pub is_published: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PlaceTranslation {
    #[serde(skip_serializing)]
    pub place_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub slug: String,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub directions: Option<String>,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
}

pub type Place = WithTranslations<PlaceRecord, PlaceTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FarmCropRecord {
    pub id: Uuid,
    pub code: String,
    /// Months 1..12.
    pub harvest_months: Vec<i16>,
    pub icon: Option<String>,
    pub cover_media_id: Option<Uuid>,
    pub sort_order: i32,
    pub is_published: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FarmCropTranslation {
    #[serde(skip_serializing)]
    pub farm_crop_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub description: Option<String>,
}

pub type FarmCrop = WithTranslations<FarmCropRecord, FarmCropTranslation>;
