//! Demo pages, built through the page builder service (so every section passes its schema):
//! the "coming soon" home page, the About page, two shared sections (call to action and
//! newsletter) placed on both, the opening top bar, then revision 1 of each page published.
//!
//! Placeholder photos are the front's graded Commons pictures (`front/public/images/photos`),
//! stored as media with storage provider `site` (the key is a path served by the front).

use std::collections::{BTreeMap, HashSet};

use anyhow::Context;
use chrono::Utc;
use mouzoun_api::{
    auth::Staff,
    modules::pages::{
        SectionRegistry,
        model::{BlockDraft, ItemDraft, PageText, SaveTree},
        service::{self, PublishRequest},
    },
    services::audit::Actor,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// `(slug, width, height, alt fr, alt en, alt ar)`
const PHOTOS: &[(&str, i32, i32, &str, &str, &str)] = &[
    (
        "hero-dunes",
        2000,
        1330,
        "Grande dune de sable doré au matin",
        "A tall golden dune in the morning",
        "كثيب رملي ذهبي كبير في الصباح",
    ),
    (
        "nomad-tent",
        2000,
        1500,
        "Tente berbère ouverte sur les dunes",
        "An open Berber tent on the dunes",
        "خيمة أمازيغية مفتوحة على الكثبان",
    ),
    (
        "camp",
        2000,
        1125,
        "Campement de tentes au pied des dunes",
        "A camp of tents at the foot of the dunes",
        "مخيم خيام عند سفح الكثبان",
    ),
    (
        "caravan",
        2000,
        1330,
        "Caravane de dromadaires au coucher du soleil",
        "A camel caravan at sunset",
        "قافلة جمال عند الغروب",
    ),
    (
        "quads",
        2000,
        1500,
        "Deux quads sur le sable des dunes",
        "Two quad bikes on the sand",
        "دراجتان رباعيتان على الرمال",
    ),
    (
        "oasis",
        2000,
        1325,
        "Palmeraie au premier plan, dune orangée au loin",
        "Palm grove in front, an orange dune in the distance",
        "واحة نخيل وكثيب برتقالي في الأفق",
    ),
    (
        "farm-fields",
        2000,
        1333,
        "Champs verts irrigués entre les palmiers dattiers",
        "Green irrigated fields between date palms",
        "حقول خضراء مسقية بين النخيل",
    ),
    (
        "ksar",
        2000,
        1306,
        "Ksar en terre crue au-dessus de la palmeraie",
        "A mud-brick ksar above the palm grove",
        "قصر من الطين فوق واحة النخيل",
    ),
    (
        "mint-tea",
        2000,
        1500,
        "Thé à la menthe versé dans des verres",
        "Mint tea poured into glasses",
        "شاي بالنعناع يُصب في الكؤوس",
    ),
    (
        "night-sky",
        2000,
        1335,
        "Ciel étoilé au-dessus des dunes",
        "Starry sky above the dunes",
        "سماء مرصعة بالنجوم فوق الكثبان",
    ),
    (
        "sunset-ripples",
        2000,
        1334,
        "Rides de sable au coucher du soleil",
        "Sand ripples at sunset",
        "تموجات الرمل عند الغروب",
    ),
    (
        "host",
        2000,
        1199,
        "Homme en chèche bleu assis sur une dune",
        "A man in a blue turban on a dune",
        "رجل بلثام أزرق على كثيب",
    ),
];

const OPENING_DATE: &str = "2027-03-20";

type AlbumText = Option<(&'static str, &'static str, &'static str)>;
/// `(gallery name, photos, title fr, en, ar, description fr/en/ar)`
type Album = (
    &'static str,
    &'static [&'static str],
    &'static str,
    &'static str,
    &'static str,
    AlbumText,
);

/// Home page gallery: six albums.
const GALLERY_ALBUMS: &[Album] = &[
    (
        "Accueil · Le camp",
        &[
            "camp",
            "nomad-tent",
            "mint-tea",
            "hero-dunes",
            "host",
            "sunset-ripples",
        ],
        "Le camp",
        "The camp",
        "المخيم",
        Some((
            "Au pied des dunes, à quelques minutes de M'ssici.",
            "At the foot of the dunes, a few minutes from M'ssici.",
            "عند سفح الكثبان، على بعد دقائق من مسيسي.",
        )),
    ),
    (
        "Accueil · Tentes et emplacements",
        &[
            "nomad-tent",
            "camp",
            "night-sky",
            "mint-tea",
            "hero-dunes",
            "sunset-ripples",
        ],
        "Tentes et emplacements",
        "Tents and pitches",
        "الخيام والمواقع",
        None,
    ),
    (
        "Accueil · Activités",
        &[
            "caravan",
            "quads",
            "hero-dunes",
            "host",
            "sunset-ripples",
            "oasis",
            "camp",
        ],
        "Activités",
        "Activities",
        "الأنشطة",
        None,
    ),
    (
        "Accueil · La ferme",
        &["farm-fields", "oasis", "ksar", "mint-tea", "host", "camp"],
        "La ferme : olives, dattes et navets",
        "The farm: olives, dates and turnips",
        "المزرعة: الزيتون والتمر واللفت",
        Some((
            "Les champs irrigués entre les palmiers, au rythme des récoltes.",
            "Irrigated fields between the palms, at the pace of the harvests.",
            "حقول مسقية بين النخيل على إيقاع مواسم الحصاد.",
        )),
    ),
    (
        "Accueil · Le village de M'ssici",
        &[
            "ksar",
            "oasis",
            "farm-fields",
            "host",
            "caravan",
            "hero-dunes",
        ],
        "Le village de M'ssici",
        "M'ssici village",
        "قرية مسيسي",
        None,
    ),
    (
        "Accueil · Le ciel la nuit",
        &[
            "night-sky",
            "sunset-ripples",
            "camp",
            "nomad-tent",
            "hero-dunes",
            "caravan",
        ],
        "Le ciel la nuit",
        "Night sky",
        "سماء الليل",
        None,
    ),
];

fn text(fr: Value, en: Value, ar: Value) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("fr".to_string(), fr),
        ("en".to_string(), en),
        ("ar".to_string(), ar),
    ])
}

fn block(code: &str, settings: Value, content: BTreeMap<String, Value>) -> BlockDraft {
    BlockDraft {
        id: Uuid::now_v7(),
        block_type: code.into(),
        name: None,
        anchor_id: None,
        is_visible: true,
        active_period: None,
        visible_on: Default::default(),
        hidden_locales: Vec::new(),
        appearance: json!({}),
        analytics_label: None,
        settings,
        content,
        items: Vec::new(),
        shared_block_id: None,
        shared: None,
    }
}

fn item(settings: Value, content: BTreeMap<String, Value>) -> ItemDraft {
    ItemDraft {
        id: Uuid::now_v7(),
        settings,
        content,
        is_visible: true,
        active_period: None,
    }
}

fn placement(shared: &BlockDraft, anchor: Option<&str>) -> BlockDraft {
    BlockDraft {
        anchor_id: anchor.map(str::to_string),
        shared_block_id: Some(shared.id),
        settings: json!({}),
        content: BTreeMap::new(),
        ..block(&shared.block_type, json!({}), BTreeMap::new())
    }
}

/// Blur-up previews from the front's photo manifest, when the repository is checked out whole.
fn lqips() -> BTreeMap<String, String> {
    let path = std::env::var("SEED_PHOTOS_MANIFEST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "../front/app/data/photos.json".into());
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<Value>>(&raw).ok())
        .map(|photos| {
            photos
                .iter()
                .filter_map(|p| {
                    Some((
                        p["slug"].as_str()?.to_string(),
                        p["lqip"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn photos(pool: &PgPool, admin: Uuid) -> anyhow::Result<BTreeMap<&'static str, Uuid>> {
    let lqip = lqips();
    let mut ids = BTreeMap::new();
    for (slug, width, height, fr, en, ar) in PHOTOS {
        let id = Uuid::now_v7();
        let variants: serde_json::Map<String, Value> = [480, 800, 1200, 1600, 2000]
            .iter()
            .map(|w| {
                (
                    format!("w{w}"),
                    json!({"key": format!("/images/photos/{slug}-{w}.webp"), "width": w}),
                )
            })
            .collect();
        sqlx::query!(
            "INSERT INTO media_assets (id, storage_provider, bucket, object_key, original_filename, mime_type, kind,
                                       size_bytes, width, height, blurhash, focal_x, focal_y, variants, uploaded_by)
             VALUES ($1, 'site', 'site', $2, $3, 'image/jpeg', 'image', 0, $4, $5, $6, 0.5, 0.5, $7, $8)",
            id,
            format!("/images/photos/{slug}.jpg"),
            format!("{slug}.jpg"),
            width,
            height,
            lqip.get(*slug),
            Value::Object(variants),
            admin
        )
        .execute(pool)
        .await?;
        for (locale, alt) in [("fr", fr), ("en", en), ("ar", ar)] {
            sqlx::query!(
                "INSERT INTO media_asset_translations (media_asset_id, locale, alt_text) VALUES ($1, $2, $3)",
                id,
                locale,
                alt
            )
            .execute(pool)
            .await?;
        }
        ids.insert(*slug, id);
    }
    Ok(ids)
}

async fn id_of(pool: &PgPool, sku: &str) -> anyhow::Result<Uuid> {
    sqlx::query_scalar!("SELECT id FROM products WHERE sku = $1", sku)
        .fetch_optional(pool)
        .await?
        .with_context(|| format!("product {sku}"))
}

async fn place_of(pool: &PgPool, slug: &str, lat: f64, lng: f64, km: f64) -> anyhow::Result<Uuid> {
    // Approximate coordinates and road distances from the camp, for the demo map.
    sqlx::query_scalar!(
        "UPDATE places SET latitude = $2::float8::numeric, longitude = $3::float8::numeric, distance_km = $4::float8::numeric
         WHERE id = (SELECT place_id FROM place_translations WHERE locale = 'fr' AND slug = $1)
         RETURNING id",
        slug,
        lat,
        lng,
        km
    )
    .fetch_optional(pool)
    .await?
    .with_context(|| format!("place {slug}"))
}

async fn new_page(
    pool: &PgPool,
    staff: &Staff,
    page_type: &str,
    texts: [(&str, &str, &str); 3],
) -> anyhow::Result<Uuid> {
    let translations = texts
        .into_iter()
        .map(|(locale, title, slug)| {
            (
                locale.to_string(),
                PageText {
                    title: title.into(),
                    slug: slug.into(),
                    excerpt: None,
                    seo_title: None,
                    seo_description: None,
                    og_media_id: None,
                    noindex: false,
                },
            )
        })
        .collect();
    Ok(service::create_page(
        pool,
        staff,
        &Actor::system(),
        service::NewPage {
            page_type: page_type.into(),
            translations,
        },
    )
    .await?)
}

async fn save(
    pool: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    page: Uuid,
    blocks: Vec<BlockDraft>,
) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    let version = service::builder_tree(&mut conn, page).await?.draft_version;
    drop(conn);
    service::save_tree(
        pool,
        reg,
        staff,
        &staff.actor(),
        page,
        version,
        SaveTree {
            blocks,
            translations: None,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("page {page}: {e:?}"))?;
    Ok(())
}

pub async fn seed(pool: &PgPool, admin_email: &str) -> anyhow::Result<()> {
    let reg = SectionRegistry::load();
    reg.sync(pool).await?;
    let admin = sqlx::query_scalar!("SELECT id FROM users WHERE email = $1", admin_email)
        .fetch_one(pool)
        .await?;
    let permissions: HashSet<String> = sqlx::query_scalar!("SELECT code FROM permissions")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
    let staff = Staff {
        id: admin,
        permissions,
    };

    sqlx::query!(
        "UPDATE settings SET value = to_jsonb($1::text) WHERE key = 'site.opening_date'",
        OPENING_DATE
    )
    .execute(pool)
    .await?;
    let photo = photos(pool, admin).await?;
    let [quad, village, farm, camel] = [
        id_of(pool, "QUAD-1H").await?,
        id_of(pool, "VILLAGE-VISIT").await?,
        id_of(pool, "FARM-VISIT").await?,
        id_of(pool, "CAMEL-SUNSET").await?,
    ];
    let places = [
        place_of(pool, "mssici", 31.2389, -4.7013, 1.0).await?,
        place_of(pool, "alnif", 31.1167, -5.1667, 47.0).await?,
        place_of(pool, "rissani", 31.2833, -4.2667, 45.0).await?,
    ];

    // ---- Shared sections (home + about) -------------------------------------------------------
    let mut cta = block(
        "cta_band",
        json!({"background_media_id": photo["night-sky"],
               "cta_primary": {"link": {"type": "anchor", "anchor": "newsletter", "new_tab": false}, "style": "primary"},
               "cta_secondary": {"link": {"type": "route", "route": "/activities", "new_tab": false}, "style": "ghost"}}),
        text(
            json!({"eyebrow": "Ouverture le {opening_date}", "title": "Soyez parmi les premiers à dormir sous ces étoiles",
                   "text": "Préréservez votre séjour : nous vous écrivons dès l'ouverture des réservations.",
                   "cta_primary_label": "Préréserver", "cta_secondary_label": "Voir les activités"}),
            json!({"eyebrow": "Opening on {opening_date}", "title": "Be among the first to sleep under these stars",
                   "text": "Pre-book your stay: we will write to you as soon as booking opens.",
                   "cta_primary_label": "Pre-book", "cta_secondary_label": "See the activities"}),
            json!({"eyebrow": "الافتتاح في {opening_date}", "title": "كن من أوائل من ينامون تحت هذه النجوم",
                   "text": "احجز مسبقًا وسنراسلك فور فتح الحجوزات.", "cta_primary_label": "احجز مسبقًا",
                   "cta_secondary_label": "اكتشف الأنشطة"}),
        ),
    );
    cta.name = Some("Pré-réservation (partagé)".into());
    cta.appearance = json!({"color_scheme": "night", "padding_top": "xl", "padding_bottom": "xl", "width": "full"});
    let cta = service::shared_save(pool, &reg, &staff, &staff.actor(), None, cta)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut newsletter = block(
        "newsletter",
        json!({"list_code": "opening", "show_consent": true}),
        text(
            json!({"title": "Recevez la date d'ouverture", "subtitle": "Une seule lettre, le jour où les réservations ouvrent.",
                   "button_label": "M'inscrire", "success_text": "Merci ! Nous vous écrirons à l'ouverture.",
                   "consent_text": "J'accepte de recevoir les nouvelles du camp. Désinscription en un clic."}),
            json!({"title": "Get the opening date", "subtitle": "One email, the day booking opens.",
                   "button_label": "Sign me up", "success_text": "Thank you! We will write when we open.",
                   "consent_text": "I agree to receive news from the camp. Unsubscribe in one click."}),
            json!({"title": "توصل بتاريخ الافتتاح", "subtitle": "رسالة واحدة يوم فتح الحجوزات.", "button_label": "سجّلني",
                   "success_text": "شكرًا! سنراسلك عند الافتتاح.",
                   "consent_text": "أوافق على تلقي أخبار المخيم. يمكن إلغاء الاشتراك بنقرة واحدة."}),
        ),
    );
    newsletter.name = Some("Newsletter ouverture (partagé)".into());
    newsletter.appearance = json!({"color_scheme": "sand"});
    let newsletter = service::shared_save(pool, &reg, &staff, &staff.actor(), None, newsletter)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    // ---- Home ---------------------------------------------------------------------------------
    let home = new_page(
        pool,
        &staff,
        "home",
        [
            ("fr", "Accueil", "accueil"),
            ("en", "Home", "home"),
            ("ar", "الرئيسية", "alraisiya"),
        ],
    )
    .await?;
    let mut hero = block(
        "hero",
        json!({"media_type": "image", "media_id": photo["oasis"], "overlay_opacity": 35, "height": "screen",
               "content_position": "bottom_start", "show_booking_search": true,
               "cta_primary": {"link": {"type": "anchor", "anchor": "camp", "new_tab": false}, "style": "primary"},
               "cta_secondary": {"link": {"type": "anchor", "anchor": "newsletter", "new_tab": false}, "style": "secondary"}}),
        text(
            json!({"eyebrow": "Ouverture le {opening_date}", "title": "Le désert à M'ssici, entre palmeraie et dunes",
                   "text": "Un camping familial entre Alnif et Rissani : tentes nomades, café, quads, village et ferme. Nous ouvrons bientôt.",
                   "cta_primary_label": "Découvrir le camp", "cta_secondary_label": "Préréserver"}),
            json!({"eyebrow": "Opening on {opening_date}", "title": "The desert at M'ssici, between palm grove and dunes",
                   "text": "A family campsite between Alnif and Rissani: nomad tents, café, quads, the village and the farm. Opening soon.",
                   "cta_primary_label": "Discover the camp", "cta_secondary_label": "Pre-book"}),
            json!({"eyebrow": "الافتتاح في {opening_date}", "title": "الصحراء في مسيسي، بين الواحة والكثبان",
                   "text": "مخيم عائلي بين ألنيف والريصاني: خيام الرحّل ومقهى ودراجات رباعية والقرية والمزرعة. نفتتح قريبًا.",
                   "cta_primary_label": "اكتشف المخيم", "cta_secondary_label": "احجز مسبقًا"}),
        ),
    );
    hero.name = Some("Hero ouverture".into());
    hero.appearance = json!({"color_scheme": "dark", "padding_top": "none", "padding_bottom": "none", "width": "full"});

    let feature = |icon: &str, fr: (&str, &str), en: (&str, &str), ar: (&str, &str)| {
        item(
            json!({"icon": icon}),
            text(
                json!({"title": fr.0, "text": fr.1}),
                json!({"title": en.0, "text": en.1}),
                json!({"title": ar.0, "text": ar.1}),
            ),
        )
    };
    let mut features = block(
        "features",
        json!({"columns": 3, "icon_style": "line", "source": "manual", "heading_align": "start"}),
        text(
            json!({"eyebrow": "Le camp", "title": "Tout le confort, au bord du désert"}),
            json!({"eyebrow": "The camp", "title": "Everything you need, at the edge of the desert"}),
            json!({"eyebrow": "المخيم", "title": "كل ما تحتاجه على حافة الصحراء"}),
        ),
    );
    features.anchor_id = Some("camp".into());
    features.items = vec![
        feature(
            "i-lucide-shower-head",
            (
                "Sanitaires et douches",
                "Blocs propres, eau chaude solaire.",
            ),
            ("Toilets and showers", "Clean blocks, solar hot water."),
            ("مراحيض ودوشات", "مرافق نظيفة وماء ساخن بالطاقة الشمسية."),
        ),
        feature(
            "i-lucide-droplet",
            ("Eau potable", "Points d'eau filtrée dans tout le camp."),
            ("Drinking water", "Filtered water points across the camp."),
            ("ماء صالح للشرب", "نقاط ماء مصفى في كل المخيم."),
        ),
        feature(
            "i-lucide-coffee",
            ("Le café", "Thé à la menthe, petits-déjeuners et dîners."),
            ("The café", "Mint tea, breakfasts and dinners."),
            ("المقهى", "شاي بالنعناع وفطور وعشاء."),
        ),
        feature(
            "i-lucide-tree-palm",
            (
                "De l'ombre",
                "Palmiers et toiles tendues aux heures chaudes.",
            ),
            ("Shade", "Palms and canopies for the hot hours."),
            ("الظل", "نخيل ومظلات في ساعات الحر."),
        ),
        feature(
            "i-lucide-square-parking",
            ("Parking", "Gratuit, à côté des emplacements."),
            ("Parking", "Free, next to the pitches."),
            ("موقف السيارات", "مجاني بجانب أماكن التخييم."),
        ),
        feature(
            "i-lucide-wifi",
            ("Wi-Fi au café", "Pour donner des nouvelles, pas plus."),
            ("Wi-Fi at the café", "To send news home, no more."),
            ("واي فاي في المقهى", "لإرسال الأخبار فقط."),
        ),
    ];

    let stays = block(
        "product_carousel",
        json!({"source": {"mode": "rule", "ids": [], "rule": {"kind": "accommodation", "sort": "sort_order", "limit": 6}},
               "card_style": "photo", "layout": "grid_3", "show_price_from": true, "show_rating": true, "show_duration": false}),
        text(
            json!({"eyebrow": "Dormir", "title": "Tentes nomades et emplacements"}),
            json!({"eyebrow": "Stay", "title": "Nomad tents and pitches"}),
            json!({"eyebrow": "الإقامة", "title": "خيام الرحّل وأماكن التخييم"}),
        ),
    );
    let activities = block(
        "product_carousel",
        json!({"source": {"mode": "manual", "ids": [quad, village, farm, camel]}, "card_style": "photo", "layout": "carousel",
               "show_price_from": true, "show_rating": true, "show_duration": true}),
        text(
            json!({"eyebrow": "Vivre", "title": "Quads, village, ferme et dromadaires"}),
            json!({"eyebrow": "Do", "title": "Quads, the village, the farm and camels"}),
            json!({"eyebrow": "الأنشطة", "title": "دراجات رباعية والقرية والمزرعة والجمال"}),
        ),
    );
    let mut fields = block(
        "image_text",
        json!({"media_id": photo["farm-fields"], "media_side": "end", "media_ratio": "4:3",
               "cta_primary": {"link": {"type": "product", "id": farm, "new_tab": false}, "style": "secondary"}}),
        text(
            json!({"eyebrow": "La ferme", "title": "Les champs traditionnels",
                   "body": "<p>Autour du camp, la famille cultive encore les <strong>oliviers</strong>, les <strong>palmiers dattiers</strong> et les <strong>navets</strong> irrigués par les canaux de la palmeraie.</p><p>Venez cueillir, goûter et rencontrer les bêtes de la ferme.</p>",
                   "cta_primary_label": "Visiter la ferme"}),
            json!({"eyebrow": "The farm", "title": "The traditional fields",
                   "body": "<p>Around the camp, the family still grows <strong>olives</strong>, <strong>date palms</strong> and <strong>turnips</strong>, watered by the palm grove's channels.</p><p>Come and pick, taste and meet the farm animals.</p>",
                   "cta_primary_label": "Visit the farm"}),
            json!({"eyebrow": "المزرعة", "title": "الحقول التقليدية",
                   "body": "<p>حول المخيم ما زالت العائلة تزرع <strong>الزيتون</strong> و<strong>النخيل</strong> و<strong>اللفت</strong>، تسقيها سواقي الواحة.</p><p>تعال للقطف والتذوق ولقاء حيوانات المزرعة.</p>",
                   "cta_primary_label": "زيارة المزرعة"}),
        ),
    );
    fields.anchor_id = Some("farm".into());
    // The farm's products, right after its fields: the shop's own cards (featured products).
    let farm_shop = block(
        "shop_products",
        json!({"source": {"mode": "rule", "ids": [], "rule": {"category_id": null, "include_subcategories": true, "featured_only": true,
                         "in_stock_only": true, "on_sale_only": false, "sort": "featured", "limit": 8}},
               "layout": "carousel", "columns_mobile": 2, "image_ratio": "square", "show_secondary_image": true,
               "show_unit_price": true, "show_rating": false, "quick_add": true, "show_view_all": true}),
        text(
            json!({"eyebrow": "Du champ à votre table", "title": "Produits de la ferme", "view_all_label": "Voir toute la boutique"}),
            json!({"eyebrow": "From our fields to your table", "title": "Farm products", "view_all_label": "Shop all"}),
            json!({"eyebrow": "من حقولنا إلى مائدتكم", "title": "منتجات الضيعة", "view_all_label": "تصفح المتجر"}),
        ),
    );
    let calendar = block(
        "seasonal_calendar",
        json!({"source": "farm_crops", "highlight_current_month": true}),
        text(
            json!({"title": "Au fil des saisons", "subtitle": "Ce qui pousse et se récolte autour du camp, mois par mois."}),
            json!({"title": "Through the seasons", "subtitle": "What grows and is harvested around the camp, month by month."}),
            json!({"title": "على مدار الفصول", "subtitle": "ما ينمو ويُحصد حول المخيم شهرًا بشهر."}),
        ),
    );
    // Gallery: six albums (one gallery each). Only twelve placeholder photos exist, so albums
    // share some of them.
    let mut albums = Vec::new();
    for (name, slugs, fr, en, ar, about) in GALLERY_ALBUMS {
        let gallery = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO galleries (id, name) VALUES ($1, $2)",
            gallery,
            name
        )
        .execute(pool)
        .await?;
        for (i, slug) in slugs.iter().enumerate() {
            sqlx::query!(
                "INSERT INTO gallery_items (gallery_id, media_id, sort_order) VALUES ($1, $2, $3)",
                gallery,
                photo[slug],
                i32::try_from(i)?
            )
            .execute(pool)
            .await?;
        }
        let mut fr = json!({ "title": fr });
        if let Some((d_fr, d_en, d_ar)) = about {
            fr["description"] = json!(d_fr);
            albums.push(item(
                json!({ "gallery_id": gallery }),
                text(
                    fr,
                    json!({"title": en, "description": d_en}),
                    json!({"title": ar, "description": d_ar}),
                ),
            ));
        } else {
            albums.push(item(
                json!({ "gallery_id": gallery }),
                text(fr, json!({"title": en}), json!({"title": ar})),
            ));
        }
    }
    let mut gallery = block(
        "gallery",
        json!({"preview_layout": "mosaic", "preview_height": "md", "preview_source": "auto", "preview_media_ids": [],
               "show_count": true, "show_album_chips": true, "viewer_layout": "albums",
               "open_lightbox_directly": false, "allow_download": false, "allow_share": true, "autoplay_videos": false}),
        text(
            json!({"title": "En images", "view_all_label": "Voir les {count} photos"}),
            json!({"title": "In pictures", "view_all_label": "View all {count} photos"}),
            json!({"title": "بالصور", "view_all_label": "عرض الصور ({count})"}),
        ),
    );
    gallery.anchor_id = Some("photos".into());
    gallery.analytics_label = Some("home_gallery".into());
    gallery.items = albums;
    let offers = block(
        "offers",
        json!({"source": "promotions", "promotion_ids": [], "product_ids": [], "show_countdown": true}),
        text(
            json!({"title": "Offres d'ouverture"}),
            json!({"title": "Opening offers"}),
            json!({"title": "عروض الافتتاح"}),
        ),
    );
    let reviews = block(
        "testimonials",
        json!({"featured_only": false, "min_rating": 4, "limit": 6, "show_summary": true, "layout": "carousel"}),
        text(
            json!({"title": "Ils sont venus"}),
            json!({"title": "They stayed with us"}),
            json!({"title": "قالوا عنا"}),
        ),
    );
    let map = block(
        "map",
        json!({"center_lat": 31.2389, "center_lng": -4.7013, "zoom": 9, "place_ids": places,
               "show_directions": true, "show_distance_table": true}),
        text(
            json!({"title": "Nous trouver", "text": "À M'ssici, entre Alnif et Rissani, au cœur du Tafilalet."}),
            json!({"title": "Find us", "text": "In M'ssici, between Alnif and Rissani, in the heart of the Tafilalet."}),
            json!({"title": "موقعنا", "text": "في مسيسي بين ألنيف والريصاني، في قلب تافيلالت."}),
        ),
    );
    let faq = block(
        "faq",
        json!({"source": {"mode": "rule", "ids": [], "rule": {"limit": 5}}, "first_open": true, "emit_schema": true}),
        text(
            json!({"title": "Questions fréquentes"}),
            json!({"title": "Frequently asked questions"}),
            json!({"title": "أسئلة شائعة"}),
        ),
    );
    save(
        pool,
        &reg,
        &staff,
        home,
        vec![
            hero,
            features,
            stays,
            activities,
            fields,
            farm_shop,
            calendar,
            gallery,
            offers,
            reviews,
            map,
            faq,
            placement(&cta, None),
            placement(&newsletter, Some("newsletter")),
        ],
    )
    .await?;

    // ---- About ----------------------------------------------------------------------------------
    let about = new_page(
        pool,
        &staff,
        "page",
        [
            ("fr", "À propos", "a-propos"),
            ("en", "About us", "about"),
            ("ar", "من نحن", "man-nahnu"),
        ],
    )
    .await?;
    let story = block(
        "image_text",
        json!({"media_id": photo["host"], "media_side": "start", "media_ratio": "portrait"}),
        text(
            json!({"eyebrow": "Mouzoun", "title": "Une coopérative familiale",
                   "body": "<p>Créée en 2022 à M'ssici, la coopérative Mouzoun réunit des familles du village autour du camp, de la ferme et des activités.</p>"}),
            json!({"eyebrow": "Mouzoun", "title": "A family cooperative",
                   "body": "<p>Founded in 2022 in M'ssici, the Mouzoun cooperative brings village families together around the camp, the farm and the activities.</p>"}),
            json!({"eyebrow": "موزون", "title": "تعاونية عائلية",
                   "body": "<p>تأسست تعاونية موزون سنة 2022 في مسيسي، وتجمع عائلات القرية حول المخيم والمزرعة والأنشطة.</p>"}),
        ),
    );
    save(
        pool,
        &reg,
        &staff,
        about,
        vec![
            story,
            placement(&cta, None),
            placement(&newsletter, Some("newsletter")),
        ],
    )
    .await?;

    // ---- Opening top bar ----------------------------------------------------------------------
    let banner = sqlx::query_scalar!(
        "INSERT INTO banners (id, placement, style, link_url, is_active, sort_order) VALUES ($1, 'top_bar', 'brand', '/#newsletter', true, 0) RETURNING id",
        Uuid::now_v7()
    )
    .fetch_one(pool)
    .await?;
    for (locale, message, cta_label) in [
        (
            "fr",
            "Ouverture le {opening_date} : préréservez dès maintenant",
            "Préréserver",
        ),
        ("en", "Opening on {opening_date}: pre-book now", "Pre-book"),
        (
            "ar",
            "الافتتاح في {opening_date}: احجز مسبقًا الآن",
            "احجز مسبقًا",
        ),
    ] {
        sqlx::query!(
            "INSERT INTO banner_translations (banner_id, locale, message, cta_label) VALUES ($1, $2, $3, $4)",
            banner,
            locale,
            message,
            cta_label
        )
        .execute(pool)
        .await?;
    }

    legal_pages(pool, &reg, &staff, admin).await?;
    for page in [home, about] {
        service::publish(
            pool,
            &reg,
            &staff.actor(),
            Some(admin),
            page,
            Utc::now(),
            PublishRequest {
                publish_at: None,
                note: Some("Seed".into()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("publish {page}: {e:?}"))?;
    }
    Ok(())
}

/// Mouzoun's legal pages from the templates (`legal/pages`), as `mouzoun-cli legal-pages install`
/// does: published, then edited by the admin in the builder.
async fn legal_pages(
    pool: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    _admin: Uuid,
) -> anyhow::Result<()> {
    mouzoun_api::services::shop::launch::install_legal_pages(pool, reg, staff, false).await?;
    Ok(())
}
