//! Demo stay and activity categories (`scope = 'product'`, Admin › Catalogue › Catégories):
//! four active categories in fr / en / ar with a cover, an icon, a description and SEO texts,
//! each holding the demo products that belong to it (matched by kind and French slug). The same
//! names as the admin's "suggested categories" (`SUGGESTED_PRODUCT`). Runs after `pages::seed`
//! (the covers are its site photos). Runtime queries: a development tool.

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// locale, name, slug, description (short HTML), SEO title, SEO description.
type Text<'a> = (&'a str, &'a str, &'a str, &'a str, &'a str, &'a str);

struct Seed<'a> {
    icon: &'a str,
    photo: &'a str,
    texts: [Text<'a>; 3],
    /// (kind, French slug) of the products it holds, in order; `(kind, "*")` = every product of
    /// that kind.
    products: &'a [(&'a str, &'a str)],
}

const SEEDS: &[Seed<'static>] = &[
    Seed {
        icon: "i-lucide-mountain",
        photo: "caravan",
        texts: [
            (
                "fr",
                "Aventure désert",
                "aventure-desert",
                "Dromadaires, quads et nuits dans les dunes de l'erg Chebbi.",
                "Aventure dans le désert · Merzouga",
                "Balades à dromadaire, quad dans les dunes et week-ends au camp, au pied de l'erg Chebbi.",
            ),
            (
                "en",
                "Desert adventure",
                "desert-adventure",
                "Camels, quads and nights in the Erg Chebbi dunes.",
                "Desert adventure · Merzouga",
                "Camel rides, quads in the dunes and weekends at the camp, at the foot of Erg Chebbi.",
            ),
            (
                "ar",
                "مغامرة في الصحراء",
                "moughamara-sahra",
                "جمال ودراجات رباعية وليالٍ في كثبان عرق الشبي.",
                "مغامرة في الصحراء · مرزوكة",
                "جولات على الجمال ودراجات رباعية في الكثبان وعطل نهاية الأسبوع في المخيم.",
            ),
        ],
        products: &[
            ("activity", "dromadaire-coucher-soleil"),
            ("activity", "quad-1h"),
            ("activity", "quad-2h"),
            ("activity", "nuit-sous-les-etoiles"),
            ("package", "week-end-desert"),
        ],
    },
    Seed {
        icon: "i-lucide-wheat",
        photo: "farm-fields",
        texts: [
            (
                "fr",
                "Vie à la ferme",
                "vie-a-la-ferme",
                "Les champs, les bêtes et la cueillette avec la famille.",
                "Vie à la ferme · Mouzoun",
                "Visitez la ferme familiale : oliviers, palmiers dattiers, élevage et cueillette.",
            ),
            (
                "en",
                "Farm life",
                "farm-life",
                "The fields, the animals and the harvest with the family.",
                "Farm life · Mouzoun",
                "Visit the family farm: olive trees, date palms, animals and harvest.",
            ),
            (
                "ar",
                "الحياة في المزرعة",
                "hayat-mazraa",
                "الحقول والحيوانات والقطف مع العائلة.",
                "الحياة في المزرعة · موزون",
                "زوروا مزرعة العائلة: الزيتون والنخيل والحيوانات والقطف.",
            ),
        ],
        products: &[("activity", "ferme-et-elevage")],
    },
    Seed {
        icon: "i-lucide-landmark",
        photo: "ksar",
        texts: [
            (
                "fr",
                "Culture & village",
                "culture-village",
                "Le ksar, le village de M'ssici et le thé chez l'habitant.",
                "Culture et village · Mouzoun",
                "Découvrez le village de M'ssici, son ksar et la vie des familles de la palmeraie.",
            ),
            (
                "en",
                "Culture & village",
                "culture-and-village",
                "The ksar, the village of M'ssici and tea with a family.",
                "Culture and village · Mouzoun",
                "Discover the village of M'ssici, its ksar and the life of the palm grove's families.",
            ),
            (
                "ar",
                "الثقافة والقرية",
                "thaqafa-qarya",
                "القصر وقرية امسيسي والشاي عند السكان.",
                "الثقافة والقرية · موزون",
                "اكتشفوا قرية امسيسي وقصرها وحياة عائلات الواحة.",
            ),
        ],
        products: &[("activity", "visite-village-mssici")],
    },
    Seed {
        icon: "i-lucide-tent",
        photo: "nomad-tent",
        texts: [
            (
                "fr",
                "Hébergements",
                "hebergements",
                "Tentes nomades et emplacements pour tente ou camping-car.",
                "Hébergements au camp · Mouzoun",
                "Dormez en tente nomade ou posez votre tente ou camping-car au camp.",
            ),
            (
                "en",
                "Accommodation",
                "accommodation",
                "Nomad tents and pitches for tents or campervans.",
                "Accommodation at the camp · Mouzoun",
                "Sleep in a nomad tent or bring your own tent or campervan to the camp.",
            ),
            (
                "ar",
                "الإقامة",
                "iqama",
                "خيام الرحّل وأماكن للخيام والعربات السكنية.",
                "الإقامة في المخيم · موزون",
                "نم في خيمة الرحّل أو انصب خيمتك أو عربتك السكنية في المخيم.",
            ),
        ],
        products: &[("accommodation", "*")],
    },
];

pub async fn seed(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    for (sort, c) in SEEDS.iter().enumerate() {
        let id = Uuid::now_v7();
        let cover: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM media_assets WHERE original_filename = $1 LIMIT 1")
                .bind(format!("{}.jpg", c.photo))
                .fetch_optional(&mut *tx)
                .await?;
        sqlx::query(
            "INSERT INTO categories (id, scope, sort_order, icon, cover_media_id, is_active)
             VALUES ($1, 'product', $2, $3, $4, true)",
        )
        .bind(id)
        .bind(i32::try_from(sort)?)
        .bind(c.icon)
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
        let mut order = 0;
        for (kind, slug) in c.products {
            let rows = sqlx::query(
                "SELECT p.id FROM products p JOIN product_translations t ON t.product_id = p.id AND t.locale = 'fr'
                 WHERE p.kind = $1 AND ($2 = '*' OR t.slug = $2) AND p.deleted_at IS NULL ORDER BY p.sort_order, p.id",
            )
            .bind(kind)
            .bind(slug)
            .fetch_all(&mut *tx)
            .await?;
            for r in rows {
                let product: Uuid = r.get("id");
                // Primary: the product's first category.
                sqlx::query(
                    "INSERT INTO product_categories (product_id, category_id, sort_order, is_primary)
                     VALUES ($1, $2, $3, NOT EXISTS (SELECT 1 FROM product_categories WHERE product_id = $1 AND is_primary))
                     ON CONFLICT DO NOTHING",
                )
                .bind(product)
                .bind(id)
                .bind(order)
                .execute(&mut *tx)
                .await?;
                order += 1;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}
