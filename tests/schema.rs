//! Schema contract: migrations are reversible, every status/kind enum mirrors its CHECK
//! constraint exactly (values and sqlx encoding), every `updated_at` table has its trigger.

use mouzoun_api::db::{
    bookings::*, catalog::*, content::*, identity::*, inventory::*, media::*, operations::*,
    pricing::*, taxonomy::*,
};
use sqlx::PgPool;

async fn table_count(db: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' AND table_name <> '_sqlx_migrations'",
    )
    .fetch_one(db)
    .await
    .unwrap()
}

#[sqlx::test(migrations = false)]
async fn migrations_run_up_down_up_on_an_empty_database(db: PgPool) {
    let migrator = sqlx::migrate!("./migrations");
    migrator.run(&db).await.unwrap();
    let tables = table_count(&db).await;
    assert!(tables > 90, "{tables} tables");

    migrator.undo(&db, 0).await.unwrap();
    assert_eq!(table_count(&db).await, 0, "down drops every table");
    let leftovers: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'public')
              + (SELECT count(*) FROM information_schema.sequences WHERE sequence_schema = 'public')
              + (SELECT count(*) FROM pg_extension WHERE extname IN ('citext', 'btree_gist', 'pg_trgm', 'unaccent'))",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        leftovers, 0,
        "no function, sequence or extension left behind"
    );

    migrator.run(&db).await.unwrap();
    assert_eq!(
        table_count(&db).await,
        tables,
        "up again rebuilds the same schema"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn every_updated_at_column_has_its_trigger(db: PgPool) {
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT c.table_name::text
         FROM information_schema.columns c
         JOIN information_schema.tables t USING (table_schema, table_name)
         WHERE c.table_schema = 'public' AND c.column_name = 'updated_at' AND t.table_type = 'BASE TABLE'
           AND NOT EXISTS (SELECT 1 FROM pg_trigger tg JOIN pg_class r ON r.oid = tg.tgrelid
                           WHERE r.relname = c.table_name AND tg.tgname = c.table_name || '_set_updated_at')",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(missing.is_empty(), "tables without trigger: {missing:?}");
}

/// Values of the single-column `CHECK (column IN (...))` constraint of `table`.
async fn check_values(db: &PgPool, table: &str, column: &str) -> Vec<String> {
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid
         WHERE t.relname = $1 AND c.contype = 'c'",
    )
    .bind(table)
    .fetch_all(db)
    .await
    .unwrap();
    let def = defs
        .into_iter()
        .find(|d| d.starts_with(&format!("CHECK (({column} = ANY")) && !d.contains(" OR "))
        .unwrap_or_else(|| panic!("no CHECK (... IN ...) on {table}.{column}"));
    let mut values: Vec<String> = def
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    values.sort();
    values
}

macro_rules! assert_mirrors {
    ($db:expr, $($table:literal . $column:literal => $enum:ty),+ $(,)?) => {{
        let mut checked = 0;
        $(
            let mut expected: Vec<String> = <$enum>::ALL.iter().map(|v| v.as_str().to_owned()).collect();
            expected.sort();
            assert_eq!(check_values($db, $table, $column).await, expected, "{}.{} vs {}", $table, $column, stringify!($enum));
            for v in <$enum>::ALL {
                let encoded: String = sqlx::query_scalar("SELECT $1::text").bind(*v).fetch_one($db).await.unwrap();
                assert_eq!(encoded, v.as_str(), "sqlx encoding of {v:?}");
                let decoded: $enum = sqlx::query_scalar("SELECT $1::text").bind(v.as_str()).fetch_one($db).await.unwrap();
                assert_eq!(decoded, *v);
            }
            checked += 1;
        )+
        checked
    }};
}

#[sqlx::test(migrations = "./migrations")]
async fn rust_enums_mirror_the_check_constraints(db: PgPool) {
    let checked = assert_mirrors!(&db,
        "users"."status" => UserStatus,
        "auth_tokens"."purpose" => TokenPurpose,
        "settings"."value_type" => SettingValueType,
        "media_assets"."kind" => MediaKind,
        "categories"."scope" => CategoryScope,
        "amenities"."amenity_group" => AmenityGroup,
        "places"."kind" => PlaceKind,
        "products"."kind" => ProductKind,
        "products"."status" => ProductStatus,
        "products"."booking_mode" => BookingMode,
        "accommodation_types"."accommodation_kind" => AccommodationKind,
        "accommodation_types"."inventory_mode" => InventoryMode,
        "activities"."location_type" => LocationType,
        "activities"."scheduling_mode" => SchedulingMode,
        "activities"."difficulty" => Difficulty,
        "extras"."applies_to" => ExtraAppliesTo,
        "packages"."pricing_mode" => PackagePricingMode,
        "rate_plans"."meal_plan" => MealPlan,
        "rate_plans"."deposit_type" => DepositType,
        "rate_plans"."payment_mode" => PaymentMode,
        "prices"."pricing_unit" => PricingUnit,
        "taxes"."calc_type" => TaxCalcType,
        "promotions"."discount_type" => DiscountType,
        "accommodation_units"."status" => AssetStatus,
        "resources"."status" => AssetStatus,
        "resources"."kind" => ResourceKind,
        "product_resource_requirements"."resource_kind" => ResourceKind,
        "activity_slots"."status" => SlotStatus,
        "closures"."scope" => ClosureScope,
        "bookings"."status" => BookingStatus,
        "bookings"."source" => BookingSource,
        "bookings"."cancelled_by" => CancelledBy,
        "booking_items"."status" => BookingItemStatus,
        "booking_items"."product_kind" => ProductKind,
        "booking_guests"."id_document_type" => IdDocumentType,
        "payments"."provider" => PaymentProvider,
        "payments"."purpose" => PaymentPurpose,
        "payments"."status" => PaymentStatus,
        "refunds"."status" => RefundStatus,
        "invoices"."kind" => InvoiceKind,
        "reviews"."source" => ReviewSource,
        "reviews"."status" => ReviewStatus,
        "pages"."page_type" => PageType,
        "pages"."status" => PageStatus,
        "nav_menu_items"."link_type" => NavLinkType,
        "banners"."placement" => BannerPlacement,
        "inquiries"."kind" => InquiryKind,
        "inquiries"."status" => InquiryStatus,
        "newsletter_subscribers"."status" => SubscriberStatus,
        "email_log"."status" => EmailStatus,
        "audit_logs"."actor_type" => ActorType,
    );
    assert_eq!(checked, 51);
}

#[sqlx::test(migrations = "./migrations")]
async fn deposits_and_discounts_have_one_column_per_unit(db: PgPool) {
    let rate_plan = "INSERT INTO rate_plans (id, code, deposit_type, deposit_percent, deposit_amount_minor, deposit_currency) VALUES (gen_random_uuid(), 'p' || gen_random_uuid(), ";
    for (values, ok) in [
        ("'percent', 30, NULL, NULL)", true),
        ("'fixed', NULL, 5000, 'EUR')", true),
        ("'none', NULL, NULL, NULL)", true),
        ("'percent', NULL, 3000, 'EUR')", false), // percentage stored as an amount
        ("'fixed', 50, NULL, NULL)", false),      // amount stored as a percentage
        ("'fixed', NULL, 5000, NULL)", false),    // amount without currency
        ("'percent', 130, NULL, NULL)", false),
        ("'full', 30, NULL, NULL)", false),
    ] {
        let result = sqlx::query(&format!("{rate_plan}{values}"))
            .execute(&db)
            .await;
        assert_eq!(result.is_ok(), ok, "{values}");
    }

    let promotion = "INSERT INTO promotions (id, discount_type, discount_percent, discount_amount_minor, discount_free_nights, currency, min_subtotal_minor) VALUES (gen_random_uuid(), ";
    for (values, ok) in [
        ("'percent', 10, NULL, NULL, NULL, NULL)", true),
        ("'fixed_amount', NULL, 1500, NULL, 'EUR', NULL)", true),
        ("'free_nights', NULL, NULL, 1, NULL, NULL)", true),
        ("'percent', 10, NULL, NULL, NULL, 20000)", false), // minimum without currency
        ("'fixed_amount', 15, NULL, NULL, 'EUR', NULL)", false),
        ("'fixed_amount', NULL, 1500, NULL, NULL, NULL)", false),
        ("'free_nights', NULL, 1500, NULL, 'EUR', NULL)", false),
    ] {
        let result = sqlx::query(&format!("{promotion}{values}"))
            .execute(&db)
            .await;
        assert_eq!(result.is_ok(), ok, "{values}");
    }
}
