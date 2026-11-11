//! Catalogue fixtures for the booking, inventory and catalogue tests.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use mouzoun_api::{
    db::{bookings::BookingSource, types::Locale},
    services::{
        booking::{BookingRequestMeta, CustomerInput, CustomerRef},
        inventory,
        pricing::GuestCount,
    },
};
use sqlx::PgPool;
use uuid::Uuid;

pub fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

/// "Now" of every booking test: 1 January 2027, 10:00 UTC.
pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2027, 1, 1, 10, 0, 0).unwrap()
}

pub fn today() -> NaiveDate {
    d(2027, 1, 1)
}

pub struct GuestIds {
    pub adult: Uuid,
    pub child: Uuid,
    pub infant: Uuid,
}

pub async fn guest_ids(db: &PgPool) -> GuestIds {
    let id = |code: &'static str| async move {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM guest_categories WHERE code = $1")
            .bind(code)
            .fetch_one(db)
            .await
            .unwrap()
    };
    GuestIds {
        adult: id("adult").await,
        child: id("child").await,
        infant: id("infant").await,
    }
}

pub fn adults(ids: &GuestIds, n: i32) -> Vec<GuestCount> {
    vec![GuestCount {
        guest_category_id: ids.adult,
        count: n,
    }]
}

pub fn meta(email: &str) -> BookingRequestMeta {
    BookingRequestMeta {
        customer: CustomerRef::New(CustomerInput {
            email: Some(email.into()),
            first_name: "Amina".into(),
            last_name: Some("Test".into()),
            phone: None,
            country_code: Some("MA".into()),
            preferred_locale: None,
        }),
        locale: Locale::new("fr"),
        source: BookingSource::Website,
        rate_plan_id: None,
        promo_code: None,
        special_requests: None,
        created_by: None,
        ip_address: None,
        user_agent: None,
        hold: chrono::Duration::minutes(30),
    }
}

async fn product(db: &PgPool, kind: &str, name: &str, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO products (id, kind, status, published_at) VALUES ($1, $2, 'published', now())",
    )
    .bind(id)
    .bind(kind)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, 'fr', $2, $3)")
        .bind(id)
        .bind(name)
        .bind(slug)
        .execute(db)
        .await
        .unwrap();
    id
}

pub async fn price(db: &PgPool, product_id: Uuid, unit: &str, amount_minor: i64) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO prices (product_id, pricing_unit, amount_minor, currency) VALUES ($1, $2, $3, 'EUR') RETURNING id",
    )
    .bind(product_id)
    .bind(unit)
    .bind(amount_minor)
    .fetch_one(db)
    .await
    .unwrap()
}

/// Accommodation type with `units` active units, priced 30 €/unit/night, calendar open for
/// January–March 2027.
pub async fn accommodation(db: &PgPool, inventory_mode: &str, units: i32) -> Uuid {
    let id = product(
        db,
        "accommodation",
        "Tente nomade",
        &format!("tente-{}", Uuid::now_v7()),
    )
    .await;
    sqlx::query(
        "INSERT INTO accommodation_types (product_id, accommodation_kind, inventory_mode, max_guests, standard_occupancy)
         VALUES ($1, 'nomad_tent', $2, 4, 2)",
    )
    .bind(id)
    .bind(inventory_mode)
    .execute(db)
    .await
    .unwrap();
    for n in 1..=units {
        sqlx::query(
            "INSERT INTO accommodation_units (product_id, code, sort_order) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(format!("U{n:02}"))
        .bind(n)
        .execute(db)
        .await
        .unwrap();
    }
    price(db, id, "per_unit_per_night", 3_000).await;
    inventory::generate_inventory_calendar(db, Some(id), d(2027, 1, 1), 3)
        .await
        .unwrap();
    id
}

pub struct ActivityFixture {
    pub product_id: Uuid,
    pub slot_id: Uuid,
    pub quads: Vec<Uuid>,
    pub starts_at: DateTime<Utc>,
}

/// Quad tour: 45 €/person, one quad per 2 participants, `quads` quads, one slot of `capacity`
/// places on 10 January 2027 at 08:00 UTC (08:00 in Casablanca, UTC+0), 1 hour.
pub async fn quad_tour(db: &PgPool, capacity: i32, quads: i32) -> ActivityFixture {
    let product_id = product(
        db,
        "activity",
        "Balade en quad",
        &format!("quad-{}", Uuid::now_v7()),
    )
    .await;
    sqlx::query(
        "INSERT INTO activities (product_id, location_type, scheduling_mode, duration_minutes, booking_cutoff_hours)
         VALUES ($1, 'off_site', 'fixed_slots', 60, 12)",
    )
    .bind(product_id)
    .execute(db)
    .await
    .unwrap();
    price(db, product_id, "per_person", 4_500).await;
    sqlx::query(
        "INSERT INTO product_resource_requirements (product_id, resource_kind, units_per_group, participants_per_group)
         VALUES ($1, 'vehicle', 1, 2)",
    )
    .bind(product_id)
    .execute(db)
    .await
    .unwrap();
    let mut quad_ids = Vec::new();
    for n in 1..=quads {
        let code = format!("QUAD-{}-{n:02}", &product_id.simple().to_string()[24..]);
        quad_ids.push(
            sqlx::query_scalar(
                "INSERT INTO resources (kind, code, name) VALUES ('vehicle', $1, $1) RETURNING id",
            )
            .bind(code)
            .fetch_one(db)
            .await
            .unwrap(),
        );
    }
    let starts_at = Utc.with_ymd_and_hms(2027, 1, 10, 8, 0, 0).unwrap();
    let slot_id = slot(db, product_id, starts_at, capacity).await;
    ActivityFixture {
        product_id,
        slot_id,
        quads: quad_ids,
        starts_at,
    }
}

pub async fn slot(db: &PgPool, product_id: Uuid, starts_at: DateTime<Utc>, capacity: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO activity_slots (product_id, starts_at, ends_at, capacity)
         VALUES ($1, $2, $2 + interval '1 hour', $3) RETURNING id",
    )
    .bind(product_id)
    .bind(starts_at)
    .bind(capacity)
    .fetch_one(db)
    .await
    .unwrap()
}

pub async fn reserved_nights(
    db: &PgPool,
    product_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<i32> {
    sqlx::query_scalar("SELECT reserved FROM inventory_calendar WHERE product_id = $1 AND day >= $2 AND day < $3 ORDER BY day")
        .bind(product_id)
        .bind(from)
        .bind(to)
        .fetch_all(db)
        .await
        .unwrap()
}
