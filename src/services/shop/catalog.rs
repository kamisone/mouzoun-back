//! Public catalog reads: product list (filters, sort, facets, pages) and product page.
//! Only published shop products with at least one active variant are shown. Availability is a
//! status (`in_stock | low_stock | out_of_stock | backorder`); the exact quantity is shown only
//! below the low-stock threshold ("only 3 left").

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::inventory::{Availability, availability};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    /// Category slug (in the request locale) or id; its sub-categories are included.
    pub category: Option<String>,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    #[serde(default)]
    pub in_stock: bool,
    pub product_type: Option<String>,
    pub q: Option<String>,
    /// featured | newest | price_asc | price_desc | best_selling
    pub sort: Option<String>,
    /// Option value filter `code:value` (e.g. `size:500g`), repeatable via comma.
    pub options: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    /// Opaque cursor (the next page), same as `page`.
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductCard {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub short_description: Option<String>,
    pub product_type: String,
    pub image_url: Option<String>,
    pub image_blurhash: Option<String>,
    pub price_min_minor: i64,
    pub price_max_minor: i64,
    pub compare_at_minor: Option<i64>,
    pub currency: String,
    pub availability: Availability,
    pub variant_count: i64,
    /// For "add to cart" from the card when there is a single variant.
    pub default_variant_id: Option<Uuid>,
    pub rating_avg: Option<f64>,
    pub rating_count: i64,
    pub is_featured: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Facet {
    pub key: String,
    pub label: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductList {
    pub data: Vec<ProductCard>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub next_cursor: Option<String>,
    pub facets: Value,
}

fn page_of(q: &ListQuery) -> (i64, i64) {
    let per_page = q.per_page.unwrap_or(24).clamp(1, 60);
    let page = q
        .cursor
        .as_deref()
        .and_then(|c| c.strip_prefix('p'))
        .and_then(|n| n.parse().ok())
        .or(q.page)
        .unwrap_or(1)
        .max(1);
    (page, per_page)
}

/// Category id from a slug or id (shop scope), with its sub-categories.
async fn category_ids(
    conn: &mut PgConnection,
    key: &str,
    locale: &str,
) -> sqlx::Result<Option<Vec<Uuid>>> {
    let root: Option<Uuid> = match key.parse::<Uuid>() {
        Ok(id) => Some(id),
        Err(_) => {
            sqlx::query_scalar!(
                "SELECT c.id FROM categories c JOIN category_translations t ON t.category_id = c.id
                 WHERE c.scope = 'shop' AND t.slug = $1 ORDER BY (t.locale = $2) DESC LIMIT 1",
                key,
                locale
            )
            .fetch_optional(&mut *conn)
            .await?
        }
    };
    let Some(root) = root else { return Ok(None) };
    Ok(Some(
        sqlx::query_scalar!(
            r#"WITH RECURSIVE tree AS (
                   SELECT id FROM categories WHERE id = $1 AND scope = 'shop' AND is_active
                   UNION ALL
                   SELECT c.id FROM categories c JOIN tree ON c.parent_id = tree.id WHERE c.is_active)
               SELECT id AS "id!" FROM tree"#,
            root
        )
        .fetch_all(conn)
        .await?,
    ))
}

#[allow(clippy::too_many_lines)]
pub async fn list(
    conn: &mut PgConnection,
    q: &ListQuery,
    locale: &str,
    low_threshold: i64,
    public_url: &(dyn Fn(&str) -> String + Sync),
    now: DateTime<Utc>,
) -> sqlx::Result<ProductList> {
    let (page, per_page) = page_of(q);
    let categories = match q.category.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => match category_ids(conn, c, locale).await? {
            Some(ids) => Some(ids),
            None => {
                return Ok(ProductList {
                    data: vec![],
                    total: 0,
                    page,
                    per_page,
                    next_cursor: None,
                    facets: json!({}),
                });
            }
        },
        None => None,
    };
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let option_filters: Vec<String> = q
        .options
        .as_deref()
        .map(|o| {
            o.split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| s.contains(':'))
                .collect()
        })
        .unwrap_or_default();
    let sort = q.sort.as_deref().unwrap_or("featured");
    let rows = sqlx::query!(
        r#"WITH base AS (
               SELECT p.id, p.is_featured, p.sort_order, p.published_at, p.created_at, sp.product_type,
                      COALESCE(t.name, d.name) AS name, COALESCE(t.slug, d.slug) AS slug,
                      COALESCE(t.short_description, d.short_description) AS short_description,
                      m.object_key AS image_key, m.blurhash,
                      v.price_min, v.price_max, v.compare_at, v.variants, v.default_variant,
                      v.free_total, v.untracked, v.backorder,
                      (SELECT COALESCE(sum(oi.quantity), 0) FROM order_items oi JOIN orders o ON o.id = oi.order_id
                       WHERE oi.product_id = p.id AND o.status IN ('open', 'completed')) AS sold,
                      r.avg AS rating_avg, COALESCE(r.n, 0) AS rating_count
               FROM products p
               JOIN shop_products sp ON sp.product_id = p.id
               LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $1
               LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = (SELECT code FROM locales WHERE is_default)
               LEFT JOIN media_assets m ON m.id = p.cover_media_id AND m.deleted_at IS NULL
               JOIN LATERAL (
                   SELECT min(pv.price_minor) AS price_min, max(pv.price_minor) AS price_max,
                          min(pv.compare_at_price_minor) AS compare_at, count(*) AS variants,
                          (array_agg(pv.id ORDER BY pv.is_default DESC, pv.sort_order))[1] AS default_variant,
                          COALESCE(sum(GREATEST(lv.free, 0)) FILTER (WHERE pv.track_inventory), 0) AS free_total,
                          bool_or(NOT pv.track_inventory) AS untracked,
                          COALESCE(sum(lv.backorder) FILTER (WHERE pv.track_inventory), 0) AS backorder
                   FROM product_variants pv
                   LEFT JOIN LATERAL (
                       SELECT sum(l.on_hand - l.reserved) AS free, sum(l.backorder_limit) AS backorder
                       FROM inventory_levels l JOIN stock_locations s ON s.id = l.location_id
                       WHERE l.variant_id = pv.id AND s.is_active AND s.fulfills_online_orders AND (s.legal_entity_id IS NULL OR s.legal_entity_id = shop_entity_id())) lv ON true
                   WHERE pv.product_id = p.id AND pv.status = 'active' AND pv.deleted_at IS NULL
               ) v ON v.variants > 0
               LEFT JOIN LATERAL (SELECT avg(rating)::float8 AS avg, count(*) AS n FROM reviews
                                  WHERE product_id = p.id AND status = 'approved') r ON true
               WHERE p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL
                 AND (sp.available_period IS NULL OR upper(sp.available_period) IS NULL OR upper(sp.available_period) > $9)
                 AND ($2::uuid[] IS NULL OR EXISTS (SELECT 1 FROM product_categories pc WHERE pc.product_id = p.id AND pc.category_id = ANY($2)))
                 AND ($3::text IS NULL OR sp.product_type = $3)
                 AND ($4::text IS NULL OR EXISTS (SELECT 1 FROM product_translations st WHERE st.product_id = p.id
                        AND (st.search_vector @@ websearch_to_tsquery('simple', $4) OR st.name ILIKE '%' || $4 || '%')))
                 AND (cardinality($10::text[]) = 0 OR EXISTS (
                        SELECT 1 FROM product_variants fv JOIN product_variant_option_values pvo ON pvo.variant_id = fv.id
                        JOIN product_options o ON o.id = pvo.option_id JOIN product_option_values ov ON ov.id = pvo.option_value_id
                        WHERE fv.product_id = p.id AND fv.status = 'active' AND fv.deleted_at IS NULL
                          AND lower(o.code::text || ':' || ov.code::text) = ANY($10)))
           )
           SELECT id AS "id!", is_featured AS "is_featured!", product_type AS "product_type!", name AS "name!", slug AS "slug!",
                  short_description AS "short_description?", image_key AS "image_key?", blurhash AS "blurhash?", price_min AS "price_min!", price_max AS "price_max!", compare_at AS "compare_at?",
                  variants AS "variants!", default_variant AS "default_variant?", free_total::bigint AS "free_total!", COALESCE(untracked, false) AS "untracked!",
                  backorder::bigint AS "backorder!", rating_avg AS "rating_avg?", rating_count AS "rating_count!", count(*) OVER () AS "total!"
           FROM base
           WHERE ($5::bigint IS NULL OR price_max >= $5) AND ($6::bigint IS NULL OR price_min <= $6)
             AND (NOT $7 OR untracked OR free_total + backorder > 0)
           ORDER BY
             CASE WHEN $8 = 'price_asc' THEN price_min END ASC,
             CASE WHEN $8 = 'price_desc' THEN price_max END DESC,
             CASE WHEN $8 = 'newest' THEN COALESCE(published_at, created_at) END DESC,
             CASE WHEN $8 = 'best_selling' THEN sold END DESC,
             CASE WHEN $8 = 'featured' THEN is_featured END DESC,
             sort_order, name, id
           LIMIT $11 OFFSET $12"#,
        locale,
        categories.as_deref(),
        q.product_type.as_deref(),
        search,
        q.min_price,
        q.max_price,
        q.in_stock,
        sort,
        now,
        &option_filters,
        per_page,
        (page - 1) * per_page
    )
    .fetch_all(&mut *conn)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let data: Vec<ProductCard> = rows
        .into_iter()
        .map(|r| ProductCard {
            id: r.id,
            slug: r.slug,
            name: r.name,
            short_description: r.short_description,
            product_type: r.product_type,
            image_url: r.image_key.as_deref().map(public_url),
            image_blurhash: r.blurhash,
            price_min_minor: r.price_min,
            price_max_minor: r.price_max,
            compare_at_minor: r.compare_at,
            currency: "EUR".into(),
            availability: if r.untracked {
                Availability::InStock
            } else {
                availability(r.free_total, r.backorder, low_threshold)
            },
            variant_count: r.variants,
            default_variant_id: (r.variants == 1).then_some(r.default_variant).flatten(),
            rating_avg: r.rating_avg,
            rating_count: r.rating_count,
            is_featured: r.is_featured,
        })
        .collect();
    let facets = facets(conn, locale).await?;
    Ok(ProductList {
        next_cursor: (page * per_page < total).then(|| format!("p{}", page + 1)),
        data,
        total,
        page,
        per_page,
        facets,
    })
}

/// Counts per category and per option value over the published catalogue.
async fn facets(conn: &mut PgConnection, locale: &str) -> sqlx::Result<Value> {
    let categories = sqlx::query!(
        r#"SELECT c.id, c.parent_id, COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!",
                  (SELECT count(DISTINCT pc.product_id) FROM product_categories pc JOIN products p ON p.id = pc.product_id
                   WHERE pc.category_id = c.id AND p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL) AS "count!"
           FROM categories c
           LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $1
           LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE c.scope = 'shop' AND c.is_active ORDER BY c.sort_order, 3"#,
        locale
    )
    .fetch_all(&mut *conn)
    .await?;
    let options = sqlx::query!(
        r#"SELECT o.code::text AS "option!", ov.code::text AS "value!",
                  COALESCE(ot.name, o.code::text) AS "option_label!", COALESCE(vt.label, ov.code::text) AS "label!",
                  count(DISTINCT v.product_id) AS "count!"
           FROM product_variant_option_values pvo
           JOIN product_variants v ON v.id = pvo.variant_id AND v.status = 'active' AND v.deleted_at IS NULL
           JOIN products p ON p.id = v.product_id AND p.status = 'published' AND p.deleted_at IS NULL
           JOIN product_options o ON o.id = pvo.option_id
           JOIN product_option_values ov ON ov.id = pvo.option_value_id
           LEFT JOIN product_option_translations ot ON ot.option_id = o.id AND ot.locale = $1
           LEFT JOIN product_option_value_translations vt ON vt.option_value_id = ov.id AND vt.locale = $1
           GROUP BY 1, 2, 3, 4 ORDER BY 1, 2"#,
        locale
    )
    .fetch_all(conn)
    .await?;
    Ok(json!({
        "categories": categories.iter().map(|c| json!({ "id": c.id, "parent_id": c.parent_id, "name": c.name, "slug": c.slug, "count": c.count })).collect::<Vec<_>>(),
        "options": options.iter().map(|o| json!({ "key": format!("{}:{}", o.option, o.value), "option": o.option_label, "label": o.label, "count": o.count })).collect::<Vec<_>>(),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantView {
    pub id: Uuid,
    pub sku: String,
    pub barcode: Option<String>,
    pub is_default: bool,
    pub price_minor: i64,
    pub compare_at_price_minor: Option<i64>,
    /// Lowest price of the 30 days before the current price (the reference of a reduction).
    pub reference_price_minor: Option<i64>,
    pub country_of_origin: Option<String>,
    /// Price per kg / L / piece when the net quantity is known.
    pub unit_price: Option<Value>,
    pub net_quantity: Option<Decimal>,
    pub net_unit: Option<String>,
    pub weight_grams: i32,
    pub availability: Availability,
    /// Only below the low-stock threshold.
    pub quantity_left: Option<i64>,
    pub max_quantity: i64,
    /// `{option_code: value_code}`.
    pub options: HashMap<String, String>,
    pub media: Vec<Value>,
}

/// Unit price (`€24.00 / kg`) from a price and a net quantity.
pub fn unit_price(
    price_minor: i64,
    net_quantity: Option<Decimal>,
    unit: Option<&str>,
) -> Option<Value> {
    let q = net_quantity?;
    if q <= Decimal::ZERO {
        return None;
    }
    let (per, base) = match unit? {
        "g" => (q / Decimal::from(1000), "kg"),
        "kg" => (q, "kg"),
        "ml" => (q / Decimal::from(1000), "l"),
        "l" => (q, "l"),
        "piece" => (q, "piece"),
        _ => return None,
    };
    let amount = (Decimal::from(price_minor) / per).round();
    Some(json!({ "amount_minor": amount.to_string().parse::<i64>().unwrap_or(0), "unit": base }))
}

#[allow(clippy::too_many_lines)]
pub async fn detail(
    conn: &mut PgConnection,
    slug: &str,
    locale: &str,
    low_threshold: i64,
    max_per_line: i64,
    public_url: &(dyn Fn(&str) -> String + Sync),
) -> sqlx::Result<Option<Value>> {
    let Some(p) = sqlx::query!(
        r#"SELECT p.id, p.cover_media_id, p.gallery_id, p.is_featured,
                  sp.product_type, sp.is_perishable, sp.withdrawal_exemption, sp.allergens, sp.nutrition, sp.shelf_life_days,
                  sp.max_per_order, sp.option_count, sp.track_lots,
                  lower(sp.available_period) AS available_from, upper(sp.available_period) AS available_until,
                  COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!",
                  COALESCE(t.subtitle, d.subtitle) AS subtitle, COALESCE(t.short_description, d.short_description) AS short_description,
                  COALESCE(t.description, d.description) AS description, COALESCE(t.seo_title, d.seo_title) AS seo_title,
                  COALESCE(t.seo_description, d.seo_description) AS seo_description,
                  st.ingredients, st.storage_instructions, st.usage_tips, st.origin_story, st.producer_name, st.legal_notice,
                  sp.origin_place_id, sp.farm_crop_id
           FROM product_translations tt
           JOIN products p ON p.id = tt.product_id
           JOIN shop_products sp ON sp.product_id = p.id
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2
           LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN LATERAL (SELECT * FROM shop_product_translations s WHERE s.product_id = p.id
                              ORDER BY (s.locale = $2) DESC, (s.locale = (SELECT code FROM locales WHERE is_default)) DESC LIMIT 1) st ON true
           WHERE tt.slug = $1 AND p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL
           ORDER BY (tt.locale = $2) DESC LIMIT 1"#,
        slug,
        locale
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };

    let options = sqlx::query!(
        r#"SELECT o.id, o.code::text AS "code!", COALESCE(ot.name, od.name, o.code::text) AS "name!", o.display_type,
                  (SELECT code::text FROM product_option_values WHERE id = o.default_value_id) AS default_value,
                  COALESCE(jsonb_agg(jsonb_build_object('code', v.code::text, 'label', COALESCE(vt.label, vd.label, v.code::text),
                                    'swatch_url', COALESCE(sm.object_key, lm.object_key), 'swatch_color', v.swatch_color)
                                    ORDER BY v.sort_order, v.code)
                           FILTER (WHERE v.id IS NOT NULL), '[]') AS "values!"
           FROM product_options o
           LEFT JOIN product_option_translations ot ON ot.option_id = o.id AND ot.locale = $2
           LEFT JOIN product_option_translations od ON od.option_id = o.id AND od.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN product_option_values v ON v.option_id = o.id
           LEFT JOIN product_option_value_translations vt ON vt.option_value_id = v.id AND vt.locale = $2
           LEFT JOIN product_option_value_translations vd ON vd.option_value_id = v.id AND vd.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN media_assets sm ON sm.id = v.swatch_media_id AND sm.deleted_at IS NULL
           -- A library value's picture when the product has none of its own.
           LEFT JOIN variation_values lv ON lv.id = v.variation_value_id
           LEFT JOIN media_assets lm ON lm.id = lv.swatch_media_id AND lm.deleted_at IS NULL
           WHERE o.product_id = $1 GROUP BY o.id, ot.name, od.name ORDER BY o.sort_order, o.code"#,
        p.id,
        locale
    )
    .fetch_all(&mut *conn)
    .await?;
    let options: Vec<Value> = options
        .into_iter()
        .map(|o| {
            let mut values = o.values;
            if let Some(arr) = values.as_array_mut() {
                for v in arr {
                    if let Some(key) = v["swatch_url"].as_str().map(str::to_string) {
                        v["swatch_url"] = json!(public_url(&key));
                    }
                }
            }
            json!({ "code": o.code, "name": o.name, "display_type": o.display_type, "default_value": o.default_value, "values": values })
        })
        .collect();

    let variants = sqlx::query!(
        r#"SELECT v.id, v.sku::text AS "sku!", v.barcode, v.is_default, v.price_minor, v.compare_at_price_minor,
                  variant_reference_price(v.id) AS reference_price_minor, v.country_of_origin::text AS country_of_origin,
                  v.net_quantity, v.net_unit, v.weight_grams, v.track_inventory, v.low_stock_threshold,
                  COALESCE((SELECT sum(l.on_hand - l.reserved) FROM inventory_levels l JOIN stock_locations s ON s.id = l.location_id
                            WHERE l.variant_id = v.id AND s.is_active AND s.fulfills_online_orders AND (s.legal_entity_id IS NULL OR s.legal_entity_id = shop_entity_id())), 0)::bigint AS "free!",
                  COALESCE((SELECT sum(l.backorder_limit) FROM inventory_levels l JOIN stock_locations s ON s.id = l.location_id
                            WHERE l.variant_id = v.id AND s.is_active AND s.fulfills_online_orders AND (s.legal_entity_id IS NULL OR s.legal_entity_id = shop_entity_id())), 0)::bigint AS "backorder!",
                  COALESCE((SELECT jsonb_object_agg(o.code::text, ov.code::text) FROM product_variant_option_values pvo
                            JOIN product_options o ON o.id = pvo.option_id JOIN product_option_values ov ON ov.id = pvo.option_value_id
                            WHERE pvo.variant_id = v.id), '{}') AS "options!",
                  COALESCE((SELECT array_agg(m.object_key ORDER BY vm.sort_order) FROM product_variant_media vm
                            JOIN media_assets m ON m.id = vm.media_id AND m.deleted_at IS NULL WHERE vm.variant_id = v.id), '{}') AS "media!",
                  cm.object_key AS "cover_key?"
           FROM product_variants v LEFT JOIN media_assets cm ON cm.id = v.cover_media_id AND cm.deleted_at IS NULL
           WHERE v.product_id = $1 AND v.status = 'active' AND v.deleted_at IS NULL
           ORDER BY v.sort_order, v.sku"#,
        p.id
    )
    .fetch_all(&mut *conn)
    .await?;
    let max_qty = p
        .max_per_order
        .map_or(max_per_line, |m| i64::from(m).min(max_per_line));
    let variants: Vec<VariantView> = variants
        .into_iter()
        .map(|v| {
            let threshold = v.low_stock_threshold.map_or(low_threshold, i64::from);
            let avail = if v.track_inventory {
                availability(v.free, v.backorder, threshold)
            } else {
                Availability::InStock
            };
            let mut media: Vec<Value> = v
                .cover_key
                .iter()
                .map(|k| json!({ "url": public_url(k) }))
                .collect();
            media.extend(v.media.iter().map(|k| json!({ "url": public_url(k) })));
            VariantView {
                id: v.id,
                sku: v.sku,
                barcode: v.barcode,
                is_default: v.is_default,
                price_minor: v.price_minor,
                compare_at_price_minor: v.compare_at_price_minor,
                reference_price_minor: v.reference_price_minor,
                country_of_origin: v.country_of_origin,
                unit_price: unit_price(v.price_minor, v.net_quantity, v.net_unit.as_deref()),
                net_quantity: v.net_quantity,
                net_unit: v.net_unit,
                weight_grams: v.weight_grams,
                availability: avail,
                quantity_left: (v.track_inventory && avail == Availability::LowStock)
                    .then_some(v.free),
                max_quantity: if v.track_inventory {
                    max_qty.min((v.free + v.backorder).max(0))
                } else {
                    max_qty
                },
                options: serde_json::from_value(v.options).unwrap_or_default(),
                media,
            }
        })
        .collect();
    if variants.is_empty() {
        return Ok(None);
    }

    let gallery: Vec<Value> = sqlx::query!(
        r#"SELECT m.object_key, m.blurhash, m.focal_x, m.focal_y, m.width, m.height,
                  (SELECT alt_text FROM media_asset_translations mt WHERE mt.media_asset_id = m.id AND mt.locale = $3) AS alt
           FROM media_assets m
           WHERE m.deleted_at IS NULL AND (m.id = $1 OR m.id IN (SELECT media_id FROM gallery_items WHERE gallery_id = $2))
           ORDER BY (m.id = $1) DESC, (SELECT sort_order FROM gallery_items gi WHERE gi.gallery_id = $2 AND gi.media_id = m.id)"#,
        p.cover_media_id,
        p.gallery_id,
        locale
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|m| json!({ "url": public_url(&m.object_key), "blurhash": m.blurhash, "focal_x": m.focal_x, "focal_y": m.focal_y,
                     "width": m.width, "height": m.height, "alt": m.alt }))
    .collect();

    let origin = match p.origin_place_id {
        Some(id) => sqlx::query!(
            r#"SELECT COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!", pl.is_published
               FROM places pl
               LEFT JOIN place_translations t ON t.place_id = pl.id AND t.locale = $2
               LEFT JOIN place_translations d ON d.place_id = pl.id AND d.locale = (SELECT code FROM locales WHERE is_default)
               WHERE pl.id = $1 AND pl.deleted_at IS NULL"#,
            id,
            locale
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|o| json!({ "name": o.name, "slug": o.is_published.then_some(o.slug) })),
        None => None,
    };
    let crop = match p.farm_crop_id {
        Some(id) => sqlx::query!(
            r#"SELECT COALESCE(t.name, d.name, c.code::text) AS "name!", c.harvest_months FROM farm_crops c
               LEFT JOIN farm_crop_translations t ON t.farm_crop_id = c.id AND t.locale = $2
               LEFT JOIN farm_crop_translations d ON d.farm_crop_id = c.id AND d.locale = (SELECT code FROM locales WHERE is_default)
               WHERE c.id = $1"#,
            id,
            locale
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|c| json!({ "name": c.name, "harvest_months": c.harvest_months })),
        None => None,
    };
    let reviews = sqlx::query!(
        r#"SELECT avg(rating)::float8 AS avg, count(*) AS "count!",
                  COALESCE(jsonb_agg(jsonb_build_object('author', author_name, 'rating', rating, 'title', title, 'body', body,
                           'verified', order_id IS NOT NULL, 'date', created_at) ORDER BY created_at DESC)
                           FILTER (WHERE rn <= 6), '[]') AS "latest!"
           FROM (SELECT *, row_number() OVER (ORDER BY created_at DESC) AS rn FROM reviews WHERE product_id = $1 AND status = 'approved') r"#,
        p.id
    )
    .fetch_one(&mut *conn)
    .await?;
    let related = sqlx::query!(
        r#"SELECT p.id, COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!", m.object_key AS "image_key?",
                  (SELECT min(price_minor) FROM product_variants v WHERE v.product_id = p.id AND v.status = 'active' AND v.deleted_at IS NULL) AS price_min
           FROM related_products r JOIN products p ON p.id = r.related_product_id
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2
           LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN media_assets m ON m.id = p.cover_media_id AND m.deleted_at IS NULL
           WHERE r.product_id = $1 AND p.status = 'published' AND p.deleted_at IS NULL
           ORDER BY r.sort_order LIMIT 8"#,
        p.id,
        locale
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| json!({ "id": r.id, "name": r.name, "slug": r.slug, "image_url": r.image_key.as_deref().map(public_url), "price_min_minor": r.price_min }))
    .collect::<Vec<_>>();
    let slugs: HashMap<String, String> = sqlx::query!(
        "SELECT locale, slug FROM product_translations WHERE product_id = $1",
        p.id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| (r.locale, r.slug))
    .collect();
    let categories = sqlx::query!(
        r#"SELECT c.id, COALESCE(t.name, d.name) AS "name!", COALESCE(t.slug, d.slug) AS "slug!"
           FROM product_categories pc JOIN categories c ON c.id = pc.category_id AND c.scope = 'shop'
           LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $2
           LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE pc.product_id = $1 ORDER BY c.sort_order"#,
        p.id,
        locale
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|c| json!({ "id": c.id, "name": c.name, "slug": c.slug }))
    .collect::<Vec<_>>();

    Ok(Some(json!({
        "id": p.id,
        "name": p.name,
        "slug": p.slug,
        "subtitle": p.subtitle,
        "short_description": p.short_description,
        "description": p.description,
        "seo": { "title": p.seo_title, "description": p.seo_description },
        "product_type": p.product_type,
        "is_featured": p.is_featured,
        "categories": categories,
        "currency": "EUR",
        "options": options,
        "variants": variants,
        "gallery": gallery,
        "food": {
            "is_perishable": p.is_perishable,
            "withdrawal_exemption": p.withdrawal_exemption,
            "allergens": p.allergens,
            "nutrition": p.nutrition,
            "shelf_life_days": p.shelf_life_days,
            "lot_tracked": p.track_lots,
            "ingredients": p.ingredients,
            "storage_instructions": p.storage_instructions,
            "usage_tips": p.usage_tips,
            "origin_story": p.origin_story,
            "producer_name": p.producer_name,
            "legal_notice": p.legal_notice,
        },
        "season": { "from": p.available_from, "until": p.available_until },
        "origin_place": origin,
        "crop": crop,
        "reviews": { "average": reviews.avg, "count": reviews.count, "latest": reviews.latest },
        "related": related,
        "slugs": slugs,
        "max_quantity": max_qty,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn unit_prices_are_per_kg_or_litre() {
        assert_eq!(
            unit_price(1200, Some(Decimal::from(500)), Some("g")).unwrap()["amount_minor"],
            2400
        );
        assert_eq!(
            unit_price(1800, Some(Decimal::from_str("0.75").unwrap()), Some("l")).unwrap()["amount_minor"],
            2400
        );
        assert_eq!(unit_price(1800, None, Some("l")), None);
    }
}
