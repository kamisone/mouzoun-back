//! Localised catalogue reads.
//!
//! - Slug lookup is `WHERE locale = $1 AND slug = $2` (a slug belongs to one language).
//! - Every text field falls back, one by one, to the default locale when missing or empty.
//! - Responses carry the slug in every active locale (`alternates`), for `hreflang` links and
//!   the language switcher.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{
    catalog::{
        BookingMode, Product, ProductKind, ProductRecord, ProductStatus, ProductTranslation,
    },
    types::{Fallback, Locale, WithTranslations},
};

#[derive(Debug, Clone, Serialize)]
pub struct Alternate {
    pub locale: Locale,
    pub slug: String,
}

/// A product in one language, ready for the public site.
#[derive(Debug, Clone, Serialize)]
pub struct LocalizedProduct {
    pub id: Uuid,
    pub kind: ProductKind,
    pub booking_mode: BookingMode,
    pub is_featured: bool,
    pub place_id: Option<Uuid>,
    pub cover_media_id: Option<Uuid>,
    pub gallery_id: Option<Uuid>,
    pub video_media_id: Option<Uuid>,
    /// The language of this response.
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
    /// Slug of this product in every active locale (including this one).
    pub alternates: Vec<Alternate>,
}

/// Published product by its slug in `locale`.
pub async fn product_by_slug(
    pool: &PgPool,
    locale: &Locale,
    slug: &str,
) -> sqlx::Result<Option<LocalizedProduct>> {
    let product_id = sqlx::query_scalar!(
        "SELECT pt.product_id
         FROM product_translations pt
         JOIN products p ON p.id = pt.product_id
         WHERE pt.locale = $1 AND pt.slug = $2 AND p.status = 'published' AND p.deleted_at IS NULL",
        locale.as_str(),
        slug
    )
    .fetch_optional(pool)
    .await?;
    let Some(product_id) = product_id else {
        return Ok(None);
    };
    let Some(product) = load_product(pool, product_id).await? else {
        return Ok(None);
    };
    let default_locale = default_locale(pool).await?;
    Ok(localize(
        product,
        locale,
        &default_locale,
        &active_locales(pool).await?,
    ))
}

/// A product with all its translations (`HashMap<Locale, ProductTranslation>`).
pub async fn load_product(pool: &PgPool, product_id: Uuid) -> sqlx::Result<Option<Product>> {
    let Some(record) = sqlx::query_as!(
        ProductRecord,
        r#"SELECT id, kind AS "kind: ProductKind", sku::text AS "sku", status AS "status: ProductStatus",
                  booking_mode AS "booking_mode: BookingMode", is_featured, place_id, cover_media_id, gallery_id,
                  video_media_id, sort_order, published_at, created_by, updated_by, created_at, updated_at,
                  deleted_at
           FROM products WHERE id = $1"#,
        product_id
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let translations = sqlx::query_as!(
        ProductTranslation,
        r#"SELECT product_id, locale AS "locale: Locale", name, slug, subtitle, short_description, description,
                  highlights, included, not_included, what_to_bring, requirements, important_info, seo_title,
                  seo_description, og_media_id
           FROM product_translations WHERE product_id = $1"#,
        product_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|t| (t.locale.clone(), t))
    .collect();
    Ok(Some(WithTranslations {
        record,
        translations,
    }))
}

/// Merges `locale` over `default` field by field. `None` when neither translation exists.
pub fn localize(
    product: Product,
    locale: &Locale,
    default: &Locale,
    active: &[Locale],
) -> Option<LocalizedProduct> {
    let WithTranslations {
        record,
        mut translations,
    } = product;
    let mut alternates: Vec<Alternate> = active
        .iter()
        .filter_map(|l| {
            translations.get(l).map(|t| Alternate {
                locale: l.clone(),
                slug: t.slug.clone(),
            })
        })
        .collect();
    alternates.dedup_by(|a, b| a.locale == b.locale);

    let fallback = translations.get(default).cloned();
    let primary = translations.remove(locale);
    let t = match (primary, fallback) {
        (Some(p), Some(f)) => merge(p, f),
        (Some(p), None) => p,
        (None, Some(f)) => f,
        (None, None) => return None,
    };
    Some(LocalizedProduct {
        id: record.id,
        kind: record.kind,
        booking_mode: record.booking_mode,
        is_featured: record.is_featured,
        place_id: record.place_id,
        cover_media_id: record.cover_media_id,
        gallery_id: record.gallery_id,
        video_media_id: record.video_media_id,
        locale: locale.clone(),
        name: t.name,
        slug: t.slug,
        subtitle: t.subtitle,
        short_description: t.short_description,
        description: t.description,
        highlights: t.highlights,
        included: t.included,
        not_included: t.not_included,
        what_to_bring: t.what_to_bring,
        requirements: t.requirements,
        important_info: t.important_info,
        seo_title: t.seo_title,
        seo_description: t.seo_description,
        og_media_id: t.og_media_id,
        alternates,
    })
}

/// The slug always stays the requested locale's own (it identifies the page).
fn merge(t: ProductTranslation, d: ProductTranslation) -> ProductTranslation {
    ProductTranslation {
        product_id: t.product_id,
        locale: t.locale,
        name: t.name.or_fallback(d.name),
        slug: t.slug,
        subtitle: t.subtitle.or_fallback(d.subtitle),
        short_description: t.short_description.or_fallback(d.short_description),
        description: t.description.or_fallback(d.description),
        highlights: t.highlights.or_fallback(d.highlights),
        included: t.included.or_fallback(d.included),
        not_included: t.not_included.or_fallback(d.not_included),
        what_to_bring: t.what_to_bring.or_fallback(d.what_to_bring),
        requirements: t.requirements.or_fallback(d.requirements),
        important_info: t.important_info.or_fallback(d.important_info),
        seo_title: t.seo_title.or_fallback(d.seo_title),
        seo_description: t.seo_description.or_fallback(d.seo_description),
        og_media_id: t.og_media_id.or(d.og_media_id),
    }
}

pub async fn default_locale(pool: &PgPool) -> sqlx::Result<Locale> {
    sqlx::query_scalar!(r#"SELECT code AS "code: Locale" FROM locales WHERE is_default"#)
        .fetch_one(pool)
        .await
}

pub async fn active_locales(pool: &PgPool) -> sqlx::Result<Vec<Locale>> {
    Ok(crate::db::foundation::active_locales(pool)
        .await?
        .into_iter()
        .map(|l| l.code)
        .collect())
}
