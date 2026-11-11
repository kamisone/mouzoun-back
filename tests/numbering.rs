//! Invariant 7: booking references and gap-free invoice numbers.

mod common;

use common::fixtures::now;
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

    // The year is the Moroccan one: 2027-12-31 23:30 UTC is already 2028 in Casablanca (UTC+1).
    let new_year = "2027-12-31T23:30:00Z".parse().unwrap();
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

#[sqlx::test(migrations = "./migrations")]
async fn invoice_numbers_are_gap_free_per_year(db: PgPool) {
    // A rolled-back invoice gives its number back.
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        next_invoice_number(&mut tx, 2027).await.unwrap(),
        "2027-000001"
    );
    tx.rollback().await.unwrap();

    // Concurrent invoices get consecutive numbers, without holes or duplicates.
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move {
                let mut tx = db.begin().await.unwrap();
                let number = next_invoice_number(&mut tx, 2027).await.unwrap();
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
    let expected: Vec<String> = (1..=10).map(|n| format!("2027-{n:06}")).collect();
    assert_eq!(numbers, expected);

    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        next_invoice_number(&mut tx, 2028).await.unwrap(),
        "2028-000001",
        "each year starts at 1"
    );
    tx.commit().await.unwrap();
}
