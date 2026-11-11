//! Invariants 2 and 3 for activities, against a real PostgreSQL.

mod common;

use std::sync::Arc;

use common::fixtures::*;
use mouzoun_api::{
    db::bookings::CancelledBy,
    services::booking::{self, ActivityRequest, BookingError, Cancellation},
};
use sqlx::PgPool;
use tokio::sync::Barrier;
use uuid::Uuid;

fn request(slot_id: Uuid, guests: &GuestIds, participants: i32, email: &str) -> ActivityRequest {
    ActivityRequest {
        slot_id,
        guests: adults(guests, participants),
        quantity: 1,
        meta: meta(email),
    }
}

async fn slot_state(db: &PgPool, slot_id: Uuid) -> (i32, String) {
    sqlx::query_as("SELECT reserved, status FROM activity_slots WHERE id = $1")
        .bind(slot_id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn active_assignments(db: &PgPool, item_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT resource_id FROM resource_assignments WHERE booking_item_id = $1 AND released_at IS NULL ORDER BY resource_id")
        .bind(item_id)
        .fetch_all(db)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn booking_a_slot_reserves_places_and_assigns_quads(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tour = quad_tour(&db, 8, 4).await;

    let created = booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 3, "q@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();

    assert_eq!(created.total_minor, 3 * 4_500);
    assert_eq!(
        created.allocated_ids.len(),
        2,
        "1 quad per 2 participants: 3 → 2 quads"
    );
    assert_eq!(slot_state(&db, tour.slot_id).await, (3, "open".into()));
    let period: String = sqlx::query_scalar(
        "SELECT period::text FROM resource_assignments WHERE booking_item_id = $1 LIMIT 1",
    )
    .bind(created.item_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(
        period.starts_with("[\"2027-01-10 08:00:00+00\",\"2027-01-10 09:00:00+00\")"),
        "{period}"
    );
    let starts_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT starts_at FROM booking_items WHERE id = $1")
            .bind(created.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(starts_at, tour.starts_at);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_assigned_quad_is_rejected_by_the_exclusion_constraint(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tour = quad_tour(&db, 8, 2).await;
    let created = booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 2, "x@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
    let quad = created.allocated_ids[0];

    // Another slot overlapping in time cannot take the same quad, whatever the code path.
    let other_slot = slot(
        &db,
        tour.product_id,
        tour.starts_at + chrono::Duration::minutes(30),
        8,
    )
    .await;
    let err = sqlx::query(
        "INSERT INTO resource_assignments (resource_id, activity_slot_id, period)
         VALUES ($1, $2, tstzrange($3, $3 + interval '1 hour'))",
    )
    .bind(quad)
    .bind(other_slot)
    .bind(tour.starts_at + chrono::Duration::minutes(30))
    .execute(&db)
    .await
    .unwrap_err();
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23P01"),
        "{err}"
    );

    // A released assignment no longer blocks the quad.
    sqlx::query("UPDATE resource_assignments SET released_at = now() WHERE resource_id = $1")
        .bind(quad)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO resource_assignments (resource_id, activity_slot_id, period)
         VALUES ($1, $2, tstzrange($3, $3 + interval '1 hour'))",
    )
    .bind(quad)
    .bind(other_slot)
    .bind(tour.starts_at + chrono::Duration::minutes(30))
    .execute(&db)
    .await
    .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn running_out_of_quads_blocks_the_booking_even_with_places_left(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tour = quad_tour(&db, 20, 3).await;
    booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 4, "a@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap(); // 2 quads
    let err = booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 3, "b@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, BookingError::NotAvailable(m) if m.contains("vehicle")),
        "{err}"
    );
    assert_eq!(
        slot_state(&db, tour.slot_id).await.0,
        4,
        "the failed booking left nothing behind"
    );
    booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 2, "c@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap(); // last quad
}

#[sqlx::test(migrations = "./migrations")]
async fn capacity_cutoff_and_closures_are_enforced(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tour = quad_tour(&db, 4, 10).await;
    let full = booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 5, "f@x.ma"),
        now(),
        today(),
    )
    .await;
    assert!(
        matches!(full, Err(BookingError::NotAvailable(_))),
        "5 > 4 places"
    );
    booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 4, "g@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(slot_state(&db, tour.slot_id).await, (4, "full".into()));
    assert!(matches!(
        booking::book_activity(
            &db,
            &request(tour.slot_id, &guests, 1, "h@x.ma"),
            now(),
            today()
        )
        .await,
        Err(BookingError::NotAvailable(_))
    ));

    let other_starts = tour.starts_at + chrono::Duration::hours(3);
    let other = slot(&db, tour.product_id, other_starts, 10).await;
    let eleven_hours_before = other_starts - chrono::Duration::hours(11);
    assert!(
        matches!(
            booking::book_activity(&db, &request(other, &guests, 1, "i@x.ma"), eleven_hours_before, today()).await,
            Err(BookingError::NotAvailable(m)) if m.contains("too late")
        ),
        "inside the 12 h cutoff"
    );

    sqlx::query("INSERT INTO closures (scope, product_id, period) VALUES ('product', $1, '[2027-01-10,2027-01-11)')")
        .bind(tour.product_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(matches!(
        booking::book_activity(&db, &request(other, &guests, 1, "j@x.ma"), now(), today()).await,
        Err(BookingError::NotAvailable(m)) if m.contains("closed")
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_bookings_never_overbook_a_slot(db: PgPool) {
    let guests = Arc::new(guest_ids(&db).await);
    let tour = quad_tour(&db, 4, 10).await;
    booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 2, "first@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|n| {
            let (db, barrier, guests) = (db.clone(), barrier.clone(), guests.clone());
            tokio::spawn(async move {
                let req = request(tour.slot_id, &guests, 2, &format!("race{n}@x.ma"));
                barrier.wait().await;
                booking::book_activity(&db, &req, now(), today()).await
            })
        })
        .collect();
    let mut won = 0;
    for racer in racers {
        if racer.await.unwrap().is_ok() {
            won += 1;
        }
    }
    assert_eq!(won, 1);
    assert_eq!(slot_state(&db, tour.slot_id).await, (4, "full".into()));
}

#[sqlx::test(migrations = "./migrations")]
async fn cancelling_releases_places_and_quads(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tour = quad_tour(&db, 2, 1).await;
    let created = booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 2, "k@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(slot_state(&db, tour.slot_id).await, (2, "full".into()));
    assert_eq!(active_assignments(&db, created.item_id).await.len(), 1);

    let customer = Cancellation {
        by: CancelledBy::Customer,
        reason: None,
        changed_by: None,
    };
    booking::cancel_booking(&db, created.booking_id, &customer, now())
        .await
        .unwrap();

    assert_eq!(slot_state(&db, tour.slot_id).await, (0, "open".into()));
    assert!(active_assignments(&db, created.item_id).await.is_empty());
    booking::book_activity(
        &db,
        &request(tour.slot_id, &guests, 2, "l@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
}
