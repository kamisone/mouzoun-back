//! Invariants 1 and 3 for accommodations, against a real PostgreSQL.

mod common;

use std::sync::Arc;

use common::fixtures::*;
use mouzoun_api::{
    db::bookings::CancelledBy,
    services::booking::{self, AccommodationRequest, BookingError, Cancellation},
};
use sqlx::PgPool;
use tokio::sync::Barrier;
use uuid::Uuid;

fn request(
    product_id: Uuid,
    arrival: chrono::NaiveDate,
    departure: chrono::NaiveDate,
    guests: &GuestIds,
    email: &str,
) -> AccommodationRequest {
    AccommodationRequest {
        product_id,
        arrival,
        departure,
        quantity: 1,
        guests: adults(guests, 2),
        meta: meta(email),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_booking_reserves_every_night_and_records_everything(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 2).await;

    let created = booking::book_accommodation(
        &db,
        &request(tent, d(2027, 1, 10), d(2027, 1, 13), &guests, "a@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();

    assert!(
        created.reference.starts_with("MZ-27-"),
        "{}",
        created.reference
    );
    assert_eq!(created.reference.len(), "MZ-27-000001".len());
    assert_eq!(created.total_minor, 3 * 3_000);
    assert_eq!(created.allocated_ids.len(), 1, "unit mode allocates a unit");
    assert_eq!(
        reserved_nights(&db, tent, d(2027, 1, 9), d(2027, 1, 14)).await,
        vec![0, 1, 1, 1, 0]
    );

    let (status, hold, total): (String, Option<chrono::DateTime<chrono::Utc>>, i64) =
        sqlx::query_as("SELECT status, hold_expires_at, total_minor FROM bookings WHERE id = $1")
            .bind(created.booking_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(hold, Some(now() + chrono::Duration::minutes(30)));
    assert_eq!(total, 9_000);
    let history: Vec<(Option<String>, String)> = sqlx::query_as(
        "SELECT from_status, to_status FROM booking_status_history WHERE booking_id = $1",
    )
    .bind(created.booking_id)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(history, vec![(None, "pending".to_string())]);
    let events: Vec<String> =
        sqlx::query_scalar("SELECT event_type FROM outbox_events WHERE aggregate_id = $1")
            .bind(created.booking_id.to_string())
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(events, vec!["booking.created"]);
    let (stay, unit): (String, Uuid) = sqlx::query_as(
        "SELECT stay::text, unit_id FROM unit_allocations WHERE booking_item_id = $1",
    )
    .bind(created.item_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(stay, "[2027-01-10,2027-01-13)");
    assert_eq!(unit, created.allocated_ids[0]);
}

#[sqlx::test(migrations = "./migrations")]
async fn restrictions_are_enforced(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 5).await;
    let book = |arrival, departure| {
        let db = db.clone();
        let req = request(tent, arrival, departure, &guests, "r@x.ma");
        async move { booking::book_accommodation(&db, &req, now(), today()).await }
    };
    let set = |sql: &'static str| {
        let db = db.clone();
        async move { sqlx::query(sql).bind(tent).execute(&db).await.unwrap() }
    };

    set("UPDATE inventory_calendar SET stop_sell = true WHERE product_id = $1 AND day = '2027-01-11'").await;
    assert!(
        matches!(
            book(d(2027, 1, 10), d(2027, 1, 12)).await,
            Err(BookingError::NotAvailable(_))
        ),
        "stop-sell night"
    );

    set("UPDATE inventory_calendar SET closed_to_arrival = true WHERE product_id = $1 AND day = '2027-01-15'").await;
    assert!(
        matches!(
            book(d(2027, 1, 15), d(2027, 1, 16)).await,
            Err(BookingError::NotAvailable(_))
        ),
        "closed to arrival"
    );
    assert!(
        book(d(2027, 1, 14), d(2027, 1, 16)).await.is_ok(),
        "CTA does not block a stay passing through"
    );

    set("UPDATE inventory_calendar SET closed_to_departure = true WHERE product_id = $1 AND day = '2027-01-20'").await;
    assert!(
        matches!(
            book(d(2027, 1, 18), d(2027, 1, 20)).await,
            Err(BookingError::NotAvailable(_))
        ),
        "closed to departure"
    );

    set("UPDATE inventory_calendar SET min_nights_override = 3 WHERE product_id = $1 AND day = '2027-01-22'").await;
    assert!(
        matches!(
            book(d(2027, 1, 22), d(2027, 1, 24)).await,
            Err(BookingError::NotAvailable(_))
        ),
        "min nights override"
    );
    assert!(book(d(2027, 1, 22), d(2027, 1, 25)).await.is_ok());

    sqlx::query("INSERT INTO closures (scope, period, reason) VALUES ('site', '[2027-02-01,2027-02-05)', 'storm')")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        matches!(
            book(d(2027, 2, 3), d(2027, 2, 6)).await,
            Err(BookingError::NotAvailable(_))
        ),
        "site closure"
    );

    assert!(
        matches!(
            book(d(2028, 6, 1), d(2028, 6, 2)).await,
            Err(BookingError::NotAvailable(_))
        ),
        "calendar not generated"
    );
    assert!(
        matches!(
            book(d(2026, 12, 30), d(2027, 1, 2)).await,
            Err(BookingError::Invalid(_))
        ),
        "arrival in the past"
    );
    assert!(
        matches!(
            book(d(2027, 1, 12), d(2027, 1, 12)).await,
            Err(BookingError::Invalid(_))
        ),
        "zero nights"
    );

    let mut too_many = request(tent, d(2027, 3, 1), d(2027, 3, 2), &guests, "big@x.ma");
    too_many.guests = adults(&guests, 5);
    assert!(
        matches!(
            booking::book_accommodation(&db, &too_many, now(), today()).await,
            Err(BookingError::Invalid(_))
        ),
        "max guests per unit"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn units_are_never_double_allocated(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 2).await;
    let first = booking::book_accommodation(
        &db,
        &request(tent, d(2027, 1, 10), d(2027, 1, 12), &guests, "1@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
    let second = booking::book_accommodation(
        &db,
        &request(tent, d(2027, 1, 11), d(2027, 1, 13), &guests, "2@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
    assert_ne!(
        first.allocated_ids, second.allocated_ids,
        "overlapping stays get different units"
    );
    let third = booking::book_accommodation(
        &db,
        &request(tent, d(2027, 1, 11), d(2027, 1, 12), &guests, "3@x.ma"),
        now(),
        today(),
    )
    .await;
    assert!(matches!(third, Err(BookingError::NotAvailable(_))));

    // The exclusion constraint itself refuses an overlapping allocation of the same unit.
    let overlap = sqlx::query("INSERT INTO unit_allocations (booking_item_id, unit_id, stay) VALUES ($1, $2, '[2027-01-11,2027-01-12)')")
        .bind(second.item_id)
        .bind(first.allocated_ids[0])
        .execute(&db)
        .await
        .unwrap_err();
    assert_eq!(
        overlap
            .as_database_error()
            .and_then(|e| e.code())
            .as_deref(),
        Some("23P01")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn two_concurrent_bookings_for_the_last_free_night_exactly_one_succeeds(db: PgPool) {
    let guests = Arc::new(guest_ids(&db).await);
    let tent = accommodation(&db, "pool", 2).await;

    // Ten races, each on a night with exactly one place left.
    for round in 0..10_u32 {
        let arrival = d(2027, 2, 1 + round);
        let departure = arrival.succ_opt().unwrap();
        booking::book_accommodation(
            &db,
            &request(
                tent,
                arrival,
                departure,
                &guests,
                &format!("first{round}@x.ma"),
            ),
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
                    let req = request(
                        tent,
                        arrival,
                        departure,
                        &guests,
                        &format!("race{round}-{n}@x.ma"),
                    );
                    barrier.wait().await;
                    booking::book_accommodation(&db, &req, now(), today()).await
                })
            })
            .collect();
        let mut outcomes = Vec::new();
        for racer in racers {
            outcomes.push(racer.await.unwrap());
        }
        let won = outcomes.iter().filter(|o| o.is_ok()).count();
        assert_eq!(won, 1, "round {round}: {outcomes:?}");
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, Err(BookingError::NotAvailable(_))))
        );
        assert_eq!(
            reserved_nights(&db, tent, arrival, departure).await,
            vec![2],
            "round {round}: never oversold"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn cancelling_releases_nights_and_units(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 1).await;
    let created = booking::book_accommodation(
        &db,
        &request(tent, d(2027, 1, 10), d(2027, 1, 12), &guests, "c@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();

    let staff = Cancellation {
        by: CancelledBy::Staff,
        reason: Some("guest asked".into()),
        changed_by: None,
    };
    booking::cancel_booking(&db, created.booking_id, &staff, now())
        .await
        .unwrap();

    assert_eq!(
        reserved_nights(&db, tent, d(2027, 1, 10), d(2027, 1, 12)).await,
        vec![0, 0]
    );
    let released: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT released_at FROM unit_allocations WHERE booking_item_id = $1")
            .bind(created.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(released, Some(now()));
    let (status, by): (String, String) =
        sqlx::query_as("SELECT status, cancelled_by FROM bookings WHERE id = $1")
            .bind(created.booking_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((status.as_str(), by.as_str()), ("cancelled", "staff"));
    let last: String = sqlx::query_scalar("SELECT to_status FROM booking_status_history WHERE booking_id = $1 ORDER BY id DESC LIMIT 1")
        .bind(created.booking_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(last, "cancelled");

    // The single unit is free again, and a cancelled booking cannot be cancelled twice.
    assert!(
        booking::book_accommodation(
            &db,
            &request(tent, d(2027, 1, 10), d(2027, 1, 12), &guests, "d@x.ma"),
            now(),
            today()
        )
        .await
        .is_ok()
    );
    assert!(matches!(
        booking::cancel_booking(&db, created.booking_id, &staff, now()).await,
        Err(BookingError::InvalidTransition(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_holds_release_their_inventory_but_confirmed_bookings_stay(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 3).await;
    let stale = booking::book_accommodation(
        &db,
        &request(tent, d(2027, 1, 10), d(2027, 1, 11), &guests, "e1@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
    let paid = booking::book_accommodation(
        &db,
        &request(tent, d(2027, 1, 10), d(2027, 1, 11), &guests, "e2@x.ma"),
        now(),
        today(),
    )
    .await
    .unwrap();
    booking::confirm_booking(&db, paid.booking_id, None, now())
        .await
        .unwrap();

    let later = now() + chrono::Duration::minutes(31);
    assert_eq!(
        booking::expire_holds(&db, now(), 100).await.unwrap(),
        0,
        "holds still running"
    );
    assert_eq!(booking::expire_holds(&db, later, 100).await.unwrap(), 1);
    assert_eq!(
        booking::expire_holds(&db, later, 100).await.unwrap(),
        0,
        "idempotent"
    );

    assert_eq!(
        reserved_nights(&db, tent, d(2027, 1, 10), d(2027, 1, 11)).await,
        vec![1],
        "only the confirmed booking holds a place"
    );
    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status FROM bookings ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
    assert!(statuses.contains(&(stale.booking_id, "expired".into())));
    assert!(statuses.contains(&(paid.booking_id, "confirmed".into())));
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM outbox_events WHERE aggregate_id = $1 ORDER BY id",
    )
    .bind(stale.booking_id.to_string())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(events, vec!["booking.created", "booking.expired"]);
}
