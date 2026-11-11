//! 0009_content: reviews, pages and their blocks, navigation, FAQs, banners, redirects.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{Locale, TsRange, WithTranslations};
use crate::text_enum;

text_enum!(ReviewSource {
    Website = "website",
    Google = "google",
    Tripadvisor = "tripadvisor",
    BookingCom = "booking_com",
    Airbnb = "airbnb",
    Manual = "manual",
});
text_enum!(ReviewStatus { Pending = "pending", Approved = "approved", Rejected = "rejected", Spam = "spam" });
text_enum!(PageType { Home = "home", Page = "page", Post = "post", Legal = "legal", Landing = "landing" });
text_enum!(PageStatus { Draft = "draft", Scheduled = "scheduled", Published = "published", Archived = "archived" });
text_enum!(NavLinkType {
    Page = "page",
    Product = "product",
    Category = "category",
    Place = "place",
    Url = "url",
    Route = "route",
});
text_enum!(BannerPlacement {
    TopBar = "top_bar",
    Popup = "popup",
    HomeHero = "home_hero",
    BookingFunnel = "booking_funnel",
});

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Review {
    pub id: Uuid,
    pub product_id: Option<Uuid>,
    pub booking_id: Option<Uuid>,
    pub customer_id: Option<Uuid>,
    pub author_name: String,
    pub author_country: Option<String>,
    pub locale: Locale,
    pub rating: i16,
    pub sub_ratings: serde_json::Value,
    pub title: Option<String>,
    pub body: Option<String>,
    pub experience_date: Option<NaiveDate>,
    pub source: ReviewSource,
    pub status: ReviewStatus,
    pub is_featured: bool,
    pub reply_body: Option<String>,
    pub replied_at: Option<DateTime<Utc>>,
    pub replied_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PageRecord {
    pub id: Uuid,
    pub page_type: PageType,
    pub template: String,
    pub parent_id: Option<Uuid>,
    pub category_id: Option<Uuid>,
    pub status: PageStatus,
    pub publish_at: Option<DateTime<Utc>>,
    pub author_id: Option<Uuid>,
    pub cover_media_id: Option<Uuid>,
    pub in_sitemap: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PageTranslation {
    #[serde(skip_serializing)]
    pub page_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub title: String,
    /// May contain `/` (nested paths).
    pub slug: String,
    pub excerpt: Option<String>,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
    pub og_media_id: Option<Uuid>,
    pub canonical_url: Option<String>,
    pub noindex: bool,
}

pub type Page = WithTranslations<PageRecord, PageTranslation>;

/// One block of the page builder, attached to exactly one page, product or place.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ContentBlockRecord {
    pub id: Uuid,
    pub page_id: Option<Uuid>,
    pub product_id: Option<Uuid>,
    pub place_id: Option<Uuid>,
    /// hero, rich_text, image, gallery, video, product_grid, activity_list, cta, faq_list, ...
    pub block_type: String,
    /// Non-translatable configuration.
    pub settings: serde_json::Value,
    pub sort_order: i32,
    pub is_visible: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ContentBlockTranslation {
    #[serde(skip_serializing)]
    pub content_block_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub content: serde_json::Value,
}

pub type ContentBlock = WithTranslations<ContentBlockRecord, ContentBlockTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NavMenu {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NavMenuItemRecord {
    pub id: Uuid,
    pub menu_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub link_type: NavLinkType,
    pub page_id: Option<Uuid>,
    pub product_id: Option<Uuid>,
    pub category_id: Option<Uuid>,
    pub place_id: Option<Uuid>,
    pub url: Option<String>,
    pub route: Option<String>,
    pub icon: Option<String>,
    pub open_in_new_tab: bool,
    pub is_visible: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NavMenuItemTranslation {
    #[serde(skip_serializing)]
    pub nav_menu_item_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub label: String,
}

pub type NavMenuItem = WithTranslations<NavMenuItemRecord, NavMenuItemTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FaqRecord {
    pub id: Uuid,
    pub category_id: Option<Uuid>,
    pub product_id: Option<Uuid>,
    pub sort_order: i32,
    pub is_published: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FaqTranslation {
    #[serde(skip_serializing)]
    pub faq_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub question: String,
    pub answer: String,
}

pub type Faq = WithTranslations<FaqRecord, FaqTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BannerRecord {
    pub id: Uuid,
    pub placement: BannerPlacement,
    pub style: Option<String>,
    pub link_url: Option<String>,
    pub media_id: Option<Uuid>,
    pub active_period: Option<TsRange>,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BannerTranslation {
    #[serde(skip_serializing)]
    pub banner_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub message: String,
    pub cta_label: Option<String>,
}

pub type Banner = WithTranslations<BannerRecord, BannerTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Redirect {
    pub id: Uuid,
    pub from_path: String,
    pub to_path: String,
    pub status_code: i16,
    pub hits: i64,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
