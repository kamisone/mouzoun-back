//! Admin catalog (`shop.catalog`; reads `shop.read`): shop products with translations, food
//! details, options and values, variants (generated from the option matrix, bulk edited),
//! variant media, duplication, CSV export / import (dry run first).

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    auth::Staff,
    db::shop::{NetUnit, ShopProductType, VariantStatus},
    error::{AppError, AppResult},
    services::{
        audit,
        outbox::{self, Event},
        shop::events::{self, ShopEvent},
    },
    state::AppState,
    web::pagination::{PageParams, Paginated},
};

pub(super) fn conflict(code: &'static str, message: &str) -> AppError {
    AppError::domain(StatusCode::CONFLICT, code, message)
}

pub(super) fn invalid(message: &str) -> AppError {
    AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message)
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.trim().chars().flat_map(char::to_lowercase) {
        let c = match c {
            'à' | 'â' | 'ä' | 'á' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' => 'i',
            'ô' | 'ö' | 'ó' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            c => c,
        };
        if c.is_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

pub(super) async fn changed(conn: &mut PgConnection, product_id: Uuid) -> sqlx::Result<()> {
    outbox::enqueue(conn, &Event::ProductUpdated { product_id }).await?;
    events::enqueue(conn, ShopEvent::VariantUpdated { product_id }).await?;
    Ok(())
}

// ---- List / detail -------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListFilter {
    pub q: Option<String>,
    pub status: Option<String>,
    pub category_id: Option<Uuid>,
    pub product_type: Option<String>,
    pub sort: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    staff: Staff,
    Query(page): Query<PageParams>,
    Query(f): Query<ListFilter>,
) -> AppResult<Json<Paginated<Value>>> {
    staff.require("shop.read")?;
    let q = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let rows = sqlx::query!(
        r#"SELECT p.id, p.status, p.is_featured, p.updated_at, sp.product_type,
                  COALESCE(t.name, '') AS "name!", COALESCE(t.slug, '') AS "slug!", m.object_key AS "image_key?",
                  (SELECT min(price_minor) FROM product_variants v WHERE v.product_id = p.id AND v.deleted_at IS NULL AND v.status = 'active') AS price_min,
                  (SELECT max(price_minor) FROM product_variants v WHERE v.product_id = p.id AND v.deleted_at IS NULL AND v.status = 'active') AS price_max,
                  (SELECT count(*) FROM product_variants v WHERE v.product_id = p.id AND v.deleted_at IS NULL AND v.status = 'active') AS "variants!",
                  (SELECT COALESCE(sum(l.on_hand), 0) FROM inventory_levels l JOIN product_variants v ON v.id = l.variant_id
                   WHERE v.product_id = p.id AND v.deleted_at IS NULL)::bigint AS "stock!",
                  (SELECT array_agg(locale) FROM product_translations WHERE product_id = p.id) AS locales,
                  count(*) OVER () AS "total!"
           FROM products p JOIN shop_products sp ON sp.product_id = p.id
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN media_assets m ON m.id = p.cover_media_id AND m.deleted_at IS NULL
           WHERE p.deleted_at IS NULL
             AND ($1::text IS NULL OR t.name ILIKE '%' || $1 || '%'
                  OR EXISTS (SELECT 1 FROM product_variants v WHERE v.product_id = p.id AND v.sku::text ILIKE '%' || $1 || '%'))
             AND ($2::text IS NULL OR p.status = $2)
             AND ($3::uuid IS NULL OR EXISTS (SELECT 1 FROM product_categories pc WHERE pc.product_id = p.id AND pc.category_id = $3))
             AND ($4::text IS NULL OR sp.product_type = $4)
           ORDER BY CASE WHEN $5 = 'name' THEN t.name END, CASE WHEN $5 = 'updated' THEN p.updated_at END DESC,
                    p.sort_order, t.name
           LIMIT $6 OFFSET $7"#,
        q,
        f.status.as_deref(),
        f.category_id,
        f.product_type.as_deref(),
        f.sort.as_deref().unwrap_or(""),
        page.limit(),
        page.offset()
    )
    .fetch_all(&state.db)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let data = rows
        .into_iter()
        .map(|r| {
            json!({ "id": r.id, "status": r.status, "is_featured": r.is_featured, "updated_at": r.updated_at,
                    "product_type": r.product_type, "name": r.name, "slug": r.slug,
                    "image_url": r.image_key.as_deref().map(|k| state.media.public_url(k)),
                    "price_min_minor": r.price_min, "price_max_minor": r.price_max, "variant_count": r.variants,
                    "stock_total": r.stock, "locales": r.locales })
        })
        .collect();
    Ok(Json(Paginated::new(data, page, total)))
}

pub async fn product_json(conn: &mut PgConnection, state: &AppState, id: Uuid) -> AppResult<Value> {
    let p = sqlx::query!(
        r#"SELECT p.id, p.status, p.is_featured, p.sort_order, p.cover_media_id, p.gallery_id, p.place_id, p.published_at,
                  p.created_at, p.updated_at, sp.product_type, sp.shipping_profile_id, sp.origin_place_id, sp.farm_crop_id,
                  sp.is_perishable, sp.withdrawal_exemption, sp.tax_class_code::text AS "tax_class_code!", sp.bundle_vat_split,
                  sp.track_lots, sp.shelf_life_days, sp.allergens, sp.nutrition, sp.option_count, sp.max_per_order, lower(sp.available_period) AS available_from, upper(sp.available_period) AS available_until,
                  m.object_key AS "cover_key?"
           FROM products p JOIN shop_products sp ON sp.product_id = p.id
           LEFT JOIN media_assets m ON m.id = p.cover_media_id
           WHERE p.id = $1 AND p.deleted_at IS NULL"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let translations: BTreeMap<String, Value> = sqlx::query!(
        "SELECT locale, name, slug, subtitle, short_description, description, seo_title, seo_description FROM product_translations WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|t| (t.locale.clone(), json!({ "name": t.name, "slug": t.slug, "subtitle": t.subtitle, "short_description": t.short_description,
                                         "description": t.description, "seo_title": t.seo_title, "seo_description": t.seo_description })))
    .collect();
    let shop_translations: BTreeMap<String, Value> = sqlx::query!(
        "SELECT locale, ingredients, storage_instructions, usage_tips, origin_story, producer_name, legal_notice FROM shop_product_translations WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|t| (t.locale.clone(), json!({ "ingredients": t.ingredients, "storage_instructions": t.storage_instructions, "usage_tips": t.usage_tips,
                                         "origin_story": t.origin_story, "producer_name": t.producer_name, "legal_notice": t.legal_notice })))
    .collect();
    let options = sqlx::query!(
        r#"SELECT o.id, o.code::text AS "code!", o.sort_order, o.attribute_id, o.display_type,
                  (SELECT code::text FROM product_option_values WHERE id = o.default_value_id) AS default_value,
                  COALESCE((SELECT jsonb_object_agg(locale, name) FROM product_option_translations WHERE option_id = o.id), '{}') AS "names!",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('id', v.id, 'code', v.code::text, 'sort_order', v.sort_order,
                            'swatch_media_id', v.swatch_media_id, 'swatch_color', v.swatch_color,
                            'price_adjustment_minor', v.price_adjustment_minor, 'variation_value_id', v.variation_value_id,
                            'labels', COALESCE((SELECT jsonb_object_agg(locale, label) FROM product_option_value_translations WHERE option_value_id = v.id), '{}'))
                            ORDER BY v.sort_order, v.code) FROM product_option_values v WHERE v.option_id = o.id), '[]') AS "values!"
           FROM product_options o WHERE o.product_id = $1 ORDER BY o.sort_order, o.code"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|o| json!({ "id": o.id, "code": o.code, "sort_order": o.sort_order, "attribute_id": o.attribute_id,
                     "display_type": o.display_type, "default_value": o.default_value, "names": o.names, "values": o.values }))
    .collect::<Vec<_>>();
    let variants = sqlx::query!(
        r#"SELECT v.id, v.sku::text AS "sku!", v.barcode, v.status, v.is_default, v.sort_order, v.price_minor, v.compare_at_price_minor,
                  v.cost_minor, v.currency::text AS "currency!", v.net_quantity, v.net_unit, v.weight_grams, v.length_mm, v.width_mm,
                  v.height_mm, v.requires_shipping, v.is_taxable, v.track_inventory, v.low_stock_threshold, v.cover_media_id,
                  v.country_of_origin::text AS country_of_origin, v.hs_code, variant_reference_price(v.id) AS reference_price_minor,
                  (SELECT min(price_minor) FROM variant_price_history h WHERE h.variant_id = v.id
                     AND COALESCE(h.valid_to, 'infinity') > now() - interval '30 days') AS lowest_30d_minor,
                  COALESCE((SELECT jsonb_object_agg(locale, customs_description) FROM product_variant_translations WHERE variant_id = v.id), '{}') AS "customs_description!",
                  COALESCE((SELECT jsonb_object_agg(o.code::text, ov.code::text) FROM product_variant_option_values pvo
                            JOIN product_options o ON o.id = pvo.option_id JOIN product_option_values ov ON ov.id = pvo.option_value_id
                            WHERE pvo.variant_id = v.id), '{}') AS "options!",
                  COALESCE((SELECT array_agg(media_id ORDER BY sort_order) FROM product_variant_media WHERE variant_id = v.id), '{}') AS "media_ids!",
                  COALESCE((SELECT sum(on_hand) FROM inventory_levels WHERE variant_id = v.id), 0)::bigint AS "on_hand!",
                  COALESCE((SELECT sum(reserved) FROM inventory_levels WHERE variant_id = v.id), 0)::bigint AS "reserved!"
           FROM product_variants v WHERE v.product_id = $1 AND v.deleted_at IS NULL ORDER BY v.status, v.sort_order, v.sku"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|v| json!({ "id": v.id, "sku": v.sku, "barcode": v.barcode, "status": v.status, "is_default": v.is_default,
                     "sort_order": v.sort_order, "price_minor": v.price_minor, "compare_at_price_minor": v.compare_at_price_minor,
                     "cost_minor": v.cost_minor, "currency": v.currency, "net_quantity": v.net_quantity, "net_unit": v.net_unit,
                     "weight_grams": v.weight_grams, "length_mm": v.length_mm, "width_mm": v.width_mm, "height_mm": v.height_mm,
                     "requires_shipping": v.requires_shipping, "is_taxable": v.is_taxable, "track_inventory": v.track_inventory,
                     "low_stock_threshold": v.low_stock_threshold, "cover_media_id": v.cover_media_id, "options": v.options,
                     "media_ids": v.media_ids, "on_hand": v.on_hand, "reserved": v.reserved,
                     "country_of_origin": v.country_of_origin, "hs_code": v.hs_code, "customs_description": v.customs_description,
                     // Omnibus: a compare-at price may not exceed the reference price (lowest of the 30 days before the current price).
                     "reference_price_minor": v.reference_price_minor, "lowest_30d_minor": v.lowest_30d_minor }))
    .collect::<Vec<_>>();
    let ids = |sql_rows: Vec<Uuid>| sql_rows;
    let categories = ids(sqlx::query_scalar!(
        "SELECT category_id FROM product_categories WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *conn)
    .await?);
    let related = ids(sqlx::query_scalar!(
        "SELECT related_product_id FROM related_products WHERE product_id = $1 ORDER BY sort_order",
        id
    )
    .fetch_all(&mut *conn)
    .await?);
    let taxes = ids(sqlx::query_scalar!(
        "SELECT tax_id FROM product_taxes WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *conn)
    .await?);
    let tags = ids(sqlx::query_scalar!(
        "SELECT tag_id FROM product_tags WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *conn)
    .await?);
    let gallery: Vec<Uuid> = match p.gallery_id {
        Some(g) => {
            sqlx::query_scalar!(
                "SELECT media_id FROM gallery_items WHERE gallery_id = $1 ORDER BY sort_order",
                g
            )
            .fetch_all(&mut *conn)
            .await?
        }
        None => vec![],
    };
    Ok(json!({
        "id": p.id, "status": p.status, "is_featured": p.is_featured, "sort_order": p.sort_order,
        "cover_media_id": p.cover_media_id, "cover_url": p.cover_key.as_deref().map(|k| state.media.public_url(k)),
        "gallery_media_ids": gallery, "published_at": p.published_at, "created_at": p.created_at, "updated_at": p.updated_at,
        "translations": translations,
        "shop": {
            "product_type": p.product_type, "shipping_profile_id": p.shipping_profile_id, "origin_place_id": p.origin_place_id,
            "farm_crop_id": p.farm_crop_id, "is_perishable": p.is_perishable, "withdrawal_exemption": p.withdrawal_exemption,
            "tax_class_code": p.tax_class_code, "bundle_vat_split": p.bundle_vat_split,
            "track_lots": p.track_lots, "shelf_life_days": p.shelf_life_days, "allergens": p.allergens, "nutrition": p.nutrition,
            "option_count": p.option_count, "max_per_order": p.max_per_order, "available_from": p.available_from,
            "available_until": p.available_until, "translations": shop_translations,
        },
        "options": options, "variants": variants, "category_ids": categories, "related_ids": related, "tax_ids": taxes, "tag_ids": tags,
    }))
}

pub async fn detail(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    Ok(Json(product_json(&mut conn, &state, id).await?))
}

// ---- Create / update -------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TranslationInput {
    pub name: String,
    pub slug: Option<String>,
    pub subtitle: Option<String>,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShopTranslationInput {
    pub ingredients: Option<String>,
    pub storage_instructions: Option<String>,
    pub usage_tips: Option<String>,
    pub origin_story: Option<String>,
    pub producer_name: Option<String>,
    pub legal_notice: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShopDetailsInput {
    pub product_type: ShopProductType,
    pub shipping_profile_id: Option<Uuid>,
    pub origin_place_id: Option<Uuid>,
    pub farm_crop_id: Option<Uuid>,
    #[serde(default)]
    pub is_perishable: bool,
    /// VAT class (`food_reduced`, `standard`, `zero`).
    #[serde(default = "food_reduced")]
    pub tax_class_code: String,
    /// Mixed-rate bundles: VAT split by share of the price (sum 100).
    #[serde(default)]
    pub bundle_vat_split: Option<Vec<VatShare>>,
    /// Exception to the right of withdrawal (`none`, `perishable`, `unsealed_hygiene`, `personalised`).
    #[serde(default)]
    pub withdrawal_exemption: Option<String>,
    #[serde(default)]
    pub track_lots: bool,
    pub shelf_life_days: Option<i32>,
    #[serde(default)]
    pub allergens: Vec<String>,
    pub nutrition: Option<Value>,
    pub max_per_order: Option<i32>,
    pub available_from: Option<DateTime<Utc>>,
    pub available_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub translations: HashMap<String, ShopTranslationInput>,
}

fn food_reduced() -> String {
    "food_reduced".into()
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct VatShare {
    pub tax_class_code: String,
    pub share_percent: Decimal,
}

const EXEMPTIONS: [&str; 4] = ["none", "perishable", "unsealed_hygiene", "personalised"];

impl ShopDetailsInput {
    /// Perishable food is always exempt from withdrawal (Consumer Code L221-28 4°).
    fn exemption(&self) -> &str {
        if self.is_perishable {
            "perishable"
        } else {
            self.withdrawal_exemption.as_deref().unwrap_or("none")
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewVariant {
    pub sku: String,
    pub price_minor: i64,
    pub weight_grams: i32,
    pub compare_at_price_minor: Option<i64>,
    pub net_quantity: Option<Decimal>,
    pub net_unit: Option<NetUnit>,
    #[serde(default)]
    pub country_of_origin: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProductInput {
    #[serde(default = "draft")]
    pub status: String,
    #[serde(default)]
    pub is_featured: bool,
    #[serde(default)]
    pub sort_order: i32,
    pub cover_media_id: Option<Uuid>,
    pub gallery_media_ids: Option<Vec<Uuid>>,
    pub translations: HashMap<String, TranslationInput>,
    pub shop: ShopDetailsInput,
    #[serde(default)]
    pub category_ids: Vec<Uuid>,
    #[serde(default)]
    pub related_ids: Vec<Uuid>,
    #[serde(default)]
    pub tax_ids: Vec<Uuid>,
    #[serde(default)]
    pub tag_ids: Vec<Uuid>,
    /// Create only: the first (default) variant.
    pub variant: Option<NewVariant>,
}

fn draft() -> String {
    "draft".into()
}

const ALLERGENS: [&str; 14] = [
    "gluten",
    "crustaceans",
    "eggs",
    "fish",
    "peanuts",
    "soybeans",
    "milk",
    "nuts",
    "celery",
    "mustard",
    "sesame",
    "sulphites",
    "lupin",
    "molluscs",
];

async fn validate(conn: &mut PgConnection, p: &ProductInput) -> AppResult<()> {
    if !["draft", "published", "archived"].contains(&p.status.as_str()) {
        return Err(invalid("unknown status"));
    }
    let default_locale = sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
        .fetch_one(&mut *conn)
        .await?;
    if p.translations
        .get(&default_locale)
        .is_none_or(|t| t.name.trim().is_empty())
    {
        return Err(invalid("the name in the default language is required"));
    }
    if let Some(a) = p
        .shop
        .allergens
        .iter()
        .find(|a| !ALLERGENS.contains(&a.as_str()))
    {
        return Err(invalid(&format!("unknown allergen {a}")));
    }
    if !EXEMPTIONS.contains(&p.shop.exemption()) {
        return Err(invalid("unknown withdrawal exemption"));
    }
    let classes = sqlx::query_scalar!(r#"SELECT code::text AS "c!" FROM tax_classes"#)
        .fetch_all(&mut *conn)
        .await?;
    if !classes.contains(&p.shop.tax_class_code) {
        return Err(invalid("unknown tax class"));
    }
    if let Some(split) = &p.shop.bundle_vat_split {
        let sum: Decimal = split.iter().map(|x| x.share_percent).sum();
        if split.is_empty()
            || sum != Decimal::from(100)
            || split
                .iter()
                .any(|x| !classes.contains(&x.tax_class_code) || x.share_percent <= Decimal::ZERO)
        {
            return Err(invalid(
                "the VAT split of a bundle needs known tax classes whose shares add up to 100 %",
            ));
        }
    }
    if let (Some(a), Some(b)) = (p.shop.available_from, p.shop.available_until)
        && b <= a
    {
        return Err(invalid("the season must end after it starts"));
    }
    Ok(())
}

/// Before a product is published (Toubon law, INCO regulation 1169/2011): the French text is
/// complete, and food or drink shows its mandatory information (ingredients, storage, food
/// business operator, net quantity and origin on every active variant).
pub async fn publish_check(conn: &mut PgConnection, id: Uuid) -> AppResult<()> {
    let r = sqlx::query!(
        r#"SELECT sp.product_type,
                  NULLIF(btrim(t.name), '') IS NOT NULL AS "name!", NULLIF(btrim(t.description), '') IS NOT NULL AS "description!",
                  NULLIF(btrim(st.ingredients), '') IS NOT NULL AS "ingredients!",
                  NULLIF(btrim(st.storage_instructions), '') IS NOT NULL AS "storage!",
                  NULLIF(btrim(st.producer_name), '') IS NOT NULL AS "operator!",
                  (SELECT count(*) FROM product_variants v WHERE v.product_id = sp.product_id AND v.status = 'active' AND v.deleted_at IS NULL
                     AND (v.net_quantity IS NULL OR v.net_unit IS NULL OR v.country_of_origin IS NULL)) AS "incomplete_variants!"
           FROM shop_products sp
           LEFT JOIN product_translations t ON t.product_id = sp.product_id AND t.locale = 'fr'
           LEFT JOIN shop_product_translations st ON st.product_id = sp.product_id AND st.locale = 'fr'
           WHERE sp.product_id = $1"#,
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    let mut missing = Vec::new();
    if !r.name {
        missing.push("translations.fr.name");
    }
    if !r.description {
        missing.push("translations.fr.description");
    }
    if matches!(r.product_type.as_str(), "food" | "drink" | "gift_box") {
        if !r.ingredients {
            missing.push("shop.translations.fr.ingredients");
        }
        if !r.storage {
            missing.push("shop.translations.fr.storage_instructions");
        }
        if !r.operator {
            missing.push("shop.translations.fr.producer_name");
        }
        if r.incomplete_variants > 0 {
            missing.push("variants.net_quantity|net_unit|country_of_origin");
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "publish_incomplete",
            format!("required before publishing: {}", missing.join(", ")),
        ))
    }
}

/// Omnibus rule (Consumer Code L112-1-1): a price shown as reduced refers to the lowest price
/// applied during the 30 days before the reduction. Called after the price was written (the
/// history trigger has run): refuses a compare-at price above that reference, or one without
/// any earlier price.
pub async fn check_compare_at(conn: &mut PgConnection, variant_id: Uuid) -> AppResult<()> {
    let r = sqlx::query!(
        "SELECT sku::text AS sku, compare_at_price_minor, variant_reference_price(id) AS reference FROM product_variants WHERE id = $1",
        variant_id
    )
    .fetch_one(conn)
    .await?;
    match (r.compare_at_price_minor, r.reference) {
        (None, _) => Ok(()),
        (Some(c), Some(reference)) if c <= reference => Ok(()),
        (Some(_), reference) => Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "compare_at_above_reference",
            match reference {
                Some(x) => format!(
                    "{}: the compare-at price may not exceed the lowest price of the last 30 days ({x})",
                    r.sku.unwrap_or_default()
                ),
                None => format!(
                    "{}: a reduction needs an earlier price (none in the last 30 days)",
                    r.sku.unwrap_or_default()
                ),
            },
        )),
    }
}

async fn default_profile(conn: &mut PgConnection) -> AppResult<Uuid> {
    sqlx::query_scalar!("SELECT id FROM shipping_profiles ORDER BY is_default DESC, code LIMIT 1")
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| invalid("create a shipping profile first"))
}

/// Writes everything but the variants (create and update share it).
async fn write_product(
    conn: &mut PgConnection,
    id: Uuid,
    p: &ProductInput,
    staff: &Staff,
) -> AppResult<()> {
    let profile = match p.shop.shipping_profile_id {
        Some(x) => x,
        None => default_profile(conn).await?,
    };
    let gallery_id = match &p.gallery_media_ids {
        Some(media) => {
            let g = match sqlx::query_scalar!("SELECT gallery_id FROM products WHERE id = $1", id)
                .fetch_one(&mut *conn)
                .await?
            {
                Some(g) => g,
                None => {
                    let g = Uuid::now_v7();
                    sqlx::query!(
                        "INSERT INTO galleries (id, name) VALUES ($1, $2)",
                        g,
                        format!("shop product {id}")
                    )
                    .execute(&mut *conn)
                    .await?;
                    g
                }
            };
            sqlx::query!("DELETE FROM gallery_items WHERE gallery_id = $1", g)
                .execute(&mut *conn)
                .await?;
            for (i, m) in media.iter().enumerate() {
                sqlx::query!(
                    "INSERT INTO gallery_items (gallery_id, media_id, sort_order) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                    g,
                    m,
                    i as i32
                )
                .execute(&mut *conn)
                .await?;
            }
            Some(g)
        }
        None => None,
    };
    sqlx::query!(
        "UPDATE products SET status = $2, booking_mode = 'not_bookable', is_featured = $3, sort_order = $4, cover_media_id = $5,
                gallery_id = COALESCE($6, gallery_id), updated_by = $7,
                published_at = CASE WHEN $2 = 'published' THEN COALESCE(published_at, now()) ELSE published_at END
         WHERE id = $1",
        id,
        p.status,
        p.is_featured,
        p.sort_order,
        p.cover_media_id,
        gallery_id,
        staff.id
    )
    .execute(&mut *conn)
    .await?;
    let period = match (p.shop.available_from, p.shop.available_until) {
        (None, None) => None,
        (a, b) => Some(crate::db::types::TsRange { start: a, end: b }),
    };
    sqlx::query!(
        "UPDATE shop_products SET product_type = $2, shipping_profile_id = $3, origin_place_id = $4, farm_crop_id = $5,
                is_perishable = $6, withdrawal_exemption = $7, track_lots = $8, shelf_life_days = $9, allergens = $10, nutrition = $11,
                max_per_order = $12, available_period = $13, tax_class_code = $14::text::citext, bundle_vat_split = $15
         WHERE product_id = $1",
        id,
        p.shop.product_type.as_str(),
        profile,
        p.shop.origin_place_id,
        p.shop.farm_crop_id,
        p.shop.is_perishable,
        p.shop.exemption(),
        p.shop.track_lots,
        p.shop.shelf_life_days,
        &p.shop.allergens,
        p.shop.nutrition,
        p.shop.max_per_order,
        period as Option<crate::db::types::TsRange>,
        p.shop.tax_class_code,
        p.shop.bundle_vat_split.as_ref().map(|s| serde_json::to_value(s).unwrap_or_default())
    )
    .execute(&mut *conn)
    .await?;
    for (locale, t) in &p.translations {
        if t.name.trim().is_empty() {
            sqlx::query!(
                "DELETE FROM product_translations WHERE product_id = $1 AND locale = $2",
                id,
                locale
            )
            .execute(&mut *conn)
            .await?;
            continue;
        }
        let slug = t
            .slug
            .as_deref()
            .map(slugify)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| slugify(&t.name));
        let res = sqlx::query!(
            "INSERT INTO product_translations (product_id, locale, name, slug, subtitle, short_description, description, seo_title, seo_description)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (product_id, locale) DO UPDATE SET name = $3, slug = $4, subtitle = $5, short_description = $6,
                 description = $7, seo_title = $8, seo_description = $9",
            id,
            locale,
            t.name.trim(),
            slug,
            t.subtitle.as_deref(),
            t.short_description.as_deref(),
            t.description.as_deref(),
            t.seo_title.as_deref(),
            t.seo_description.as_deref()
        )
        .execute(&mut *conn)
        .await;
        if let Err(e) = res {
            if e.as_database_error().and_then(|d| d.constraint())
                == Some("product_translations_locale_slug_key")
            {
                return Err(conflict(
                    "slug_taken",
                    &format!("the address \"{slug}\" ({locale}) is already used"),
                ));
            }
            return Err(e.into());
        }
    }
    for (locale, t) in &p.shop.translations {
        sqlx::query!(
            "INSERT INTO shop_product_translations (product_id, locale, ingredients, storage_instructions, usage_tips, origin_story,
                                                    producer_name, legal_notice)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (product_id, locale) DO UPDATE SET ingredients = $3, storage_instructions = $4, usage_tips = $5,
                 origin_story = $6, producer_name = $7, legal_notice = $8",
            id,
            locale,
            t.ingredients.as_deref(),
            t.storage_instructions.as_deref(),
            t.usage_tips.as_deref(),
            t.origin_story.as_deref(),
            t.producer_name.as_deref(),
            t.legal_notice.as_deref()
        )
        .execute(&mut *conn)
        .await?;
    }
    let bad_category = sqlx::query_scalar!(
        "SELECT count(*) AS \"n!\" FROM categories WHERE id = ANY($1) AND scope NOT IN ('shop', 'product')",
        &p.category_ids
    )
    .fetch_one(&mut *conn)
    .await?;
    if bad_category > 0 {
        return Err(invalid("only shop categories can be used"));
    }
    sqlx::query!("DELETE FROM product_categories WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for c in &p.category_ids {
        sqlx::query!("INSERT INTO product_categories (product_id, category_id) VALUES ($1, $2) ON CONFLICT DO NOTHING", id, c)
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query!("DELETE FROM related_products WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for (i, r) in p.related_ids.iter().filter(|r| **r != id).enumerate() {
        sqlx::query!(
            "INSERT INTO related_products (product_id, related_product_id, sort_order) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            id,
            r,
            i as i32
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!("DELETE FROM product_taxes WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for t in &p.tax_ids {
        sqlx::query!(
            "INSERT INTO product_taxes (product_id, tax_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            id,
            t
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!("DELETE FROM product_tags WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for t in &p.tag_ids {
        sqlx::query!(
            "INSERT INTO product_tags (product_id, tag_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            id,
            t
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub async fn create(
    State(state): State<AppState>,
    staff: Staff,
    Json(p): Json<ProductInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    validate(&mut tx, &p).await?;
    let v = p
        .variant
        .clone()
        .ok_or_else(|| invalid("a shop product needs its first variant (SKU, price, weight)"))?;
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO products (id, kind, status, booking_mode, created_by) VALUES ($1, 'shop_product', 'draft', 'not_bookable', $2)",
        id,
        staff.id
    )
    .execute(&mut *tx)
    .await?;
    let profile = match p.shop.shipping_profile_id {
        Some(x) => x,
        None => default_profile(&mut tx).await?,
    };
    sqlx::query!(
        "INSERT INTO shop_products (product_id, product_type, shipping_profile_id) VALUES ($1, $2, $3)",
        id,
        p.shop.product_type.as_str(),
        profile
    )
    .execute(&mut *tx)
    .await?;
    write_product(&mut tx, id, &p, &staff).await?;
    insert_variant(&mut tx, id, &v, true, &HashMap::new()).await?;
    if p.status == "published" {
        publish_check(&mut tx, id).await?;
    }
    let after = product_json(&mut tx, &state, id).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "shop_product.created",
        "product",
        &id.to_string(),
        json!({ "after": after }),
    )
    .await?;
    changed(&mut tx, id).await?;
    let out = product_json(&mut tx, &state, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(p): Json<ProductInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let before = product_json(&mut tx, &state, id).await?;
    validate(&mut tx, &p).await?;
    if p.status == "published" {
        let active = sqlx::query_scalar!(
            "SELECT count(*) AS \"n!\" FROM product_variants WHERE product_id = $1 AND status = 'active' AND deleted_at IS NULL",
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if active == 0 {
            return Err(conflict(
                "no_active_variant",
                "publish needs at least one active variant",
            ));
        }
    }
    write_product(&mut tx, id, &p, &staff).await?;
    if p.status == "published" {
        publish_check(&mut tx, id).await?;
    }
    let after = product_json(&mut tx, &state, id).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "shop_product.updated",
        "product",
        &id.to_string(),
        json!({ "before": before, "after": after }),
    )
    .await?;
    changed(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(after))
}

/// Soft delete (archived and hidden); refused while open orders reference it.
pub async fn delete(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let open = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM order_items oi JOIN orders o ON o.id = oi.order_id
                        WHERE oi.product_id = $1 AND o.status IN ('draft', 'pending', 'open')) AS \"x!\"",
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if open {
        return Err(conflict(
            "product_in_open_orders",
            "orders in progress contain this product: archive it instead",
        ));
    }
    let n = sqlx::query!(
        "UPDATE products SET deleted_at = now(), status = 'archived' WHERE id = $1 AND kind = 'shop_product' AND deleted_at IS NULL",
        id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "shop_product.deleted",
        "product",
        &id.to_string(),
        json!({}),
    )
    .await?;
    changed(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct BulkAction {
    pub ids: Vec<Uuid>,
    /// publish | archive | draft | set_category
    pub action: String,
    pub category_id: Option<Uuid>,
}

pub async fn bulk(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<BulkAction>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let mut done = 0u64;
    for id in &b.ids {
        let n = match b.action.as_str() {
            "publish" => sqlx::query!(
                "UPDATE products SET status = 'published', published_at = COALESCE(published_at, now())
                 WHERE id = $1 AND kind = 'shop_product' AND deleted_at IS NULL
                   AND EXISTS (SELECT 1 FROM product_variants v WHERE v.product_id = $1 AND v.status = 'active' AND v.deleted_at IS NULL)",
                id
            )
            .execute(&mut *tx)
            .await?
            .rows_affected(),
            "archive" | "draft" => sqlx::query!(
                "UPDATE products SET status = $2 WHERE id = $1 AND kind = 'shop_product' AND deleted_at IS NULL",
                id,
                if b.action == "archive" { "archived" } else { "draft" }
            )
            .execute(&mut *tx)
            .await?
            .rows_affected(),
            "set_category" => {
                let c = b.category_id.ok_or_else(|| invalid("category_id required"))?;
                sqlx::query!("INSERT INTO product_categories (product_id, category_id) VALUES ($1, $2) ON CONFLICT DO NOTHING", id, c)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
            }
            _ => return Err(invalid("unknown action")),
        };
        if n > 0 {
            done += 1;
            changed(&mut tx, *id).await?;
        }
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "shop_product.bulk",
        "product",
        "bulk",
        json!({ "action": b.action, "ids": b.ids }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "updated": done })))
}

// ---- Options and variants -----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct OptionValueInput {
    /// Library value (`variation_values.id`) for an option linked to a library attribute: its
    /// code, labels, colour and price adjustment come from the library.
    pub variation_value_id: Option<Uuid>,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Per-product picture (also for library values: the red of a scarf is not the red of a bag).
    pub swatch_media_id: Option<Uuid>,
    pub swatch_color: Option<String>,
    pub price_adjustment_minor: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct OptionInput {
    /// Library attribute (`variation_attributes.id`); without it the option is product-specific.
    pub attribute_id: Option<Uuid>,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub names: HashMap<String, String>,
    pub display_type: Option<DisplayType>,
    /// Code of the value preselected on the product page (decides the default variant).
    pub default_value: Option<String>,
    pub values: Vec<OptionValueInput>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisplayType {
    Button,
    Swatch,
    Dropdown,
}

impl DisplayType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Swatch => "swatch",
            Self::Dropdown => "dropdown",
        }
    }
}

pub fn valid_color(c: &Option<String>) -> AppResult<Option<String>> {
    match c.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(c)
            if c.len() == 7
                && c.starts_with('#')
                && c[1..].chars().all(|x| x.is_ascii_hexdigit()) =>
        {
            Ok(Some(c.to_ascii_lowercase()))
        }
        Some(_) => Err(invalid("a swatch colour is written #rrggbb")),
    }
}

/// An option as written: custom ones as sent, library ones resolved from the library.
struct NormOption {
    attribute_id: Option<Uuid>,
    code: String,
    names: HashMap<String, String>,
    display_type: &'static str,
    default_value: Option<String>,
    values: Vec<NormValue>,
}

struct NormValue {
    variation_value_id: Option<Uuid>,
    code: String,
    labels: HashMap<String, String>,
    swatch_media_id: Option<Uuid>,
    swatch_color: Option<String>,
    price_adjustment_minor: Option<i64>,
}

async fn normalize_option(
    conn: &mut PgConnection,
    product_id: Uuid,
    o: &OptionInput,
) -> AppResult<NormOption> {
    let Some(attribute_id) = o.attribute_id else {
        let mut values = Vec::new();
        for v in &o.values {
            values.push(NormValue {
                variation_value_id: None,
                code: slugify(&v.code).replace('-', "_"),
                labels: v.labels.clone(),
                swatch_media_id: v.swatch_media_id,
                swatch_color: valid_color(&v.swatch_color)?,
                price_adjustment_minor: v.price_adjustment_minor.filter(|a| *a != 0),
            });
        }
        return Ok(NormOption {
            attribute_id: None,
            code: slugify(&o.code).replace('-', "_"),
            names: o.names.clone(),
            display_type: o.display_type.unwrap_or(DisplayType::Button).as_str(),
            default_value: o.default_value.clone(),
            values,
        });
    };
    // An inactive attribute stays usable on the products already linked to it.
    let a = sqlx::query!(
        r#"SELECT a.code::text AS "code!", a.display_type, a.is_active,
                  EXISTS (SELECT 1 FROM product_options WHERE product_id = $2 AND attribute_id = a.id) AS "linked!"
           FROM variation_attributes a WHERE a.id = $1"#,
        attribute_id,
        product_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| invalid("unknown variation"))?;
    if !a.is_active && !a.linked {
        return Err(invalid("this variation is inactive"));
    }
    let mut values = Vec::new();
    for v in &o.values {
        let id = v
            .variation_value_id
            .ok_or_else(|| invalid("pick the values of a library variation from the library"))?;
        let lib = sqlx::query!(
            r#"SELECT code::text AS "code!", swatch_color, price_adjustment_minor FROM variation_values WHERE id = $1 AND attribute_id = $2"#,
            id,
            attribute_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| invalid("this value does not belong to the variation"))?;
        values.push(NormValue {
            variation_value_id: Some(id),
            code: lib.code,
            // Names and labels are copied by `sync_linked` after the write.
            labels: HashMap::new(),
            swatch_media_id: v.swatch_media_id,
            swatch_color: lib.swatch_color,
            price_adjustment_minor: lib.price_adjustment_minor,
        });
    }
    Ok(NormOption {
        attribute_id: Some(attribute_id),
        code: a.code,
        names: HashMap::new(),
        display_type: match a.display_type.as_str() {
            "swatch" => "swatch",
            "dropdown" => "dropdown",
            _ => "button",
        },
        default_value: o.default_value.clone(),
        values,
    })
}

/// Copies the library's names, labels, display type, colours and price adjustments onto every
/// product option linked to `attribute_id` (called after each library edit).
pub async fn sync_linked(conn: &mut PgConnection, attribute_id: Uuid) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query!(
        "UPDATE product_options o SET display_type = a.display_type FROM variation_attributes a WHERE a.id = $1 AND o.attribute_id = a.id",
        attribute_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM product_option_translations t USING product_options o WHERE t.option_id = o.id AND o.attribute_id = $1",
        attribute_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO product_option_translations (option_id, locale, name)
         SELECT o.id, t.locale, t.name FROM product_options o JOIN variation_attribute_translations t ON t.attribute_id = o.attribute_id
         WHERE o.attribute_id = $1",
        attribute_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE product_option_values pv SET swatch_color = vv.swatch_color, price_adjustment_minor = vv.price_adjustment_minor
         FROM variation_values vv WHERE pv.variation_value_id = vv.id AND vv.attribute_id = $1",
        attribute_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM product_option_value_translations t USING product_option_values pv, variation_values vv
         WHERE t.option_value_id = pv.id AND pv.variation_value_id = vv.id AND vv.attribute_id = $1",
        attribute_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO product_option_value_translations (option_value_id, locale, label)
         SELECT pv.id, t.locale, t.label FROM product_option_values pv
         JOIN variation_values vv ON vv.id = pv.variation_value_id JOIN variation_value_translations t ON t.value_id = vv.id
         WHERE vv.attribute_id = $1",
        attribute_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query_scalar!(
        "SELECT DISTINCT product_id FROM product_options WHERE attribute_id = $1",
        attribute_id
    )
    .fetch_all(&mut *conn)
    .await
}

/// Replaces the product's options (max 3): library variations (`attribute_id` + chosen values)
/// or product-specific ones. Values removed lose their variants' link; regenerate the variants
/// afterwards.
pub async fn set_options(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(input): Json<Vec<OptionInput>>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    if input.len() > 3 {
        return Err(invalid("at most 3 options"));
    }
    let mut tx = state.db.begin().await?;
    let mut options = Vec::new();
    for o in &input {
        options.push(normalize_option(&mut tx, id, o).await?);
    }
    let mut seen = HashSet::new();
    let mut linked = HashSet::new();
    for o in &options {
        if o.code.is_empty() || !seen.insert(o.code.clone()) {
            return Err(invalid("option codes must be unique and not empty"));
        }
        if o.attribute_id.is_some_and(|a| !linked.insert(a)) {
            return Err(invalid("a variation is linked only once"));
        }
        if o.values.is_empty() {
            return Err(invalid("an option needs at least one value"));
        }
        let mut codes = HashSet::new();
        if o.values
            .iter()
            .any(|v| v.code.is_empty() || !codes.insert(v.code.clone()))
        {
            return Err(invalid("value codes must be unique and not empty"));
        }
        if o.default_value.as_ref().is_some_and(|d| !codes.contains(d)) {
            return Err(invalid(
                "the default value must be one of the option's values",
            ));
        }
    }
    let before = product_json(&mut tx, &state, id).await?;
    let keep_codes: Vec<String> = options.iter().map(|o| o.code.clone()).collect();
    sqlx::query!(
        "DELETE FROM product_options WHERE product_id = $1 AND NOT (code::text = ANY($2))",
        id,
        &keep_codes
    )
    .execute(&mut *tx)
    .await?;
    for (i, o) in options.iter().enumerate() {
        let option_id = sqlx::query_scalar!(
            "INSERT INTO product_options (id, product_id, code, sort_order, attribute_id, display_type) VALUES ($1, $2, $3::text::citext, $4, $5, $6)
             ON CONFLICT (product_id, code) DO UPDATE SET sort_order = $4, attribute_id = $5, display_type = $6, default_value_id = NULL RETURNING id",
            Uuid::now_v7(),
            id,
            o.code,
            i as i32,
            o.attribute_id,
            o.display_type
        )
        .fetch_one(&mut *tx)
        .await?;
        if o.attribute_id.is_none() {
            // A custom option keeps only the names sent (a former library link loses its copies).
            sqlx::query!(
                "DELETE FROM product_option_translations WHERE option_id = $1",
                option_id
            )
            .execute(&mut *tx)
            .await?;
        }
        for (locale, name) in o.names.iter().filter(|(_, n)| !n.trim().is_empty()) {
            sqlx::query!(
                "INSERT INTO product_option_translations (option_id, locale, name) VALUES ($1, $2, $3)
                 ON CONFLICT (option_id, locale) DO UPDATE SET name = $3",
                option_id,
                locale,
                name.trim()
            )
            .execute(&mut *tx)
            .await?;
        }
        let value_codes: Vec<String> = o.values.iter().map(|v| v.code.clone()).collect();
        sqlx::query!(
            "DELETE FROM product_option_values WHERE option_id = $1 AND NOT (code::text = ANY($2))",
            option_id,
            &value_codes
        )
        .execute(&mut *tx)
        .await?;
        for (j, v) in o.values.iter().enumerate() {
            let vid = sqlx::query_scalar!(
                "INSERT INTO product_option_values (id, option_id, code, sort_order, swatch_media_id, swatch_color, price_adjustment_minor, variation_value_id)
                 VALUES ($1, $2, $3::text::citext, $4, $5, $6, $7, $8)
                 ON CONFLICT (option_id, code) DO UPDATE SET sort_order = $4, swatch_media_id = $5, swatch_color = $6,
                     price_adjustment_minor = $7, variation_value_id = $8 RETURNING id",
                Uuid::now_v7(),
                option_id,
                v.code,
                j as i32,
                v.swatch_media_id,
                v.swatch_color,
                v.price_adjustment_minor,
                v.variation_value_id
            )
            .fetch_one(&mut *tx)
            .await?;
            if v.variation_value_id.is_none() {
                sqlx::query!(
                    "DELETE FROM product_option_value_translations WHERE option_value_id = $1",
                    vid
                )
                .execute(&mut *tx)
                .await?;
            }
            for (locale, label) in v.labels.iter().filter(|(_, l)| !l.trim().is_empty()) {
                sqlx::query!(
                    "INSERT INTO product_option_value_translations (option_value_id, locale, label) VALUES ($1, $2, $3)
                     ON CONFLICT (option_value_id, locale) DO UPDATE SET label = $3",
                    vid,
                    locale,
                    label.trim()
                )
                .execute(&mut *tx)
                .await?;
            }
            if o.default_value.as_deref() == Some(v.code.as_str()) {
                sqlx::query!(
                    "UPDATE product_options SET default_value_id = $2 WHERE id = $1",
                    option_id,
                    vid
                )
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    for a in &linked {
        sync_linked(&mut tx, *a).await?;
    }
    sqlx::query!(
        "UPDATE shop_products SET option_count = $2 WHERE product_id = $1",
        id,
        options.len() as i16
    )
    .execute(&mut *tx)
    .await?;
    let after = product_json(&mut tx, &state, id).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "shop_product.options_set",
        "product",
        &id.to_string(),
        json!({ "before": before["options"], "after": after["options"] }),
    )
    .await?;
    changed(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(after))
}

/// `{option_code: value_code}` → `(option_id, value_id)` pairs; every option must have a value.
async fn resolve_combination(
    conn: &mut PgConnection,
    product_id: Uuid,
    values: &HashMap<String, String>,
) -> AppResult<Vec<(Uuid, Uuid)>> {
    let options = sqlx::query!(
        "SELECT o.id, o.code::text AS \"code!\" FROM product_options o WHERE o.product_id = $1",
        product_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for o in options {
        let code = values
            .get(&o.code)
            .ok_or_else(|| invalid(&format!("the variant needs a value for {}", o.code)))?;
        let vid = sqlx::query_scalar!(
            "SELECT id FROM product_option_values WHERE option_id = $1 AND code = $2::text::citext",
            o.id,
            code
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| invalid(&format!("unknown value {code} for {}", o.code)))?;
        out.push((o.id, vid));
    }
    Ok(out)
}

async fn combination_taken(
    conn: &mut PgConnection,
    product_id: Uuid,
    combo: &[(Uuid, Uuid)],
    except: Option<Uuid>,
) -> sqlx::Result<bool> {
    let values: Vec<Uuid> = combo.iter().map(|c| c.1).collect();
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM product_variants v
               WHERE v.product_id = $1 AND v.status = 'active' AND v.deleted_at IS NULL AND ($3::uuid IS NULL OR v.id <> $3)
                 AND (SELECT COALESCE(array_agg(option_value_id ORDER BY option_value_id), '{}') FROM product_variant_option_values WHERE variant_id = v.id)
                     = (SELECT COALESCE(array_agg(x ORDER BY x), '{}') FROM unnest($2::uuid[]) x)) AS "x!""#,
        product_id,
        &values,
        except
    )
    .fetch_one(conn)
    .await
}

async fn insert_variant(
    conn: &mut PgConnection,
    product_id: Uuid,
    v: &NewVariant,
    is_default: bool,
    values: &HashMap<String, String>,
) -> AppResult<Uuid> {
    if v.price_minor < 0 || v.weight_grams <= 0 || v.sku.trim().is_empty() {
        return Err(invalid("SKU, a price ≥ 0 and a weight > 0 are required"));
    }
    if v.compare_at_price_minor.is_some_and(|c| c <= v.price_minor) {
        return Err(invalid(
            "the compare-at price must be higher than the price",
        ));
    }
    let combo = resolve_combination(conn, product_id, values).await?;
    if combination_taken(conn, product_id, &combo, None).await? {
        return Err(conflict(
            "duplicate_combination",
            "another active variant has these options",
        ));
    }
    let id = Uuid::now_v7();
    let res = sqlx::query!(
        "INSERT INTO product_variants (id, product_id, sku, price_minor, compare_at_price_minor, weight_grams, net_quantity, net_unit, is_default)
         VALUES ($1, $2, $3::text::citext, $4, $5, $6, $7, $8, $9)",
        id,
        product_id,
        v.sku.trim().to_uppercase(),
        v.price_minor,
        v.compare_at_price_minor,
        v.weight_grams,
        v.net_quantity,
        v.net_unit.map(|u| u.as_str()),
        is_default
    )
    .execute(&mut *conn)
    .await;
    if let Err(e) = res {
        if e.as_database_error()
            .is_some_and(|d| d.is_unique_violation())
        {
            return Err(conflict("sku_taken", "this SKU is already used"));
        }
        return Err(e.into());
    }
    for (o, val) in combo {
        sqlx::query!(
            "INSERT INTO product_variant_option_values (variant_id, option_id, option_value_id) VALUES ($1, $2, $3)",
            id,
            o,
            val
        )
        .execute(&mut *conn)
        .await?;
    }
    let origin = match v
        .country_of_origin
        .as_deref()
        .map(|c| c.trim().to_ascii_uppercase())
    {
        Some(c) if c.len() == 2 && c.chars().all(|x| x.is_ascii_uppercase()) => Some(c),
        Some(c) if !c.is_empty() => {
            return Err(invalid("the country of origin is a 2-letter code"));
        }
        _ => None,
    };
    sqlx::query!("UPDATE product_variants SET country_of_origin = COALESCE($2, country_of_origin) WHERE id = $1", id, origin)
        .execute(&mut *conn)
        .await?;
    check_compare_at(conn, id).await?;
    Ok(id)
}

#[derive(Debug, Deserialize)]
pub struct VariantCreate {
    #[serde(flatten)]
    pub variant: NewVariant,
    #[serde(default)]
    pub options: HashMap<String, String>,
}

pub async fn create_variant(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<VariantCreate>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let has_default = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM product_variants WHERE product_id = $1 AND is_default AND deleted_at IS NULL) AS \"x!\"",
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    let vid = insert_variant(&mut tx, id, &b.variant, !has_default, &b.options).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "variant.created",
        "variant",
        &vid.to_string(),
        json!({ "product_id": id, "sku": b.variant.sku }),
    )
    .await?;
    changed(&mut tx, id).await?;
    let out = product_json(&mut tx, &state, id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct GenerateInput {
    pub price_minor: i64,
    pub weight_grams: i32,
    /// SKU prefix (default: the product name in capitals).
    pub sku_prefix: Option<String>,
    /// Also reset the price of the existing active variants to base + adjustments.
    #[serde(default)]
    pub reprice: bool,
}

/// Builds the variants from the option matrix: existing combinations are kept (archived ones
/// come back), new ones are created, combinations no longer in the matrix are archived.
/// Price of a new variant = `price_minor` + the price adjustments of its values; the default
/// variant is the first combination with the default values chosen on the options.
pub async fn generate_variants(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(g): Json<GenerateInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let options = sqlx::query!(
        r#"SELECT o.id, o.code::text AS "code!",
                  (SELECT code::text FROM product_option_values WHERE id = o.default_value_id) AS default_value,
                  COALESCE(array_agg(v.code::text ORDER BY v.sort_order, v.code) FILTER (WHERE v.id IS NOT NULL), '{}') AS "values!",
                  COALESCE(array_agg(COALESCE(v.price_adjustment_minor, 0) ORDER BY v.sort_order, v.code) FILTER (WHERE v.id IS NOT NULL), '{}') AS "adjustments!"
           FROM product_options o LEFT JOIN product_option_values v ON v.option_id = o.id
           WHERE o.product_id = $1 GROUP BY o.id ORDER BY o.sort_order, o.code"#,
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    let prefix = match &g.sku_prefix {
        Some(p) if !p.trim().is_empty() => p.trim().to_uppercase(),
        _ => sqlx::query_scalar!(
            "SELECT name FROM product_translations WHERE product_id = $1 ORDER BY (locale = (SELECT code FROM locales WHERE is_default)) DESC LIMIT 1",
            id
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|n| slugify(&n).to_uppercase().chars().take(16).collect())
        .unwrap_or_else(|| "SKU".into()),
    };
    // Cartesian product of the values.
    let mut combos: Vec<HashMap<String, String>> = vec![HashMap::new()];
    for o in &options {
        let mut next = Vec::new();
        for c in &combos {
            for v in &o.values {
                let mut c2 = c.clone();
                c2.insert(o.code.clone(), v.clone());
                next.push(c2);
            }
        }
        combos = next;
    }
    let existing = sqlx::query!(
        r#"SELECT v.id, v.status, COALESCE((SELECT jsonb_object_agg(o.code::text, ov.code::text) FROM product_variant_option_values pvo
                   JOIN product_options o ON o.id = pvo.option_id JOIN product_option_values ov ON ov.id = pvo.option_value_id
                   WHERE pvo.variant_id = v.id), '{}') AS "options!"
           FROM product_variants v WHERE v.product_id = $1 AND v.deleted_at IS NULL ORDER BY v.status, v.created_at"#,
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    let price_of = |combo: &HashMap<String, String>| -> AppResult<i64> {
        let mut price = g.price_minor;
        for o in &options {
            if let Some(i) = combo
                .get(&o.code)
                .and_then(|c| o.values.iter().position(|v| v == c))
            {
                price += o.adjustments.get(i).copied().unwrap_or(0);
            }
        }
        if price < 0 {
            return Err(invalid(
                "the price adjustments make a variant cost less than 0",
            ));
        }
        Ok(price)
    };
    // The first combination (in value order) with every default value set on the options.
    let defaults: Vec<(&String, &String)> = options
        .iter()
        .filter_map(|o| o.default_value.as_ref().map(|d| (&o.code, d)))
        .collect();
    let default_combo: Option<HashMap<String, String>> = if defaults.is_empty() {
        None
    } else {
        combos
            .iter()
            .find(|c| defaults.iter().all(|(o, d)| c.get(*o) == Some(*d)))
            .cloned()
    };
    let mut default_id: Option<Uuid> = None;
    let mut matched: HashSet<Uuid> = HashSet::new();
    let (mut created, mut restored, mut repriced) = (0, 0, 0);
    for combo in &combos {
        let price = price_of(combo)?;
        let found = existing.iter().find(|e| {
            !matched.contains(&e.id)
                && serde_json::from_value::<HashMap<String, String>>(e.options.clone())
                    .unwrap_or_default()
                    == *combo
        });
        match found {
            Some(e) => {
                matched.insert(e.id);
                if default_combo.as_ref() == Some(combo) {
                    default_id = Some(e.id);
                }
                if g.reprice {
                    // A compare-at price no longer above the new price is dropped (it must stay higher).
                    let n = sqlx::query!(
                        "UPDATE product_variants SET price_minor = $2,
                             compare_at_price_minor = CASE WHEN compare_at_price_minor > $2 THEN compare_at_price_minor END
                         WHERE id = $1 AND price_minor <> $2",
                        e.id,
                        price
                    )
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                    if n > 0 {
                        // Omnibus: a remaining compare-at price must still respect the reference price.
                        check_compare_at(&mut tx, e.id).await?;
                    }
                    repriced += n;
                }
                if e.status != "active" {
                    sqlx::query!(
                        "UPDATE product_variants SET status = 'active' WHERE id = $1",
                        e.id
                    )
                    .execute(&mut *tx)
                    .await?;
                    restored += 1;
                }
            }
            None => {
                let suffix: Vec<String> = options
                    .iter()
                    .filter_map(|o| combo.get(&o.code).map(|v| v.to_uppercase()))
                    .collect();
                let mut sku = if suffix.is_empty() {
                    prefix.clone()
                } else {
                    format!("{prefix}-{}", suffix.join("-"))
                };
                let mut n = 1;
                while sqlx::query_scalar!("SELECT EXISTS (SELECT 1 FROM product_variants WHERE sku = $1::text::citext) AS \"x!\"", sku)
                    .fetch_one(&mut *tx)
                    .await?
                {
                    n += 1;
                    sku = format!("{sku}-{n}");
                }
                let vid = insert_variant(
                    &mut tx,
                    id,
                    &NewVariant {
                        sku,
                        price_minor: price,
                        weight_grams: g.weight_grams,
                        compare_at_price_minor: None,
                        net_quantity: None,
                        net_unit: None,
                        country_of_origin: None,
                    },
                    false,
                    combo,
                )
                .await?;
                matched.insert(vid);
                if default_combo.as_ref() == Some(combo) {
                    default_id = Some(vid);
                }
                created += 1;
            }
        }
    }
    let archived = sqlx::query!(
        "UPDATE product_variants SET status = 'archived', is_default = false
         WHERE product_id = $1 AND deleted_at IS NULL AND status = 'active' AND NOT (id = ANY($2))",
        id,
        &matched.iter().copied().collect::<Vec<_>>()
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // Exactly one default among the active variants: the default values' combination, else the
    // current default, else the first one.
    sqlx::query!(
        "UPDATE product_variants SET is_default = (id = COALESCE($2, (SELECT id FROM product_variants WHERE product_id = $1 AND status = 'active'
                AND deleted_at IS NULL ORDER BY is_default DESC, sort_order, created_at LIMIT 1)))
         WHERE product_id = $1 AND deleted_at IS NULL",
        id,
        default_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "variant.generated",
        "product",
        &id.to_string(),
        json!({ "created": created, "restored": restored, "archived": archived, "repriced": repriced }),
    )
    .await?;
    changed(&mut tx, id).await?;
    let out = product_json(&mut tx, &state, id).await?;
    tx.commit().await?;
    Ok(Json(
        json!({ "product": out, "created": created, "restored": restored, "archived": archived, "repriced": repriced }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct VariantPatch {
    pub id: Uuid,
    pub sku: Option<String>,
    pub barcode: Option<Option<String>>,
    pub status: Option<VariantStatus>,
    pub is_default: Option<bool>,
    pub sort_order: Option<i32>,
    pub price_minor: Option<i64>,
    pub compare_at_price_minor: Option<Option<i64>>,
    pub cost_minor: Option<Option<i64>>,
    pub net_quantity: Option<Option<Decimal>>,
    pub net_unit: Option<Option<NetUnit>>,
    pub weight_grams: Option<i32>,
    pub requires_shipping: Option<bool>,
    pub is_taxable: Option<bool>,
    pub track_inventory: Option<bool>,
    pub low_stock_threshold: Option<Option<i32>>,
    pub cover_media_id: Option<Option<Uuid>>,
    pub options: Option<HashMap<String, String>>,
    pub country_of_origin: Option<Option<String>>,
    pub hs_code: Option<Option<String>>,
    /// Customs description per locale (shipments outside the EU).
    pub customs_description: Option<HashMap<String, String>>,
}

/// Bulk edit of variants (inline grid). Every change is validated, then written atomically.
pub async fn bulk_variants(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(patches): Json<Vec<VariantPatch>>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let pricing = patches.iter().any(|p| {
        p.price_minor.is_some() || p.compare_at_price_minor.is_some() || p.cost_minor.is_some()
    });
    if pricing && !staff.can("shop.pricing") && !staff.can("shop.catalog") {
        return Err(AppError::Forbidden);
    }
    let mut tx = state.db.begin().await?;
    let before = product_json(&mut tx, &state, id).await?;
    for p in &patches {
        let cur = sqlx::query!(
            "SELECT price_minor, compare_at_price_minor FROM product_variants WHERE id = $1 AND product_id = $2 AND deleted_at IS NULL FOR UPDATE",
            p.id,
            id
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let price = p.price_minor.unwrap_or(cur.price_minor);
        let compare = p
            .compare_at_price_minor
            .unwrap_or(cur.compare_at_price_minor);
        if price < 0 || compare.is_some_and(|c| c <= price) {
            return Err(invalid(
                "the compare-at price must be higher than the price",
            ));
        }
        if p.weight_grams.is_some_and(|w| w <= 0) {
            return Err(invalid("the weight must be positive"));
        }
        if let Some(values) = &p.options {
            let combo = resolve_combination(&mut tx, id, values).await?;
            sqlx::query!(
                "DELETE FROM product_variant_option_values WHERE variant_id = $1",
                p.id
            )
            .execute(&mut *tx)
            .await?;
            for (o, v) in combo {
                sqlx::query!(
                    "INSERT INTO product_variant_option_values (variant_id, option_id, option_value_id) VALUES ($1, $2, $3)",
                    p.id,
                    o,
                    v
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        if p.is_default == Some(true) {
            sqlx::query!(
                "UPDATE product_variants SET is_default = false WHERE product_id = $1 AND id <> $2",
                id,
                p.id
            )
            .execute(&mut *tx)
            .await?;
        }
        let res = sqlx::query!(
            "UPDATE product_variants SET
                sku = COALESCE($2::text::citext, sku), barcode = CASE WHEN $3 THEN $4 ELSE barcode END,
                status = COALESCE($5, status), is_default = COALESCE($6, is_default), sort_order = COALESCE($7, sort_order),
                price_minor = $8, compare_at_price_minor = $9,
                cost_minor = CASE WHEN $10 THEN $11 ELSE cost_minor END,
                net_quantity = CASE WHEN $12 THEN $13 ELSE net_quantity END,
                net_unit = CASE WHEN $12 THEN $14 ELSE net_unit END,
                weight_grams = COALESCE($15, weight_grams), requires_shipping = COALESCE($16, requires_shipping),
                is_taxable = COALESCE($17, is_taxable), track_inventory = COALESCE($18, track_inventory),
                low_stock_threshold = CASE WHEN $19 THEN $20 ELSE low_stock_threshold END,
                cover_media_id = CASE WHEN $21 THEN $22 ELSE cover_media_id END
             WHERE id = $1",
            p.id,
            p.sku.as_deref().map(|s| s.trim().to_uppercase()),
            p.barcode.is_some(),
            p.barcode.clone().flatten(),
            p.status.map(|s| s.as_str()),
            p.is_default,
            p.sort_order,
            price,
            compare,
            p.cost_minor.is_some(),
            p.cost_minor.flatten(),
            p.net_quantity.is_some() || p.net_unit.is_some(),
            p.net_quantity.flatten(),
            p.net_unit.flatten().map(|u| u.as_str()),
            p.weight_grams,
            p.requires_shipping,
            p.is_taxable,
            p.track_inventory,
            p.low_stock_threshold.is_some(),
            p.low_stock_threshold.flatten(),
            p.cover_media_id.is_some(),
            p.cover_media_id.flatten()
        )
        .execute(&mut *tx)
        .await;
        if let Err(e) = res {
            if e.as_database_error()
                .is_some_and(|d| d.is_unique_violation())
            {
                return Err(conflict("sku_taken", "this SKU is already used"));
            }
            if e.as_database_error()
                .and_then(|d| d.code())
                .is_some_and(|c| c == "23514")
            {
                return Err(invalid(
                    "a value is out of range (barcode: 8 to 14 digits; net quantity needs its unit)",
                ));
            }
            return Err(e.into());
        }
        if p.country_of_origin.is_some() || p.hs_code.is_some() {
            let origin = p
                .country_of_origin
                .clone()
                .flatten()
                .map(|c| c.trim().to_ascii_uppercase())
                .filter(|c| !c.is_empty());
            let hs = p
                .hs_code
                .clone()
                .flatten()
                .map(|c| c.replace([' ', '.'], ""))
                .filter(|c| !c.is_empty());
            let res = sqlx::query!(
                "UPDATE product_variants SET country_of_origin = CASE WHEN $2 THEN $3 ELSE country_of_origin END,
                        hs_code = CASE WHEN $4 THEN $5 ELSE hs_code END WHERE id = $1",
                p.id,
                p.country_of_origin.is_some(),
                origin,
                p.hs_code.is_some(),
                hs
            )
            .execute(&mut *tx)
            .await;
            if res.is_err() {
                return Err(invalid(
                    "country of origin: 2 letters; HS code: 6 to 10 digits",
                ));
            }
        }
        if let Some(descriptions) = &p.customs_description {
            for (locale, text) in descriptions {
                sqlx::query!(
                    "INSERT INTO product_variant_translations (variant_id, locale, customs_description) VALUES ($1, $2, NULLIF(btrim($3), ''))
                     ON CONFLICT (variant_id, locale) DO UPDATE SET customs_description = EXCLUDED.customs_description",
                    p.id,
                    locale,
                    text
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        check_compare_at(&mut tx, p.id).await?;
    }
    // Active variants: unique combinations, every option valued.
    let active = sqlx::query!(
        r#"SELECT v.id, (SELECT count(*) FROM product_variant_option_values WHERE variant_id = v.id) AS "valued!",
                  (SELECT count(*) FROM product_options WHERE product_id = $1) AS "options!"
           FROM product_variants v WHERE v.product_id = $1 AND v.status = 'active' AND v.deleted_at IS NULL"#,
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    for v in &active {
        if v.valued != v.options {
            return Err(invalid(
                "an active variant is missing a value for one of the options",
            ));
        }
        let combo: Vec<(Uuid, Uuid)> = sqlx::query!("SELECT option_id, option_value_id FROM product_variant_option_values WHERE variant_id = $1", v.id)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|r| (r.option_id, r.option_value_id))
            .collect();
        if !combo.is_empty() && combination_taken(&mut tx, id, &combo, Some(v.id)).await? {
            return Err(conflict(
                "duplicate_combination",
                "two active variants have the same options",
            ));
        }
    }
    let after = product_json(&mut tx, &state, id).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "variant.bulk_updated",
        "product",
        &id.to_string(),
        json!({ "before": before["variants"], "after": after["variants"] }),
    )
    .await?;
    changed(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(after))
}

#[derive(Debug, Deserialize)]
pub struct VariantMedia {
    pub media_ids: Vec<Uuid>,
}

pub async fn variant_media(
    State(state): State<AppState>,
    staff: Staff,
    Path(variant_id): Path<Uuid>,
    Json(b): Json<VariantMedia>,
) -> AppResult<StatusCode> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let product_id = sqlx::query_scalar!(
        "SELECT product_id FROM product_variants WHERE id = $1",
        variant_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    sqlx::query!(
        "DELETE FROM product_variant_media WHERE variant_id = $1",
        variant_id
    )
    .execute(&mut *tx)
    .await?;
    for (i, m) in b.media_ids.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO product_variant_media (variant_id, media_id, sort_order) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            variant_id,
            m,
            i as i32
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "variant.media_set",
        "variant",
        &variant_id.to_string(),
        json!({ "media_ids": b.media_ids }),
    )
    .await?;
    changed(&mut tx, product_id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Copy as a draft: translations (slugs suffixed), options, variants (SKUs suffixed), links.
pub async fn duplicate(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let new_id = Uuid::now_v7();
    let n = sqlx::query!(
        "INSERT INTO products (id, kind, status, booking_mode, is_featured, place_id, cover_media_id, sort_order, created_by)
         SELECT $2, kind, 'draft', 'not_bookable', false, place_id, cover_media_id, sort_order, $3 FROM products
         WHERE id = $1 AND kind = 'shop_product' AND deleted_at IS NULL",
        id,
        new_id,
        staff.id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    let suffix = new_id.simple().to_string()[24..].to_string();
    sqlx::query!(
        "INSERT INTO shop_products (product_id, product_type, shipping_profile_id, origin_place_id, farm_crop_id, is_perishable,
                                    withdrawal_exemption, tax_class_code, bundle_vat_split, track_lots, shelf_life_days, allergens,
                                    nutrition, option_count, max_per_order, available_period)
         SELECT $2, product_type, shipping_profile_id, origin_place_id, farm_crop_id, is_perishable, withdrawal_exemption,
                tax_class_code, bundle_vat_split, track_lots, shelf_life_days, allergens, nutrition, option_count, max_per_order,
                available_period FROM shop_products WHERE product_id = $1",
        id,
        new_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO product_translations (product_id, locale, name, slug, subtitle, short_description, description, seo_title, seo_description)
         SELECT $2, locale, name || ' (copie)', slug || '-' || $3, subtitle, short_description, description, seo_title, seo_description
         FROM product_translations WHERE product_id = $1",
        id,
        new_id,
        suffix
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO shop_product_translations (product_id, locale, ingredients, storage_instructions, usage_tips, origin_story, producer_name, legal_notice)
         SELECT $2, locale, ingredients, storage_instructions, usage_tips, origin_story, producer_name, legal_notice
         FROM shop_product_translations WHERE product_id = $1",
        id,
        new_id
    )
    .execute(&mut *tx)
    .await?;
    for table in ["product_categories", "product_taxes"] {
        let col = if table == "product_categories" {
            "category_id"
        } else {
            "tax_id"
        };
        sqlx::query(&format!("INSERT INTO {table} (product_id, {col}) SELECT $2, {col} FROM {table} WHERE product_id = $1"))
            .bind(id)
            .bind(new_id)
            .execute(&mut *tx)
            .await?;
    }
    // Options, values and variants with a mapping old → new.
    let mut value_map: HashMap<Uuid, Uuid> = HashMap::new();
    let mut option_map: HashMap<Uuid, Uuid> = HashMap::new();
    for o in sqlx::query!(
        "SELECT id, code::text AS \"code!\", sort_order, attribute_id, display_type, default_value_id FROM product_options WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?
    {
        let oid = Uuid::now_v7();
        option_map.insert(o.id, oid);
        sqlx::query!(
            "INSERT INTO product_options (id, product_id, code, sort_order, attribute_id, display_type) VALUES ($1, $2, $3::text::citext, $4, $5, $6)",
            oid,
            new_id,
            o.code,
            o.sort_order,
            o.attribute_id,
            o.display_type
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO product_option_translations (option_id, locale, name) SELECT $2, locale, name FROM product_option_translations WHERE option_id = $1",
            o.id,
            oid
        )
        .execute(&mut *tx)
        .await?;
        for v in sqlx::query!(
            "SELECT id, code::text AS \"code!\", sort_order, swatch_media_id, swatch_color, price_adjustment_minor, variation_value_id
             FROM product_option_values WHERE option_id = $1",
            o.id
        )
        .fetch_all(&mut *tx)
        .await?
        {
            let vid = Uuid::now_v7();
            value_map.insert(v.id, vid);
            sqlx::query!(
                "INSERT INTO product_option_values (id, option_id, code, sort_order, swatch_media_id, swatch_color, price_adjustment_minor, variation_value_id)
                 VALUES ($1, $2, $3::text::citext, $4, $5, $6, $7, $8)",
                vid,
                oid,
                v.code,
                v.sort_order,
                v.swatch_media_id,
                v.swatch_color,
                v.price_adjustment_minor,
                v.variation_value_id
            )
            .execute(&mut *tx)
            .await?;
            if o.default_value_id == Some(v.id) {
                sqlx::query!("UPDATE product_options SET default_value_id = $2 WHERE id = $1", oid, vid)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query!(
                "INSERT INTO product_option_value_translations (option_value_id, locale, label)
                 SELECT $2, locale, label FROM product_option_value_translations WHERE option_value_id = $1",
                v.id,
                vid
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    for v in sqlx::query!(
        "SELECT id FROM product_variants WHERE product_id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_all(&mut *tx)
    .await?
    {
        let nv = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO product_variants (id, product_id, sku, barcode, status, is_default, sort_order, price_minor, compare_at_price_minor,
                                           cost_minor, currency, net_quantity, net_unit, weight_grams, length_mm, width_mm, height_mm,
                                           requires_shipping, is_taxable, track_inventory, low_stock_threshold, cover_media_id)
             SELECT $2, $3, sku || '-COPY-' || $4, NULL, status, is_default, sort_order, price_minor, compare_at_price_minor, cost_minor,
                    currency, net_quantity, net_unit, weight_grams, length_mm, width_mm, height_mm, requires_shipping, is_taxable,
                    track_inventory, low_stock_threshold, cover_media_id
             FROM product_variants WHERE id = $1",
            v.id,
            nv,
            new_id,
            suffix.to_uppercase()
        )
        .execute(&mut *tx)
        .await?;
        for link in sqlx::query!("SELECT option_id, option_value_id FROM product_variant_option_values WHERE variant_id = $1", v.id)
            .fetch_all(&mut *tx)
            .await?
        {
            if let (Some(o), Some(val)) = (option_map.get(&link.option_id), value_map.get(&link.option_value_id)) {
                sqlx::query!(
                    "INSERT INTO product_variant_option_values (variant_id, option_id, option_value_id) VALUES ($1, $2, $3)",
                    nv,
                    o,
                    val
                )
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "shop_product.duplicated",
        "product",
        &new_id.to_string(),
        json!({ "from": id }),
    )
    .await?;
    changed(&mut tx, new_id).await?;
    let out = product_json(&mut tx, &state, new_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// ---- CSV ---------------------------------------------------------------------------------------

fn csv_field(s: &str) -> String {
    // Formulas are neutralised (CSV injection in spreadsheets).
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

pub const CSV_HEADER: &str = "sku,product,options,status,price_minor,compare_at_price_minor,cost_minor,weight_grams,barcode,on_hand";

pub async fn export_csv(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<impl IntoResponse> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT v.sku::text AS "sku!", COALESCE(t.name, '') AS "name!", v.status, v.price_minor, v.compare_at_price_minor, v.cost_minor,
                  v.weight_grams, v.barcode,
                  COALESCE((SELECT string_agg(o.code::text || '=' || ov.code::text, ';' ORDER BY o.sort_order) FROM product_variant_option_values pvo
                            JOIN product_options o ON o.id = pvo.option_id JOIN product_option_values ov ON ov.id = pvo.option_value_id
                            WHERE pvo.variant_id = v.id), '') AS "options!",
                  COALESCE((SELECT sum(on_hand) FROM inventory_levels WHERE variant_id = v.id), 0)::bigint AS "on_hand!"
           FROM product_variants v JOIN products p ON p.id = v.product_id AND p.deleted_at IS NULL
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
           WHERE v.deleted_at IS NULL ORDER BY t.name, v.sort_order, v.sku"#
    )
    .fetch_all(&state.db)
    .await?;
    let can_cost = staff.can("shop.pricing");
    let mut out = format!("{CSV_HEADER}\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            csv_field(&r.sku),
            csv_field(&r.name),
            csv_field(&r.options),
            r.status,
            r.price_minor,
            r.compare_at_price_minor
                .map(|x| x.to_string())
                .unwrap_or_default(),
            if can_cost {
                r.cost_minor.map(|x| x.to_string()).unwrap_or_default()
            } else {
                String::new()
            },
            r.weight_grams,
            csv_field(r.barcode.as_deref().unwrap_or("")),
            r.on_hand
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"shop-variants.csv\"",
            ),
        ],
        out,
    ))
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ImportBody {
    pub csv: String,
    #[serde(default = "yes")]
    pub dry_run: bool,
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur)),
            (c, _) => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|f| f.trim().trim_start_matches('\'').to_string())
        .collect()
}

/// Updates variants by SKU (price, compare-at, cost, weight, status, barcode). `dry_run`
/// returns the report without writing. New products are created in the editor, not by CSV.
pub async fn import_csv(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<ImportBody>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let mut lines = b.csv.lines().filter(|l| !l.trim().is_empty());
    let header: Vec<String> = lines.next().map(parse_csv_line).unwrap_or_default();
    let col = |name: &str| header.iter().position(|h| h.eq_ignore_ascii_case(name));
    let sku_col = col("sku").ok_or_else(|| invalid("the CSV needs a sku column"))?;
    let mut tx = state.db.begin().await?;
    let mut report = Vec::new();
    let (mut ok, mut errors) = (0, 0);
    for (n, line) in lines.enumerate() {
        let f = parse_csv_line(line);
        let get = |c: Option<usize>| {
            c.and_then(|i| f.get(i))
                .map(String::as_str)
                .filter(|s| !s.is_empty())
        };
        let sku = get(Some(sku_col)).unwrap_or_default().to_uppercase();
        let num = |c: &str| -> Result<Option<i64>, String> {
            get(col(c))
                .map(|v| v.parse::<i64>().map_err(|_| format!("{c}: not a number")))
                .transpose()
        };
        let parsed = (|| -> Result<_, String> {
            Ok((
                num("price_minor")?,
                num("compare_at_price_minor")?,
                num("cost_minor")?,
                num("weight_grams")?,
            ))
        })();
        let (price, compare, cost, weight) = match parsed {
            Ok(p) => p,
            Err(e) => {
                errors += 1;
                report.push(json!({ "line": n + 2, "sku": sku, "error": e }));
                continue;
            }
        };
        let status = get(col("status"));
        let cur = sqlx::query!(
            "SELECT id, product_id, price_minor, compare_at_price_minor FROM product_variants WHERE sku = $1::text::citext AND deleted_at IS NULL",
            sku
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(cur) = cur else {
            errors += 1;
            report.push(json!({ "line": n + 2, "sku": sku, "error": "unknown SKU" }));
            continue;
        };
        let new_price = price.unwrap_or(cur.price_minor);
        let new_compare = if col("compare_at_price_minor").is_some() {
            compare
        } else {
            cur.compare_at_price_minor
        };
        if new_price < 0
            || new_compare.is_some_and(|c| c <= new_price)
            || weight.is_some_and(|w| w <= 0)
            || status.is_some_and(|s| s != "active" && s != "archived")
        {
            errors += 1;
            report.push(json!({ "line": n + 2, "sku": sku, "error": "invalid price, compare-at, weight or status" }));
            continue;
        }
        report.push(json!({ "line": n + 2, "sku": sku, "changes": { "price_minor": [cur.price_minor, new_price], "compare_at_price_minor": new_compare,
                             "cost_minor": cost, "weight_grams": weight, "status": status } }));
        ok += 1;
        if !b.dry_run {
            sqlx::query!(
                "UPDATE product_variants SET price_minor = $2, compare_at_price_minor = $3, cost_minor = COALESCE($4, cost_minor),
                        weight_grams = COALESCE($5, weight_grams), status = COALESCE($6, status), barcode = COALESCE($7, barcode)
                 WHERE id = $1",
                cur.id,
                new_price,
                new_compare,
                cost,
                weight.map(|w| w as i32),
                status,
                get(col("barcode"))
            )
            .execute(&mut *tx)
            .await?;
            check_compare_at(&mut tx, cur.id).await?;
            changed(&mut tx, cur.product_id).await?;
        }
    }
    if !b.dry_run && errors == 0 {
        audit::record(
            &mut tx,
            &staff.actor(),
            "variant.imported",
            "variant",
            "csv",
            json!({ "rows": ok }),
        )
        .await?;
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(Json(
        json!({ "dry_run": b.dry_run, "applied": !b.dry_run && errors == 0, "ok": ok, "errors": errors, "rows": report }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_csv() {
        assert_eq!(slugify("Dattes Medjool — 500 g"), "dattes-medjool-500-g");
        assert_eq!(slugify("Huile d'olive"), "huile-d-olive");
        assert_eq!(
            parse_csv_line(r#"A,"b, c","d ""e""",'=1"#),
            vec!["A", "b, c", "d \"e\"", "=1"]
        );
        assert_eq!(csv_field("=HYPERLINK()"), "'=HYPERLINK()");
    }
}
