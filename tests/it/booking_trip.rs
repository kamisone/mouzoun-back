//! Trips (stay + activities + extras in one booking) against a real PostgreSQL: pricing,
//! promotions, restrictions, payment options, concurrency and price tampering.

use std::sync::Arc;

use crate::common::fixtures::*;
use mouzoun_api::{
    db::{
        bookings::{BookingSource, BookingStatus, PaymentOption},
        types::Locale,
    },
    services::{
        audit::Actor,
        booking::{
            BookingError, CustomerInput, CustomerRef,
            settings::BookingSettings,
            trip::{self, CheckoutInput, TripContext, TripItem, TripRequest},
        },
        pricing::GuestCount,
    },
};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Barrier;
use uuid::Uuid;

fn ctx() -> TripContext {
    TripContext {
        locale: Locale::new("fr"),
        public: true,
        now: now(),
        settings: BookingSettings {
            enabled: true,
            ..BookingSettings::default()
        },
        online_payments: true,
    }
}

fn stay(
    product_id: Uuid,
    arrival: chrono::NaiveDate,
    departure: chrono::NaiveDate,
    guests: Vec<GuestCount>,
) -> TripItem {
    TripItem::Stay {
        product_id,
        arrival,
        departure,
        quantity: 1,
        guests,
        unit_ids: vec![],
    }
}

fn activity(product_id: Uuid, slot_id: Uuid, guests: Vec<GuestCount>) -> TripItem {
    TripItem::Activity {
        product_id,
        slot_id: Some(slot_id),
        date: None,
        time: None,
        guests,
        quantity: 1,
        language: None,
    }
}

fn checkout(
    request: TripRequest,
    email: &str,
    option: PaymentOption,
    expected: Option<i64>,
) -> CheckoutInput {
    CheckoutInput {
        request,
        customer: CustomerRef::New(CustomerInput {
            email: Some(email.into()),
            first_name: "Amina".into(),
            last_name: Some("Test".into()),
            phone: Some("+212600000000".into()),
            country_code: Some("MA".into()),
            preferred_locale: None,
        }),
        payment_option: option,
        expected_total_minor: expected,
        special_requests: None,
        estimated_arrival_time: None,
        terms_version: Some("2026-11".into()),
        marketing_consent: false,
        whatsapp_preferred: false,
        save_card_for_balance: false,
        source: BookingSource::Website,
        actor: Actor::customer(),
        skip_guest_emails: false,
        request_id: None,
        ip_address: None,
        user_agent: None,
    }
}

/// Rate plan: `payment_mode`, optional percent deposit, flexible policy (7 days 100 %, 2 days
/// 50 %, then 0 %). Public, first in order.
async fn rate_plan(db: &PgPool, payment_mode: &str, deposit_percent: Option<i32>) -> Uuid {
    let policy: Uuid = sqlx::query_scalar(
        "INSERT INTO cancellation_policies (code, rules) VALUES ($1, $2) RETURNING id",
    )
    .bind(format!("p-{}", Uuid::now_v7()))
    .bind(json!([
        {"min_hours_before": 168, "refund_percent": 100},
        {"min_hours_before": 48, "refund_percent": 50},
        {"min_hours_before": 0, "refund_percent": 0}
    ]))
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO rate_plans (code, cancellation_policy_id, payment_mode, deposit_type, deposit_percent)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(format!("rp-{}", Uuid::now_v7()))
    .bind(policy)
    .bind(payment_mode)
    .bind(if deposit_percent.is_some() { "percent" } else { "none" })
    .bind(deposit_percent.map(rust_decimal::Decimal::from))
    .fetch_one(db)
    .await
    .unwrap()
}

async fn extra(db: &PgPool, unit: &str, amount: i64, mandatory_for: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO products (id, kind, status, published_at) VALUES ($1, 'extra', 'published', now())")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, 'fr', 'Extra', $2)")
        .bind(id)
        .bind(format!("extra-{id}"))
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO extras (product_id, applies_to) VALUES ($1, 'any')")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    price(db, id, unit, amount).await;
    if let Some(parent) = mandatory_for {
        sqlx::query("INSERT INTO product_extras (product_id, extra_product_id, is_mandatory) VALUES ($1, $2, true)")
            .bind(parent)
            .bind(id)
            .execute(db)
            .await
            .unwrap();
    }
    id
}

async fn quote(db: &PgPool, request: &TripRequest) -> trip::TripQuote {
    quote_with(db, request, &ctx()).await
}

async fn quote_with(db: &PgPool, request: &TripRequest, ctx: &TripContext) -> trip::TripQuote {
    let mut conn = db.acquire().await.unwrap();
    trip::quote(&mut conn, request, ctx).await.unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_trip_books_a_stay_an_activity_and_an_extra_under_one_reference(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 2).await;
    let quad = quad_tour(&db, 8, 4).await;
    let firewood = extra(&db, "per_unit", 500, None).await;
    rate_plan(&db, "optional_online", Some(30)).await;

    let request = TripRequest {
        items: vec![
            stay(tent, d(2027, 1, 9), d(2027, 1, 12), adults(&guests, 2)),
            activity(quad.product_id, quad.slot_id, adults(&guests, 2)),
            TripItem::Extra {
                product_id: firewood,
                parent: Some(0),
                date: None,
                quantity: 2,
            },
        ],
        ..TripRequest::default()
    };
    // Balance due 3 days before the trip starts (9 January): 6 January, still ahead.
    let mut c = ctx();
    c.settings.balance_due_days = 3;
    let q = quote_with(&db, &request, &c).await;
    assert!(
        !q.has_issues,
        "{:?}",
        q.items.iter().map(|i| &i.issue).collect::<Vec<_>>()
    );
    // 3 nights × 30 € + 2 × 45 € + 2 × 5 €.
    assert_eq!(q.total_minor, 9_000 + 9_000 + 1_000);
    assert_eq!(
        q.items.iter().map(|i| i.total_minor).sum::<i64>(),
        q.total_minor
    );
    assert_eq!(
        q.payment_options
            .iter()
            .map(|c| c.option)
            .collect::<Vec<_>>(),
        vec![
            PaymentOption::Full,
            PaymentOption::Deposit,
            PaymentOption::OnSite
        ]
    );
    let deposit = q.choice(PaymentOption::Deposit).unwrap();
    assert_eq!(deposit.due_now_minor, 5_700, "30 % of 190 €");
    assert_eq!(deposit.due_later_minor, 13_300);
    assert_eq!(q.cancellation.len(), 3);

    let created = trip::create(
        &db,
        &checkout(
            request,
            "trip@x.ma",
            PaymentOption::Deposit,
            Some(q.total_minor),
        ),
        &c,
    )
    .await
    .unwrap();
    assert_eq!(created.status, BookingStatus::AwaitingPayment);
    assert_eq!(created.due_now_minor, 5_700);
    assert!(created.hold_expires_at.is_some());
    assert!(created.reference.starts_with("MZ-27-"));

    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM booking_items WHERE booking_id = $1")
        .bind(created.booking_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(items, 3);
    assert_eq!(
        reserved_nights(&db, tent, d(2027, 1, 9), d(2027, 1, 12)).await,
        vec![1, 1, 1]
    );
    let (slot_reserved, assigned): (i32, i64) = sqlx::query_as(
        "SELECT (SELECT reserved FROM activity_slots WHERE id = $1),
                (SELECT count(*) FROM resource_assignments WHERE activity_slot_id = $1 AND released_at IS NULL)",
    )
    .bind(quad.slot_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (slot_reserved, assigned),
        (2, 1),
        "2 seats, one quad for two"
    );
    let parent_linked: bool = sqlx::query_scalar(
        "SELECT parent_item_id IS NOT NULL FROM booking_items WHERE booking_id = $1 AND product_id = $2",
    )
    .bind(created.booking_id)
    .bind(firewood)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(parent_linked);
    let (deposit_due, balance_due_on): (i64, Option<chrono::NaiveDate>) =
        sqlx::query_as("SELECT deposit_due_minor, balance_due_on FROM bookings WHERE id = $1")
            .bind(created.booking_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(deposit_due, 5_700);
    assert_eq!(balance_due_on, Some(d(2027, 1, 6)));
    // With the default 14 days, the balance would already be due: no deposit option.
    let q = quote(
        &db,
        &TripRequest {
            items: vec![stay(
                tent,
                d(2027, 1, 9),
                d(2027, 1, 10),
                adults(&guests, 2),
            )],
            ..TripRequest::default()
        },
    )
    .await;
    assert!(q.choice(PaymentOption::Deposit).is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn paying_at_the_camp_confirms_at_once_and_mandatory_extras_are_added(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 3).await;
    let cleaning = extra(&db, "per_booking", 1_000, Some(tent)).await;
    rate_plan(&db, "pay_on_site", None).await;
    let request = TripRequest {
        items: vec![stay(tent, d(2027, 2, 1), d(2027, 2, 3), adults(&guests, 2))],
        ..TripRequest::default()
    };
    let q = quote(&db, &request).await;
    assert_eq!(q.items.len(), 2);
    assert!(q.items[1].mandatory && q.items[1].product_id == cleaning);
    assert_eq!(q.total_minor, 6_000 + 1_000);
    assert_eq!(q.payment_options.len(), 1);

    let created = trip::create(
        &db,
        &checkout(request, "camp@x.ma", PaymentOption::OnSite, Some(7_000)),
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(created.status, BookingStatus::Confirmed);
    assert_eq!(created.due_now_minor, 0);
    assert!(created.hold_expires_at.is_none());
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM outbox_events WHERE aggregate_id = $1 ORDER BY id",
    )
    .bind(created.booking_id.to_string())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(events, vec!["booking.created", "booking.confirmed"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_tampered_or_stale_price_is_refused(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 1).await;
    rate_plan(&db, "pay_on_site", None).await;
    let request = TripRequest {
        items: vec![stay(tent, d(2027, 2, 1), d(2027, 2, 3), adults(&guests, 2))],
        ..TripRequest::default()
    };
    let err = trip::create(
        &db,
        &checkout(request.clone(), "t@x.ma", PaymentOption::OnSite, Some(100)),
        &ctx(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            BookingError::PriceChanged {
                expected: 100,
                actual: 6_000
            }
        ),
        "{err:?}"
    );
    // Nothing was reserved.
    assert_eq!(
        reserved_nights(&db, tent, d(2027, 2, 1), d(2027, 2, 3)).await,
        vec![0, 0]
    );
    // A payment option the rate plan does not offer is refused too.
    let err = trip::create(
        &db,
        &checkout(request, "t@x.ma", PaymentOption::Full, Some(6_000)),
        &ctx(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, BookingError::PaymentOptionNotAllowed),
        "{err:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn two_concurrent_checkouts_for_the_last_unit_one_wins(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 1).await;
    rate_plan(&db, "pay_on_site", None).await;
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for n in 0..2 {
        let db = db.clone();
        let barrier = barrier.clone();
        let request = TripRequest {
            items: vec![stay(
                tent,
                d(2027, 1, 20),
                d(2027, 1, 22),
                adults(&guests, 2),
            )],
            ..TripRequest::default()
        };
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            trip::create(
                &db,
                &checkout(
                    request,
                    &format!("race{n}@x.ma"),
                    PaymentOption::OnSite,
                    Some(6_000),
                ),
                &ctx(),
            )
            .await
        }));
    }
    let mut ok = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => ok += 1,
            Err(BookingError::ItemUnavailable { issue, .. }) => assert_eq!(issue.code, "sold_out"),
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(
        reserved_nights(&db, tent, d(2027, 1, 20), d(2027, 1, 22)).await,
        vec![1, 1]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn two_concurrent_trips_for_the_last_quad_one_wins(db: PgPool) {
    let guests = guest_ids(&db).await;
    // 8 seats but a single quad: two parties of two need two quads.
    let quad = quad_tour(&db, 8, 1).await;
    rate_plan(&db, "pay_on_site", None).await;
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for n in 0..2 {
        let db = db.clone();
        let barrier = barrier.clone();
        let request = TripRequest {
            items: vec![activity(quad.product_id, quad.slot_id, adults(&guests, 2))],
            ..TripRequest::default()
        };
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            trip::create(
                &db,
                &checkout(
                    request,
                    &format!("quad{n}@x.ma"),
                    PaymentOption::OnSite,
                    Some(9_000),
                ),
                &ctx(),
            )
            .await
        }));
    }
    let results: Vec<_> = futures_join(handles).await;
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        match r {
            BookingError::ItemUnavailable { issue, .. } => assert_eq!(issue.code, "no_resource"),
            BookingError::NotAvailable(_) => {} // exclusion constraint, last line of defence
            e => panic!("unexpected error: {e:?}"),
        }
    }
}

async fn futures_join<T>(handles: Vec<tokio::task::JoinHandle<T>>) -> Vec<T> {
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

#[sqlx::test(migrations = "./migrations")]
async fn restrictions_are_explained_item_by_item(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 2).await;
    rate_plan(&db, "pay_on_site", None).await;
    sqlx::query("UPDATE inventory_calendar SET min_nights_override = 3 WHERE product_id = $1 AND day = '2027-02-05'")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE inventory_calendar SET closed_to_arrival = true WHERE product_id = $1 AND day = '2027-02-10'")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE inventory_calendar SET closed_to_departure = true WHERE product_id = $1 AND day = '2027-02-14'")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE inventory_calendar SET stop_sell = true WHERE product_id = $1 AND day = '2027-02-20'")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO closures (scope, period) VALUES ('site', '[2027-03-01,2027-03-05)')")
        .execute(&db)
        .await
        .unwrap();
    let cases = [
        (
            d(2027, 2, 5),
            d(2027, 2, 7),
            adults(&guests, 2),
            "min_nights",
        ),
        (
            d(2027, 2, 10),
            d(2027, 2, 12),
            adults(&guests, 2),
            "closed_to_arrival",
        ),
        (
            d(2027, 2, 12),
            d(2027, 2, 14),
            adults(&guests, 2),
            "closed_to_departure",
        ),
        (
            d(2027, 2, 19),
            d(2027, 2, 21),
            adults(&guests, 2),
            "sold_out",
        ),
        (d(2027, 3, 2), d(2027, 3, 3), adults(&guests, 2), "closed"),
        (
            d(2027, 2, 1),
            d(2027, 2, 2),
            adults(&guests, 5),
            "too_many_guests",
        ),
        (
            d(2026, 12, 30),
            d(2027, 1, 2),
            adults(&guests, 2),
            "past_date",
        ),
        (d(2027, 5, 1), d(2027, 5, 3), adults(&guests, 2), "not_open"),
    ];
    for (arrival, departure, party, code) in cases {
        let q = quote(
            &db,
            &TripRequest {
                items: vec![stay(tent, arrival, departure, party)],
                ..TripRequest::default()
            },
        )
        .await;
        assert_eq!(
            q.items[0].issue.as_ref().map(|i| i.code.as_str()),
            Some(code),
            "{arrival}"
        );
        assert!(q.has_issues);
        assert_eq!(q.total_minor, 0, "unavailable items are not counted");
    }
    let too_many = quote(
        &db,
        &TripRequest {
            items: vec![stay(tent, d(2027, 2, 1), d(2027, 2, 2), adults(&guests, 6))],
            ..TripRequest::default()
        },
    )
    .await;
    assert_eq!(
        too_many.items[0].issue.as_ref().unwrap().details["suggested_quantity"],
        2
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn activity_rules_cutoff_min_age_and_seats(db: PgPool) {
    let guests = guest_ids(&db).await;
    let quad = quad_tour(&db, 3, 4).await;
    sqlx::query("UPDATE activities SET min_age = 16 WHERE product_id = $1")
        .bind(quad.product_id)
        .execute(&db)
        .await
        .unwrap();
    rate_plan(&db, "pay_on_site", None).await;
    let child = vec![
        GuestCount {
            guest_category_id: guests.adult,
            count: 1,
        },
        GuestCount {
            guest_category_id: guests.child,
            count: 1,
        },
    ];
    let code = |q: &trip::TripQuote| q.items[0].issue.as_ref().map(|i| i.code.clone());
    let q = quote(
        &db,
        &TripRequest {
            items: vec![activity(quad.product_id, quad.slot_id, child)],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(code(&q).as_deref(), Some("min_age"));
    let q = quote(
        &db,
        &TripRequest {
            items: vec![activity(quad.product_id, quad.slot_id, adults(&guests, 2))],
            child_ages: vec![12],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        code(&q).as_deref(),
        Some("min_age"),
        "a 12-year-old cannot ride"
    );
    let q = quote(
        &db,
        &TripRequest {
            items: vec![activity(quad.product_id, quad.slot_id, adults(&guests, 4))],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(code(&q).as_deref(), Some("not_enough_seats"));

    // 12-hour cutoff: the departure on 1 January at 20:00 cannot be booked at 10:00.
    let late = slot(
        &db,
        quad.product_id,
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2027, 1, 1, 20, 0, 0).unwrap(),
        8,
    )
    .await;
    let q = quote(
        &db,
        &TripRequest {
            items: vec![activity(quad.product_id, late, adults(&guests, 2))],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(code(&q).as_deref(), Some("too_late"));
}

async fn promotion(
    db: &PgPool,
    code: Option<&str>,
    columns: &str,
    values: &str,
    stackable: bool,
) -> Uuid {
    sqlx::query_scalar(&format!(
        "INSERT INTO promotions (code, is_stackable, {columns}) VALUES ($1, $2, {values}) RETURNING id"
    ))
    .bind(code)
    .bind(stackable)
    .fetch_one(db)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn promotions_apply_at_trip_level_with_stacking_and_clear_errors(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 5).await;
    let quad = quad_tour(&db, 8, 4).await;
    rate_plan(&db, "pay_on_site", None).await;
    let request = |code: Option<&str>| TripRequest {
        items: vec![
            stay(tent, d(2027, 1, 9), d(2027, 1, 12), adults(&guests, 2)),
            activity(quad.product_id, quad.slot_id, adults(&guests, 2)),
        ],
        promo_code: code.map(str::to_string),
        ..TripRequest::default()
    };
    // Subtotal: 90 € stay + 90 € quads.
    let q = quote(&db, &request(None)).await;
    assert_eq!(q.subtotal_minor, 18_000);
    assert_eq!(q.discount_minor, 0);

    // Fixed 15 € code: split 7.50 / 7.50 across the two items, exactly 15 € in total.
    promotion(
        &db,
        Some("FIX15"),
        "discount_type, discount_amount_minor, currency",
        "'fixed_amount', 1500, 'EUR'",
        false,
    )
    .await;
    let q = quote(&db, &request(Some("fix15"))).await;
    assert_eq!(q.promo.as_ref().unwrap().status, "applied");
    assert_eq!(q.discount_minor, 1_500);
    assert_eq!(
        q.items.iter().map(|i| i.discount_minor).collect::<Vec<_>>(),
        vec![750, 750]
    );
    assert_eq!(q.total_minor, 16_500);

    // Unknown code and unmet minimum are explained.
    let q = quote(&db, &request(Some("NOPE"))).await;
    assert_eq!(
        q.promo.as_ref().unwrap().reason.as_deref(),
        Some("unknown_code")
    );
    promotion(
        &db,
        Some("BIG"),
        "discount_type, discount_percent, currency, min_subtotal_minor",
        "'percent', 10, 'EUR', 50000",
        false,
    )
    .await;
    let q = quote(&db, &request(Some("BIG"))).await;
    assert_eq!(
        q.promo.as_ref().unwrap().reason.as_deref(),
        Some("subtotal_too_low")
    );
    assert_eq!(q.discount_minor, 0);

    // Automatic long-stay offer (3 nights, -10 %), not stackable: beats the 15 € code
    // (18 € > 15 €), and the code is reported as not combinable.
    let long_stay = promotion(
        &db,
        None,
        "discount_type, discount_percent, min_nights",
        "'percent', 10, 3",
        false,
    )
    .await;
    let q = quote(&db, &request(Some("FIX15"))).await;
    assert_eq!(q.discount_minor, 1_800);
    assert_eq!(q.discounts[0].promotion_id, long_stay);
    assert_eq!(q.promo.as_ref().unwrap().status, "not_combinable");

    // Free night on the stay only (the cheapest night).
    sqlx::query("UPDATE promotions SET is_active = false")
        .execute(&db)
        .await
        .unwrap();
    promotion(
        &db,
        Some("NIGHT"),
        "discount_type, discount_free_nights",
        "'free_nights', 1",
        false,
    )
    .await;
    let q = quote(&db, &request(Some("NIGHT"))).await;
    assert_eq!(q.discount_minor, 3_000);
    assert_eq!(q.items[1].discount_minor, 0);

    // Two stackable offers add up.
    sqlx::query("UPDATE promotions SET is_active = false")
        .execute(&db)
        .await
        .unwrap();
    promotion(
        &db,
        None,
        "discount_type, discount_percent",
        "'percent', 5",
        true,
    )
    .await;
    promotion(
        &db,
        Some("STACK"),
        "discount_type, discount_amount_minor, currency",
        "'fixed_amount', 1000, 'EUR'",
        true,
    )
    .await;
    let q = quote(&db, &request(Some("STACK"))).await;
    assert_eq!(q.discount_minor, 900 + 1_000);
    assert_eq!(q.promo.as_ref().unwrap().status, "applied");

    // Limits: one redemption per customer.
    sqlx::query("UPDATE promotions SET max_per_customer = 1 WHERE code = 'STACK'")
        .execute(&db)
        .await
        .unwrap();
    let total = q.total_minor;
    trip::create(
        &db,
        &checkout(
            request(Some("STACK")),
            "promo@x.ma",
            PaymentOption::OnSite,
            Some(total),
        ),
        &ctx(),
    )
    .await
    .unwrap();
    let err = trip::create(
        &db,
        &checkout(
            request(Some("STACK")),
            "promo@x.ma",
            PaymentOption::OnSite,
            Some(total),
        ),
        &ctx(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, BookingError::Invalid(ref m) if m.contains("already used")),
        "{err:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn tourist_tax_exempts_children_and_rounds_once_per_item(db: PgPool) {
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 2).await;
    rate_plan(&db, "pay_on_site", None).await;
    let tax: Uuid = sqlx::query_scalar(
        "INSERT INTO taxes (code, calc_type, amount_minor, currency, exempt_guest_category_ids)
         VALUES ('tourist_tax', 'fixed_per_person_per_night', 250, 'EUR', $1) RETURNING id",
    )
    .bind(vec![guests.child, guests.infant])
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO product_taxes (product_id, tax_id) VALUES ($1, $2)")
        .bind(tent)
        .bind(tax)
        .execute(&db)
        .await
        .unwrap();
    let family = vec![
        GuestCount {
            guest_category_id: guests.adult,
            count: 2,
        },
        GuestCount {
            guest_category_id: guests.child,
            count: 1,
        },
    ];
    let q = quote(
        &db,
        &TripRequest {
            items: vec![stay(tent, d(2027, 1, 9), d(2027, 1, 11), family)],
            ..Default::default()
        },
    )
    .await;
    // 2 adults × 2 nights × 2.50 €; the child is exempt.
    assert_eq!(q.tax_minor, 1_000);
    assert_eq!(q.total_minor, 6_000 + 1_000);

    // A 33.33 % code on 60 €: 19.998 € rounds once to 20.00 €.
    promotion(
        &db,
        Some("THIRD"),
        "discount_type, discount_percent",
        "'percent', 33.33",
        false,
    )
    .await;
    let q = quote(
        &db,
        &TripRequest {
            items: vec![stay(
                tent,
                d(2027, 1, 9),
                d(2027, 1, 11),
                adults(&guests, 2),
            )],
            promo_code: Some("THIRD".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(q.discount_minor, 2_000);
}
