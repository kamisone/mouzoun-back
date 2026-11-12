//! The migration history as production lived it: 0001–0012 as first deployed, real rows, then
//! the later migrations. Guards the 0020 corrections (money columns split per unit).

use std::borrow::Cow;

use sqlx::{
    PgPool,
    migrate::{Migration, Migrator},
};

use rust_decimal::Decimal;

/// (code, percent, amount in minor units, currency) of a rate plan deposit.
type DepositRow = (String, Option<Decimal>, Option<i64>, Option<String>);
/// (code, percent, amount in minor units, free nights) of a promotion.
type DiscountRow = (String, Option<Decimal>, Option<i64>, Option<i32>);

static ALL: Migrator = sqlx::migrate!("./migrations");

fn up_to(max: i64) -> Migrator {
    let migrations: Vec<Migration> = ALL
        .iter()
        .filter(|m| m.version <= max && m.migration_type.is_up_migration())
        .cloned()
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
}

#[sqlx::test(migrations = false)]
async fn later_migrations_convert_the_first_production_schema(db: PgPool) {
    up_to(12).run(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO rate_plans (code, deposit_type, deposit_value) VALUES
            ('flex', 'percent', 30), ('fixed', 'fixed', 50.25), ('none', 'none', NULL)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO promotions (code, discount_type, discount_value, currency) VALUES
            ('TEN', 'percent', 10, NULL), ('TWENTY', 'fixed_amount', 20, 'EUR'), ('NIGHT', 'free_nights', 1, NULL)",
    )
    .execute(&db)
    .await
    .unwrap();

    ALL.run(&db).await.unwrap();

    let plans: Vec<DepositRow> = sqlx::query_as(
        "SELECT code::text, deposit_percent, deposit_amount_minor, deposit_currency::text FROM rate_plans ORDER BY code",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        plans,
        vec![
            ("fixed".into(), None, Some(5025), Some("EUR".into())),
            ("flex".into(), Some(Decimal::from(30)), None, None),
            ("none".into(), None, None, None),
        ]
    );
    let promos: Vec<DiscountRow> = sqlx::query_as(
        "SELECT code::text, discount_percent, discount_amount_minor, discount_free_nights FROM promotions ORDER BY code",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        promos,
        vec![
            ("NIGHT".into(), None, None, Some(1)),
            ("TEN".into(), Some(Decimal::from(10)), None, None),
            ("TWENTY".into(), None, Some(2000), None),
        ]
    );
    // The new rules hold: a percent deposit above 100 is refused.
    assert!(
        sqlx::query("INSERT INTO rate_plans (code, deposit_type, deposit_percent) VALUES ('bad', 'percent', 120)")
            .execute(&db)
            .await
            .is_err()
    );
    let counters: bool =
        sqlx::query_scalar("SELECT to_regclass('booking_reference_counters') IS NOT NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(counters);
    // Running everything again changes nothing (0020 is idempotent by design, SQLx skips it).
    ALL.run(&db).await.unwrap();
}
