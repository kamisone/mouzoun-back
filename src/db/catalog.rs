//! 0005_catalog: products (everything sellable) and their per-kind detail tables.

use chrono::{DateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{Locale, WithTranslations};
use crate::text_enum;

text_enum!(ProductKind {
    Accommodation = "accommodation",
    Activity = "activity",
    Extra = "extra",
    Package = "package",
});
text_enum!(ProductStatus { Draft = "draft", Published = "published", Archived = "archived" });
text_enum!(BookingMode { Instant = "instant", OnRequest = "on_request", NotBookable = "not_bookable" });
text_enum!(AccommodationKind {
    TentPitch = "tent_pitch",
    CampervanPitch = "campervan_pitch",
    CaravanPitch = "caravan_pitch",
    FurnishedTent = "furnished_tent",
    NomadTent = "nomad_tent",
    Bivouac = "bivouac",
    Bungalow = "bungalow",
    Room = "room",
    DormitoryBed = "dormitory_bed",
    Other = "other",
});
text_enum!(InventoryMode { Unit = "unit", Pool = "pool" });
text_enum!(LocationType { OnSite = "on_site", OffSite = "off_site" });
text_enum!(SchedulingMode { FixedSlots = "fixed_slots", OnRequest = "on_request", Anytime = "anytime" });
text_enum!(Difficulty { Easy = "easy", Moderate = "moderate", Challenging = "challenging", Extreme = "extreme" });
text_enum!(ExtraAppliesTo { Booking = "booking", Accommodation = "accommodation", Activity = "activity", Any = "any" });
text_enum!(PackagePricingMode { Fixed = "fixed", SumOfItems = "sum_of_items", SumMinusDiscount = "sum_minus_discount" });

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProductRecord {
    pub id: Uuid,
    pub kind: ProductKind,
    pub sku: Option<String>,
    pub status: ProductStatus,
    pub booking_mode: BookingMode,
    pub is_featured: bool,
    pub place_id: Option<Uuid>,
    pub cover_media_id: Option<Uuid>,
    pub gallery_id: Option<Uuid>,
    pub video_media_id: Option<Uuid>,
    pub sort_order: i32,
    pub published_at: Option<DateTime<Utc>>,
    pub created_by: Option<Uuid>,
    pub updated_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// `search_vector` is a generated column used in SQL only, never loaded.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProductTranslation {
    #[serde(skip_serializing)]
    pub product_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub slug: String,
    pub subtitle: Option<String>,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub highlights: Vec<String>,
    pub included: Vec<String>,
    pub not_included: Vec<String>,
    pub what_to_bring: Vec<String>,
    pub requirements: Option<String>,
    pub important_info: Option<String>,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
    pub og_media_id: Option<Uuid>,
}

pub type Product = WithTranslations<ProductRecord, ProductTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AccommodationType {
    pub product_id: Uuid,
    pub accommodation_kind: AccommodationKind,
    pub inventory_mode: InventoryMode,
    pub max_guests: i32,
    pub max_adults: Option<i32>,
    pub max_children: Option<i32>,
    pub standard_occupancy: i32,
    pub bed_configuration: serde_json::Value,
    pub surface_m2: Option<Decimal>,
    pub min_nights: i32,
    pub max_nights: Option<i32>,
    pub check_in_from: Option<NaiveTime>,
    pub check_in_until: Option<NaiveTime>,
    pub check_out_until: Option<NaiveTime>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ActivityDetails {
    pub product_id: Uuid,
    pub location_type: LocationType,
    pub scheduling_mode: SchedulingMode,
    pub duration_minutes: Option<i32>,
    pub difficulty: Option<Difficulty>,
    pub min_participants: i32,
    pub max_participants: Option<i32>,
    pub min_age: Option<i16>,
    pub is_guided: bool,
    pub guide_languages: Vec<String>,
    pub meeting_place_id: Option<Uuid>,
    pub booking_cutoff_hours: i32,
    pub pickup_available: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ExtraDetails {
    pub product_id: Uuid,
    pub applies_to: ExtraAppliesTo,
    pub max_quantity: Option<i32>,
    pub requires_date: bool,
    /// `None` = unlimited.
    pub stock_quantity: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PackageDetails {
    pub product_id: Uuid,
    pub duration_days: Option<i32>,
    pub duration_nights: Option<i32>,
    pub pricing_mode: PackagePricingMode,
    pub discount_percent: Decimal,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ItineraryStepRecord {
    pub id: Uuid,
    pub product_id: Uuid,
    pub day_number: i32,
    pub sort_order: i32,
    pub place_id: Option<Uuid>,
    pub duration_minutes: Option<i32>,
    pub media_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ItineraryStepTranslation {
    #[serde(skip_serializing)]
    pub itinerary_step_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub title: String,
    pub description: Option<String>,
}

pub type ItineraryStep = WithTranslations<ItineraryStepRecord, ItineraryStepTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProductCategory {
    pub product_id: Uuid,
    pub category_id: Uuid,
    pub is_primary: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RelatedProduct {
    pub product_id: Uuid,
    pub related_product_id: Uuid,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProductExtra {
    pub product_id: Uuid,
    pub extra_product_id: Uuid,
    pub is_mandatory: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PackageItem {
    pub id: Uuid,
    pub package_product_id: Uuid,
    pub item_product_id: Uuid,
    pub quantity: i32,
    pub day_number: Option<i32>,
    pub is_optional: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
