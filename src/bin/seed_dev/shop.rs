//! Demo shop: categories, seven farm products (fr / en / ar), the camp farm store with stock and
//! two lots of Medjool dates, shipping profiles and zones, placeholder TVA rates ("to confirm"),
//! an inactive welcome code. The shop stays `catalog_only` (products visible, buying disabled).
//! Runtime queries: a development tool, not part of the API's checked SQL.

use anyhow::Context;
use chrono::{Duration, Utc};
use mouzoun_api::services::shop::inventory::{self, MovementRefs, NewLot};
use serde_json::json;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

type T3<'a> = [(&'a str, &'a str); 3];

async fn translate(
    tx: &mut PgConnection,
    table: &str,
    fk: &str,
    id: Uuid,
    col: &str,
    values: T3<'_>,
) -> anyhow::Result<()> {
    for (locale, value) in values {
        sqlx::query(&format!(
            "INSERT INTO {table} ({fk}, locale, {col}) VALUES ($1, $2, $3)"
        ))
        .bind(id)
        .bind(locale)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// locale, name, slug, description (short HTML), SEO title, SEO description.
type CategoryText<'a> = (&'a str, &'a str, &'a str, &'a str, &'a str, &'a str);

struct CategorySeed<'a> {
    sort: i32,
    parent: Option<Uuid>,
    show_in_menu: bool,
    /// Cover: a site photo (`<photo>.jpg`, seeded with its alt texts by `pages::seed`).
    photo: Option<&'a str>,
    texts: [CategoryText<'a>; 3],
}

async fn category(tx: &mut PgConnection, c: CategorySeed<'_>) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    let cover: Option<Uuid> = match c.photo {
        Some(photo) => {
            sqlx::query_scalar("SELECT id FROM media_assets WHERE original_filename = $1 LIMIT 1")
                .bind(format!("{photo}.jpg"))
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    sqlx::query(
        "INSERT INTO categories (id, scope, parent_id, sort_order, show_in_menu, cover_media_id, default_sort)
         VALUES ($1, 'shop', $2, $3, $4, $5, 'manual')",
    )
    .bind(id)
    .bind(c.parent)
    .bind(c.sort)
    .bind(c.show_in_menu)
    .bind(cover)
    .execute(&mut *tx)
    .await?;
    for (locale, name, slug, description, seo_title, seo_description) in c.texts {
        sqlx::query(
            "INSERT INTO category_translations (category_id, locale, name, slug, description, seo_title, seo_description)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(locale)
        .bind(name)
        .bind(slug)
        .bind(format!("<p>{description}</p>"))
        .bind(seo_title)
        .bind(seo_description)
        .execute(&mut *tx)
        .await?;
    }
    Ok(id)
}

struct ProductSeed<'a> {
    names: [(&'a str, &'a str, &'a str, &'a str); 3], // locale, name, slug, short description
    product_type: &'a str,
    profile: Uuid,
    category: Uuid,
    perishable: bool,
    track_lots: bool,
    allergens: &'a [&'a str],
    photo: &'a str,
    tax: Uuid,
    place: Option<Uuid>,
    crop: Option<Uuid>,
    featured: bool,
    ingredients: Option<T3<'a>>,
}

async fn product(tx: &mut PgConnection, p: &ProductSeed<'_>) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    let cover: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM media_assets WHERE original_filename = $1 LIMIT 1")
            .bind(format!("{}.jpg", p.photo))
            .fetch_optional(&mut *tx)
            .await?;
    sqlx::query(
        "INSERT INTO products (id, kind, status, booking_mode, is_featured, cover_media_id, published_at)
         VALUES ($1, 'shop_product', 'published', 'not_bookable', $2, $3, now())",
    )
    .bind(id)
    .bind(p.featured)
    .bind(cover)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO shop_products (product_id, product_type, shipping_profile_id, origin_place_id, farm_crop_id, is_perishable,
                                    withdrawal_exemption, track_lots, allergens, shelf_life_days, tax_class_code)
         VALUES ($1, $2, $3, $4, $5, $6,
                 CASE WHEN $6 THEN 'perishable' WHEN $2 IN ('food', 'drink', 'gift_box') THEN 'unsealed_hygiene' ELSE 'none' END,
                 $7, $8, $9, (SELECT CASE WHEN code = 'tva_standard' THEN 'standard' ELSE 'food_reduced' END FROM taxes WHERE id = $10))",
    )
    .bind(id)
    .bind(p.product_type)
    .bind(p.profile)
    .bind(p.place)
    .bind(p.crop)
    .bind(p.perishable)
    .bind(p.track_lots)
    .bind(p.allergens.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    .bind(if p.product_type == "food" { Some(365) } else { None::<i32> })
    .bind(p.tax)
    .execute(&mut *tx)
    .await?;
    for (locale, name, slug, short) in p.names {
        sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug, short_description, description) VALUES ($1, $2, $3, $4, $5, $5)")
            .bind(id)
            .bind(locale)
            .bind(name)
            .bind(slug)
            .bind(short)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(ing) = p.ingredients {
        for (locale, text) in ing {
            sqlx::query(
                "INSERT INTO shop_product_translations (product_id, locale, ingredients, storage_instructions, producer_name)
                 VALUES ($1, $2, $3, $4, 'Ferme de Mouzoun, M''ssici, Alnif (Maroc) — importé par MOUZOUN SAS (adresse à compléter)')",
            )
            .bind(id)
            .bind(locale)
            .bind(text)
            .bind(match locale {
                "fr" => "À conserver au sec, à l'abri de la lumière.",
                "ar" => "يُحفظ في مكان جاف بعيدًا عن الضوء.",
                _ => "Keep in a dry place, away from light.",
            })
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query(
        "INSERT INTO product_categories (product_id, category_id, is_primary, sort_order)
         VALUES ($1, $2, true, (SELECT COALESCE(max(sort_order) + 1, 0) FROM product_categories WHERE category_id = $2))",
    )
    .bind(id)
    .bind(p.category)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO product_taxes (product_id, tax_id) VALUES ($1, $2)")
        .bind(id)
        .bind(p.tax)
        .execute(&mut *tx)
        .await?;
    Ok(id)
}

/// An option with its values: `(code, names, [(code, labels)])`.
type OptionSeed<'a> = (&'a str, T3<'a>, Vec<(&'a str, T3<'a>)>);

async fn options(
    tx: &mut PgConnection,
    product: Uuid,
    opts: &[OptionSeed<'_>],
) -> anyhow::Result<Vec<Vec<(String, Uuid, Uuid)>>> {
    let mut out = Vec::new();
    for (i, (code, names, values)) in opts.iter().enumerate() {
        let oid = Uuid::now_v7();
        sqlx::query("INSERT INTO product_options (id, product_id, code, sort_order) VALUES ($1, $2, $3, $4)")
            .bind(oid)
            .bind(product)
            .bind(code)
            .bind(i as i32)
            .execute(&mut *tx)
            .await?;
        translate(
            tx,
            "product_option_translations",
            "option_id",
            oid,
            "name",
            *names,
        )
        .await?;
        let mut vals = Vec::new();
        for (j, (vcode, labels)) in values.iter().enumerate() {
            let vid = Uuid::now_v7();
            sqlx::query("INSERT INTO product_option_values (id, option_id, code, sort_order) VALUES ($1, $2, $3, $4)")
                .bind(vid)
                .bind(oid)
                .bind(vcode)
                .bind(j as i32)
                .execute(&mut *tx)
                .await?;
            translate(
                tx,
                "product_option_value_translations",
                "option_value_id",
                vid,
                "label",
                *labels,
            )
            .await?;
            vals.push((vcode.to_string(), oid, vid));
        }
        out.push(vals);
    }
    sqlx::query("UPDATE shop_products SET option_count = $2 WHERE product_id = $1")
        .bind(product)
        .bind(opts.len() as i16)
        .execute(&mut *tx)
        .await?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn variant(
    tx: &mut PgConnection,
    product: Uuid,
    sku: &str,
    price: i64,
    weight: i32,
    net: Option<(&str, &str)>,
    default: bool,
    option: Option<(Uuid, Uuid)>,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO product_variants (id, product_id, sku, price_minor, weight_grams, net_quantity, net_unit, is_default, cost_minor, sort_order,
                                       country_of_origin)
         VALUES ($1, $2, $3, $4, $5, $6::numeric, $7, $8, $4 / 2, $4, 'MA')",
    )
    .bind(id)
    .bind(product)
    .bind(sku)
    .bind(price)
    .bind(weight)
    .bind(net.map(|n| n.0))
    .bind(net.map(|n| n.1))
    .bind(default)
    .execute(&mut *tx)
    .await?;
    if let Some((o, v)) = option {
        sqlx::query("INSERT INTO product_variant_option_values (variant_id, option_id, option_value_id) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(o)
            .bind(v)
            .execute(&mut *tx)
            .await?;
    }
    Ok(id)
}

#[allow(clippy::too_many_lines)]
pub async fn seed(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let t = &mut *tx;

    // Categories: five main ones, a subcategory (nesting) and a seasonal one hidden from menus.
    let dates = category(
        t,
        CategorySeed {
            sort: 1,
            parent: None,
            show_in_menu: true,
            photo: Some("farm-fields"),
            texts: [
                ("fr", "Dattes", "dattes", "Medjool et Boufeggous de notre palmeraie de M'ssici, cueillies à la main et triées à la ferme.",
                 "Dattes de M'ssici : Medjool et Boufeggous", "Dattes Medjool et Boufeggous de la palmeraie de M'ssici, près d'Alnif : cueillies à la main, triées à la ferme, expédiées en France."),
                ("en", "Dates", "dates", "Medjool and Boufeggous dates from our palm grove in M'ssici, picked by hand and sorted on the farm.",
                 "Dates from M'ssici: Medjool and Boufeggous", "Medjool and Boufeggous dates from the M'ssici palm grove near Alnif: hand-picked, sorted on the farm, shipped to France."),
                ("ar", "التمور", "tumur", "تمور المجهول وبوفقوس من واحتنا في مسيسي، مقطوفة باليد ومفروزة في الضيعة.",
                 "تمور مسيسي: المجهول وبوفقوس", "تمور المجهول وبوفقوس من واحة مسيسي قرب ألنيف، مقطوفة باليد ومفروزة في الضيعة."),
            ],
        },
    )
    .await?;
    let medjool_dates = category(
        t,
        CategorySeed {
            sort: 1,
            parent: Some(dates),
            show_in_menu: true,
            photo: None,
            texts: [
                (
                    "fr",
                    "Dattes Medjool",
                    "dattes-medjool",
                    "La reine des dattes : grosse, moelleuse, au goût de caramel.",
                    "Dattes Medjool de M'ssici",
                    "Dattes Medjool de notre palmeraie : grosses, moelleuses, goût caramel.",
                ),
                (
                    "en",
                    "Medjool dates",
                    "medjool-dates",
                    "The queen of dates: large, soft, with a caramel taste.",
                    "Medjool dates from M'ssici",
                    "Medjool dates from our palm grove: large, soft, caramel taste.",
                ),
                (
                    "ar",
                    "تمر المجهول",
                    "tamr-majhoul-categorie",
                    "ملكة التمور: كبيرة وطرية بطعم الكراميل.",
                    "تمر المجهول من مسيسي",
                    "تمر المجهول من واحتنا: كبير وطري بطعم الكراميل.",
                ),
            ],
        },
    )
    .await?;
    let oil = category(
        t,
        CategorySeed {
            sort: 2,
            parent: None,
            show_in_menu: true,
            photo: Some("oasis"),
            texts: [
                ("fr", "Huile d'olive et olives", "huile-olives", "Huile d'olive pressée à froid et olives de nos oliviers, entre dunes et oasis.",
                 "Huile d'olive et olives de M'ssici", "Huile d'olive vierge extra pressée à froid et olives de la ferme Mouzoun, dans le sud du Maroc."),
                ("en", "Olive oil & olives", "olive-oil-olives", "Cold-pressed olive oil and olives from our trees, between dunes and oasis.",
                 "Olive oil and olives from M'ssici", "Cold-pressed extra virgin olive oil and olives from the Mouzoun farm in southern Morocco."),
                ("ar", "زيت الزيتون والزيتون", "zayt-zaytoun", "زيت زيتون معصور على البارد وزيتون من أشجارنا بين الكثبان والواحة.",
                 "زيت الزيتون والزيتون من مسيسي", "زيت زيتون بكر ممتاز معصور على البارد وزيتون من ضيعة موزون في جنوب المغرب."),
            ],
        },
    )
    .await?;
    let pantry = category(
        t,
        CategorySeed {
            sort: 3,
            parent: None,
            show_in_menu: true,
            photo: Some("mint-tea"),
            texts: [
                (
                    "fr",
                    "Épices et épicerie",
                    "epices-epicerie",
                    "Ras el hanout, herbes du désert et produits de l'épicerie de la ferme.",
                    "Épices et épicerie du désert",
                    "Ras el hanout, herbes séchées et épicerie de la ferme Mouzoun à M'ssici.",
                ),
                (
                    "en",
                    "Spices & pantry",
                    "spices-pantry",
                    "Ras el hanout, desert herbs and pantry goods from the farm.",
                    "Desert spices and pantry",
                    "Ras el hanout, dried herbs and pantry goods from the Mouzoun farm in M'ssici.",
                ),
                (
                    "ar",
                    "التوابل والبقالة",
                    "tawabil",
                    "رأس الحانوت وأعشاب الصحراء ومنتجات بقالة الضيعة.",
                    "توابل وبقالة الصحراء",
                    "رأس الحانوت والأعشاب المجففة وبقالة ضيعة موزون في مسيسي.",
                ),
            ],
        },
    )
    .await?;
    let crafts = category(
        t,
        CategorySeed {
            sort: 4,
            parent: None,
            show_in_menu: true,
            photo: Some("ksar"),
            texts: [
                ("fr", "Artisanat", "artisanat", "Paniers et objets tressés par les artisanes de la région d'Alnif.",
                 "Artisanat du sud marocain", "Paniers et objets faits main par les artisanes de la région d'Alnif et de Rissani."),
                ("en", "Crafts", "crafts", "Baskets and woven pieces made by women artisans around Alnif.",
                 "Crafts from southern Morocco", "Handmade baskets and woven pieces by women artisans around Alnif and Rissani."),
                ("ar", "الصناعة التقليدية", "sinaa-taqlidiya", "سلال وقطع منسوجة من صنع حرفيات منطقة ألنيف.",
                 "صناعة تقليدية من جنوب المغرب", "سلال وقطع يدوية من صنع حرفيات منطقة ألنيف والريصاني."),
            ],
        },
    )
    .await?;
    let boxes = category(
        t,
        CategorySeed {
            sort: 5,
            parent: None,
            show_in_menu: true,
            photo: Some("camp"),
            texts: [
                ("fr", "Coffrets cadeaux", "coffrets", "Des coffrets prêts à offrir : dattes, huile, épices et thé.",
                 "Coffrets cadeaux de la ferme", "Coffrets cadeaux de la ferme Mouzoun : dattes, huile d'olive, épices et thé, prêts à offrir."),
                ("en", "Gift boxes", "gift-boxes", "Ready-to-give boxes: dates, oil, spices and tea.",
                 "Gift boxes from the farm", "Mouzoun farm gift boxes: dates, olive oil, spices and tea, ready to give."),
                ("ar", "علب الهدايا", "ulab-hadaya", "علب جاهزة للإهداء: تمور وزيت وتوابل وشاي.",
                 "علب هدايا من الضيعة", "علب هدايا من ضيعة موزون: تمور وزيت زيتون وتوابل وشاي، جاهزة للإهداء."),
            ],
        },
    )
    .await?;
    // Seasonal (Ramadan, end of year): its page works, it is not in the sidebar or menus.
    let seasonal = category(
        t,
        CategorySeed {
            sort: 6,
            parent: None,
            show_in_menu: false,
            photo: Some("night-sky"),
            texts: [
                (
                    "fr",
                    "Coffrets de saison",
                    "coffrets-de-saison",
                    "Nos coffrets du moment, pour le Ramadan et les fêtes de fin d'année.",
                    "Coffrets de saison",
                    "Coffrets de saison de la ferme Mouzoun pour le Ramadan et les fêtes.",
                ),
                (
                    "en",
                    "Seasonal boxes",
                    "seasonal-boxes",
                    "Our boxes of the moment, for Ramadan and the end-of-year holidays.",
                    "Seasonal boxes",
                    "Seasonal boxes from the Mouzoun farm for Ramadan and the holidays.",
                ),
                (
                    "ar",
                    "علب الموسم",
                    "ulab-al-mawsim",
                    "علبنا لهذا الموسم، لرمضان وأعياد نهاية السنة.",
                    "علب الموسم",
                    "علب الموسم من ضيعة موزون لرمضان والأعياد.",
                ),
            ],
        },
    )
    .await?;

    // Shipping.
    let mut profiles = Vec::new();
    for (code, name, default) in [
        ("standard", "Standard", true),
        ("liquids", "Liquides (huile)", false),
        ("perishable", "Périssable", false),
    ] {
        let id: Uuid = sqlx::query_scalar("INSERT INTO shipping_profiles (id, code, name, is_default) VALUES ($1, $2, $3, $4) RETURNING id")
            .bind(Uuid::now_v7())
            .bind(code)
            .bind(name)
            .bind(default)
            .fetch_one(&mut *t)
            .await?;
        profiles.push(id);
    }
    let (standard, liquids, perishable) = (profiles[0], profiles[1], profiles[2]);
    let morocco: Uuid = sqlx::query_scalar("INSERT INTO shipping_zones (id, code, country_codes, sort_order) VALUES ($1, 'morocco', '{MA}', 1) RETURNING id")
        .bind(Uuid::now_v7())
        .fetch_one(&mut *t)
        .await?;
    translate(
        t,
        "shipping_zone_translations",
        "zone_id",
        morocco,
        "name",
        [("fr", "Maroc"), ("en", "Morocco"), ("ar", "المغرب")],
    )
    .await?;
    let europe: Uuid = sqlx::query_scalar(
        "INSERT INTO shipping_zones (id, code, country_codes, is_active, sort_order) VALUES ($1, 'europe', '{ES,BE,DE,IT,NL,PT,AT,LU}', false, 3) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .fetch_one(&mut *t)
    .await?;
    translate(
        t,
        "shipping_zone_translations",
        "zone_id",
        europe,
        "name",
        [("fr", "Europe"), ("en", "Europe"), ("ar", "أوروبا")],
    )
    .await?;
    // Morocco: flat rate, free over 50 € (about 550 MAD), cash on delivery allowed.
    for (profile, code) in [
        (standard, "ma_standard"),
        (liquids, "ma_liquids"),
        (perishable, "ma_perishable"),
    ] {
        let rate: Uuid = sqlx::query_scalar(
            "INSERT INTO shipping_rates (id, zone_id, profile_id, code, kind, carrier, amount_minor, free_over_minor, delivery_days_min,
                                         delivery_days_max, allows_cash_on_delivery, cod_fee_minor)
             VALUES ($1, $2, $3, $4, 'flat', 'amana', 450, 5000, 2, 5, true, 150) RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(morocco)
        .bind(profile)
        .bind(code)
        .fetch_one(&mut *t)
        .await?;
        translate(
            t,
            "shipping_rate_translations",
            "rate_id",
            rate,
            "name",
            [
                ("fr", "Amana (2 à 5 jours)"),
                ("en", "Amana (2 to 5 days)"),
                ("ar", "أمانة (من 2 إلى 5 أيام)"),
            ],
        )
        .await?;
    }
    // Europe (inactive zone): weight-based, liquids allowed, perishable not shipped.
    for (profile, code, base, per_kg) in [
        (standard, "eu_standard", 1500, 400),
        (liquids, "eu_liquids", 1900, 600),
    ] {
        let rate: Uuid = sqlx::query_scalar(
            "INSERT INTO shipping_rates (id, zone_id, profile_id, code, kind, carrier, amount_minor, per_kg_minor, delivery_days_min, delivery_days_max)
             VALUES ($1, $2, $3, $4, 'weight_based', 'dhl', $5, $6, 4, 8) RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(europe)
        .bind(profile)
        .bind(code)
        .bind(base as i64)
        .bind(per_kg as i64)
        .fetch_one(&mut *t)
        .await?;
        translate(
            t,
            "shipping_rate_translations",
            "rate_id",
            rate,
            "name",
            [
                ("fr", "DHL Express"),
                ("en", "DHL Express"),
                ("ar", "DHL السريع"),
            ],
        )
        .await?;
    }

    // France métropolitaine (the French company ships from its warehouse): Colissimo at home,
    // Mondial Relay in relay points, free over 60 €. Corsica (20xxx) and overseas (97xxx, 98xxx)
    // are not covered: no zone matches them.
    let metro_patterns: Vec<String> = (1..=95)
        .filter(|d| *d != 20)
        .map(|d| format!("{d:02}*"))
        .collect();
    let france: Uuid = sqlx::query_scalar(
        "INSERT INTO shipping_zones (id, code, country_codes, postal_code_patterns, sort_order) VALUES ($1, 'france_metro', '{FR}', $2, 0) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(&metro_patterns)
    .fetch_one(&mut *t)
    .await?;
    translate(
        t,
        "shipping_zone_translations",
        "zone_id",
        france,
        "name",
        [
            ("fr", "France métropolitaine"),
            ("en", "Mainland France"),
            ("ar", "فرنسا القارية"),
        ],
    )
    .await?;
    for (profile, code, carrier, delivery, amount, days, names) in [
        (
            standard,
            "fr_colissimo",
            "colissimo",
            "home",
            790,
            (2, 3),
            [
                ("fr", "Colissimo à domicile (2 à 3 jours)"),
                ("en", "Colissimo home delivery (2 to 3 days)"),
                ("ar", "كوليسيمو إلى المنزل (2 إلى 3 أيام)"),
            ],
        ),
        (
            liquids,
            "fr_colissimo_liquids",
            "colissimo",
            "home",
            990,
            (2, 3),
            [
                ("fr", "Colissimo à domicile (2 à 3 jours)"),
                ("en", "Colissimo home delivery (2 to 3 days)"),
                ("ar", "كوليسيمو إلى المنزل (2 إلى 3 أيام)"),
            ],
        ),
        (
            standard,
            "fr_mondial_relay",
            "mondial_relay",
            "relay_point",
            450,
            (3, 5),
            [
                ("fr", "Mondial Relay en point relais (3 à 5 jours)"),
                ("en", "Mondial Relay pick-up point (3 to 5 days)"),
                ("ar", "مونديال ريلاي في نقطة استلام (3 إلى 5 أيام)"),
            ],
        ),
        (
            liquids,
            "fr_mondial_relay_liquids",
            "mondial_relay",
            "relay_point",
            590,
            (3, 5),
            [
                ("fr", "Mondial Relay en point relais (3 à 5 jours)"),
                ("en", "Mondial Relay pick-up point (3 to 5 days)"),
                ("ar", "مونديال ريلاي في نقطة استلام (3 إلى 5 أيام)"),
            ],
        ),
    ] {
        let rate: Uuid = sqlx::query_scalar(
            "INSERT INTO shipping_rates (id, zone_id, profile_id, code, kind, carrier, amount_minor, free_over_minor, delivery_days_min,
                                         delivery_days_max, delivery_type)
             VALUES ($1, $2, $3, $4, 'flat', $5, $6, 6000, $7, $8, $9) RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(france)
        .bind(profile)
        .bind(code)
        .bind(carrier)
        .bind(amount as i64)
        .bind(days.0)
        .bind(days.1)
        .bind(delivery)
        .fetch_one(&mut *t)
        .await?;
        translate(
            t,
            "shipping_rate_translations",
            "rate_id",
            rate,
            "name",
            names,
        )
        .await?;
    }

    // TVA: placeholder rates, to confirm with the accountant before going live.
    let mut taxes = Vec::new();
    for (code, rate, fr, en, ar) in [
        (
            "tva_food",
            "10",
            "TVA produits alimentaires (taux à confirmer)",
            "VAT food (rate to confirm)",
            "ضريبة المواد الغذائية (المعدل للتأكيد)",
        ),
        (
            "tva_standard",
            "20",
            "TVA taux normal (à confirmer)",
            "VAT standard rate (to confirm)",
            "الضريبة بالمعدل العادي (للتأكيد)",
        ),
    ] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO taxes (id, code, calc_type, rate_percent, is_inclusive) VALUES ($1, $2, 'percent', $3::numeric, true)
             ON CONFLICT (code) DO UPDATE SET rate_percent = EXCLUDED.rate_percent RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(code)
        .bind(rate)
        .fetch_one(&mut *t)
        .await?;
        translate(
            t,
            "tax_translations",
            "tax_id",
            id,
            "name",
            [("fr", fr), ("en", en), ("ar", ar)],
        )
        .await?;
        taxes.push(id);
    }
    let (tva_food, tva_standard) = (taxes[0], taxes[1]);

    // Origin: the farm fields and the date crop (seeded with the camp).
    let fields: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM places WHERE kind = 'farm_field' ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(&mut *t)
    .await?;
    let crop_of = |code: &'static str| {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM farm_crops WHERE code = $1").bind(code)
    };
    let date_crop = crop_of("date").fetch_optional(&mut *t).await?;
    let olive_crop = crop_of("olive").fetch_optional(&mut *t).await?;

    // Stock location.
    let store = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO stock_locations (id, code, place_id, is_pickup_point, fulfills_online_orders, address, country_code)
         VALUES ($1, 'camp_store', NULL, true, true, $2, 'MA')",
    )
    .bind(store)
    .bind(json!({ "line1": "Camp Mouzoun", "city": "M'ssici", "country_code": "MA" }))
    .execute(&mut *t)
    .await?;
    // The camp store belongs to the Moroccan entity: not sold by the French shop.
    sqlx::query("UPDATE stock_locations SET country_code = 'MA', legal_entity_id = (SELECT id FROM legal_entities WHERE code = 'mouzoun_ma') WHERE id = $1")
        .bind(store)
        .execute(&mut *t)
        .await?;
    let warehouse = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO stock_locations (id, code, is_pickup_point, fulfills_online_orders, address, country_code, legal_entity_id, sort_order)
         VALUES ($1, 'entrepot_france', false, true, $2, 'FR', (SELECT id FROM legal_entities WHERE code = 'mouzoun_fr'), 0)",
    )
    .bind(warehouse)
    .bind(json!({ "line1": "À COMPLÉTER", "postal_code": "75000", "city": "Paris", "country_code": "FR" }))
    .execute(&mut *t)
    .await?;
    translate(
        t,
        "stock_location_translations",
        "location_id",
        warehouse,
        "name",
        [
            ("fr", "Entrepôt France"),
            ("en", "France warehouse"),
            ("ar", "مستودع فرنسا"),
        ],
    )
    .await?;
    for (locale, name, instructions) in [
        (
            "fr",
            "Boutique de la ferme du camp",
            "À l'accueil du camp, tous les jours de 9 h à 19 h. Présentez votre code de retrait.",
        ),
        (
            "en",
            "Camp farm store",
            "At the camp reception, daily 9 am to 7 pm. Show your pickup code.",
        ),
        (
            "ar",
            "متجر مزرعة المخيم",
            "عند استقبال المخيم يوميًا من 9 صباحًا إلى 7 مساءً. قدّم رمز الاستلام.",
        ),
    ] {
        sqlx::query("INSERT INTO stock_location_translations (location_id, locale, name, pickup_instructions) VALUES ($1, $2, $3, $4)")
            .bind(store)
            .bind(locale)
            .bind(name)
            .bind(instructions)
            .execute(&mut *t)
            .await?;
    }

    let size = |a: &'static str, b: &'static str, c: &'static str| -> T3<'static> {
        [("fr", a), ("en", b), ("ar", c)]
    };
    let ingredients_dates = Some(size("Dattes (100 %).", "Dates (100%).", "تمر (100٪)."));

    // 1. Medjool dates: Size 500 g / 1 kg, lots with best-before.
    let medjool = product(
        t,
        &ProductSeed {
            names: [
                (
                    "fr",
                    "Dattes Medjool",
                    "dattes-medjool",
                    "Grosses dattes moelleuses de notre palmeraie, cueillies à la main.",
                ),
                (
                    "en",
                    "Medjool dates",
                    "medjool-dates",
                    "Large, soft dates from our palm grove, picked by hand.",
                ),
                (
                    "ar",
                    "تمر المجهول",
                    "tamr-majhoul",
                    "تمور كبيرة وطرية من واحتنا، مقطوفة باليد.",
                ),
            ],
            product_type: "food",
            profile: standard,
            category: dates,
            perishable: false,
            track_lots: true,
            allergens: &[],
            photo: "farm-fields",
            tax: tva_food,
            place: fields,
            crop: date_crop,
            featured: true,
            ingredients: ingredients_dates,
        },
    )
    .await?;
    let opts = options(
        t,
        medjool,
        &[(
            "size",
            size("Taille", "Size", "الحجم"),
            vec![
                ("500g", size("500 g", "500 g", "500 غ")),
                ("1kg", size("1 kg", "1 kg", "1 كغ")),
            ],
        )],
    )
    .await?;
    let m500 = variant(
        t,
        medjool,
        "MEDJOOL-500G",
        1200,
        560,
        Some(("500", "g")),
        true,
        Some((opts[0][0].1, opts[0][0].2)),
    )
    .await?;
    let m1kg = variant(
        t,
        medjool,
        "MEDJOOL-1KG",
        2200,
        1080,
        Some(("1", "kg")),
        false,
        Some((opts[0][1].1, opts[0][1].2)),
    )
    .await?;

    // 2. Boufeggous dates 500 g.
    let boufeggous = product(
        t,
        &ProductSeed {
            names: [
                (
                    "fr",
                    "Dattes Boufeggous",
                    "dattes-boufeggous",
                    "Variété locale à la chair ferme et au goût de miel.",
                ),
                (
                    "en",
                    "Boufeggous dates",
                    "boufeggous-dates",
                    "A local variety with firm flesh and a honey taste.",
                ),
                (
                    "ar",
                    "تمر بوفقوس",
                    "tamr-boufeggous",
                    "صنف محلي بلب متماسك وطعم العسل.",
                ),
            ],
            product_type: "food",
            profile: standard,
            category: dates,
            perishable: false,
            track_lots: false,
            allergens: &[],
            photo: "farm-fields",
            tax: tva_food,
            place: fields,
            crop: date_crop,
            featured: false,
            ingredients: ingredients_dates,
        },
    )
    .await?;
    let b500 = variant(
        t,
        boufeggous,
        "BOUFEGGOUS-500G",
        900,
        560,
        Some(("500", "g")),
        true,
        None,
    )
    .await?;

    // 3. Extra virgin olive oil: 500 ml / 1 L / 5 L tin (liquids).
    let olive_oil = product(
        t,
        &ProductSeed {
            names: [
                (
                    "fr",
                    "Huile d'olive vierge extra",
                    "huile-olive-vierge-extra",
                    "Pressée à froid à la récolte de novembre.",
                ),
                (
                    "en",
                    "Extra virgin olive oil",
                    "extra-virgin-olive-oil",
                    "Cold-pressed at the November harvest.",
                ),
                (
                    "ar",
                    "زيت زيتون بكر ممتاز",
                    "zayt-zaytoun-bikr",
                    "معصور على البارد في جني نونبر.",
                ),
            ],
            product_type: "food",
            profile: liquids,
            category: oil,
            perishable: false,
            track_lots: false,
            allergens: &[],
            photo: "farm-fields",
            tax: tva_food,
            place: fields,
            crop: olive_crop,
            featured: true,
            ingredients: Some(size(
                "Huile d'olive vierge extra (100 %).",
                "Extra virgin olive oil (100%).",
                "زيت زيتون بكر ممتاز (100٪).",
            )),
        },
    )
    .await?;
    let opts = options(
        t,
        olive_oil,
        &[(
            "size",
            size("Contenance", "Size", "السعة"),
            vec![
                ("500ml", size("500 ml", "500 ml", "500 مل")),
                ("1l", size("1 L", "1 L", "1 لتر")),
                ("5l", size("Bidon 5 L", "5 L tin", "صفيحة 5 لتر")),
            ],
        )],
    )
    .await?;
    let o500 = variant(
        t,
        olive_oil,
        "OLIVE-OIL-500ML",
        1100,
        750,
        Some(("500", "ml")),
        true,
        Some((opts[0][0].1, opts[0][0].2)),
    )
    .await?;
    let o1l = variant(
        t,
        olive_oil,
        "OLIVE-OIL-1L",
        1800,
        1350,
        Some(("1", "l")),
        false,
        Some((opts[0][1].1, opts[0][1].2)),
    )
    .await?;
    let o5l = variant(
        t,
        olive_oil,
        "OLIVE-OIL-5L",
        7500,
        5400,
        Some(("5", "l")),
        false,
        Some((opts[0][2].1, opts[0][2].2)),
    )
    .await?;

    // 4. Marinated olives, 400 g jar (perishable).
    let olives = product(
        t,
        &ProductSeed {
            names: [
                (
                    "fr",
                    "Olives marinées",
                    "olives-marinees",
                    "Olives de la ferme marinées au citron beldi et aux herbes.",
                ),
                (
                    "en",
                    "Marinated olives",
                    "marinated-olives",
                    "Farm olives marinated with beldi lemon and herbs.",
                ),
                (
                    "ar",
                    "زيتون مُتبّل",
                    "zaytoun-mutabbal",
                    "زيتون المزرعة متبّل بالليمون البلدي والأعشاب.",
                ),
            ],
            product_type: "food",
            profile: perishable,
            category: oil,
            perishable: true,
            track_lots: false,
            allergens: &["sulphites"],
            photo: "mint-tea",
            tax: tva_food,
            place: fields,
            crop: olive_crop,
            featured: true,
            ingredients: Some(size(
                "Olives, eau, sel, citron, herbes, conservateur (sulfites).",
                "Olives, water, salt, lemon, herbs, preservative (sulphites).",
                "زيتون، ماء، ملح، ليمون، أعشاب، مادة حافظة (كبريتيت).",
            )),
        },
    )
    .await?;
    let jar = variant(
        t,
        olives,
        "OLIVES-JAR-400G",
        650,
        650,
        Some(("400", "g")),
        true,
        None,
    )
    .await?;

    // 5. Ras el hanout, 100 g.
    let ras = product(
        t,
        &ProductSeed {
            names: [
                (
                    "fr",
                    "Ras el hanout",
                    "ras-el-hanout",
                    "Mélange de 27 épices moulu à la ferme.",
                ),
                (
                    "en",
                    "Ras el hanout",
                    "ras-el-hanout-spice",
                    "A blend of 27 spices ground at the farm.",
                ),
                (
                    "ar",
                    "رأس الحانوت",
                    "ras-al-hanout",
                    "خليط من 27 نوعًا من التوابل مطحون في المزرعة.",
                ),
            ],
            product_type: "food",
            profile: standard,
            category: pantry,
            perishable: false,
            track_lots: false,
            allergens: &["mustard", "celery"],
            photo: "ksar",
            tax: tva_food,
            place: None,
            crop: None,
            featured: false,
            ingredients: Some(size(
                "Cumin, coriandre, gingembre, curcuma, cannelle, moutarde, céleri…",
                "Cumin, coriander, ginger, turmeric, cinnamon, mustard, celery…",
                "كمون، كزبرة، زنجبيل، كركم، قرفة، خردل، كرفس…",
            )),
        },
    )
    .await?;
    let ras100 = variant(
        t,
        ras,
        "RAS-EL-HANOUT-100G",
        550,
        130,
        Some(("100", "g")),
        true,
        None,
    )
    .await?;

    // 6. Handwoven basket (craft, no options).
    let basket = product(
        t,
        &ProductSeed {
            names: [
                (
                    "fr",
                    "Panier tressé à la main",
                    "panier-tresse",
                    "Tressé en palmes de doum par les femmes du village.",
                ),
                (
                    "en",
                    "Handwoven basket",
                    "handwoven-basket",
                    "Woven from doum palm leaves by the village women.",
                ),
                (
                    "ar",
                    "سلة منسوجة يدويًا",
                    "salla-mansouja",
                    "منسوجة من سعف الدوم بأيدي نساء القرية.",
                ),
            ],
            product_type: "craft",
            profile: standard,
            category: crafts,
            perishable: false,
            track_lots: false,
            allergens: &[],
            photo: "ksar",
            tax: tva_standard,
            place: None,
            crop: None,
            featured: false,
            ingredients: None,
        },
    )
    .await?;
    let basket_v = variant(t, basket, "BASKET-DOUM", 2500, 600, None, true, None).await?;

    // 7. "Taste of M'ssici" gift box.
    let gift = product(
        t,
        &ProductSeed {
            names: [
                (
                    "fr",
                    "Coffret « Saveurs de M'ssici »",
                    "coffret-saveurs-mssici",
                    "Dattes Medjool, huile d'olive et ras el hanout dans un panier tressé.",
                ),
                (
                    "en",
                    "\"Taste of M'ssici\" gift box",
                    "taste-of-mssici-gift-box",
                    "Medjool dates, olive oil and ras el hanout in a woven basket.",
                ),
                (
                    "ar",
                    "علبة «نكهات مسيسي»",
                    "ulbat-nakahat-mssici",
                    "تمر المجهول وزيت الزيتون ورأس الحانوت في سلة منسوجة.",
                ),
            ],
            product_type: "gift_box",
            profile: liquids,
            category: boxes,
            perishable: false,
            track_lots: false,
            allergens: &["mustard", "celery"],
            photo: "camp",
            tax: tva_food,
            place: None,
            crop: None,
            featured: true,
            ingredients: Some(size(
                "Dattes Medjool 500 g, huile d'olive vierge extra 50 cl, ras el hanout 100 g (moutarde, céleri), panier en feuilles de palmier.",
                "Medjool dates 500 g, extra virgin olive oil 50 cl, ras el hanout 100 g (mustard, celery), palm-leaf basket.",
                "تمر المجهول 500 غ، زيت زيتون بكر ممتاز 50 سل، رأس الحانوت 100 غ (خردل، كرفس)، سلة من سعف النخيل.",
            )),
        },
    )
    .await?;
    // Manual order in "Dattes": Boufeggous pinned first, then Medjool. Medjool is also in its
    // subcategory; the gift box also sits in the seasonal boxes.
    sqlx::query("UPDATE product_categories SET is_pinned = true, sort_order = 0 WHERE category_id = $1 AND product_id = $2")
        .bind(dates)
        .bind(boufeggous)
        .execute(&mut *t)
        .await?;
    sqlx::query(
        "UPDATE product_categories SET sort_order = 1 WHERE category_id = $1 AND product_id = $2",
    )
    .bind(dates)
    .bind(medjool)
    .execute(&mut *t)
    .await?;
    for (category, product) in [(medjool_dates, medjool), (seasonal, gift)] {
        sqlx::query("INSERT INTO product_categories (product_id, category_id, sort_order) VALUES ($1, $2, 0)")
            .bind(product)
            .bind(category)
            .execute(&mut *t)
            .await?;
    }
    let gift_v = variant(
        t,
        gift,
        "GIFT-BOX-MSSICI",
        4900,
        2400,
        Some(("1", "piece")),
        true,
        None,
    )
    .await?;

    // Related products.
    for (a, b) in [
        (medjool, boufeggous),
        (medjool, gift),
        (olive_oil, olives),
        (olive_oil, gift),
        (ras, gift),
    ] {
        sqlx::query("INSERT INTO related_products (product_id, related_product_id) VALUES ($1, $2), ($2, $1) ON CONFLICT DO NOTHING")
            .bind(a)
            .bind(b)
            .execute(&mut *t)
            .await?;
    }

    // Stock: Medjool in two harvest lots, the rest without lots.
    let today = mouzoun_api::camp_time::today(Utc::now());
    let refs = MovementRefs {
        note: Some("initial stock (demo)".into()),
        ..MovementRefs::default()
    };
    for (v, code, harvested_days_ago, best_before_days, qty) in [
        (m500, "MJ-2026-A", 60, 300, 40),
        (m500, "MJ-2026-B", 20, 340, 30),
        (m1kg, "MJ-2026-A", 60, 300, 15),
    ] {
        inventory::receive(
            t,
            v,
            store,
            qty,
            Some(&NewLot {
                lot_code: code.into(),
                produced_on: None,
                harvested_on: Some(today - Duration::days(harvested_days_ago)),
                best_before: Some(today + Duration::days(best_before_days)),
                notes: None,
            }),
            &refs,
        )
        .await
        .context("receiving dates")?;
    }
    // The French warehouse holds the stock sold online (imported, cleared through customs).
    for (v, qty) in [(m500, 40), (m1kg, 15)] {
        let lot = NewLot {
            lot_code: "MJ-2026-FR".into(),
            produced_on: None,
            harvested_on: Some(today - Duration::days(45)),
            best_before: Some(today + Duration::days(320)),
            notes: Some("imported to France".into()),
        };
        inventory::receive(t, v, warehouse, qty, Some(&lot), &refs).await?;
    }
    for (v, qty) in [
        (b500, 25),
        (o500, 30),
        (o1l, 20),
        (o5l, 6),
        (jar, 12),
        (ras100, 40),
        (basket_v, 8),
        (gift_v, 5),
    ] {
        inventory::receive(t, v, warehouse, qty, None, &refs).await?;
    }
    // Gift box: dates and oil (5.5 %) with a palm-leaf basket (20 %), split by value.
    sqlx::query(
        "UPDATE shop_products SET bundle_vat_split = '[{\"tax_class_code\": \"food_reduced\", \"share_percent\": 70}, {\"tax_class_code\": \"standard\", \"share_percent\": 30}]'
         WHERE product_type = 'gift_box'",
    )
    .execute(&mut *t)
    .await?;
    for (v, qty) in [
        (b500, 25),
        (o500, 30),
        (o1l, 20),
        (o5l, 6),
        (jar, 12),
        (ras100, 40),
        (basket_v, 8),
        (gift_v, 5),
    ] {
        inventory::receive(t, v, store, qty, None, &refs).await?;
    }

    // Welcome code (inactive): −10 % on a first order.
    let promo = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO promotions (id, code, scope, discount_type, discount_percent, customer_eligibility, is_active, applies_to_all)
         VALUES ($1, 'BIENVENUE10', 'shop', 'percent', 10, 'first_order', false, true)",
    )
    .bind(promo)
    .execute(&mut *t)
    .await?;
    translate(
        t,
        "promotion_translations",
        "promotion_id",
        promo,
        "name",
        [
            ("fr", "Bienvenue : −10 %"),
            ("en", "Welcome: −10%"),
            ("ar", "مرحبًا: −10٪"),
        ],
    )
    .await?;

    let count: i64 = sqlx::query("SELECT count(*) AS n FROM products WHERE kind = 'shop_product'")
        .fetch_one(&mut *t)
        .await?
        .get("n");
    anyhow::ensure!(count == 7, "expected 7 shop products, found {count}");
    tx.commit().await?;
    Ok(())
}
