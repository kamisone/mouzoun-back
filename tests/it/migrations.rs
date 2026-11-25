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

/// 0035 drops the review status of legal pages: reviewed and draft pages both survive, unchanged,
/// and the migration reverts.
#[sqlx::test(migrations = false)]
async fn the_review_columns_go_and_the_pages_stay(db: PgPool) {
    up_to(34).run(&db).await.unwrap();
    // One draft page (the default) and one marked reviewed.
    let reviewed = uuid::Uuid::now_v7();
    for id in [reviewed, uuid::Uuid::now_v7()] {
        sqlx::query("INSERT INTO pages (id, page_type, status) VALUES ($1, 'legal', 'published')")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query(&format!(
        "DO $$ DECLARE c text; BEGIN
           SELECT column_name INTO c FROM information_schema.columns
            WHERE table_name = 'pages' AND column_name LIKE 'legal_review%';
           EXECUTE format('UPDATE pages SET %I = ''reviewed'' WHERE id = %L', c, '{reviewed}');
         END $$"
    ))
    .execute(&db)
    .await
    .unwrap();
    let review_columns = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM information_schema.columns WHERE table_name = 'pages'
               AND (column_name LIKE 'legal_review%' OR column_name LIKE 'reviewed%')",
        )
        .fetch_one(&db)
        .await
        .unwrap()
    };
    assert_eq!(review_columns().await, 3);

    ALL.run(&db).await.unwrap();
    assert_eq!(review_columns().await, 0);
    let legal: i64 = sqlx::query_scalar("SELECT count(*) FROM pages WHERE page_type = 'legal'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(legal, 2);

    ALL.undo(&db, 34).await.unwrap();
    assert_eq!(review_columns().await, 3);
}
