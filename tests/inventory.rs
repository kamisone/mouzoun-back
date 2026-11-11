//! Invariants 5 (calendar generator) and 6 (slot generator).

mod common;

use chrono::{Datelike, TimeZone, Utc, Weekday};
use common::fixtures::*;
use mouzoun_api::services::inventory::{generate_activity_slots, generate_inventory_calendar};
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn the_calendar_covers_18_months_and_never_overwrites_manual_changes(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 3).await; // calendar already open Jan–Mar 2027
    sqlx::query("UPDATE accommodation_units SET status = 'maintenance' WHERE product_id = $1 AND code = 'U03'")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE inventory_calendar SET capacity = 10, stop_sell = true WHERE product_id = $1 AND day = '2027-02-01'")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    mouzoun_api::services::booking::book_accommodation(
        &db,
        &mouzoun_api::services::booking::AccommodationRequest {
            product_id: tent,
            arrival: d(2027, 1, 10),
            departure: d(2027, 1, 11),
            quantity: 1,
            guests: adults(&guests, 2),
            meta: meta("cal@x.ma"),
        },
        now(),
        today(),
    )
    .await
    .unwrap();

    let created = generate_inventory_calendar(&db, None, d(2027, 1, 1), 18)
        .await
        .unwrap();
    let days_in_18_months = (d(2028, 7, 1) - d(2027, 1, 1)).num_days();
    let existing_before = (d(2027, 4, 1) - d(2027, 1, 1)).num_days();
    assert_eq!(
        created as i64,
        days_in_18_months - existing_before,
        "only missing nights are created"
    );
    assert_eq!(
        generate_inventory_calendar(&db, None, d(2027, 1, 1), 18)
            .await
            .unwrap(),
        0,
        "idempotent"
    );

    let row = |day: chrono::NaiveDate| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (i32, i32, bool)>("SELECT capacity, reserved, stop_sell FROM inventory_calendar WHERE product_id = $1 AND day = $2")
                .bind(tent)
                .bind(day)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        row(d(2027, 2, 1)).await,
        (10, 0, true),
        "manual override preserved"
    );
    assert_eq!(
        row(d(2027, 1, 10)).await,
        (3, 1, false),
        "reservation preserved"
    );
    assert_eq!(
        row(d(2028, 6, 30)).await,
        (2, 0, false),
        "new nights: active units only (U03 is in maintenance)"
    );
    let last: chrono::NaiveDate =
        sqlx::query_scalar("SELECT max(day) FROM inventory_calendar WHERE product_id = $1")
            .bind(tent)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(last, d(2028, 6, 30));
}

#[sqlx::test(migrations = "./migrations")]
async fn slots_follow_the_schedule_in_casablanca_time_and_are_idempotent(db: PgPool) {
    let tour = quad_tour(&db, 8, 2).await;
    sqlx::query("DELETE FROM activity_slots WHERE product_id = $1")
        .bind(tour.product_id)
        .execute(&db)
        .await
        .unwrap();
    // Monday, Wednesday, Friday at 09:00 local, January → February 2027.
    sqlx::query(
        "INSERT INTO activity_schedules (product_id, days_of_week, start_time, capacity, valid_period)
         VALUES ($1, 1 | 4 | 16, '09:00', 6, '[2027-01-01,2027-03-01)')",
    )
    .bind(tour.product_id)
    .execute(&db)
    .await
    .unwrap();

    let created = generate_activity_slots(&db, d(2027, 1, 1), 120)
        .await
        .unwrap();
    assert_eq!(
        generate_activity_slots(&db, d(2027, 1, 1), 120)
            .await
            .unwrap(),
        0,
        "idempotent"
    );

    let slots: Vec<(chrono::DateTime<Utc>, chrono::DateTime<Utc>, i32)> =
        sqlx::query_as("SELECT starts_at, ends_at, capacity FROM activity_slots WHERE product_id = $1 ORDER BY starts_at")
            .bind(tour.product_id)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(slots.len() as u64, created);
    let expected = d(2027, 1, 1)
        .iter_days()
        .take_while(|day| *day < d(2027, 3, 1))
        .filter(|day| matches!(day.weekday(), Weekday::Mon | Weekday::Wed | Weekday::Fri))
        .count();
    assert_eq!(
        slots.len(),
        expected,
        "only scheduled weekdays inside the valid period"
    );
    assert!(
        slots
            .iter()
            .all(|(s, e, c)| *e - *s == chrono::Duration::minutes(60) && *c == 6)
    );

    let at = |y, m, day, h| Utc.with_ymd_and_hms(y, m, day, h, 0, 0).unwrap();
    assert!(
        slots.iter().any(|s| s.0 == at(2027, 1, 4, 8)),
        "09:00 Casablanca = 08:00 UTC (UTC+1)"
    );
    assert!(
        slots.iter().any(|s| s.0 == at(2027, 2, 22, 9)),
        "during Ramadan, Morocco is UTC+0"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_new_unit_type_gets_its_calendar_from_the_generator(db: PgPool) {
    let tent = accommodation(&db, "pool", 20).await;
    let product: Uuid = tent;
    let capacity: i32 = sqlx::query_scalar(
        "SELECT capacity FROM inventory_calendar WHERE product_id = $1 AND day = '2027-01-01'",
    )
    .bind(product)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(capacity, 20, "pool size = active units");
}
