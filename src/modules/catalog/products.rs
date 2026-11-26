//! The shared editor shell: list, detail, create, save (optimistic lock), status changes with
//! the publish checklist, delete vs archive, duplicate, order, bulk actions, itinerary.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Fields, Kind, activities, changed, field_error, packages, pricing, stays};
use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    modules::{seo::redirects, shop::admin::catalog::slugify},
    services::{audit::Actor, inventory},
    state::AppState,
};

const LOCALES: [&str; 3] = ["fr", "en", "ar"];
const FROM_TTL: std::time::Duration = std::time::Duration::from_secs(600);

// ---- Bodies ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TranslationBody {
    pub name: String,
    pub slug: Option<String>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ExtraLink {
    pub extra_product_id: Uuid,
    #[serde(default)]
    pub is_mandatory: bool,
}

fn instant() -> String {
    "instant".into()
}

/// Everything the editor's save bar sends. The optional sections are the tab being edited:
/// written in the same transaction, each under its own permission.
#[derive(Debug, Deserialize)]
pub struct ProductBody {
    /// The `updated_at` the editor loaded (required to save an existing product).
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub sku: Option<String>,
    #[serde(default = "instant")]
    pub booking_mode: String,
    #[serde(default)]
    pub is_featured: bool,
    #[serde(default)]
    pub place_id: Option<Uuid>,
    #[serde(default)]
    pub cover_media_id: Option<Uuid>,
    #[serde(default)]
    pub video_media_id: Option<Uuid>,
    #[serde(default)]
    pub gallery_media_ids: Vec<Uuid>,
    #[serde(default)]
    pub translations: BTreeMap<String, TranslationBody>,
    #[serde(default)]
    pub category_ids: Vec<Uuid>,
    #[serde(default)]
    pub primary_category_id: Option<Uuid>,
    #[serde(default)]
    pub tag_ids: Vec<Uuid>,
    #[serde(default)]
    pub amenity_ids: Vec<Uuid>,
    #[serde(default)]
    pub related_ids: Vec<Uuid>,
    #[serde(default)]
    pub extras: Vec<ExtraLink>,
    /// Kind-specific fields (`accommodation_types`, `activities`, `packages`, `extras` row).
    #[serde(default)]
    pub details: Value,
    #[serde(default)]
    pub itinerary: Option<Vec<StepBody>>,
    #[serde(default)]
    pub items: Option<Vec<packages::ItemBody>>,
    #[serde(default)]
    pub prices: Option<Vec<pricing::PriceBody>>,
    #[serde(default)]
    pub tax_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub schedules: Option<Vec<activities::ScheduleBody>>,
    #[serde(default)]
    pub resource_requirements: Option<Vec<activities::RequirementBody>>,
}

// ---- Kind details ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Bed {
    #[serde(rename = "type")]
    pub bed_type: String,
    pub count: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StayDetails {
    pub accommodation_kind: String,
    #[serde(default)]
    pub inventory_mode: Option<String>,
    pub max_guests: i32,
    #[serde(default)]
    pub max_adults: Option<i32>,
    #[serde(default)]
    pub max_children: Option<i32>,
    pub standard_occupancy: i32,
    #[serde(default)]
    pub bed_configuration: Vec<Bed>,
    #[serde(default, deserialize_with = "crate::web::percent::option")]
    pub surface_m2: Option<Decimal>,
    #[serde(default = "one")]
    pub min_nights: i32,
    #[serde(default)]
    pub max_nights: Option<i32>,
    #[serde(default)]
    pub check_in_from: Option<NaiveTime>,
    #[serde(default)]
    pub check_in_until: Option<NaiveTime>,
    #[serde(default)]
    pub check_out_until: Option<NaiveTime>,
    /// Pool mode, on creation (then `PUT .../inventory`).
    #[serde(default)]
    pub pool_capacity: Option<i32>,
}

fn one() -> i32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActivityDetails {
    pub location_type: String,
    pub scheduling_mode: String,
    #[serde(default)]
    pub duration_minutes: Option<i32>,
    #[serde(default)]
    pub difficulty: Option<String>,
    #[serde(default = "one")]
    pub min_participants: i32,
    #[serde(default)]
    pub max_participants: Option<i32>,
    #[serde(default)]
    pub min_age: Option<i16>,
    #[serde(default)]
    pub is_guided: bool,
    #[serde(default)]
    pub guide_languages: Vec<String>,
    #[serde(default)]
    pub meeting_place_id: Option<Uuid>,
    #[serde(default = "twelve")]
    pub booking_cutoff_hours: i32,
    #[serde(default)]
    pub pickup_available: bool,
    #[serde(default = "yes")]
    pub standalone_bookable: bool,
}

fn twelve() -> i32 {
    12
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageDetails {
    #[serde(default)]
    pub duration_days: Option<i32>,
    #[serde(default)]
    pub duration_nights: Option<i32>,
    pub pricing_mode: String,
    #[serde(default, deserialize_with = "crate::web::percent::option")]
    pub discount_percent: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtraDetails {
    pub applies_to: String,
    #[serde(default)]
    pub max_quantity: Option<i32>,
    #[serde(default)]
    pub requires_date: bool,
    /// Empty = unlimited.
    #[serde(default)]
    pub stock_quantity: Option<i32>,
}

pub const ACCOMMODATION_KINDS: [&str; 10] = [
    "tent_pitch",
    "campervan_pitch",
    "caravan_pitch",
    "furnished_tent",
    "nomad_tent",
    "bivouac",
    "bungalow",
    "room",
    "dormitory_bed",
    "other",
];

enum Details {
    Stay(StayDetails),
    Activity(ActivityDetails),
    Package(PackageDetails),
    Extra(ExtraDetails),
}

fn parse_details(kind: Kind, v: &Value) -> AppResult<Details> {
    let bad = |e: serde_json::Error| {
        AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_fields",
            "details are not valid",
        )
        .with_details(json!({ "fields": { "details": "invalid" }, "reason": e.to_string() }))
    };
    Ok(match kind {
        Kind::Stays => Details::Stay(serde_json::from_value(v.clone()).map_err(bad)?),
        Kind::Activities => Details::Activity(serde_json::from_value(v.clone()).map_err(bad)?),
        Kind::Packages => Details::Package(serde_json::from_value(v.clone()).map_err(bad)?),
        Kind::Extras => Details::Extra(serde_json::from_value(v.clone()).map_err(bad)?),
    })
}

fn validate_details(d: &Details, f: &mut Fields) {
    match d {
        Details::Stay(s) => {
            if !ACCOMMODATION_KINDS.contains(&s.accommodation_kind.as_str()) {
                f.add("details.accommodation_kind", "invalid");
            }
            if let Some(m) = &s.inventory_mode
                && !["unit", "pool"].contains(&m.as_str())
            {
                f.add("details.inventory_mode", "invalid");
            }
            if s.max_guests < 1 {
                f.add("details.max_guests", "min");
            }
            if s.standard_occupancy < 1 {
                f.add("details.standard_occupancy", "min");
            } else if s.standard_occupancy > s.max_guests {
                f.add("details.standard_occupancy", "above_max_guests");
            }
            if s.max_adults.is_some_and(|a| a < 1 || a > s.max_guests) {
                f.add("details.max_adults", "above_max_guests");
            }
            if s.max_children.is_some_and(|c| c < 0 || c > s.max_guests) {
                f.add("details.max_children", "above_max_guests");
            }
            if s.min_nights < 1 {
                f.add("details.min_nights", "min");
            }
            if s.max_nights.is_some_and(|m| m < s.min_nights) {
                f.add("details.max_nights", "below_min_nights");
            }
            if s.surface_m2.is_some_and(|m| m <= Decimal::ZERO) {
                f.add("details.surface_m2", "min");
            }
            for (i, b) in s.bed_configuration.iter().enumerate() {
                if b.bed_type.trim().is_empty() {
                    f.add(format!("details.bed_configuration[{i}].type"), "required");
                }
                if b.count < 1 {
                    f.add(format!("details.bed_configuration[{i}].count"), "min");
                }
            }
            if s.pool_capacity.is_some_and(|c| c < 0) {
                f.add("details.pool_capacity", "min");
            }
            if let (Some(a), Some(b)) = (s.check_in_from, s.check_in_until)
                && b < a
            {
                f.add("details.check_in_until", "before_check_in_from");
            }
        }
        Details::Activity(a) => {
            if !["on_site", "off_site"].contains(&a.location_type.as_str()) {
                f.add("details.location_type", "invalid");
            }
            if !["fixed_slots", "on_request", "anytime"].contains(&a.scheduling_mode.as_str()) {
                f.add("details.scheduling_mode", "invalid");
            }
            if a.duration_minutes.is_some_and(|d| d < 1) {
                f.add("details.duration_minutes", "min");
            }
            if let Some(d) = &a.difficulty
                && !["easy", "moderate", "challenging", "extreme"].contains(&d.as_str())
            {
                f.add("details.difficulty", "invalid");
            }
            if a.min_participants < 1 {
                f.add("details.min_participants", "min");
            }
            if a.max_participants.is_some_and(|m| m < a.min_participants) {
                f.add("details.max_participants", "below_min_participants");
            }
            if a.min_age.is_some_and(|m| m < 0) {
                f.add("details.min_age", "min");
            }
            if a.booking_cutoff_hours < 0 {
                f.add("details.booking_cutoff_hours", "min");
            }
            for (i, l) in a.guide_languages.iter().enumerate() {
                if !(2..=8).contains(&l.trim().len()) {
                    f.add(format!("details.guide_languages[{i}]"), "invalid");
                }
            }
        }
        Details::Package(p) => {
            if !["fixed", "sum_of_items", "sum_minus_discount"].contains(&p.pricing_mode.as_str()) {
                f.add("details.pricing_mode", "invalid");
            }
            if p.duration_days.is_some_and(|d| d < 1) {
                f.add("details.duration_days", "min");
            }
            if p.duration_nights.is_some_and(|n| n < 0) {
                f.add("details.duration_nights", "min");
            }
            if let (Some(d), Some(n)) = (p.duration_days, p.duration_nights)
                && n > d
            {
                f.add("details.duration_nights", "above_days");
            }
            match (p.pricing_mode.as_str(), p.discount_percent) {
                ("sum_minus_discount", None) => f.add("details.discount_percent", "required"),
                (_, Some(x)) if x < Decimal::ZERO || x > Decimal::ONE_HUNDRED => {
                    f.add("details.discount_percent", "range");
                }
                ("sum_minus_discount", Some(x)) if x == Decimal::ZERO => {
                    f.add("details.discount_percent", "required");
                }
                _ => {}
            }
        }
        Details::Extra(e) => {
            if !["booking", "accommodation", "activity", "any"].contains(&e.applies_to.as_str()) {
                f.add("details.applies_to", "invalid");
            }
            if e.max_quantity.is_some_and(|m| m < 1) {
                f.add("details.max_quantity", "min");
            }
            if e.stock_quantity.is_some_and(|s| s < 0) {
                f.add("details.stock_quantity", "min");
            }
        }
    }
}

async fn write_details(
    conn: &mut PgConnection,
    id: Uuid,
    d: &Details,
    creating: bool,
) -> AppResult<()> {
    match d {
        Details::Stay(s) => {
            let beds = serde_json::to_value(&s.bed_configuration).unwrap_or_else(|_| json!([]));
            if creating {
                sqlx::query!(
                    "INSERT INTO accommodation_types (product_id, accommodation_kind, inventory_mode, max_guests, standard_occupancy,
                                                      pool_capacity)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                    id,
                    s.accommodation_kind,
                    s.inventory_mode.as_deref().unwrap_or("unit"),
                    s.max_guests,
                    s.standard_occupancy,
                    if s.inventory_mode.as_deref() == Some("pool") { Some(s.pool_capacity.unwrap_or(0)) } else { None }
                )
                .execute(&mut *conn)
                .await?;
            }
            sqlx::query!(
                "UPDATE accommodation_types SET accommodation_kind = $2, max_guests = $3, max_adults = $4, max_children = $5,
                        standard_occupancy = $6, bed_configuration = $7, surface_m2 = $8, min_nights = $9, max_nights = $10,
                        check_in_from = $11, check_in_until = $12, check_out_until = $13
                 WHERE product_id = $1",
                id,
                s.accommodation_kind,
                s.max_guests,
                s.max_adults,
                s.max_children,
                s.standard_occupancy,
                beds,
                s.surface_m2,
                s.min_nights,
                s.max_nights,
                s.check_in_from,
                s.check_in_until,
                s.check_out_until
            )
            .execute(&mut *conn)
            .await?;
        }
        Details::Activity(a) => {
            let langs: Vec<String> = a
                .guide_languages
                .iter()
                .map(|l| l.trim().to_lowercase())
                .filter(|l| !l.is_empty())
                .collect();
            sqlx::query!(
                "INSERT INTO activities (product_id, location_type, scheduling_mode, duration_minutes, difficulty, min_participants,
                                         max_participants, min_age, is_guided, guide_languages, meeting_place_id,
                                         booking_cutoff_hours, pickup_available, standalone_bookable)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                 ON CONFLICT (product_id) DO UPDATE SET location_type = $2, scheduling_mode = $3, duration_minutes = $4,
                     difficulty = $5, min_participants = $6, max_participants = $7, min_age = $8, is_guided = $9,
                     guide_languages = $10, meeting_place_id = $11, booking_cutoff_hours = $12, pickup_available = $13,
                     standalone_bookable = $14",
                id,
                a.location_type,
                a.scheduling_mode,
                a.duration_minutes,
                a.difficulty,
                a.min_participants,
                a.max_participants,
                a.min_age,
                a.is_guided,
                &langs,
                a.meeting_place_id,
                a.booking_cutoff_hours,
                a.pickup_available,
                a.standalone_bookable
            )
            .execute(&mut *conn)
            .await?;
        }
        Details::Package(p) => {
            let discount = if p.pricing_mode == "sum_minus_discount" {
                p.discount_percent.unwrap_or_default()
            } else {
                Decimal::ZERO
            };
            sqlx::query!(
                "INSERT INTO packages (product_id, duration_days, duration_nights, pricing_mode, discount_percent)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (product_id) DO UPDATE SET duration_days = $2, duration_nights = $3, pricing_mode = $4,
                     discount_percent = $5",
                id,
                p.duration_days,
                p.duration_nights,
                p.pricing_mode,
                discount
            )
            .execute(&mut *conn)
            .await?;
        }
        Details::Extra(e) => {
            sqlx::query!(
                "INSERT INTO extras (product_id, applies_to, max_quantity, requires_date, stock_quantity)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (product_id) DO UPDATE SET applies_to = $2, max_quantity = $3, requires_date = $4,
                     stock_quantity = $5",
                id,
                e.applies_to,
                e.max_quantity,
                e.requires_date,
                e.stock_quantity
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

// ---- Validation of the common part ---------------------------------------------------------------

fn clean_list(v: &[String]) -> Vec<String> {
    v.iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn opt(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The slug a translation will get: the one typed (normalised), or one made from the name.
fn slug_of(t: &TranslationBody) -> String {
    t.slug
        .as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| slugify(&t.name))
}

async fn validate_common(
    conn: &mut PgConnection,
    id: Option<Uuid>,
    kind: Kind,
    b: &ProductBody,
    f: &mut Fields,
) -> AppResult<()> {
    if !["instant", "on_request", "not_bookable"].contains(&b.booking_mode.as_str()) {
        f.add("booking_mode", "invalid");
    }
    if b.translations
        .get("fr")
        .is_none_or(|t| t.name.trim().is_empty())
    {
        f.add("translations.fr.name", "required");
    }
    for (l, t) in &b.translations {
        if !LOCALES.contains(&l.as_str()) {
            f.add(format!("translations.{l}"), "unknown_locale");
            continue;
        }
        if t.name.trim().is_empty() {
            continue;
        }
        if t.name.trim().chars().count() > 160 {
            f.add(format!("translations.{l}.name"), "too_long");
        }
        let slug = slug_of(t);
        if slug.is_empty() {
            f.add(format!("translations.{l}.slug"), "required");
            continue;
        }
        if slug.chars().count() > 120 {
            f.add(format!("translations.{l}.slug"), "too_long");
        }
        if kind.entity().is_some() {
            let taken = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM product_translations
                                   WHERE locale = $1 AND slug = $2 AND ($3::uuid IS NULL OR product_id <> $3)) AS "x!""#,
                l,
                slug,
                id
            )
            .fetch_one(&mut *conn)
            .await?;
            if taken {
                f.add(format!("translations.{l}.slug"), "slug_taken");
            }
        }
    }
    if let Some(sku) = opt(&b.sku) {
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM products WHERE sku = $1::text::citext AND ($2::uuid IS NULL OR id <> $2)) AS "x!""#,
            sku,
            id
        )
        .fetch_one(&mut *conn)
        .await?;
        if taken {
            f.add("sku", "taken");
        }
    }
    let wrong_scope = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM categories WHERE id = ANY($1) AND scope <> 'product'"#,
        &b.category_ids
    )
    .fetch_one(&mut *conn)
    .await?;
    if wrong_scope > 0 {
        f.add("category_ids", "wrong_scope");
    }
    if let Some(p) = b.primary_category_id
        && !b.category_ids.contains(&p)
    {
        f.add("primary_category_id", "not_in_categories");
    }
    for (i, r) in b.related_ids.iter().enumerate() {
        if Some(*r) == id {
            f.add(format!("related_ids[{i}]"), "self");
        }
    }
    let related_kinds = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM products
           WHERE id = ANY($1) AND (kind NOT IN ('accommodation', 'activity', 'package', 'extra') OR deleted_at IS NOT NULL)"#,
        &b.related_ids
    )
    .fetch_one(&mut *conn)
    .await?;
    if related_kinds > 0 {
        f.add("related_ids", "not_bookable_kind");
    }
    let extra_ids: Vec<Uuid> = b.extras.iter().map(|e| e.extra_product_id).collect();
    let known_extras = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM extras x JOIN products p ON p.id = x.product_id
           WHERE x.product_id = ANY($1) AND p.deleted_at IS NULL"#,
        &extra_ids
    )
    .fetch_one(&mut *conn)
    .await?;
    if usize::try_from(known_extras).unwrap_or_default()
        != extra_ids
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
    {
        f.add("extras", "unknown_extra");
    }
    if extra_ids.iter().any(|e| Some(*e) == id) {
        f.add("extras", "self");
    }
    Ok(())
}

// ---- Writes ---------------------------------------------------------------------------------------

async fn write_common(
    conn: &mut PgConnection,
    id: Uuid,
    b: &ProductBody,
    staff: &Staff,
) -> AppResult<()> {
    // Gallery: created with the first image.
    let gallery_id = if b.gallery_media_ids.is_empty() {
        sqlx::query_scalar!("SELECT gallery_id FROM products WHERE id = $1", id)
            .fetch_one(&mut *conn)
            .await?
    } else {
        Some(
            match sqlx::query_scalar!("SELECT gallery_id FROM products WHERE id = $1", id)
                .fetch_one(&mut *conn)
                .await?
            {
                Some(g) => g,
                None => {
                    let g = Uuid::now_v7();
                    sqlx::query!(
                        "INSERT INTO galleries (id, name) VALUES ($1, $2)",
                        g,
                        format!("product {id}")
                    )
                    .execute(&mut *conn)
                    .await?;
                    g
                }
            },
        )
    };
    if let Some(g) = gallery_id {
        sqlx::query!("DELETE FROM gallery_items WHERE gallery_id = $1", g)
            .execute(&mut *conn)
            .await?;
        for (i, m) in b.gallery_media_ids.iter().enumerate() {
            sqlx::query!(
                "INSERT INTO gallery_items (gallery_id, media_id, sort_order) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                g,
                m,
                i as i32
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    sqlx::query!(
        "UPDATE products SET sku = $2::text::citext, booking_mode = $3, is_featured = $4, place_id = $5, cover_media_id = $6,
                video_media_id = $7, gallery_id = $8, updated_by = $9, updated_at = now()
         WHERE id = $1",
        id,
        opt(&b.sku),
        b.booking_mode,
        b.is_featured,
        b.place_id,
        b.cover_media_id,
        b.video_media_id,
        gallery_id,
        staff.id
    )
    .execute(&mut *conn)
    .await?;
    for (locale, t) in &b.translations {
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
        sqlx::query!(
            "INSERT INTO product_translations (product_id, locale, name, slug, subtitle, short_description, description,
                                               highlights, included, not_included, what_to_bring, requirements, important_info,
                                               seo_title, seo_description, og_media_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (product_id, locale) DO UPDATE SET name = $3, slug = $4, subtitle = $5, short_description = $6,
                 description = $7, highlights = $8, included = $9, not_included = $10, what_to_bring = $11,
                 requirements = $12, important_info = $13, seo_title = $14, seo_description = $15, og_media_id = $16",
            id,
            locale,
            t.name.trim(),
            slug_of(t),
            opt(&t.subtitle),
            opt(&t.short_description),
            opt(&t.description),
            &clean_list(&t.highlights),
            &clean_list(&t.included),
            &clean_list(&t.not_included),
            &clean_list(&t.what_to_bring),
            opt(&t.requirements),
            opt(&t.important_info),
            opt(&t.seo_title),
            opt(&t.seo_description),
            t.og_media_id
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!(
        "DELETE FROM product_categories WHERE product_id = $1 AND NOT (category_id = ANY($2))",
        id,
        &b.category_ids
    )
    .execute(&mut *conn)
    .await?;
    for c in &b.category_ids {
        sqlx::query!(
            "INSERT INTO product_categories (product_id, category_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            id,
            c
        )
        .execute(&mut *conn)
        .await?;
    }
    let primary = b.primary_category_id.or(b.category_ids.first().copied());
    sqlx::query!(
        "UPDATE product_categories SET is_primary = false WHERE product_id = $1 AND is_primary AND category_id IS DISTINCT FROM $2",
        id,
        primary
    )
    .execute(&mut *conn)
    .await?;
    if let Some(p) = primary {
        sqlx::query!(
            "UPDATE product_categories SET is_primary = true WHERE product_id = $1 AND category_id = $2",
            id,
            p
        )
        .execute(&mut *conn)
        .await?;
    }
    for (table, column, ids) in [
        ("product_tags", "tag_id", &b.tag_ids),
        ("product_amenities", "amenity_id", &b.amenity_ids),
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE product_id = $1 AND NOT ({column} = ANY($2))"
        ))
        .bind(id)
        .bind(ids)
        .execute(&mut *conn)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO {table} (product_id, {column}) SELECT $1, unnest($2::uuid[]) ON CONFLICT DO NOTHING"
        ))
        .bind(id)
        .bind(ids)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!("DELETE FROM related_products WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for (i, r) in b.related_ids.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO related_products (product_id, related_product_id, sort_order) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            id,
            r,
            i as i32
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!("DELETE FROM product_extras WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for (i, e) in b.extras.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO product_extras (product_id, extra_product_id, is_mandatory, sort_order) VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
            id,
            e.extra_product_id,
            e.is_mandatory,
            i as i32
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The tab sections of a save, each under its permission.
async fn write_sections(
    conn: &mut PgConnection,
    state: &AppState,
    staff: &Staff,
    kind: Kind,
    id: Uuid,
    b: &ProductBody,
) -> AppResult<Value> {
    let mut report = json!({});
    if let Some(steps) = &b.itinerary {
        staff.require("products.write")?;
        write_itinerary(conn, kind, id, steps).await?;
    }
    if let Some(items) = &b.items {
        staff.require("products.write")?;
        packages::write_items(conn, kind, id, items).await?;
    }
    if let Some(prices) = &b.prices {
        staff.require("pricing.write")?;
        pricing::write_prices(conn, kind, id, prices).await?;
    }
    if let Some(taxes) = &b.tax_ids {
        staff.require("pricing.write")?;
        pricing::write_taxes(conn, id, taxes).await?;
    }
    if let Some(reqs) = &b.resource_requirements {
        staff.require("inventory.write")?;
        activities::write_requirements(conn, kind, id, reqs).await?;
    }
    if let Some(schedules) = &b.schedules {
        staff.require("inventory.write")?;
        let sync = activities::write_schedules(conn, state, kind, id, schedules).await?;
        report["slots"] = json!(sync);
    }
    Ok(report)
}

// ---- Handlers -------------------------------------------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    staff: Staff,
    Path(kind): Path<String>,
    Json(b): Json<ProductBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.write")?;
    let details = parse_details(kind, &b.details)?;
    let mut f = Fields::default();
    validate_details(&details, &mut f);
    let mut tx = state.db.begin().await?;
    validate_common(&mut tx, None, kind, &b, &mut f).await?;
    f.check()?;
    let id = Uuid::now_v7();
    let booking_mode = forced_booking_mode(&details, &b.booking_mode).to_string();
    sqlx::query!(
        "INSERT INTO products (id, kind, status, booking_mode, sort_order, created_by, updated_by)
         VALUES ($1, $2, 'draft', $3, (SELECT COALESCE(max(sort_order) + 1, 0) FROM products WHERE kind = $2), $4, $4)",
        id,
        kind.product_kind(),
        booking_mode,
        staff.id
    )
    .execute(&mut *tx)
    .await?;
    write_details(&mut tx, id, &details, true).await?;
    write_common(&mut tx, id, &with_mode(b, &booking_mode), &staff).await?;
    if kind == Kind::Stays {
        inventory::generate_calendar_rows(
            &mut tx,
            Some(id),
            crate::camp_time::today(Utc::now()),
            inventory::CALENDAR_HORIZON_MONTHS,
        )
        .await?;
    }
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "product.created",
        json!({ "kind": kind.product_kind() }),
    )
    .await?;
    let out = detail_json(&mut tx, &state, kind, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

fn forced_booking_mode<'a>(details: &Details, asked: &'a str) -> &'a str {
    match details {
        // Activities on request can only be requested.
        Details::Activity(a) if a.scheduling_mode == "on_request" && asked == "instant" => {
            "on_request"
        }
        _ => asked,
    }
}

fn with_mode(mut b: ProductBody, mode: &str) -> ProductBody {
    b.booking_mode = mode.to_string();
    b
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<ProductBody>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.read")?;
    let touches_content = !b.details.is_null() || !b.translations.is_empty();
    if touches_content {
        staff.require("products.write")?;
    }
    let mut tx = state.db.begin().await?;
    let current = sqlx::query!(
        "SELECT updated_at, status FROM products WHERE id = $1 AND kind = $2 AND deleted_at IS NULL FOR UPDATE",
        id,
        kind.product_kind()
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    match b.updated_at {
        None => return Err(field_error("updated_at", "required")),
        Some(t) if t != current.updated_at => {
            return Err(AppError::domain(
                StatusCode::CONFLICT,
                "stale",
                "someone saved this product after you opened it",
            )
            .with_details(json!({ "updated_at": current.updated_at })));
        }
        Some(_) => {}
    }
    let mut report = json!({});
    if touches_content {
        let details = parse_details(kind, &b.details)?;
        let mut f = Fields::default();
        validate_details(&details, &mut f);
        validate_common(&mut tx, Some(id), kind, &b, &mut f).await?;
        if let Details::Stay(s) = &details {
            stays::check_mode_unchanged(&mut tx, id, s.inventory_mode.as_deref(), &mut f).await?;
        }
        f.check()?;
        let before = match kind.entity() {
            Some(e) => redirects::snapshot(&mut tx, e, id).await?,
            None => None,
        };
        let was_publishable =
            current.status == "published" && checklist_of(&mut tx, kind, id).await?.blocking_ok();
        let mode = forced_booking_mode(&details, &b.booking_mode).to_string();
        write_details(&mut tx, id, &details, false).await?;
        let b = with_mode(b, &mode);
        write_common(&mut tx, id, &b, &staff).await?;
        report = write_sections(&mut tx, &state, &staff, kind, id, &b).await?;
        if let Some(e) = kind.entity() {
            redirects::after_change(&mut tx, &staff.actor(), e, id, before).await?;
        }
        if was_publishable {
            // A published product that passed its blocking checks keeps passing them (an older
            // product that never did can still be edited).
            let list = checklist_of(&mut tx, kind, id).await?;
            if !list.blocking_ok() {
                return Err(publish_blocked(&list));
            }
        }
    } else {
        report = write_sections(&mut tx, &state, &staff, kind, id, &b).await?;
        sqlx::query!(
            "UPDATE products SET updated_by = $2, updated_at = now() WHERE id = $1",
            id,
            staff.id
        )
        .execute(&mut *tx)
        .await?;
    }
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "product.updated",
        json!({ "sections": sections_of_body_names(&report) }),
    )
    .await?;
    let mut out = detail_json(&mut tx, &state, kind, id).await?;
    tx.commit().await?;
    out["save_report"] = report;
    Ok(Json(out))
}

fn sections_of_body_names(report: &Value) -> Vec<String> {
    report
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

pub async fn detail(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    super::product_of_kind(&mut conn, kind, id).await?;
    Ok(Json(detail_json(&mut conn, &state, kind, id).await?))
}

pub async fn detail_json(
    conn: &mut PgConnection,
    state: &AppState,
    kind: Kind,
    id: Uuid,
) -> AppResult<Value> {
    let p = sqlx::query!(
        r#"SELECT p.id, p.kind, p.sku::text AS "sku?", p.status, p.booking_mode, p.is_featured, p.place_id, p.cover_media_id,
                  p.video_media_id, p.gallery_id, p.sort_order, p.published_at, p.created_at, p.updated_at,
                  COALESCE((SELECT array_agg(gi.media_id ORDER BY gi.sort_order) FROM gallery_items gi WHERE gi.gallery_id = p.gallery_id),
                           '{}') AS "gallery_media_ids!",
                  COALESCE((SELECT array_agg(pc.category_id ORDER BY pc.is_primary DESC, pc.created_at) FROM product_categories pc
                            WHERE pc.product_id = p.id), '{}') AS "category_ids!",
                  (SELECT pc.category_id FROM product_categories pc WHERE pc.product_id = p.id AND pc.is_primary) AS "primary_category_id?",
                  COALESCE((SELECT array_agg(tag_id) FROM product_tags WHERE product_id = p.id), '{}') AS "tag_ids!",
                  COALESCE((SELECT array_agg(amenity_id) FROM product_amenities WHERE product_id = p.id), '{}') AS "amenity_ids!",
                  COALESCE((SELECT array_agg(related_product_id ORDER BY sort_order) FROM related_products WHERE product_id = p.id),
                           '{}') AS "related_ids!",
                  COALESCE((SELECT array_agg(tax_id) FROM product_taxes WHERE product_id = p.id), '{}') AS "tax_ids!",
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('extra_product_id', extra_product_id, 'is_mandatory', is_mandatory)
                                             ORDER BY sort_order)
                            FROM product_extras WHERE product_id = p.id), '[]') AS "extras!",
                  EXISTS (SELECT 1 FROM booking_items i WHERE i.product_id = p.id) AS "booked!"
           FROM products p WHERE p.id = $1"#,
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    let translations: BTreeMap<String, Value> = sqlx::query!(
        "SELECT locale, name, slug, subtitle, short_description, description, highlights, included, not_included,
                what_to_bring, requirements, important_info, seo_title, seo_description, og_media_id
         FROM product_translations WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|t| {
        (
            t.locale,
            json!({
                "name": t.name, "slug": t.slug, "subtitle": t.subtitle, "short_description": t.short_description,
                "description": t.description, "highlights": t.highlights, "included": t.included,
                "not_included": t.not_included, "what_to_bring": t.what_to_bring, "requirements": t.requirements,
                "important_info": t.important_info, "seo_title": t.seo_title, "seo_description": t.seo_description,
                "og_media_id": t.og_media_id,
            }),
        )
    })
    .collect();
    let details = details_json(conn, kind, id).await?;
    let paths = match kind.entity() {
        Some(e) => {
            let ctx = crate::modules::seo::routes::Ctx::load(conn, Utc::now()).await?;
            crate::modules::seo::routes::paths_for(conn, &ctx, e, id).await?
        }
        None => BTreeMap::new(),
    };
    let media = media_info(
        conn,
        state,
        &[p.cover_media_id, p.video_media_id],
        &p.gallery_media_ids,
    )
    .await?;
    let used_in = used_in(conn, id).await?;
    Ok(json!({
        "id": p.id, "kind": p.kind, "segment": kind.segment(), "sku": p.sku, "status": p.status, "booking_mode": p.booking_mode,
        "is_featured": p.is_featured, "place_id": p.place_id, "cover_media_id": p.cover_media_id,
        "video_media_id": p.video_media_id, "gallery_id": p.gallery_id, "gallery_media_ids": p.gallery_media_ids,
        "sort_order": p.sort_order, "published_at": p.published_at, "created_at": p.created_at, "updated_at": p.updated_at,
        "translations": translations, "category_ids": p.category_ids, "primary_category_id": p.primary_category_id,
        "tag_ids": p.tag_ids, "amenity_ids": p.amenity_ids, "related_ids": p.related_ids, "tax_ids": p.tax_ids,
        "extras": p.extras, "details": details, "paths": paths, "media": media, "booked": p.booked,
        "used_in": used_in,
    }))
}

/// Media of the editor with their alt text per locale (cover and gallery need one).
async fn media_info(
    conn: &mut PgConnection,
    state: &AppState,
    singles: &[Option<Uuid>],
    gallery: &[Uuid],
) -> AppResult<Value> {
    let ids: Vec<Uuid> = singles
        .iter()
        .flatten()
        .copied()
        .chain(gallery.iter().copied())
        .collect();
    let rows = sqlx::query!(
        r#"SELECT m.id, m.object_key, m.kind, m.visibility,
                  COALESCE((SELECT jsonb_object_agg(t.locale, t.alt_text) FROM media_asset_translations t
                            WHERE t.media_asset_id = m.id AND t.alt_text IS NOT NULL AND t.alt_text <> ''), '{}') AS "alt!"
           FROM media_assets m WHERE m.id = ANY($1) AND m.deleted_at IS NULL"#,
        &ids
    )
    .fetch_all(conn)
    .await?;
    Ok(json!(rows
        .into_iter()
        .map(|m| (m.id.to_string(), json!({
            "url": (m.visibility == "public").then(|| state.media.public_url(&m.object_key)),
            "kind": m.kind, "alt": m.alt,
        })))
        .collect::<serde_json::Map<String, Value>>()))
}

/// Published packages that include the product, and products where it is a mandatory extra.
pub async fn used_in(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Value> {
    let packages = sqlx::query!(
        r#"SELECT DISTINCT p.id, p.status, COALESCE(t.name, '') AS "name!"
           FROM package_items i JOIN products p ON p.id = i.package_product_id AND p.deleted_at IS NULL
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = 'fr'
           WHERE i.item_product_id = $1"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    let mandatory = sqlx::query!(
        r#"SELECT p.id, p.kind, p.status, COALESCE(t.name, '') AS "name!"
           FROM product_extras e JOIN products p ON p.id = e.product_id AND p.deleted_at IS NULL
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = 'fr'
           WHERE e.extra_product_id = $1 AND e.is_mandatory"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(json!({
        "packages": packages.iter().map(|p| json!({ "id": p.id, "status": p.status, "name": p.name })).collect::<Vec<_>>(),
        "mandatory_extra_of": mandatory.iter().map(|p| json!({
            "id": p.id, "kind": p.kind, "segment": Kind::from_product_kind(&p.kind).map(Kind::segment), "status": p.status, "name": p.name,
        })).collect::<Vec<_>>(),
    }))
}

async fn details_json(conn: &mut PgConnection, kind: Kind, id: Uuid) -> AppResult<Value> {
    Ok(match kind {
        Kind::Stays => sqlx::query!(
            r#"SELECT accommodation_kind, inventory_mode, max_guests, max_adults, max_children, standard_occupancy,
                      bed_configuration, surface_m2, min_nights, max_nights, check_in_from, check_in_until, check_out_until,
                      pool_capacity, pitch_selection_extra_id
               FROM accommodation_types WHERE product_id = $1"#,
            id
        )
        .fetch_optional(conn)
        .await?
        .map(|a| json!({
            "accommodation_kind": a.accommodation_kind, "inventory_mode": a.inventory_mode, "max_guests": a.max_guests,
            "max_adults": a.max_adults, "max_children": a.max_children, "standard_occupancy": a.standard_occupancy,
            "bed_configuration": a.bed_configuration, "surface_m2": a.surface_m2.map(|d| d.normalize()), "min_nights": a.min_nights,
            "max_nights": a.max_nights, "check_in_from": a.check_in_from, "check_in_until": a.check_in_until,
            "check_out_until": a.check_out_until, "pool_capacity": a.pool_capacity,
            "pitch_selection_extra_id": a.pitch_selection_extra_id,
        })),
        Kind::Activities => sqlx::query!(
            "SELECT location_type, scheduling_mode, duration_minutes, difficulty, min_participants, max_participants, min_age,
                    is_guided, guide_languages, meeting_place_id, booking_cutoff_hours, pickup_available, standalone_bookable
             FROM activities WHERE product_id = $1",
            id
        )
        .fetch_optional(conn)
        .await?
        .map(|a| json!({
            "location_type": a.location_type, "scheduling_mode": a.scheduling_mode, "duration_minutes": a.duration_minutes,
            "difficulty": a.difficulty, "min_participants": a.min_participants, "max_participants": a.max_participants,
            "min_age": a.min_age, "is_guided": a.is_guided, "guide_languages": a.guide_languages,
            "meeting_place_id": a.meeting_place_id, "booking_cutoff_hours": a.booking_cutoff_hours,
            "pickup_available": a.pickup_available, "standalone_bookable": a.standalone_bookable,
        })),
        Kind::Packages => sqlx::query!(
            "SELECT duration_days, duration_nights, pricing_mode, discount_percent FROM packages WHERE product_id = $1",
            id
        )
        .fetch_optional(conn)
        .await?
        .map(|p| json!({
            "duration_days": p.duration_days, "duration_nights": p.duration_nights, "pricing_mode": p.pricing_mode,
            "discount_percent": p.discount_percent.normalize(),
        })),
        Kind::Extras => sqlx::query!(
            "SELECT applies_to, max_quantity, requires_date, stock_quantity FROM extras WHERE product_id = $1",
            id
        )
        .fetch_optional(conn)
        .await?
        .map(|e| json!({
            "applies_to": e.applies_to, "max_quantity": e.max_quantity, "requires_date": e.requires_date,
            "stock_quantity": e.stock_quantity,
        })),
    }
    .unwrap_or(Value::Null))
}

// ---- List -----------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub q: Option<String>,
    pub status: Option<String>,
    pub category_id: Option<Uuid>,
    pub place_id: Option<Uuid>,
    pub booking_mode: Option<String>,
    /// `manual` (default: `sort_order`), `name`, `updated`.
    pub sort: Option<String>,
    /// Admin language of the names (fallback `fr`).
    pub locale: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    staff: Staff,
    Path(kind): Path<String>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.read")?;
    let locale = q.locale.clone().unwrap_or_else(|| "fr".into());
    let search =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
    let now = Utc::now();
    let rows = sqlx::query!(
        r#"SELECT p.id, p.status, p.booking_mode, p.sort_order, p.updated_at, p.sku::text AS "sku?", p.is_featured,
                  COALESCE(NULLIF(t.name, ''), d.name, '') AS "name!", m.object_key AS "cover_key?", m.visibility AS "cover_visibility?",
                  ARRAY(SELECT l FROM unnest(ARRAY['fr', 'en', 'ar']) l
                        WHERE NOT EXISTS (SELECT 1 FROM product_translations x WHERE x.product_id = p.id AND x.locale = l
                                          AND x.name <> '')) AS "missing!",
                  a.inventory_mode AS "inventory_mode?", a.max_guests AS "max_guests?", a.pool_capacity AS "pool_capacity?",
                  (SELECT count(*) FROM accommodation_units u WHERE u.product_id = p.id AND u.status = 'active') AS "units!",
                  act.duration_minutes AS "duration_minutes?", act.scheduling_mode AS "scheduling_mode?",
                  (SELECT min(s.starts_at) FROM activity_slots s WHERE s.product_id = p.id AND s.starts_at > $4 AND s.status = 'open')
                      AS "next_departure?",
                  (SELECT sum(GREATEST(s.capacity - s.reserved, 0))::int8 FROM activity_slots s WHERE s.product_id = p.id
                      AND s.starts_at > $4 AND s.starts_at < $4 + interval '7 days' AND s.status = 'open') AS "seats_week?",
                  pk.duration_nights AS "nights?", pk.duration_days AS "days?", pk.pricing_mode AS "pricing_mode?",
                  (SELECT count(*) FROM package_items i WHERE i.package_product_id = p.id) AS "items!",
                  x.applies_to AS "applies_to?", x.stock_quantity AS "stock_quantity?"
           FROM products p
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2
           LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = 'fr'
           LEFT JOIN media_assets m ON m.id = p.cover_media_id AND m.deleted_at IS NULL
           LEFT JOIN accommodation_types a ON a.product_id = p.id
           LEFT JOIN activities act ON act.product_id = p.id
           LEFT JOIN packages pk ON pk.product_id = p.id
           LEFT JOIN extras x ON x.product_id = p.id
           WHERE p.kind = $1 AND p.deleted_at IS NULL
             AND ($3::text IS NULL OR t.name ILIKE $3 OR d.name ILIKE $3 OR p.sku ILIKE $3)
             AND ($5::text IS NULL OR p.status = $5)
             AND ($6::uuid IS NULL OR EXISTS (SELECT 1 FROM product_categories pc WHERE pc.product_id = p.id AND pc.category_id = $6))
             AND ($7::uuid IS NULL OR p.place_id = $7)
             AND ($8::text IS NULL OR p.booking_mode = $8)
           ORDER BY CASE WHEN $9 = 'name' THEN COALESCE(NULLIF(t.name, ''), d.name) END,
                    CASE WHEN $9 = 'updated' THEN p.updated_at END DESC,
                    p.sort_order, p.created_at"#,
        kind.product_kind(),
        locale,
        search,
        now,
        q.status,
        q.category_id,
        q.place_id,
        q.booking_mode,
        q.sort.as_deref().unwrap_or("manual")
    )
    .fetch_all(&state.db)
    .await?;
    let mut data = Vec::with_capacity(rows.len());
    for r in rows {
        let from = from_price(&state, kind, r.id).await.unwrap_or(None);
        data.push(json!({
            "id": r.id, "name": r.name, "status": r.status, "booking_mode": r.booking_mode, "sort_order": r.sort_order,
            "updated_at": r.updated_at, "sku": r.sku, "is_featured": r.is_featured, "missing_locales": r.missing,
            "cover_url": r.cover_key.filter(|_| r.cover_visibility.as_deref() == Some("public")).map(|k| state.media.public_url(&k)),
            "from_price_minor": from,
            "numbers": match kind {
                Kind::Stays => json!({ "inventory_mode": r.inventory_mode, "units": r.units, "pool_capacity": r.pool_capacity, "max_guests": r.max_guests }),
                Kind::Activities => json!({ "duration_minutes": r.duration_minutes, "scheduling_mode": r.scheduling_mode,
                                            "next_departure": r.next_departure, "seats_left_week": r.seats_week }),
                Kind::Packages => json!({ "nights": r.nights, "days": r.days, "items": r.items, "pricing_mode": r.pricing_mode }),
                Kind::Extras => json!({ "applies_to": r.applies_to, "stock_quantity": r.stock_quantity }),
            },
        }));
    }
    Ok(Json(json!({ "data": data })))
}

/// Lowest price of the next 30 days from the pricing engine (one night / one guest / one unit),
/// cached 10 minutes under `catalog:` (dropped by every product or pricing change).
pub async fn from_price(state: &AppState, kind: Kind, id: Uuid) -> AppResult<Option<i64>> {
    let key = format!(
        "{}admin-from:{id}",
        crate::services::cache_invalidation::CATALOG_PREFIX
    );
    if let Some(v) = state.cache.get_json::<Option<i64>>(&key).await {
        return Ok(v);
    }
    let mut conn = state.db.acquire().await?;
    let v = pricing::lowest_price(&mut conn, state, kind, id, 30).await?;
    state.cache.set_json(&key, &v, FROM_TTL).await;
    Ok(v)
}

// ---- Status ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub code: &'static str,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Checklist {
    pub blocking: Vec<Check>,
    pub warnings: Vec<Check>,
}

impl Checklist {
    pub fn blocking_ok(&self) -> bool {
        self.blocking.iter().all(|c| c.ok)
    }
}

fn publish_blocked(list: &Checklist) -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "publish_blocked",
        "some required items are missing",
    )
    .with_details(json!({ "checklist": list }))
}

pub async fn checklist_of(conn: &mut PgConnection, kind: Kind, id: Uuid) -> AppResult<Checklist> {
    let c = sqlx::query!(
        r#"SELECT p.booking_mode, p.cover_media_id, p.gallery_id,
                  fr.name AS "fr_name?", fr.slug AS "fr_slug?", fr.short_description AS "fr_short?", fr.seo_description AS "fr_seo?",
                  EXISTS (SELECT 1 FROM product_translations x WHERE x.product_id = p.id AND x.locale = 'en' AND x.name <> '') AS "has_en!",
                  EXISTS (SELECT 1 FROM product_translations x WHERE x.product_id = p.id AND x.locale = 'ar' AND x.name <> '') AS "has_ar!",
                  EXISTS (SELECT 1 FROM media_asset_translations mt WHERE mt.media_asset_id = p.cover_media_id
                          AND mt.locale = 'fr' AND btrim(COALESCE(mt.alt_text, '')) <> '') AS "cover_alt!",
                  EXISTS (SELECT 1 FROM gallery_items gi WHERE gi.gallery_id = p.gallery_id) AS "has_gallery!",
                  NOT EXISTS (SELECT 1 FROM gallery_items gi WHERE gi.gallery_id = p.gallery_id
                              AND NOT EXISTS (SELECT 1 FROM media_asset_translations mt WHERE mt.media_asset_id = gi.media_id
                                              AND mt.locale = 'fr' AND btrim(COALESCE(mt.alt_text, '')) <> '')) AS "gallery_alt!",
                  EXISTS (SELECT 1 FROM product_categories pc WHERE pc.product_id = p.id) AS "has_category!",
                  EXISTS (SELECT 1 FROM prices pr WHERE pr.product_id = p.id AND pr.is_active) AS "has_price!",
                  -- A cancellation policy reachable through a rate plan the product's prices allow.
                  EXISTS (SELECT 1 FROM rate_plans rp
                          WHERE rp.is_active AND rp.cancellation_policy_id IS NOT NULL
                            AND EXISTS (SELECT 1 FROM prices pr WHERE pr.product_id = p.id AND pr.is_active
                                        AND (pr.rate_plan_id IS NULL OR pr.rate_plan_id = rp.id))) AS "has_policy!"
           FROM products p
           LEFT JOIN product_translations fr ON fr.product_id = p.id AND fr.locale = 'fr'
           WHERE p.id = $1"#,
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    let filled = |s: &Option<String>| s.as_deref().is_some_and(|x| !x.trim().is_empty());
    let mut blocking = vec![
        Check {
            code: "fr_name",
            ok: filled(&c.fr_name),
        },
        Check {
            code: "fr_slug",
            ok: filled(&c.fr_slug),
        },
        Check {
            code: "fr_short_description",
            ok: filled(&c.fr_short),
        },
        Check {
            code: "cover",
            ok: c.cover_media_id.is_some(),
        },
        Check {
            code: "cover_alt",
            ok: c.cover_media_id.is_some() && c.cover_alt,
        },
    ];
    let bookable = c.booking_mode != "not_bookable";
    match kind {
        Kind::Packages => {
            let pk = packages::summary(conn, id).await?;
            blocking.push(Check {
                code: "package_items",
                ok: pk.items > 0,
            });
            blocking.push(Check {
                code: "package_items_published",
                ok: pk.unpublished_required == 0,
            });
            if bookable {
                blocking.push(Check {
                    code: "price",
                    ok: if pk.pricing_mode == "fixed" {
                        c.has_price
                    } else {
                        pk.items > 0 && pk.items_without_price == 0
                    },
                });
                blocking.push(Check {
                    code: "cancellation_policy",
                    ok: pk.pricing_mode != "fixed" || c.has_policy,
                });
            }
        }
        _ if bookable => {
            blocking.push(Check {
                code: "price",
                ok: c.has_price,
            });
            if kind != Kind::Extras {
                blocking.push(Check {
                    code: "cancellation_policy",
                    ok: c.has_policy,
                });
            }
        }
        _ => {}
    }
    match kind {
        Kind::Stays => blocking.push(Check {
            code: "inventory",
            ok: stays::has_inventory(conn, id).await?,
        }),
        Kind::Activities => {
            if let Some(ok) = activities::departures_ok(conn, id).await? {
                blocking.push(Check {
                    code: "departures",
                    ok,
                });
            }
        }
        _ => {}
    }
    let warnings = vec![
        Check {
            code: "en_missing",
            ok: c.has_en,
        },
        Check {
            code: "ar_missing",
            ok: c.has_ar,
        },
        Check {
            code: "gallery",
            ok: c.has_gallery,
        },
        Check {
            code: "gallery_alt",
            ok: c.gallery_alt,
        },
        Check {
            code: "seo_description",
            ok: filled(&c.fr_seo),
        },
        Check {
            code: "category",
            ok: c.has_category,
        },
    ];
    Ok(Checklist { blocking, warnings })
}

pub async fn checklist(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Checklist>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    super::product_of_kind(&mut conn, kind, id).await?;
    Ok(Json(checklist_of(&mut conn, kind, id).await?))
}

/// Published packages using the product, products where it is a mandatory extra: it cannot
/// leave the site before they stop depending on it.
async fn dependants(conn: &mut PgConnection, id: Uuid) -> AppResult<()> {
    let used = used_in(conn, id).await?;
    let packages: Vec<Value> = used["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["status"] == "published")
        .cloned()
        .collect();
    let mandatory = used["mandatory_extra_of"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if packages.is_empty() && mandatory.is_empty() {
        return Ok(());
    }
    Err(AppError::domain(
        StatusCode::CONFLICT,
        "in_use",
        "used in a published package or as a mandatory extra",
    )
    .with_details(json!({ "packages": packages, "mandatory_extra_of": mandatory })))
}

pub async fn set_status(
    conn: &mut PgConnection,
    actor: &Actor,
    kind: Kind,
    id: Uuid,
    to: &str,
) -> AppResult<()> {
    let current = sqlx::query_scalar!(
        "SELECT status FROM products WHERE id = $1 AND kind = $2 AND deleted_at IS NULL FOR UPDATE",
        id,
        kind.product_kind()
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    if current == to {
        return Ok(());
    }
    if to == "published" {
        let list = checklist_of(conn, kind, id).await?;
        if !list.blocking_ok() {
            return Err(publish_blocked(&list));
        }
    } else if current == "published" {
        dependants(conn, id).await?;
    }
    let before = match kind.entity() {
        Some(e) => redirects::snapshot(conn, e, id).await?,
        None => None,
    };
    sqlx::query!(
        "UPDATE products SET status = $2, updated_by = $3,
                published_at = CASE WHEN $2 = 'published' THEN COALESCE(published_at, now()) ELSE published_at END
         WHERE id = $1",
        id,
        to,
        actor.user_id
    )
    .execute(&mut *conn)
    .await?;
    if let Some(e) = kind.entity() {
        redirects::after_change(conn, actor, e, id, before).await?;
    }
    changed(
        conn,
        actor,
        kind,
        id,
        "product.status_changed",
        json!({ "from": current, "to": to }),
    )
    .await
}

async fn status_handler(
    state: &AppState,
    staff: &Staff,
    kind: &str,
    id: Uuid,
    to: &str,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(kind)?;
    staff.require("products.write")?;
    let mut tx = state.db.begin().await?;
    set_status(&mut tx, &staff.actor(), kind, id, to).await?;
    let out = detail_json(&mut tx, state, kind, id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

pub async fn publish(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    status_handler(&state, &staff, &kind, id, "published").await
}

pub async fn unpublish(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    status_handler(&state, &staff, &kind, id, "draft").await
}

pub async fn archive(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    status_handler(&state, &staff, &kind, id, "archived").await
}

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    /// The French name, typed by staff when the product has prices, schedules or units.
    pub confirm: Option<String>,
}

/// Soft delete, only for products never booked (otherwise: archive).
pub async fn delete(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Query(q): Query<DeleteQuery>,
) -> AppResult<StatusCode> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.write")?;
    let mut tx = state.db.begin().await?;
    let p = sqlx::query!(
        r#"SELECT COALESCE(t.name, '') AS "name!",
                  EXISTS (SELECT 1 FROM booking_items i WHERE i.product_id = p.id) AS "booked!",
                  (EXISTS (SELECT 1 FROM prices WHERE product_id = p.id)
                   OR EXISTS (SELECT 1 FROM activity_schedules WHERE product_id = p.id)
                   OR EXISTS (SELECT 1 FROM accommodation_units WHERE product_id = p.id)) AS "has_setup!"
           FROM products p LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = 'fr'
           WHERE p.id = $1 AND p.kind = $2 AND p.deleted_at IS NULL FOR UPDATE OF p"#,
        id,
        kind.product_kind()
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if p.booked {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "booked",
            "this product has bookings: archive it instead",
        ));
    }
    if p.has_setup && q.confirm.as_deref().map(str::trim) != Some(p.name.trim()) {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm_name",
            "type the product name to delete it",
        ));
    }
    let used = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM package_items i JOIN products k ON k.id = i.package_product_id
                          WHERE i.item_product_id = $1 AND k.deleted_at IS NULL) AS "x!""#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if used {
        dependants(&mut tx, id).await?;
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "in_use",
            "remove it from the packages that include it first",
        ));
    }
    let before = match kind.entity() {
        Some(e) => redirects::snapshot(&mut tx, e, id).await?,
        None => None,
    };
    sqlx::query!(
        "UPDATE products SET deleted_at = now(), status = 'archived', updated_by = $2 WHERE id = $1",
        id,
        staff.id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM product_extras WHERE extra_product_id = $1", id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM related_products WHERE related_product_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;
    if let Some(e) = kind.entity() {
        redirects::after_change(&mut tx, &staff.actor(), e, id, before).await?;
    }
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "product.deleted",
        json!({ "name": p.name }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Duplicate ------------------------------------------------------------------------------------

/// The slug of a copy: fr `-copie`, en `-copy`, ar (and others) the shop rule (`-<id suffix>`);
/// `-2`, `-3`... when taken.
pub fn slug_for_copy(
    locale: &str,
    slug: &str,
    id_suffix: &str,
    taken: &dyn Fn(&str) -> bool,
) -> String {
    let base = match locale {
        "fr" => format!("{slug}-copie"),
        "en" => format!("{slug}-copy"),
        _ => format!("{slug}-{id_suffix}"),
    };
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|s| !taken(s))
        .unwrap_or(base)
}

pub async fn duplicate(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.write")?;
    let mut tx = state.db.begin().await?;
    super::product_of_kind(&mut tx, kind, id).await?;
    let new_id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO products (id, kind, status, booking_mode, is_featured, place_id, cover_media_id, video_media_id, sort_order,
                               created_by, updated_by)
         SELECT $2, kind, 'draft', booking_mode, false, place_id, cover_media_id, video_media_id,
                (SELECT COALESCE(max(sort_order) + 1, 0) FROM products WHERE kind = p.kind), $3, $3
         FROM products p WHERE id = $1",
        id,
        new_id,
        staff.id
    )
    .execute(&mut *tx)
    .await?;
    // Detail row (never units, slots or calendar rows).
    match kind {
        Kind::Stays => sqlx::query!(
            "INSERT INTO accommodation_types (product_id, accommodation_kind, inventory_mode, max_guests, max_adults, max_children,
                                              standard_occupancy, bed_configuration, surface_m2, min_nights, max_nights,
                                              check_in_from, check_in_until, check_out_until, pool_capacity)
             SELECT $2, accommodation_kind, inventory_mode, max_guests, max_adults, max_children, standard_occupancy,
                    bed_configuration, surface_m2, min_nights, max_nights, check_in_from, check_in_until, check_out_until,
                    pool_capacity
             FROM accommodation_types WHERE product_id = $1",
            id,
            new_id
        )
        .execute(&mut *tx)
        .await?,
        Kind::Activities => sqlx::query!(
            "INSERT INTO activities (product_id, location_type, scheduling_mode, duration_minutes, difficulty, min_participants,
                                     max_participants, min_age, is_guided, guide_languages, meeting_place_id,
                                     booking_cutoff_hours, pickup_available, standalone_bookable)
             SELECT $2, location_type, scheduling_mode, duration_minutes, difficulty, min_participants, max_participants,
                    min_age, is_guided, guide_languages, meeting_place_id, booking_cutoff_hours, pickup_available,
                    standalone_bookable
             FROM activities WHERE product_id = $1",
            id,
            new_id
        )
        .execute(&mut *tx)
        .await?,
        Kind::Packages => sqlx::query!(
            "INSERT INTO packages (product_id, duration_days, duration_nights, pricing_mode, discount_percent)
             SELECT $2, duration_days, duration_nights, pricing_mode, discount_percent FROM packages WHERE product_id = $1",
            id,
            new_id
        )
        .execute(&mut *tx)
        .await?,
        Kind::Extras => sqlx::query!(
            "INSERT INTO extras (product_id, applies_to, max_quantity, requires_date, stock_quantity)
             SELECT $2, applies_to, max_quantity, requires_date, stock_quantity FROM extras WHERE product_id = $1",
            id,
            new_id
        )
        .execute(&mut *tx)
        .await?,
    };
    // Translations with copy slugs.
    let suffix = new_id.simple().to_string()[24..].to_string();
    let taken: Vec<(String, String)> =
        sqlx::query!("SELECT locale, slug FROM product_translations")
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|r| (r.locale, r.slug))
            .collect();
    for t in sqlx::query!(
        "SELECT locale, name, slug FROM product_translations WHERE product_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?
    {
        let slug = slug_for_copy(&t.locale, &t.slug, &suffix, &|s| {
            taken.iter().any(|(l, x)| *l == t.locale && x == s)
        });
        let name = match t.locale.as_str() {
            "fr" => format!("{} (copie)", t.name),
            "en" => format!("{} (copy)", t.name),
            "ar" => format!("{} (نسخة)", t.name),
            _ => t.name.clone(),
        };
        sqlx::query!(
            "INSERT INTO product_translations (product_id, locale, name, slug, subtitle, short_description, description, highlights,
                                               included, not_included, what_to_bring, requirements, important_info, seo_title,
                                               seo_description, og_media_id)
             SELECT $2, locale, $4, $5, subtitle, short_description, description, highlights, included, not_included,
                    what_to_bring, requirements, important_info, seo_title, seo_description, og_media_id
             FROM product_translations WHERE product_id = $1 AND locale = $3",
            id,
            new_id,
            t.locale,
            name,
            slug
        )
        .execute(&mut *tx)
        .await?;
    }
    // Gallery: a new one with the same images.
    if let Some(g) = sqlx::query_scalar!("SELECT gallery_id FROM products WHERE id = $1", id)
        .fetch_one(&mut *tx)
        .await?
    {
        let ng = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO galleries (id, name) VALUES ($1, $2)",
            ng,
            format!("product {new_id}")
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO gallery_items (gallery_id, media_id, sort_order) SELECT $2, media_id, sort_order FROM gallery_items WHERE gallery_id = $1",
            g,
            ng
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE products SET gallery_id = $2 WHERE id = $1",
            new_id,
            ng
        )
        .execute(&mut *tx)
        .await?;
    }
    for sql in [
        "INSERT INTO product_categories (product_id, category_id, is_primary) SELECT $2, category_id, is_primary FROM product_categories WHERE product_id = $1",
        "INSERT INTO product_tags (product_id, tag_id) SELECT $2, tag_id FROM product_tags WHERE product_id = $1",
        "INSERT INTO product_amenities (product_id, amenity_id) SELECT $2, amenity_id FROM product_amenities WHERE product_id = $1",
        "INSERT INTO related_products (product_id, related_product_id, sort_order) SELECT $2, related_product_id, sort_order FROM related_products WHERE product_id = $1",
        "INSERT INTO product_extras (product_id, extra_product_id, is_mandatory, sort_order) SELECT $2, extra_product_id, is_mandatory, sort_order FROM product_extras WHERE product_id = $1",
        "INSERT INTO product_taxes (product_id, tax_id) SELECT $2, tax_id FROM product_taxes WHERE product_id = $1",
        "INSERT INTO prices (product_id, rate_plan_id, season_id, guest_category_id, pricing_unit, amount_minor, currency,
                             extra_guest_amount_minor, min_quantity, max_quantity, min_nights, days_of_week, valid_period, priority, is_active)
         SELECT $2, rate_plan_id, season_id, guest_category_id, pricing_unit, amount_minor, currency, extra_guest_amount_minor,
                min_quantity, max_quantity, min_nights, days_of_week, valid_period, priority, is_active FROM prices WHERE product_id = $1",
        "INSERT INTO product_resource_requirements (product_id, resource_kind, units_per_group, participants_per_group)
         SELECT $2, resource_kind, units_per_group, participants_per_group FROM product_resource_requirements WHERE product_id = $1",
        // Schedules come inactive: no departure exists until staff turn them on.
        "INSERT INTO activity_schedules (product_id, days_of_week, start_time, capacity, valid_period, is_active)
         SELECT $2, days_of_week, start_time, capacity, valid_period, false FROM activity_schedules WHERE product_id = $1",
        "INSERT INTO package_items (package_product_id, item_product_id, quantity, day_number, is_optional, sort_order)
         SELECT $2, item_product_id, quantity, day_number, is_optional, sort_order FROM package_items WHERE package_product_id = $1",
    ] {
        sqlx::query(sql).bind(id).bind(new_id).execute(&mut *tx).await?;
    }
    for step in sqlx::query!("SELECT id FROM itinerary_steps WHERE product_id = $1", id)
        .fetch_all(&mut *tx)
        .await?
    {
        let ns = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO itinerary_steps (id, product_id, day_number, sort_order, place_id, duration_minutes, media_id)
             SELECT $3, $2, day_number, sort_order, place_id, duration_minutes, media_id FROM itinerary_steps WHERE id = $1",
            step.id,
            new_id,
            ns
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO itinerary_step_translations (itinerary_step_id, locale, title, description)
             SELECT $2, locale, title, description FROM itinerary_step_translations WHERE itinerary_step_id = $1",
            step.id,
            ns
        )
        .execute(&mut *tx)
        .await?;
    }
    if kind == Kind::Stays {
        inventory::generate_calendar_rows(
            &mut tx,
            Some(new_id),
            crate::camp_time::today(Utc::now()),
            inventory::CALENDAR_HORIZON_MONTHS,
        )
        .await?;
    }
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        new_id,
        "product.duplicated",
        json!({ "from": id }),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": new_id }))))
}

// ---- Order and bulk -------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct OrderBody {
    pub ids: Vec<Uuid>,
}

pub fn sort_order_body(ids: &[Uuid]) -> Vec<(Uuid, i32)> {
    ids.iter()
        .enumerate()
        .map(|(i, id)| (*id, i32::try_from(i).unwrap_or(i32::MAX)))
        .collect()
}

pub async fn reorder(
    State(state): State<AppState>,
    staff: Staff,
    Path(kind): Path<String>,
    Json(b): Json<OrderBody>,
) -> AppResult<StatusCode> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.write")?;
    let mut tx = state.db.begin().await?;
    for (id, n) in sort_order_body(&b.ids) {
        sqlx::query!(
            "UPDATE products SET sort_order = $3 WHERE id = $1 AND kind = $2 AND sort_order <> $3",
            id,
            kind.product_kind(),
            n
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(first) = b.ids.first() {
        changed(
            &mut tx,
            &staff.actor(),
            kind,
            *first,
            "product.reordered",
            json!({ "count": b.ids.len() }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct BulkBody {
    /// `publish`, `unpublish`, `archive`.
    pub action: String,
    pub ids: Vec<Uuid>,
}

/// Each product in its own transaction: one refusal does not stop the others.
pub async fn bulk(
    State(state): State<AppState>,
    staff: Staff,
    Path(kind): Path<String>,
    Json(b): Json<BulkBody>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.write")?;
    let to = match b.action.as_str() {
        "publish" => "published",
        "unpublish" => "draft",
        "archive" => "archived",
        _ => return Err(field_error("action", "invalid")),
    };
    let mut results = Vec::new();
    for id in b.ids.iter().take(200) {
        let mut tx = state.db.begin().await?;
        match set_status(&mut tx, &staff.actor(), kind, *id, to).await {
            Ok(()) => {
                tx.commit().await?;
                results.push(json!({ "id": id, "ok": true }));
            }
            Err(e) => {
                let (code, details) = match &e {
                    AppError::Domain { code, details, .. } => (*code, details.clone()),
                    AppError::NotFound => ("not_found", None),
                    _ => return Err(e),
                };
                results.push(json!({ "id": id, "ok": false, "code": code, "details": details }));
            }
        }
    }
    Ok(Json(json!({ "results": results })))
}

// ---- Itinerary (activities and packages) ----------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct StepBody {
    #[serde(default = "one")]
    pub day_number: i32,
    #[serde(default)]
    pub place_id: Option<Uuid>,
    #[serde(default)]
    pub duration_minutes: Option<i32>,
    #[serde(default)]
    pub media_id: Option<Uuid>,
    /// `{locale: {title, description}}`.
    #[serde(default)]
    pub translations: BTreeMap<String, StepText>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StepText {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

pub async fn write_itinerary(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    steps: &[StepBody],
) -> AppResult<()> {
    if !matches!(kind, Kind::Activities | Kind::Packages) {
        return Err(field_error("itinerary", "not_allowed"));
    }
    let days = sqlx::query_scalar!(
        "SELECT duration_days FROM packages WHERE product_id = $1",
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    let mut f = Fields::default();
    for (i, s) in steps.iter().enumerate() {
        if s.day_number < 1 || days.is_some_and(|d| s.day_number > d) {
            f.add(format!("itinerary[{i}].day_number"), "range");
        }
        if s.duration_minutes.is_some_and(|d| d < 1) {
            f.add(format!("itinerary[{i}].duration_minutes"), "min");
        }
        if s.translations
            .get("fr")
            .is_none_or(|t| t.title.trim().is_empty())
        {
            f.add(format!("itinerary[{i}].translations.fr.title"), "required");
        }
    }
    f.check()?;
    sqlx::query!("DELETE FROM itinerary_steps WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for (i, s) in steps.iter().enumerate() {
        let step = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO itinerary_steps (id, product_id, day_number, sort_order, place_id, duration_minutes, media_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            step,
            id,
            s.day_number,
            i as i32,
            s.place_id,
            s.duration_minutes,
            s.media_id
        )
        .execute(&mut *conn)
        .await?;
        for (l, t) in &s.translations {
            if t.title.trim().is_empty() || !LOCALES.contains(&l.as_str()) {
                continue;
            }
            sqlx::query!(
                "INSERT INTO itinerary_step_translations (itinerary_step_id, locale, title, description) VALUES ($1, $2, $3, $4)",
                step,
                l,
                t.title.trim(),
                opt(&t.description)
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

pub async fn itinerary(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    super::product_of_kind(&mut conn, kind, id).await?;
    let steps = sqlx::query!(
        r#"SELECT s.id, s.day_number, s.sort_order, s.place_id, s.duration_minutes, s.media_id,
                  COALESCE((SELECT jsonb_object_agg(t.locale, jsonb_build_object('title', t.title, 'description', t.description))
                            FROM itinerary_step_translations t WHERE t.itinerary_step_id = s.id), '{}') AS "translations!"
           FROM itinerary_steps s WHERE s.product_id = $1 ORDER BY s.day_number, s.sort_order"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(json!({
        "data": steps.iter().map(|s| json!({
            "id": s.id, "day_number": s.day_number, "sort_order": s.sort_order, "place_id": s.place_id,
            "duration_minutes": s.duration_minutes, "media_id": s.media_id, "translations": s.translations,
        })).collect::<Vec<_>>(),
    })))
}

pub async fn put_itinerary(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(steps): Json<Vec<StepBody>>,
) -> AppResult<StatusCode> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.write")?;
    let mut tx = state.db.begin().await?;
    super::product_of_kind(&mut tx, kind, id).await?;
    write_itinerary(&mut tx, kind, id, &steps).await?;
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "product.itinerary_updated",
        json!({ "steps": steps.len() }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
