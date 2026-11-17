//! Invariant 7: booking references and gap-free invoice numbers.

use crate::common::fixtures::now;
use mouzoun_api::services::numbering::{next_booking_reference, next_invoice_number};
use sqlx::PgPool;

#[sqlx::test(migrations = "./migrations")]
async fn booking_references_restart_every_year_and_never_repeat(db: PgPool) {
    assert_eq!(
        next_booking_reference(&db, now()).await.unwrap(),
        "MZ-27-000001"
    );
    assert_eq!(
        next_booking_reference(&db, now()).await.unwrap(),
        "MZ-27-000002"
    );

    // Concurrent bookings get distinct numbers.
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move { next_booking_reference(&db, now()).await.unwrap() })
        })
        .collect();
    let mut references = Vec::new();
    for task in tasks {
        references.push(task.await.unwrap());
    }
    references.sort();
    let expected: Vec<String> = (3..=22).map(|n| format!("MZ-27-{n:06}")).collect();
    assert_eq!(references, expected);

    // The year is the camp's (Morocco is UTC+0 since September 2026).
    let new_year = "2028-01-01T00:30:00Z".parse().unwrap();
    assert_eq!(
        next_booking_reference(&db, new_year).await.unwrap(),
        "MZ-28-000001"
    );
    assert_eq!(
        next_booking_reference(&db, now()).await.unwrap(),
        "MZ-27-000023",
        "each year keeps its own counter"
    );
}

async fn entity(db: &PgPool, code: &str) -> uuid::Uuid {
    sqlx::query_scalar("SELECT id FROM legal_entities WHERE code = $1")
        .bind(code)
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn invoice_numbers_are_gap_free_per_entity_and_year(db: PgPool) {
    let (fr, ma) = (
        entity(&db, "mouzoun_fr").await,
        entity(&db, "mouzoun_ma").await,
    );
    // A rolled-back invoice gives its number back.
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        next_invoice_number(&mut tx, fr, 2027).await.unwrap(),
        "F2027-000001"
    );
    tx.rollback().await.unwrap();

    // Concurrent invoices of two entities: each entity gets consecutive numbers, no hole, no duplicate.
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let db = db.clone();
            let entity = if i % 2 == 0 { fr } else { ma };
            tokio::spawn(async move {
                let mut tx = db.begin().await.unwrap();
                let number = next_invoice_number(&mut tx, entity, 2027).await.unwrap();
                tx.commit().await.unwrap();
                number
            })
        })
        .collect();
    let mut numbers = Vec::new();
    for task in tasks {
        numbers.push(task.await.unwrap());
    }
    numbers.sort();
    let mut expected: Vec<String> = (1..=10).map(|n| format!("2027-{n:06}")).collect();
    expected.extend((1..=10).map(|n| format!("F2027-{n:06}")));
    assert_eq!(numbers, expected);

    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        next_invoice_number(&mut tx, fr, 2028).await.unwrap(),
        "F2028-000001",
        "each year starts at 1"
    );
    tx.commit().await.unwrap();
}
