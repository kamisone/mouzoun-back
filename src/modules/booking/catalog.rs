//! Public catalogue reads for the funnel: product cards, product pages (every fact the guest
//! needs before booking), guest categories. Text falls back field by field to the default
//! locale. Cached in Redis under `catalog:` (invalidated by `product.updated`).

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    db::catalog::{ProductKind, SchedulingMode},
    error::{AppError, AppResult},
    state::AppState,
    web::client::RequestLocale,
};

const TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaRef {
    pub id: Uuid,
    pub url: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub blurhash: Option<String>,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rating {
    pub average: f64,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductCard {
    pub id: Uuid,
    pub kind: ProductKind,
    pub slug: String,
    pub name: String,
    pub subtitle: Option<String>,
    pub short_description: Option<String>,
    pub booking_mode: String,
    pub is_featured: bool,
    pub cover: Option<MediaRef>,
    /// Lowest active price and its unit ("from 45 € per night").
    pub from_price_minor: Option<i64>,
    pub from_price_unit: Option<String>,
    pub rating: Option<Rating>,
    pub facts: Value,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub kind: Option<ProductKind>,
    pub featured: Option<bool>,
}

fn media(
    state: &AppState,
    id: Option<Uuid>,
    key: Option<String>,
    width: Option<i32>,
    height: Option<i32>,
    blurhash: Option<String>,
    alt: Option<String>,
) -> Option<MediaRef> {
    Some(MediaRef {
        id: id?,
        url: state.media.public_url(&key?),
        width,
        height,
        blurhash,
        alt,
    })
}

pub async fn list(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<ProductCard>>> {
    let key = format!(
        "catalog:list:{}:{}:{}",
        locale.as_str(),
        q.kind.map_or("all", |k| k.as_str()),
        q.featured.unwrap_or(false)
    );
    if let Some(cached) = state.cache.get_json::<Vec<ProductCard>>(&key).await {
        return Ok(Json(cached));
    }
    let cards = cards(
        &state,
        locale.as_str(),
        q.kind,
        q.featured.unwrap_or(false),
        None,
    )
    .await?;
    state.cache.set_json(&key, &cards, TTL).await;
    Ok(Json(cards))
}

pub(crate) async fn cards(
    state: &AppState,
    locale: &str,
    kind: Option<ProductKind>,
    featured_only: bool,
    ids: Option<&[Uuid]>,
) -> AppResult<Vec<ProductCard>> {
    let rows = sqlx::query!(
        r#"SELECT p.id, p.kind AS "kind: ProductKind", p.booking_mode, p.is_featured,
                  COALESCE(NULLIF(t.name, ''), d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!",
                  COALESCE(t.subtitle, d.subtitle) AS "subtitle?",
                  COALESCE(t.short_description, d.short_description) AS "short_description?",
                  m.id AS "media_id?", m.object_key AS "media_key?", m.width AS "media_width?", m.height AS "media_height?",
                  m.blurhash AS "blurhash?",
                  (SELECT COALESCE(mt.alt_text, md.alt_text) FROM media_assets x
                   LEFT JOIN media_asset_translations mt ON mt.media_asset_id = x.id AND mt.locale = $1
                   LEFT JOIN media_asset_translations md ON md.media_asset_id = x.id
                        AND md.locale = (SELECT code FROM locales WHERE is_default)
                   WHERE x.id = p.cover_media_id) AS "alt?",
                  (SELECT pr.amount_minor FROM prices pr WHERE pr.product_id = p.id AND pr.is_active
                     AND pr.currency = (SELECT code FROM currencies WHERE is_default) AND pr.amount_minor > 0
                   ORDER BY pr.amount_minor LIMIT 1) AS "from_price?",
                  (SELECT pr.pricing_unit FROM prices pr WHERE pr.product_id = p.id AND pr.is_active
                     AND pr.currency = (SELECT code FROM currencies WHERE is_default) AND pr.amount_minor > 0
                   ORDER BY pr.amount_minor LIMIT 1) AS "from_unit?",
                  (SELECT avg(r.rating)::float8 FROM reviews r WHERE r.product_id = p.id AND r.status = 'approved') AS "rating_avg?",
                  (SELECT count(*) FROM reviews r WHERE r.product_id = p.id AND r.status = 'approved') AS "rating_count!",
                  a.accommodation_kind AS "accommodation_kind?", a.max_guests AS "max_guests?",
                  a.surface_m2 AS "surface_m2?: Decimal", a.bed_configuration AS "bed_configuration?",
                  a.min_nights AS "min_nights?", a.inventory_mode AS "inventory_mode?",
                  act.duration_minutes AS "duration_minutes?", act.difficulty AS "difficulty?", act.min_age AS "min_age?",
                  act.scheduling_mode AS "scheduling_mode?", act.is_guided AS "is_guided?",
                  act.guide_languages AS "guide_languages?", act.standalone_bookable AS "standalone?",
                  pk.duration_days AS "duration_days?", pk.duration_nights AS "duration_nights?",
                  (SELECT COALESCE(array_agg(am.code::text ORDER BY am.sort_order), '{}')
                   FROM product_amenities pa JOIN amenities am ON am.id = pa.amenity_id
                   WHERE pa.product_id = p.id) AS "amenities!"
           FROM products p
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $1
           LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN media_assets m ON m.id = p.cover_media_id AND m.deleted_at IS NULL
           LEFT JOIN accommodation_types a ON a.product_id = p.id
           LEFT JOIN activities act ON act.product_id = p.id
           LEFT JOIN packages pk ON pk.product_id = p.id
           WHERE p.status = 'published' AND p.deleted_at IS NULL AND p.kind <> 'extra'
             AND ($2::text IS NULL OR p.kind = $2)
             AND (NOT $3 OR p.is_featured)
             AND ($4::uuid[] IS NULL OR p.id = ANY($4))
             AND COALESCE(t.slug, d.slug) IS NOT NULL
           ORDER BY p.sort_order, 5"#,
        locale,
        kind.map(ProductKind::as_str),
        featured_only,
        ids.map(<[Uuid]>::to_vec) as Option<Vec<Uuid>>
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ProductCard {
            id: r.id,
            kind: r.kind,
            slug: r.slug,
            name: r.name,
            subtitle: r.subtitle,
            short_description: r.short_description,
            booking_mode: r.booking_mode,
            is_featured: r.is_featured,
            cover: media(
                state,
                r.media_id,
                r.media_key,
                r.media_width,
                r.media_height,
                r.blurhash,
                r.alt,
            ),
            from_price_minor: r.from_price,
            from_price_unit: r.from_unit,
            rating: r.rating_avg.map(|average| Rating {
                average: (average * 10.0).round() / 10.0,
                count: r.rating_count,
            }),
            facts: serde_json::json!({
                "accommodation_kind": r.accommodation_kind,
                "max_guests": r.max_guests,
                "surface_m2": r.surface_m2,
                "beds": r.bed_configuration,
                "min_nights": r.min_nights,
                "inventory_mode": r.inventory_mode,
                "duration_minutes": r.duration_minutes,
                "difficulty": r.difficulty,
                "min_age": r.min_age,
                "scheduling_mode": r.scheduling_mode,
                "is_guided": r.is_guided,
                "guide_languages": r.guide_languages,
                "standalone_bookable": r.standalone,
                "duration_days": r.duration_days,
                "duration_nights": r.duration_nights,
                "amenities": r.amenities,
            }),
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductPage {
    #[serde(flatten)]
    pub card: ProductCard,
    pub description: Option<String>,
    pub highlights: Vec<String>,
    pub included: Vec<String>,
    pub not_included: Vec<String>,
    pub what_to_bring: Vec<String>,
    pub requirements: Option<String>,
    pub important_info: Option<String>,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
    /// Slug in every locale (hreflang, language switcher).
    pub alternates: Vec<Value>,
    pub gallery: Vec<MediaRef>,
    pub amenities: Vec<Value>,
    pub accommodation: Option<Value>,
    pub activity: Option<Value>,
    pub package: Option<Value>,
    pub itinerary: Vec<Value>,
    pub extras: Vec<Value>,
    /// Pitches on the site map (unit-mode accommodations).
    pub units: Vec<Value>,
    pub place: Option<Value>,
    pub reviews: Vec<Value>,
    pub faqs: Vec<Value>,
    pub related: Vec<ProductCard>,
}

pub async fn detail(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Path(slug): Path<String>,
) -> AppResult<Json<ProductPage>> {
    let key = format!("catalog:page:{}:{slug}", locale.as_str());
    if let Some(cached) = state.cache.get_json::<ProductPage>(&key).await {
        return Ok(Json(cached));
    }
    let loc = locale.as_str();
    let id = sqlx::query_scalar!(
        "SELECT pt.product_id FROM product_translations pt JOIN products p ON p.id = pt.product_id
         WHERE pt.slug = $1 AND p.status = 'published' AND p.deleted_at IS NULL
         ORDER BY (pt.locale = $2) DESC LIMIT 1",
        slug,
        loc
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let card = cards(&state, loc, None, false, Some(&[id]))
        .await?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)?;
    let db = &state.db;

    let text = sqlx::query!(
        r#"SELECT COALESCE(t.description, d.description) AS "description?",
                  COALESCE(NULLIF(t.highlights, '{}'), d.highlights, '{}') AS "highlights!",
                  COALESCE(NULLIF(t.included, '{}'), d.included, '{}') AS "included!",
                  COALESCE(NULLIF(t.not_included, '{}'), d.not_included, '{}') AS "not_included!",
                  COALESCE(NULLIF(t.what_to_bring, '{}'), d.what_to_bring, '{}') AS "what_to_bring!",
                  COALESCE(t.requirements, d.requirements) AS "requirements?",
                  COALESCE(t.important_info, d.important_info) AS "important_info?",
                  COALESCE(t.seo_title, d.seo_title) AS "seo_title?",
                  COALESCE(t.seo_description, d.seo_description) AS "seo_description?"
           FROM products p
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2
           LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE p.id = $1"#,
        id,
        loc
    )
    .fetch_one(db)
    .await?;

    let alternates = sqlx::query!(
        "SELECT pt.locale, pt.slug FROM product_translations pt JOIN locales l ON l.code = pt.locale
         WHERE pt.product_id = $1 AND l.is_active ORDER BY l.sort_order",
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| serde_json::json!({ "locale": r.locale, "slug": r.slug }))
    .collect();

    let gallery = sqlx::query!(
        r#"SELECT m.id, m.object_key, m.width, m.height, m.blurhash,
                  (SELECT COALESCE(mt.alt_text, md.alt_text) FROM media_assets x
                   LEFT JOIN media_asset_translations mt ON mt.media_asset_id = x.id AND mt.locale = $2
                   LEFT JOIN media_asset_translations md ON md.media_asset_id = x.id
                        AND md.locale = (SELECT code FROM locales WHERE is_default)
                   WHERE x.id = m.id) AS "alt?"
           FROM products p
           JOIN gallery_items gi ON gi.gallery_id = p.gallery_id
           JOIN media_assets m ON m.id = gi.media_id AND m.deleted_at IS NULL AND m.kind = 'image'
           WHERE p.id = $1 ORDER BY gi.sort_order, m.created_at"#,
        id,
        loc
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .filter_map(|m| media(&state, Some(m.id), Some(m.object_key), m.width, m.height, m.blurhash, m.alt))
    .collect();

    let amenities = sqlx::query!(
        r#"SELECT a.code::text AS "code!", a.amenity_group, a.icon,
                  COALESCE(t.name, d.name, a.code::text) AS "name!", COALESCE(t.description, d.description) AS "description?"
           FROM product_amenities pa JOIN amenities a ON a.id = pa.amenity_id
           LEFT JOIN amenity_translations t ON t.amenity_id = a.id AND t.locale = $2
           LEFT JOIN amenity_translations d ON d.amenity_id = a.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE pa.product_id = $1 ORDER BY a.amenity_group, a.sort_order"#,
        id,
        loc
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|a| serde_json::json!({ "code": a.code, "group": a.amenity_group, "icon": a.icon, "name": a.name, "description": a.description }))
    .collect();

    let accommodation = sqlx::query!(
        r#"SELECT accommodation_kind, inventory_mode, max_guests, max_adults, max_children, standard_occupancy,
                  bed_configuration, surface_m2 AS "surface_m2: Decimal", min_nights, max_nights,
                  check_in_from, check_in_until, check_out_until, pitch_selection_extra_id
           FROM accommodation_types WHERE product_id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?
    .map(|a| serde_json::to_value(serde_json::json!({
        "accommodation_kind": a.accommodation_kind, "inventory_mode": a.inventory_mode,
        "max_guests": a.max_guests, "max_adults": a.max_adults, "max_children": a.max_children,
        "standard_occupancy": a.standard_occupancy, "beds": a.bed_configuration, "surface_m2": a.surface_m2,
        "min_nights": a.min_nights, "max_nights": a.max_nights, "check_in_from": a.check_in_from,
        "check_in_until": a.check_in_until, "check_out_until": a.check_out_until,
        "pitch_selection_extra_id": a.pitch_selection_extra_id,
    })).unwrap_or_default());

    let activity_row = sqlx::query!(
        r#"SELECT location_type, scheduling_mode AS "scheduling_mode: SchedulingMode", duration_minutes, difficulty,
                  min_participants, max_participants, min_age, is_guided, guide_languages, meeting_place_id,
                  booking_cutoff_hours, pickup_available, standalone_bookable
           FROM activities WHERE product_id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?;
    let meeting_place = activity_row.as_ref().and_then(|a| a.meeting_place_id);
    let activity = activity_row.map(|a| {
        serde_json::json!({
            "location_type": a.location_type, "scheduling_mode": a.scheduling_mode,
            "duration_minutes": a.duration_minutes, "difficulty": a.difficulty,
            "min_participants": a.min_participants, "max_participants": a.max_participants,
            "min_age": a.min_age, "is_guided": a.is_guided, "guide_languages": a.guide_languages,
            "booking_cutoff_hours": a.booking_cutoff_hours, "pickup_available": a.pickup_available,
            "standalone_bookable": a.standalone_bookable,
        })
    });

    let package = sqlx::query!(
        "SELECT duration_days, duration_nights, pricing_mode, discount_percent FROM packages WHERE product_id = $1",
        id
    )
    .fetch_optional(db)
    .await?;
    let package = match package {
        None => None,
        Some(p) => {
            let items: Vec<Value> = sqlx::query!(
                r#"SELECT pi.item_product_id, pi.quantity, pi.day_number, pi.is_optional,
                          pr.kind AS "kind!", COALESCE(t.name, d.name) AS "name?", COALESCE(t.slug, d.slug) AS "slug?"
                   FROM package_items pi JOIN products pr ON pr.id = pi.item_product_id
                   LEFT JOIN product_translations t ON t.product_id = pr.id AND t.locale = $2
                   LEFT JOIN product_translations d ON d.product_id = pr.id AND d.locale = (SELECT code FROM locales WHERE is_default)
                   WHERE pi.package_product_id = $1 ORDER BY pi.day_number NULLS FIRST, pi.sort_order"#,
                id,
                loc
            )
            .fetch_all(db)
            .await?
            .into_iter()
            .map(|i| serde_json::json!({
                "product_id": i.item_product_id, "kind": i.kind, "name": i.name, "slug": i.slug,
                "quantity": i.quantity, "day_number": i.day_number, "is_optional": i.is_optional,
            }))
            .collect();
            Some(serde_json::json!({
                "duration_days": p.duration_days, "duration_nights": p.duration_nights,
                "pricing_mode": p.pricing_mode, "discount_percent": p.discount_percent, "items": items,
            }))
        }
    };

    let itinerary = sqlx::query!(
        r#"SELECT s.day_number, s.duration_minutes, COALESCE(t.title, d.title) AS "title?",
                  COALESCE(t.description, d.description) AS "description?"
           FROM itinerary_steps s
           LEFT JOIN itinerary_step_translations t ON t.itinerary_step_id = s.id AND t.locale = $2
           LEFT JOIN itinerary_step_translations d ON d.itinerary_step_id = s.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE s.product_id = $1 ORDER BY s.day_number, s.sort_order"#,
        id,
        loc
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|s| serde_json::json!({ "day": s.day_number, "duration_minutes": s.duration_minutes, "title": s.title, "description": s.description }))
    .collect();

    let extras = sqlx::query!(
        r#"SELECT e.product_id, pe.is_mandatory, e.applies_to, e.max_quantity, e.requires_date,
                  COALESCE(t.name, d.name) AS "name?", COALESCE(t.short_description, d.short_description) AS "description?",
                  (SELECT pr.amount_minor FROM prices pr WHERE pr.product_id = e.product_id AND pr.is_active
                   ORDER BY pr.amount_minor LIMIT 1) AS "price_minor?",
                  (SELECT pr.pricing_unit FROM prices pr WHERE pr.product_id = e.product_id AND pr.is_active
                   ORDER BY pr.amount_minor LIMIT 1) AS "price_unit?"
           FROM product_extras pe
           JOIN extras e ON e.product_id = pe.extra_product_id
           JOIN products p ON p.id = e.product_id AND p.status = 'published' AND p.deleted_at IS NULL
           LEFT JOIN product_translations t ON t.product_id = e.product_id AND t.locale = $2
           LEFT JOIN product_translations d ON d.product_id = e.product_id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE pe.product_id = $1 ORDER BY pe.sort_order"#,
        id,
        loc
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|e| serde_json::json!({
        "product_id": e.product_id, "name": e.name, "description": e.description, "mandatory": e.is_mandatory,
        "applies_to": e.applies_to, "max_quantity": e.max_quantity, "requires_date": e.requires_date,
        "price_minor": e.price_minor, "price_unit": e.price_unit,
    }))
    .collect();

    let units = sqlx::query!(
        r#"SELECT id, code::text AS "code!", name, map_x AS "map_x: Decimal", map_y AS "map_y: Decimal"
           FROM accommodation_units WHERE product_id = $1 AND status = 'active' ORDER BY sort_order, code"#,
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|u| serde_json::json!({ "id": u.id, "code": u.code, "name": u.name, "map_x": u.map_x, "map_y": u.map_y }))
    .collect();

    let place = sqlx::query!(
        r#"SELECT pl.id, pl.kind, pl.latitude AS "latitude: Decimal", pl.longitude AS "longitude: Decimal",
                  pl.distance_km AS "distance_km: Decimal", COALESCE(t.name, d.name) AS "name?",
                  COALESCE(t.directions, d.directions) AS "directions?"
           FROM places pl
           LEFT JOIN place_translations t ON t.place_id = pl.id AND t.locale = $2
           LEFT JOIN place_translations d ON d.place_id = pl.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE pl.id = COALESCE($3, (SELECT place_id FROM products WHERE id = $1))"#,
        id,
        loc,
        meeting_place
    )
    .fetch_optional(db)
    .await?
    .map(|p| serde_json::json!({
        "id": p.id, "kind": p.kind, "name": p.name, "latitude": p.latitude, "longitude": p.longitude,
        "distance_km": p.distance_km, "directions": p.directions, "is_meeting_point": meeting_place.is_some(),
    }));

    let reviews = sqlx::query!(
        r#"SELECT r.id, r.author_name, r.author_country, r.rating, r.title, r.body, r.experience_date, r.locale,
                  r.booking_id IS NOT NULL AS "verified!", r.reply_body, r.created_at
           FROM reviews r WHERE r.product_id = $1 AND r.status = 'approved'
           ORDER BY r.is_featured DESC, r.created_at DESC LIMIT 12"#,
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| serde_json::json!({
        "id": r.id, "author_name": r.author_name, "author_country": r.author_country, "rating": r.rating,
        "title": r.title, "body": r.body, "experience_date": r.experience_date, "locale": r.locale,
        "verified": r.verified, "reply": r.reply_body, "created_at": r.created_at,
    }))
    .collect();

    let faqs = sqlx::query!(
        r#"SELECT COALESCE(t.question, d.question) AS "question!", COALESCE(t.answer, d.answer) AS "answer!"
           FROM faqs f
           LEFT JOIN faq_translations t ON t.faq_id = f.id AND t.locale = $2
           LEFT JOIN faq_translations d ON d.faq_id = f.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE f.product_id = $1 AND f.is_published AND COALESCE(t.question, d.question) IS NOT NULL
           ORDER BY f.sort_order"#,
        id,
        loc
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|f| serde_json::json!({ "question": f.question, "answer": f.answer }))
    .collect();

    let related_ids = sqlx::query_scalar!(
        "SELECT related_product_id FROM related_products WHERE product_id = $1 ORDER BY sort_order",
        id
    )
    .fetch_all(db)
    .await?;
    let mut related = if related_ids.is_empty() {
        // "Make the most of your stay": activities for stays, stays for activities.
        let kind = if card.kind == ProductKind::Accommodation {
            ProductKind::Activity
        } else {
            ProductKind::Accommodation
        };
        cards(&state, loc, Some(kind), false, None).await?
    } else {
        cards(&state, loc, None, false, Some(&related_ids)).await?
    };
    related.retain(|c| c.id != id);
    related.truncate(4);

    let page = ProductPage {
        card,
        description: text.description,
        highlights: text.highlights,
        included: text.included,
        not_included: text.not_included,
        what_to_bring: text.what_to_bring,
        requirements: text.requirements,
        important_info: text.important_info,
        seo_title: text.seo_title,
        seo_description: text.seo_description,
        alternates,
        gallery,
        amenities,
        accommodation,
        activity,
        package,
        itinerary,
        extras,
        units,
        place,
        reviews,
        faqs,
        related,
    };
    state.cache.set_json(&key, &page, TTL).await;
    Ok(Json(page))
}

pub async fn guest_categories(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
) -> AppResult<Json<Vec<Value>>> {
    let rows = sqlx::query!(
        r#"SELECT g.id, g.code::text AS "code!", g.min_age, g.max_age, g.counts_toward_capacity,
                  COALESCE(t.name, d.name, g.code::text) AS "name!", COALESCE(t.description, d.description) AS "description?"
           FROM guest_categories g
           LEFT JOIN guest_category_translations t ON t.guest_category_id = g.id AND t.locale = $1
           LEFT JOIN guest_category_translations d ON d.guest_category_id = g.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE g.is_active ORDER BY g.sort_order"#,
        locale.as_str()
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|g| serde_json::json!({
                "id": g.id, "code": g.code, "name": g.name, "description": g.description,
                "min_age": g.min_age, "max_age": g.max_age, "counts_toward_capacity": g.counts_toward_capacity,
            }))
            .collect(),
    ))
}
