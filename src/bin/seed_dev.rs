//! Development seed: realistic fr/en/ar demo content (`cargo run --bin seed_dev`).
//!
//! Reference data (locales, currencies, guest categories, roles, settings) comes from the
//! migrations; this binary adds demo content on top. It runs in one transaction and refuses to
//! run twice. The super admin comes from SEED_ADMIN_EMAIL / SEED_ADMIN_PASSWORD (falling back to
//! ADMIN_EMAIL / ADMIN_PASSWORD).
//!
//! It also books one pending demo stay and sends staff notifications to the admin address, so
//! the email pipeline can be tried without the HTTP API:
//! `cargo run --bin seed_dev -- confirm MZ-26-000001` confirms it (→ confirmation email).
//!
//! `cargo run --bin seed_dev -- staff <email> <password> <role>` creates or resets a staff
//! account (development only; e.g. the full-stack tests' E2E_STAFF_EMAIL).

use anyhow::{Context, bail};
use chrono::{NaiveDate, Utc};
use mouzoun_api::{
    app, camp_time,
    db::{bookings::BookingSource, types::Locale},
    infra,
    services::{
        booking::{self, AccommodationRequest, BookingRequestMeta, CustomerInput, CustomerRef},
        identity, inventory,
        pricing::GuestCount,
    },
};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

/// `(locale, name, slug, short description)`
type Text<'a> = [(&'a str, &'a str, &'a str, &'a str); 3];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::install_crypto_provider();
    dotenvy::dotenv().ok();
    let env = |a: &str, b: &str| {
        std::env::var(a)
            .or_else(|_| std::env::var(b))
            .ok()
            .filter(|v| !v.is_empty())
    };
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [command, email, password, role] = args.as_slice()
        && command == "staff"
    {
        // Development accounts only (e.g. the full-stack test suite's staff login).
        if std::env::var("APP_ENV").as_deref() == Ok("production") {
            bail!("refused with APP_ENV=production");
        }
        let pool = infra::db::connect(&database_url, 2).await?;
        identity::upsert_staff(&pool, email, password, "Staff (development)", role).await?;
        println!("staff account ready: {email} ({role})");
        return Ok(());
    }
    let email = env("SEED_ADMIN_EMAIL", "ADMIN_EMAIL")
        .context("SEED_ADMIN_EMAIL (or ADMIN_EMAIL) is required")?;
    let password = env("SEED_ADMIN_PASSWORD", "ADMIN_PASSWORD")
        .context("SEED_ADMIN_PASSWORD (or ADMIN_PASSWORD) is required")?;

    let pool = infra::db::connect(&database_url, 5).await?;
    infra::db::migrate(&pool).await?;

    if let [command, reference] = args.as_slice() {
        if command != "confirm" {
            bail!(
                "usage: seed_dev [confirm <booking reference> | staff <email> <password> <role>]"
            );
        }
        let id = sqlx::query_scalar!("SELECT id FROM bookings WHERE reference = $1", reference)
            .fetch_optional(&pool)
            .await?
            .with_context(|| format!("no booking {reference}"))?;
        booking::confirm_booking(&pool, id, None, Utc::now()).await?;
        println!("{reference} confirmed: the worker sends the confirmation email");
        return Ok(());
    }
    if identity::ensure_super_admin(&pool, &email, &password, "Administrator").await? {
        println!("super admin created: {email}");
    }

    let already: bool = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM products WHERE sku = 'TENT-PITCH') AS "x!""#
    )
    .fetch_one(&pool)
    .await?;
    if already {
        bail!("demo content already present (reset the database to seed again)");
    }
    let mut tx = pool.begin().await?;
    seed(&mut tx).await?;
    tx.commit().await?;

    let today = mouzoun_api::camp_time::today(Utc::now());
    let nights = inventory::generate_inventory_calendar(
        &pool,
        None,
        today,
        inventory::CALENDAR_HORIZON_MONTHS,
    )
    .await?;
    let slots = inventory::generate_activity_slots(&pool, today, 365).await?;
    println!("demo content seeded: {nights} calendar nights, {slots} activity slots");

    sqlx::query!(
        "UPDATE settings SET value = to_jsonb($1::text) WHERE key = 'notifications.staff_email'",
        email
    )
    .execute(&pool)
    .await?;
    let reference = demo_booking(&pool, today).await?;
    println!(
        "demo booking {reference} (pending): confirm it with `cargo run --bin seed_dev -- confirm {reference}`"
    );
    Ok(())
}

/// Two nights in a nomad tent in two weeks, for Amina (French).
async fn demo_booking(pool: &sqlx::PgPool, today: NaiveDate) -> anyhow::Result<String> {
    let nomad = sqlx::query_scalar!("SELECT id FROM products WHERE sku = 'NOMAD-TENT'")
        .fetch_one(pool)
        .await?;
    let adult = sqlx::query_scalar!("SELECT id FROM guest_categories WHERE code = 'adult'")
        .fetch_one(pool)
        .await?;
    let arrival = today + chrono::Days::new(14);
    let created = booking::book_accommodation(
        pool,
        &AccommodationRequest {
            product_id: nomad,
            arrival,
            departure: arrival + chrono::Days::new(2),
            quantity: 1,
            guests: vec![GuestCount {
                guest_category_id: adult,
                count: 2,
            }],
            meta: BookingRequestMeta {
                customer: CustomerRef::New(CustomerInput {
                    email: Some("amina.benali@example.com".into()),
                    first_name: "Amina".into(),
                    last_name: Some("Benali".into()),
                    phone: Some("+33612345678".into()),
                    country_code: Some("FR".into()),
                    preferred_locale: None,
                }),
                locale: Locale::new("fr"),
                source: BookingSource::Website,
                rate_plan_id: None,
                promo_code: None,
                special_requests: Some("Nous arriverons vers 18 h.".into()),
                created_by: None,
                ip_address: None,
                user_agent: None,
                hold: chrono::Duration::days(2),
            },
        },
        Utc::now(),
        camp_time::today(Utc::now()),
    )
    .await?;
    Ok(created.reference)
}

async fn seed(tx: &mut PgConnection) -> anyhow::Result<()> {
    let guests = guest_categories(tx).await?;

    // ---- Places & crops -------------------------------------------------------------------
    place(
        tx,
        "facility",
        None,
        [
            (
                "fr",
                "Le café du camp",
                "cafe-du-camp",
                "Thé à la menthe, jus frais et vue sur les dunes.",
            ),
            (
                "en",
                "The camp café",
                "camp-cafe",
                "Mint tea, fresh juices and a view over the dunes.",
            ),
            (
                "ar",
                "مقهى المخيم",
                "maqha-al-mukhayyam",
                "شاي بالنعناع وعصائر طازجة وإطلالة على الكثبان.",
            ),
        ],
    )
    .await?;
    let fields = place(
        tx,
        "farm_field",
        None,
        [
            (
                "fr",
                "Les champs de la ferme",
                "champs-de-la-ferme",
                "Oliviers, palmiers dattiers et navets cultivés à l'ancienne.",
            ),
            (
                "en",
                "The farm fields",
                "farm-fields",
                "Olive trees, date palms and turnips grown the traditional way.",
            ),
            (
                "ar",
                "حقول المزرعة",
                "huqul-al-mazraa",
                "أشجار الزيتون والنخيل واللفت تزرع بالطرق التقليدية.",
            ),
        ],
    )
    .await?;
    for (code, months, names) in [
        (
            "olive",
            vec![10_i16, 11, 12],
            [
                ("fr", "Olive", "Récolte d'octobre à décembre."),
                ("en", "Olive", "Harvested October to December."),
                ("ar", "الزيتون", "يُجنى من أكتوبر إلى ديسمبر."),
            ],
        ),
        (
            "date",
            vec![9, 10, 11],
            [
                ("fr", "Datte", "Récolte de septembre à novembre."),
                ("en", "Date", "Harvested September to November."),
                ("ar", "التمر", "يُجنى من سبتمبر إلى نوفمبر."),
            ],
        ),
        (
            "turnip",
            vec![11, 12, 1, 2, 3],
            [
                ("fr", "Navet", "Récolte de novembre à mars."),
                ("en", "Turnip", "Harvested November to March."),
                ("ar", "اللفت", "يُجنى من نوفمبر إلى مارس."),
            ],
        ),
    ] {
        let crop = sqlx::query_scalar!(
            "INSERT INTO farm_crops (id, code, harvest_months, is_published) VALUES ($1, $2::text::citext, $3, true) RETURNING id",
            Uuid::now_v7(), code, &months
        ).fetch_one(&mut *tx).await?;
        for (locale, name, description) in names {
            sqlx::query!("INSERT INTO farm_crop_translations (farm_crop_id, locale, name, description) VALUES ($1, $2, $3, $4)", crop, locale, name, description)
                .execute(&mut *tx).await?;
        }
        sqlx::query!(
            "INSERT INTO place_crops (place_id, crop_id) VALUES ($1, $2)",
            fields,
            crop
        )
        .execute(&mut *tx)
        .await?;
    }
    let mssici = place(
        tx,
        "village",
        None,
        [
            (
                "fr",
                "M'ssici",
                "mssici",
                "Le village au pied du camp, ses ksour et sa palmeraie.",
            ),
            (
                "en",
                "M'ssici",
                "mssici",
                "The village next to the camp, its ksour and palm grove.",
            ),
            (
                "ar",
                "مسيسي",
                "mssici",
                "القرية المجاورة للمخيم بقصورها وواحتها.",
            ),
        ],
    )
    .await?;
    place(
        tx,
        "town",
        None,
        [
            (
                "fr",
                "Alnif",
                "alnif",
                "Capitale des fossiles, à l'ouest de M'ssici.",
            ),
            (
                "en",
                "Alnif",
                "alnif",
                "The fossil capital, west of M'ssici.",
            ),
            ("ar", "ألنيف", "alnif", "عاصمة الأحافير غرب مسيسي."),
        ],
    )
    .await?;
    let rissani = place(
        tx,
        "town",
        None,
        [
            (
                "fr",
                "Rissani",
                "rissani",
                "Son souk historique et les portes du Tafilalet.",
            ),
            (
                "en",
                "Rissani",
                "rissani",
                "Its historic souk, gateway to the Tafilalet.",
            ),
            (
                "ar",
                "الريصاني",
                "rissani",
                "سوقها التاريخي وبوابة تافيلالت.",
            ),
        ],
    )
    .await?;

    // ---- Pricing building blocks ----------------------------------------------------------
    let flexible_policy = policy(
        tx,
        "flexible",
        json!([
            {"min_hours_before": 168, "refund_percent": 100},
            {"min_hours_before": 48, "refund_percent": 50},
            {"min_hours_before": 0, "refund_percent": 0}
        ]),
        true,
        [("fr", "Flexible"), ("en", "Flexible"), ("ar", "مرن")],
    )
    .await?;
    let strict_policy = policy(
        tx,
        "non_refundable",
        json!([{"min_hours_before": 0, "refund_percent": 0}]),
        false,
        [
            ("fr", "Non remboursable"),
            ("en", "Non-refundable"),
            ("ar", "غير قابل للاسترداد"),
        ],
    )
    .await?;
    // Prices without a rate plan apply to both plans; the non-refundable plan overrides some.
    rate_plan(
        tx,
        "flexible",
        flexible_policy,
        "percent",
        Some(30),
        "optional_online",
        1,
        [
            ("fr", "Tarif flexible"),
            ("en", "Flexible rate"),
            ("ar", "سعر مرن"),
        ],
    )
    .await?;
    let non_refundable = rate_plan(
        tx,
        "non_refundable",
        strict_policy,
        "full",
        None,
        "online_required",
        2,
        [
            ("fr", "Tarif non remboursable"),
            ("en", "Non-refundable rate"),
            ("ar", "سعر غير قابل للاسترداد"),
        ],
    )
    .await?;

    let low = season(
        tx,
        "low",
        "#7FB3D5",
        1,
        [
            ("fr", "Basse saison"),
            ("en", "Low season"),
            ("ar", "الموسم المنخفض"),
        ],
    )
    .await?;
    let high = season(
        tx,
        "high",
        "#E67E22",
        10,
        [
            ("fr", "Haute saison"),
            ("en", "High season"),
            ("ar", "الموسم المرتفع"),
        ],
    )
    .await?;
    for year in 2026..=2028 {
        let date = |m, d| NaiveDate::from_ymd_opt(year, m, d).unwrap();
        // Desert high season: October → April; low season: May → September.
        period(tx, high, date(1, 1), date(5, 1)).await?;
        period(tx, low, date(5, 1), date(10, 1)).await?;
        period(
            tx,
            high,
            date(10, 1),
            NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap(),
        )
        .await?;
    }
    let tourist_tax = sqlx::query_scalar!(
        "INSERT INTO taxes (id, code, calc_type, amount_minor, currency, exempt_guest_category_ids)
         VALUES ($1, 'tourist_tax', 'fixed_per_person_per_night', 250, 'EUR', $2) RETURNING id",
        Uuid::now_v7(),
        &[guests.child, guests.infant]
    )
    .fetch_one(&mut *tx)
    .await?;
    for (locale, name) in [
        ("fr", "Taxe de séjour"),
        ("en", "Tourist tax"),
        ("ar", "ضريبة الإقامة"),
    ] {
        sqlx::query!(
            "INSERT INTO tax_translations (tax_id, locale, name) VALUES ($1, $2, $3)",
            tourist_tax,
            locale,
            name
        )
        .execute(&mut *tx)
        .await?;
    }

    // ---- Accommodations -------------------------------------------------------------------
    let pitch = product(
        tx,
        "accommodation",
        "TENT-PITCH",
        None,
        true,
        [
            (
                "fr",
                "Emplacement tente",
                "emplacement-tente",
                "Plantez votre tente sous les étoiles, sanitaires à deux pas.",
            ),
            (
                "en",
                "Tent pitch",
                "tent-pitch",
                "Pitch your tent under the stars, washrooms close by.",
            ),
            (
                "ar",
                "مكان للخيمة",
                "makan-lilkhayma",
                "انصب خيمتك تحت النجوم مع مرافق صحية قريبة.",
            ),
        ],
    )
    .await?;
    accommodation(tx, pitch, "tent_pitch", "pool", 6, 2).await?;
    units(tx, pitch, "P", 20).await?;
    price(
        tx,
        pitch,
        "per_person_per_night",
        800,
        None,
        None,
        Some(guests.adult),
        None,
    )
    .await?;
    price(
        tx,
        pitch,
        "per_person_per_night",
        1_000,
        None,
        Some(high),
        Some(guests.adult),
        None,
    )
    .await?;
    price(
        tx,
        pitch,
        "per_person_per_night",
        400,
        None,
        None,
        Some(guests.child),
        None,
    )
    .await?;
    price(
        tx,
        pitch,
        "per_person_per_night",
        0,
        None,
        None,
        Some(guests.infant),
        None,
    )
    .await?;

    let nomad = product(
        tx,
        "accommodation",
        "NOMAD-TENT",
        None,
        true,
        [
            (
                "fr",
                "Tente nomade",
                "tente-nomade",
                "Tente berbère meublée, lits, tapis et lanternes.",
            ),
            (
                "en",
                "Nomad tent",
                "nomad-tent",
                "Furnished Berber tent with beds, rugs and lanterns.",
            ),
            (
                "ar",
                "خيمة رحّل",
                "khaymat-ruhhal",
                "خيمة أمازيغية مجهزة بأسرّة وزرابي وفوانيس.",
            ),
        ],
    )
    .await?;
    accommodation(tx, nomad, "nomad_tent", "unit", 4, 2).await?;
    units(tx, nomad, "N", 6).await?;
    price(
        tx,
        nomad,
        "per_unit_per_night",
        4_500,
        None,
        None,
        None,
        Some(1_500),
    )
    .await?;
    price(
        tx,
        nomad,
        "per_unit_per_night",
        6_000,
        None,
        Some(high),
        None,
        Some(1_500),
    )
    .await?;
    price(
        tx,
        nomad,
        "per_unit_per_night",
        5_400,
        Some(non_refundable),
        Some(high),
        None,
        Some(1_500),
    )
    .await?;
    price(
        tx,
        nomad,
        "per_unit_per_night",
        4_000,
        Some(non_refundable),
        None,
        None,
        Some(1_500),
    )
    .await?;

    let campervan = product(
        tx,
        "accommodation",
        "CAMPERVAN-PITCH",
        None,
        false,
        [
            (
                "fr",
                "Emplacement camping-car",
                "emplacement-camping-car",
                "Grand emplacement plat avec eau et électricité.",
            ),
            (
                "en",
                "Campervan pitch",
                "campervan-pitch",
                "Large flat pitch with water and electricity.",
            ),
            (
                "ar",
                "مكان للعربة السكنية",
                "makan-lilaraba",
                "مكان واسع ومستوٍ مع الماء والكهرباء.",
            ),
        ],
    )
    .await?;
    accommodation(tx, campervan, "campervan_pitch", "pool", 4, 2).await?;
    units(tx, campervan, "C", 8).await?;
    price(
        tx,
        campervan,
        "per_unit_per_night",
        1_500,
        None,
        None,
        None,
        None,
    )
    .await?;
    for accommodation_id in [pitch, nomad, campervan] {
        sqlx::query!(
            "INSERT INTO product_taxes (product_id, tax_id) VALUES ($1, $2)",
            accommodation_id,
            tourist_tax
        )
        .execute(&mut *tx)
        .await?;
    }

    // ---- Activities -----------------------------------------------------------------------
    let mut quads = Vec::new();
    for n in 1..=4 {
        quads.push(sqlx::query_scalar!(
            "INSERT INTO resources (id, kind, code, name, seats) VALUES ($1, 'vehicle', $2::text::citext, $3, 2) RETURNING id",
            Uuid::now_v7(), format!("QUAD-{n:02}"), format!("Quad {n}")
        ).fetch_one(&mut *tx).await?);
    }
    for (sku, minutes, amount, text) in [
        (
            "QUAD-1H",
            60,
            4_500,
            [
                (
                    "fr",
                    "Quad 1 h",
                    "quad-1h",
                    "Une heure de pistes et de dunes avec un guide.",
                ),
                (
                    "en",
                    "Quad 1 h",
                    "quad-1h",
                    "One hour of tracks and dunes with a guide.",
                ),
                (
                    "ar",
                    "دراجة رباعية ساعة",
                    "quad-saa",
                    "ساعة بين المسالك والكثبان مع مرشد.",
                ),
            ],
        ),
        (
            "QUAD-2H",
            120,
            8_000,
            [
                (
                    "fr",
                    "Quad 2 h",
                    "quad-2h",
                    "Jusqu'aux grandes dunes et retour au coucher du soleil.",
                ),
                (
                    "en",
                    "Quad 2 h",
                    "quad-2h",
                    "Out to the big dunes and back at sunset.",
                ),
                (
                    "ar",
                    "دراجة رباعية ساعتان",
                    "quad-saatan",
                    "حتى الكثبان الكبيرة والعودة مع الغروب.",
                ),
            ],
        ),
    ] {
        let quad_tour = product(tx, "activity", sku, None, true, text).await?;
        activity(
            tx,
            quad_tour,
            "off_site",
            "fixed_slots",
            Some(minutes),
            Some("moderate"),
            Some(16),
            true,
        )
        .await?;
        price(tx, quad_tour, "per_person", amount, None, None, None, None).await?;
        sqlx::query!(
            "INSERT INTO product_resource_requirements (product_id, resource_kind, units_per_group, participants_per_group)
             VALUES ($1, 'vehicle', 1, 2)",
            quad_tour
        ).execute(&mut *tx).await?;
        for start in ["09:00", "16:00"] {
            schedule(tx, quad_tour, start, 8).await?;
        }
    }
    let village = product(
        tx,
        "activity",
        "VILLAGE-VISIT",
        Some(mssici),
        true,
        [
            (
                "fr",
                "Visite du village de M'ssici",
                "visite-village-mssici",
                "Ksour, four à pain et thé chez l'habitant.",
            ),
            (
                "en",
                "M'ssici village visit",
                "mssici-village-visit",
                "Ksour, the bread oven and tea with a local family.",
            ),
            (
                "ar",
                "زيارة قرية مسيسي",
                "ziyarat-mssici",
                "القصور وفرن الخبز والشاي عند السكان.",
            ),
        ],
    )
    .await?;
    activity(
        tx,
        village,
        "off_site",
        "on_request",
        Some(150),
        Some("easy"),
        None,
        true,
    )
    .await?;
    price(tx, village, "per_person", 1_500, None, None, None, None).await?;
    let farm = product(
        tx,
        "activity",
        "FARM-VISIT",
        Some(fields),
        false,
        [
            (
                "fr",
                "Ferme et élevage",
                "ferme-et-elevage",
                "Les champs, les chèvres, les dromadaires et la traite.",
            ),
            (
                "en",
                "Farm & livestock visit",
                "farm-livestock-visit",
                "The fields, goats, camels and milking time.",
            ),
            (
                "ar",
                "المزرعة والماشية",
                "al-mazraa-wal-mashiya",
                "الحقول والماعز والإبل ووقت الحلب.",
            ),
        ],
    )
    .await?;
    activity(
        tx,
        farm,
        "on_site",
        "anytime",
        Some(90),
        Some("easy"),
        None,
        true,
    )
    .await?;
    price(tx, farm, "per_person", 1_200, None, None, None, None).await?;
    let camel = product(
        tx,
        "activity",
        "CAMEL-SUNSET",
        None,
        true,
        [
            (
                "fr",
                "Balade à dos de dromadaire au coucher du soleil",
                "dromadaire-coucher-soleil",
                "Les dunes dorées au pas lent des dromadaires.",
            ),
            (
                "en",
                "Sunset camel ride",
                "sunset-camel-ride",
                "Golden dunes at the slow pace of the camels.",
            ),
            (
                "ar",
                "جولة على الجمال عند الغروب",
                "jawla-jamal-ghurub",
                "الكثبان الذهبية على إيقاع الجمال.",
            ),
        ],
    )
    .await?;
    activity(
        tx,
        camel,
        "off_site",
        "fixed_slots",
        Some(90),
        Some("easy"),
        None,
        true,
    )
    .await?;
    price(tx, camel, "per_person", 2_500, None, None, None, None).await?;
    schedule(tx, camel, "17:30", 12).await?;

    // ---- Extras & package -----------------------------------------------------------------
    let breakfast = extra(
        tx,
        "BREAKFAST",
        "accommodation",
        true,
        "per_person",
        600,
        [
            (
                "fr",
                "Petit-déjeuner",
                "petit-dejeuner",
                "Msemen, amlou, œufs et thé.",
            ),
            (
                "en",
                "Breakfast",
                "breakfast",
                "Msemen, amlou, eggs and tea.",
            ),
            ("ar", "فطور", "futur", "مسمن وأملو وبيض وشاي."),
        ],
    )
    .await?;
    let dinner = extra(
        tx,
        "DINNER",
        "accommodation",
        true,
        "per_person",
        1_500,
        [
            (
                "fr",
                "Dîner berbère",
                "diner-berbere",
                "Tajine ou medfouna autour du feu.",
            ),
            (
                "en",
                "Berber dinner",
                "berber-dinner",
                "Tagine or medfouna around the fire.",
            ),
            (
                "ar",
                "عشاء أمازيغي",
                "ashaa-amazighi",
                "طاجين أو مدفونة حول النار.",
            ),
        ],
    )
    .await?;
    let firewood = extra(
        tx,
        "FIREWOOD",
        "accommodation",
        false,
        "per_unit",
        500,
        [
            (
                "fr",
                "Bois pour le feu",
                "bois-feu",
                "Un fagot pour la soirée.",
            ),
            ("en", "Firewood", "firewood", "A bundle for the evening."),
            ("ar", "حطب", "hatab", "حزمة حطب للسهرة."),
        ],
    )
    .await?;
    let transfer = extra(
        tx,
        "TRANSFER-RISSANI",
        "booking",
        true,
        "per_booking",
        4_000,
        [
            (
                "fr",
                "Transfert depuis Rissani",
                "transfert-rissani",
                "Prise en charge à Rissani en 4x4.",
            ),
            (
                "en",
                "Transfer from Rissani",
                "transfer-rissani",
                "4x4 pick-up in Rissani.",
            ),
            (
                "ar",
                "نقل من الريصاني",
                "naql-rissani",
                "نقل بسيارة رباعية الدفع من الريصاني.",
            ),
        ],
    )
    .await?;
    sqlx::query!(
        "UPDATE products SET place_id = $2 WHERE id = $1",
        transfer,
        rissani
    )
    .execute(&mut *tx)
    .await?;
    for (accommodation_id, extra_id) in [
        (nomad, breakfast),
        (nomad, dinner),
        (nomad, firewood),
        (pitch, breakfast),
        (pitch, firewood),
    ] {
        sqlx::query!(
            "INSERT INTO product_extras (product_id, extra_product_id) VALUES ($1, $2)",
            accommodation_id,
            extra_id
        )
        .execute(&mut *tx)
        .await?;
    }
    let weekend = product(
        tx,
        "package",
        "DESERT-WEEKEND",
        None,
        true,
        [
            (
                "fr",
                "Week-end dans le désert",
                "week-end-desert",
                "Deux nuits en tente nomade, quad, dîners et coucher de soleil.",
            ),
            (
                "en",
                "Desert weekend",
                "desert-weekend",
                "Two nights in a nomad tent, quads, dinners and sunset.",
            ),
            (
                "ar",
                "عطلة نهاية الأسبوع في الصحراء",
                "otla-sahra",
                "ليلتان في خيمة رحّل ودراجة رباعية وعشاء وغروب.",
            ),
        ],
    )
    .await?;
    sqlx::query!(
        "INSERT INTO packages (product_id, duration_days, duration_nights, pricing_mode) VALUES ($1, 3, 2, 'fixed')",
        weekend
    ).execute(&mut *tx).await?;
    price(tx, weekend, "per_person", 18_000, None, None, None, None).await?;
    for (item, quantity, day) in [(nomad, 2, 1), (dinner, 2, 1), (camel, 1, 1)] {
        sqlx::query!(
            "INSERT INTO package_items (id, package_product_id, item_product_id, quantity, day_number) VALUES ($1, $2, $3, $4, $5)",
            Uuid::now_v7(), weekend, item, quantity, day
        ).execute(&mut *tx).await?;
    }
    let quad_1h = sqlx::query_scalar!("SELECT id FROM products WHERE sku = 'QUAD-1H'")
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query!(
        "INSERT INTO package_items (id, package_product_id, item_product_id, quantity, day_number) VALUES ($1, $2, $3, 1, 2)",
        Uuid::now_v7(), weekend, quad_1h
    ).execute(&mut *tx).await?;

    // ---- Content: home page, menus, FAQs --------------------------------------------------
    let home = sqlx::query_scalar!(
        "INSERT INTO pages (id, page_type, status, publish_at) VALUES ($1, 'home', 'published', now()) RETURNING id",
        Uuid::now_v7()
    ).fetch_one(&mut *tx).await?;
    for (locale, title, slug) in [
        ("fr", "Accueil", "accueil"),
        ("en", "Home", "home"),
        ("ar", "الرئيسية", "alraisiya"),
    ] {
        sqlx::query!(
            "INSERT INTO page_translations (page_id, locale, title, slug) VALUES ($1, $2, $3, $4)",
            home,
            locale,
            title,
            slug
        )
        .execute(&mut *tx)
        .await?;
    }
    let blocks = [
        (
            "hero",
            json!({"layout": "full"}),
            [
                json!({"title": "Le désert à M'ssici", "subtitle": "Camping, oasis et traditions entre Alnif et Rissani", "cta": "Réserver"}),
                json!({"title": "The desert at M'ssici", "subtitle": "Camping, oases and traditions between Alnif and Rissani", "cta": "Book now"}),
                json!({"title": "الصحراء في مسيسي", "subtitle": "تخييم وواحات وتقاليد بين ألنيف والريصاني", "cta": "احجز الآن"}),
            ],
        ),
        (
            "rich_text",
            json!({}),
            [
                json!({"html": "<p>Un camp familial au bord du désert, une ferme vivante et un village accueillant.</p>"}),
                json!({"html": "<p>A family camp on the edge of the desert, a working farm and a welcoming village.</p>"}),
                json!({"html": "<p>مخيم عائلي على حافة الصحراء ومزرعة حية وقرية مضيافة.</p>"}),
            ],
        ),
        (
            "product_grid",
            json!({"kind": "accommodation", "limit": 3}),
            [
                json!({"title": "Où dormir"}),
                json!({"title": "Where to stay"}),
                json!({"title": "أين تقيم"}),
            ],
        ),
        (
            "activity_list",
            json!({"featured": true, "limit": 6}),
            [
                json!({"title": "Activités"}),
                json!({"title": "Activities"}),
                json!({"title": "الأنشطة"}),
            ],
        ),
        (
            "faq_list",
            json!({"limit": 5}),
            [
                json!({"title": "Questions fréquentes"}),
                json!({"title": "FAQ"}),
                json!({"title": "أسئلة شائعة"}),
            ],
        ),
        (
            "map",
            json!({"zoom": 10}),
            [
                json!({"title": "Nous trouver"}),
                json!({"title": "Find us"}),
                json!({"title": "موقعنا"}),
            ],
        ),
    ];
    for (order, (block_type, settings, contents)) in blocks.into_iter().enumerate() {
        let block = sqlx::query_scalar!(
            "INSERT INTO content_blocks (id, page_id, block_type, settings, sort_order) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            Uuid::now_v7(), home, block_type, settings, i32::try_from(order)?
        ).fetch_one(&mut *tx).await?;
        for (locale, content) in ["fr", "en", "ar"].into_iter().zip(contents) {
            sqlx::query!("INSERT INTO content_block_translations (content_block_id, locale, content) VALUES ($1, $2, $3)", block, locale, content)
                .execute(&mut *tx).await?;
        }
    }
    let header = menu(tx, "header", "Header").await?;
    let footer = menu(tx, "footer", "Footer").await?;
    for (order, (route, labels)) in [
        (
            "accommodations",
            [
                ("fr", "Hébergements"),
                ("en", "Accommodation"),
                ("ar", "الإقامة"),
            ],
        ),
        (
            "activities",
            [("fr", "Activités"), ("en", "Activities"), ("ar", "الأنشطة")],
        ),
        (
            "farm",
            [("fr", "La ferme"), ("en", "The farm"), ("ar", "المزرعة")],
        ),
        (
            "contact",
            [("fr", "Contact"), ("en", "Contact"), ("ar", "اتصل بنا")],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        menu_item(tx, header, route, i32::try_from(order)?, labels).await?;
    }
    for (order, (route, labels)) in [
        (
            "faq",
            [
                ("fr", "Questions fréquentes"),
                ("en", "FAQ"),
                ("ar", "أسئلة شائعة"),
            ],
        ),
        (
            "legal",
            [
                ("fr", "Mentions légales"),
                ("en", "Legal notice"),
                ("ar", "إشعار قانوني"),
            ],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        menu_item(tx, footer, route, i32::try_from(order)?, labels).await?;
    }
    for (order, qa) in [
        [("fr", "Comment venir au camp ?", "Depuis Rissani (environ 1 h) ou Alnif ; un transfert en 4x4 est proposé."),
         ("en", "How do I get to the camp?", "From Rissani (about 1 h) or Alnif; a 4x4 transfer is available."),
         ("ar", "كيف أصل إلى المخيم؟", "من الريصاني (حوالي ساعة) أو ألنيف، ويتوفر نقل بسيارة رباعية الدفع.")],
        [("fr", "Y a-t-il de l'électricité ?", "Oui, solaire, avec des prises dans les tentes nomades et au café."),
         ("en", "Is there electricity?", "Yes, solar, with sockets in the nomad tents and at the café."),
         ("ar", "هل توجد كهرباء؟", "نعم، طاقة شمسية مع مقابس في خيام الرحّل والمقهى.")],
        [("fr", "Les enfants sont-ils bienvenus ?", "Bien sûr : les moins de 4 ans sont gratuits, la taxe de séjour ne s'applique qu'aux adultes."),
         ("en", "Are children welcome?", "Of course: under-4s stay free and the tourist tax only applies to adults."),
         ("ar", "هل الأطفال مرحب بهم؟", "بالطبع: الأطفال دون 4 سنوات مجانًا وضريبة الإقامة على البالغين فقط.")],
        [("fr", "Quelle est la meilleure saison ?", "D'octobre à avril : journées douces et nuits fraîches."),
         ("en", "What is the best season?", "October to April: mild days and cool nights."),
         ("ar", "ما هو أفضل موسم؟", "من أكتوبر إلى أبريل: أيام معتدلة وليالٍ باردة.")],
        [("fr", "Peut-on annuler ?", "Avec le tarif flexible : remboursement total jusqu'à 7 jours avant, 50 % jusqu'à 48 h."),
         ("en", "Can I cancel?", "With the flexible rate: full refund up to 7 days before, 50 % up to 48 h."),
         ("ar", "هل يمكن الإلغاء؟", "مع السعر المرن: استرداد كامل حتى 7 أيام قبل الوصول و50٪ حتى 48 ساعة.")],
    ].into_iter().enumerate() {
        let faq = sqlx::query_scalar!(
            "INSERT INTO faqs (id, sort_order, is_published) VALUES ($1, $2, true) RETURNING id",
            Uuid::now_v7(), i32::try_from(order)?
        ).fetch_one(&mut *tx).await?;
        for (locale, question, answer) in qa {
            sqlx::query!("INSERT INTO faq_translations (faq_id, locale, question, answer) VALUES ($1, $2, $3, $4)", faq, locale, question, answer)
                .execute(&mut *tx).await?;
        }
    }
    Ok(())
}

// ---- Helpers (one checked query each) ---------------------------------------------------------

struct GuestIds {
    adult: Uuid,
    child: Uuid,
    infant: Uuid,
}

async fn guest_categories(tx: &mut PgConnection) -> anyhow::Result<GuestIds> {
    let id = |rows: &[(String, Uuid)], code: &str| {
        rows.iter()
            .find(|r| r.0 == code)
            .map(|r| r.1)
            .context("guest category missing")
    };
    let rows: Vec<(String, Uuid)> =
        sqlx::query!(r#"SELECT code::text AS "code!", id FROM guest_categories"#)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|r| (r.code, r.id))
            .collect();
    Ok(GuestIds {
        adult: id(&rows, "adult")?,
        child: id(&rows, "child")?,
        infant: id(&rows, "infant")?,
    })
}

async fn place(
    tx: &mut PgConnection,
    kind: &str,
    parent: Option<Uuid>,
    text: Text<'_>,
) -> anyhow::Result<Uuid> {
    let id = sqlx::query_scalar!(
        "INSERT INTO places (id, kind, parent_id, is_published) VALUES ($1, $2, $3, true) RETURNING id",
        Uuid::now_v7(), kind, parent
    ).fetch_one(&mut *tx).await?;
    for (locale, name, slug, short) in text {
        sqlx::query!(
            "INSERT INTO place_translations (place_id, locale, name, slug, short_description) VALUES ($1, $2, $3, $4, $5)",
            id, locale, name, slug, short
        ).execute(&mut *tx).await?;
    }
    Ok(id)
}

async fn product(
    tx: &mut PgConnection,
    kind: &str,
    sku: &str,
    place_id: Option<Uuid>,
    featured: bool,
    text: Text<'_>,
) -> anyhow::Result<Uuid> {
    let id = sqlx::query_scalar!(
        "INSERT INTO products (id, kind, sku, status, is_featured, place_id, published_at)
         VALUES ($1, $2, $3::text::citext, 'published', $4, $5, now()) RETURNING id",
        Uuid::now_v7(),
        kind,
        sku,
        featured,
        place_id
    )
    .fetch_one(&mut *tx)
    .await?;
    for (locale, name, slug, short) in text {
        sqlx::query!(
            "INSERT INTO product_translations (product_id, locale, name, slug, short_description) VALUES ($1, $2, $3, $4, $5)",
            id, locale, name, slug, short
        ).execute(&mut *tx).await?;
    }
    Ok(id)
}

async fn accommodation(
    tx: &mut PgConnection,
    id: Uuid,
    kind: &str,
    mode: &str,
    max_guests: i32,
    standard: i32,
) -> anyhow::Result<()> {
    sqlx::query!(
        "INSERT INTO accommodation_types (product_id, accommodation_kind, inventory_mode, max_guests, standard_occupancy,
                                          check_in_from, check_out_until)
         VALUES ($1, $2, $3, $4, $5, '14:00', '11:00')",
        id, kind, mode, max_guests, standard
    ).execute(&mut *tx).await?;
    Ok(())
}

async fn units(
    tx: &mut PgConnection,
    product_id: Uuid,
    prefix: &str,
    count: i32,
) -> anyhow::Result<()> {
    for n in 1..=count {
        sqlx::query!(
            "INSERT INTO accommodation_units (id, product_id, code, sort_order) VALUES ($1, $2, $3::text::citext, $4)",
            Uuid::now_v7(), product_id, format!("{prefix}{n:02}"), n
        ).execute(&mut *tx).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn price(
    tx: &mut PgConnection,
    product_id: Uuid,
    unit: &str,
    amount_minor: i64,
    rate_plan: Option<Uuid>,
    season: Option<Uuid>,
    guest_category: Option<Uuid>,
    extra_guest_minor: Option<i64>,
) -> anyhow::Result<()> {
    sqlx::query!(
        "INSERT INTO prices (id, product_id, rate_plan_id, season_id, guest_category_id, pricing_unit, amount_minor,
                             currency, extra_guest_amount_minor)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'EUR', $8)",
        Uuid::now_v7(), product_id, rate_plan, season, guest_category, unit, amount_minor, extra_guest_minor
    ).execute(&mut *tx).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn activity(
    tx: &mut PgConnection,
    id: Uuid,
    location: &str,
    scheduling: &str,
    minutes: Option<i32>,
    difficulty: Option<&str>,
    min_age: Option<i16>,
    guided: bool,
) -> anyhow::Result<()> {
    sqlx::query!(
        "INSERT INTO activities (product_id, location_type, scheduling_mode, duration_minutes, difficulty, min_age,
                                 is_guided, guide_languages)
         VALUES ($1, $2, $3, $4, $5, $6, $7, ARRAY['fr', 'en', 'ar'])",
        id, location, scheduling, minutes, difficulty, min_age, guided
    ).execute(&mut *tx).await?;
    Ok(())
}

async fn schedule(
    tx: &mut PgConnection,
    product_id: Uuid,
    start: &str,
    capacity: i32,
) -> anyhow::Result<()> {
    sqlx::query!(
        "INSERT INTO activity_schedules (id, product_id, start_time, capacity, valid_period)
         VALUES ($1, $2, $3::text::time, $4, daterange(current_date, (current_date + interval '2 years')::date))",
        Uuid::now_v7(), product_id, start, capacity
    ).execute(&mut *tx).await?;
    Ok(())
}

async fn extra(
    tx: &mut PgConnection,
    sku: &str,
    applies_to: &str,
    requires_date: bool,
    unit: &str,
    amount: i64,
    text: Text<'_>,
) -> anyhow::Result<Uuid> {
    let id = product(tx, "extra", sku, None, false, text).await?;
    sqlx::query!(
        "INSERT INTO extras (product_id, applies_to, requires_date) VALUES ($1, $2, $3)",
        id,
        applies_to,
        requires_date
    )
    .execute(&mut *tx)
    .await?;
    price(tx, id, unit, amount, None, None, None, None).await?;
    Ok(id)
}

async fn policy(
    tx: &mut PgConnection,
    code: &str,
    rules: serde_json::Value,
    default: bool,
    names: [(&str, &str); 3],
) -> anyhow::Result<Uuid> {
    let id = sqlx::query_scalar!(
        "INSERT INTO cancellation_policies (id, code, rules, is_default) VALUES ($1, $2::text::citext, $3, $4) RETURNING id",
        Uuid::now_v7(), code, rules, default
    ).fetch_one(&mut *tx).await?;
    for (locale, name) in names {
        sqlx::query!("INSERT INTO cancellation_policy_translations (cancellation_policy_id, locale, name) VALUES ($1, $2, $3)", id, locale, name)
            .execute(&mut *tx).await?;
    }
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
async fn rate_plan(
    tx: &mut PgConnection,
    code: &str,
    policy: Uuid,
    deposit_type: &str,
    deposit_percent: Option<i64>,
    payment_mode: &str,
    sort: i32,
    names: [(&str, &str); 3],
) -> anyhow::Result<Uuid> {
    let id = sqlx::query_scalar!(
        "INSERT INTO rate_plans (id, code, cancellation_policy_id, deposit_type, deposit_percent, payment_mode, sort_order)
         VALUES ($1, $2::text::citext, $3, $4, $5::bigint::numeric, $6, $7) RETURNING id",
        Uuid::now_v7(), code, policy, deposit_type, deposit_percent, payment_mode, sort
    ).fetch_one(&mut *tx).await?;
    for (locale, name) in names {
        sqlx::query!(
            "INSERT INTO rate_plan_translations (rate_plan_id, locale, name) VALUES ($1, $2, $3)",
            id,
            locale,
            name
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(id)
}

async fn season(
    tx: &mut PgConnection,
    code: &str,
    color: &str,
    priority: i32,
    names: [(&str, &str); 3],
) -> anyhow::Result<Uuid> {
    let id = sqlx::query_scalar!(
        "INSERT INTO seasons (id, code, color, priority) VALUES ($1, $2::text::citext, $3, $4) RETURNING id",
        Uuid::now_v7(), code, color, priority
    ).fetch_one(&mut *tx).await?;
    for (locale, name) in names {
        sqlx::query!(
            "INSERT INTO season_translations (season_id, locale, name) VALUES ($1, $2, $3)",
            id,
            locale,
            name
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(id)
}

async fn period(
    tx: &mut PgConnection,
    season_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<()> {
    sqlx::query!(
        "INSERT INTO season_periods (id, season_id, period) VALUES ($1, $2, daterange($3, $4))",
        Uuid::now_v7(),
        season_id,
        from,
        to
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn menu(tx: &mut PgConnection, code: &str, name: &str) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar!(
        "INSERT INTO nav_menus (id, code, name) VALUES ($1, $2::text::citext, $3) RETURNING id",
        Uuid::now_v7(),
        code,
        name
    )
    .fetch_one(&mut *tx)
    .await?)
}

async fn menu_item(
    tx: &mut PgConnection,
    menu_id: Uuid,
    route: &str,
    sort: i32,
    labels: [(&str, &str); 3],
) -> anyhow::Result<()> {
    let id = sqlx::query_scalar!(
        "INSERT INTO nav_menu_items (id, menu_id, link_type, route, sort_order) VALUES ($1, $2, 'route', $3, $4) RETURNING id",
        Uuid::now_v7(), menu_id, route, sort
    ).fetch_one(&mut *tx).await?;
    for (locale, label) in labels {
        sqlx::query!("INSERT INTO nav_menu_item_translations (nav_menu_item_id, locale, label) VALUES ($1, $2, $3)", id, locale, label)
            .execute(&mut *tx).await?;
    }
    Ok(())
}
