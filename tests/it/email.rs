//! Transactional emails end to end, against a real PostgreSQL: events → handlers → templates →
//! sender, with idempotency, retries, locales and scheduling.

use std::{sync::Arc, time::Duration as StdDuration};

use crate::common::{JWT_SECRET, fixtures::*, test_config};
use chrono::{Duration, Utc};
use mouzoun_api::{
    db::{bookings::CancelledBy, types::Locale},
    services::{
        booking::{self, AccommodationRequest, CustomerRef},
        email::{
            EmailSettings, Mailer, admin, handler::EmailHandler, samples::TEMPLATE_CODES,
            sender::RecordingSender, templates,
        },
        outbox::{self, Event},
    },
    worker::{self, Registry, process_batch, scheduler},
};
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SITE: &str = "https://mouzoun.test";

fn mailer(db: &PgPool, sender: &Arc<RecordingSender>) -> Mailer {
    Mailer::new(
        db.clone(),
        sender.clone(),
        EmailSettings {
            public_site_url: SITE.into(),
            media_base_url: "https://cdn.test".into(),
            link_secret: JWT_SECRET.into(),
            reply_to: None,
        },
    )
}

fn registry(db: &PgPool, mailer: &Mailer) -> Registry {
    Registry::new().register(Arc::new(EmailHandler::new(db.clone(), mailer.clone())))
}

/// A booking (pending) for `email` in `locale`; its `booking.created` event is marked done.
async fn booked(db: &PgPool, locale: &str, email: &str, preferred: Option<&str>) -> Uuid {
    let guests = guest_ids(db).await;
    let tent = accommodation(db, "unit", 2).await;
    let mut meta = meta(email);
    meta.locale = Locale::new(locale);
    if let CustomerRef::New(customer) = &mut meta.customer {
        customer.preferred_locale = preferred.map(Locale::new);
    }
    let created = booking::book_accommodation(
        db,
        &AccommodationRequest {
            product_id: tent,
            arrival: d(2027, 1, 10),
            departure: d(2027, 1, 12),
            quantity: 1,
            guests: adults(&guests, 2),
            meta,
        },
        now(),
        today(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE outbox_events SET processed_at = now() WHERE event_type = 'booking.created'",
    )
    .execute(db)
    .await
    .unwrap();
    created.booking_id
}

async fn event(db: &PgPool, event_type: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT id FROM outbox_events WHERE event_type = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(event_type)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn drain(db: &PgPool, registry: &Registry) {
    while process_batch(db, registry, 20, 8, now())
        .await
        .unwrap()
        .claimed
        > 0
    {}
}

#[sqlx::test(migrations = "./migrations")]
async fn confirming_a_booking_emails_the_guest_in_their_language_within_two_seconds(db: PgPool) {
    let booking_id = booked(&db, "ar", "amina@example.com", None).await;
    let sender = Arc::new(RecordingSender::default());
    let mut config = test_config().worker;
    config.poll_interval = StdDuration::from_secs(60); // only NOTIFY can wake it in time
    let shutdown = CancellationToken::new();
    let tasks = worker::spawn(
        db.clone(),
        registry(&db, &mailer(&db, &sender)),
        config,
        shutdown.clone(),
    );
    // No wait for LISTEN: the dispatcher listens first, then drains what is already due.
    let started = std::time::Instant::now();
    booking::confirm_booking(&db, booking_id, None, now())
        .await
        .unwrap();
    sender.wait_for(1, StdDuration::from_secs(2)).await;
    let elapsed = started.elapsed();
    shutdown.cancel();
    for t in tasks {
        t.await.unwrap();
    }

    let sent = sender.sent();
    assert_eq!(sent.len(), 1, "sent within {elapsed:?}");
    assert!(elapsed < StdDuration::from_secs(2), "{elapsed:?}");
    let email = &sent[0];
    assert_eq!(email.to_email, "amina@example.com");
    assert!(
        email.subject.starts_with("تم تأكيد إقامتك"),
        "{}",
        email.subject
    );
    assert!(email.html.contains(r#"dir="rtl""#) && email.html.contains(r#"lang="ar""#));
    assert!(email.text.contains("MZ-"), "text version present");
    let status: String = sqlx::query_scalar("SELECT status FROM email_log WHERE booking_id = $1")
        .bind(booking_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(status, "sent");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_sender_failing_twice_retries_with_backoff_and_sends_once(db: PgPool) {
    let booking_id = booked(&db, "fr", "retry@example.com", None).await;
    booking::confirm_booking(&db, booking_id, None, now())
        .await
        .unwrap();
    let id = event(&db, "booking.confirmed").await;
    let sender = Arc::new(RecordingSender::failing(2));
    let registry = registry(&db, &mailer(&db, &sender));

    let mut t = now();
    for (attempt, nominal) in [(1, 30.0), (2, 120.0)] {
        assert_eq!(
            process_batch(&db, &registry, 10, 8, t)
                .await
                .unwrap()
                .retried,
            1
        );
        let (attempts, available_at): (i32, chrono::DateTime<Utc>) =
            sqlx::query_as("SELECT attempts, available_at FROM outbox_events WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(attempts, attempt);
        let delay = (available_at - t).num_milliseconds() as f64 / 1000.0;
        assert!(
            (nominal * 0.9 - 0.01..=nominal * 1.1 + 0.01).contains(&delay),
            "delay {delay}"
        );
        t = available_at;
    }
    assert_eq!(
        process_batch(&db, &registry, 10, 8, t)
            .await
            .unwrap()
            .processed,
        1
    );

    let (attempts, processed): (i32, bool) = sqlx::query_as(
        "SELECT attempts, processed_at IS NOT NULL FROM outbox_events WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((attempts, processed), (2, true));
    assert_eq!(sender.attempts(), 3);
    assert_eq!(sender.sent().len(), 1, "exactly one email");
    let (status, log_attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM email_log WHERE outbox_event_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((status.as_str(), log_attempts), ("sent", 3));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_retried_event_never_emails_twice(db: PgPool) {
    let booking_id = booked(&db, "fr", "crash@example.com", None).await;
    booking::confirm_booking(&db, booking_id, None, now())
        .await
        .unwrap();
    let id = event(&db, "booking.confirmed").await;
    let sender = Arc::new(RecordingSender::default());
    let registry = registry(&db, &mailer(&db, &sender));

    // Crash after the provider accepted the email, before the event was committed: the log says
    // `sent`, the event is still pending.
    sqlx::query(
        "INSERT INTO email_log (template_code, to_email, booking_id, outbox_event_id, status, sent_at)
         VALUES ('booking_confirmation', 'crash@example.com', $1, $2, 'sent', now())",
    )
    .bind(booking_id)
    .bind(id)
    .execute(&db)
    .await
    .unwrap();
    drain(&db, &registry).await;
    assert!(sender.sent().is_empty(), "already sent: not sent again");

    // Crash *during* the send (status `sending`): delivery unknown, still not resent.
    let other = booked(&db, "fr", "unsure@example.com", None).await;
    booking::confirm_booking(&db, other, None, now())
        .await
        .unwrap();
    let id = event(&db, "booking.confirmed").await;
    sqlx::query(
        "INSERT INTO email_log (template_code, to_email, booking_id, outbox_event_id, status)
         VALUES ('booking_confirmation', 'unsure@example.com', $1, $2, 'sending')",
    )
    .bind(other)
    .bind(id)
    .execute(&db)
    .await
    .unwrap();
    drain(&db, &registry).await;
    assert!(
        sender.sent().is_empty(),
        "uncertain delivery is never retried automatically"
    );
    let (status, error): (String, Option<String>) =
        sqlx::query_as("SELECT status, error FROM email_log WHERE outbox_event_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(status, "sending");
    assert!(error.unwrap().contains("delivery unknown"));
}

#[sqlx::test(migrations = "./migrations")]
async fn every_seeded_template_renders_in_french_english_and_arabic(db: PgPool) {
    let sender = Arc::new(RecordingSender::default());
    let mailer = mailer(&db, &sender);
    for code in TEMPLATE_CODES {
        for locale in ["fr", "en", "ar"] {
            let out = admin::render_preview(&mailer, code, locale, None)
                .await
                .unwrap_or_else(|e| panic!("{code} [{locale}]: {e}"));
            assert!(!out.subject.is_empty(), "{code} [{locale}]");
            assert!(
                out.html.contains(&format!(r#"lang="{locale}""#)),
                "{code} [{locale}]"
            );
            assert_eq!(
                out.html.contains(r#"dir="rtl""#),
                locale == "ar",
                "{code} [{locale}]"
            );
            for rendered in [&out.subject, &out.html, &out.text] {
                assert!(
                    !rendered.contains("{{") && !rendered.contains("{%"),
                    "{code} [{locale}] left a tag"
                );
            }
            assert!(
                out.text.len() > 80,
                "{code} [{locale}]: text version generated"
            );
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn locales_follow_the_booking_then_customer_then_fall_back(db: PgPool) {
    let sender = Arc::new(RecordingSender::default());
    let registry = registry(&db, &mailer(&db, &sender));

    // English deactivated: booking.locale (en) is skipped, the customer's preference (ar) wins.
    sqlx::query("UPDATE locales SET is_active = false WHERE code = 'en'")
        .execute(&db)
        .await
        .unwrap();
    let b = booked(&db, "en", "pref@example.com", Some("ar")).await;
    booking::confirm_booking(&db, b, None, now()).await.unwrap();
    drain(&db, &registry).await;
    assert!(sender.sent()[0].html.contains(r#"lang="ar""#));

    // The Arabic translation is missing: the default locale (fr) is used.
    sqlx::query(
        "DELETE FROM email_template_translations WHERE locale = 'ar' AND email_template_id =
           (SELECT id FROM email_templates WHERE code = 'booking_confirmation')",
    )
    .execute(&db)
    .await
    .unwrap();
    let b = booked(&db, "ar", "fallback@example.com", None).await;
    booking::confirm_booking(&db, b, None, now()).await.unwrap();
    drain(&db, &registry).await;
    let email = &sender.sent()[1];
    assert!(
        email.html.contains(r#"lang="fr""#)
            && email.subject.starts_with("Votre séjour est confirmé")
    );
    let locale: String =
        sqlx::query_scalar("SELECT locale FROM email_log WHERE to_email = 'fallback@example.com'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(locale, "fr");
}

#[sqlx::test(migrations = "./migrations")]
async fn templates_with_unknown_variables_are_rejected_on_save_and_at_send_time(db: PgPool) {
    let err = templates::save_translation(
        &db,
        "booking_confirmation",
        "fr",
        "Hi",
        "<p>{{ guest.password }} {{ card_number }}</p>",
        "",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("card_number"), "{err}");
    templates::save_translation(
        &db,
        "booking_confirmation",
        "fr",
        "Hi {{ guest.first_name }}",
        "<p>{{ booking.reference }}</p>",
        "",
    )
    .await
    .unwrap();

    // A template broken behind the validator's back: the event dies at once, nothing is sent.
    sqlx::query(
        "UPDATE email_template_translations SET body_html = '<p>{{ card_number }}</p>'
         WHERE email_template_id = (SELECT id FROM email_templates WHERE code = 'booking_confirmation')",
    )
    .execute(&db)
    .await
    .unwrap();
    let b = booked(&db, "fr", "broken@example.com", None).await;
    booking::confirm_booking(&db, b, None, now()).await.unwrap();
    let sender = Arc::new(RecordingSender::default());
    let report = process_batch(&db, &registry(&db, &mailer(&db, &sender)), 10, 8, now())
        .await
        .unwrap();
    assert_eq!(report.dead, 1, "render errors are permanent, not retried");
    assert!(sender.sent().is_empty());
    let (status, error): (String, String) =
        sqlx::query_as("SELECT status, error FROM email_log WHERE to_email = 'broken@example.com'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert!(error.contains("card_number"), "{error}");
}

#[sqlx::test(migrations = "./migrations")]
async fn new_bookings_notify_guest_and_staff_each_in_their_language(db: PgPool) {
    sqlx::query("UPDATE settings SET value = '\"ops@mouzoun.test\"' WHERE key = 'notifications.staff_email'")
        .execute(&db)
        .await
        .unwrap();
    booked(&db, "en", "guest@example.com", None).await;
    sqlx::query(
        "UPDATE outbox_events SET processed_at = NULL WHERE event_type = 'booking.created'",
    )
    .execute(&db)
    .await
    .unwrap();
    let sender = Arc::new(RecordingSender::default());
    drain(&db, &registry(&db, &mailer(&db, &sender))).await;

    let sent = sender.sent();
    assert_eq!(sent.len(), 2);
    let guest = sent
        .iter()
        .find(|e| e.to_email == "guest@example.com")
        .unwrap();
    assert!(
        guest
            .subject
            .starts_with("We have received your booking request"),
        "{}",
        guest.subject
    );
    let staff = sent
        .iter()
        .find(|e| e.to_email == "ops@mouzoun.test")
        .unwrap();
    assert!(
        staff.subject.starts_with("Nouvelle réservation"),
        "staff emails use the default locale"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_holds_emit_booking_expired_and_tell_the_guest(db: PgPool) {
    let b = booked(&db, "fr", "late@example.com", None).await;
    let expired = booking::expire_holds(&db, now() + Duration::hours(1), 10)
        .await
        .unwrap();
    assert_eq!(expired, 1);
    event(&db, "booking.expired").await;
    let sender = Arc::new(RecordingSender::default());
    drain(&db, &registry(&db, &mailer(&db, &sender))).await;
    let sent = sender.sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].subject.contains("a expiré"), "{}", sent[0].subject);

    // A guest cancellation is a different email.
    let b2 = booked(&db, "fr", "cancel@example.com", None).await;
    let cancellation = booking::Cancellation {
        by: CancelledBy::Customer,
        reason: None,
        changed_by: None,
    };
    booking::cancel_booking(&db, b2, &cancellation, now())
        .await
        .unwrap();
    drain(&db, &registry(&db, &mailer(&db, &sender))).await;
    assert!(
        sender.sent()[1]
            .subject
            .starts_with("Annulation de votre réservation")
    );
    let _ = b;
}

#[sqlx::test(migrations = "./migrations")]
async fn pre_arrival_emails_are_planned_once_and_sent_on_time(db: PgPool) {
    let b = booked(&db, "fr", "soon@example.com", None).await;
    booking::confirm_booking(&db, b, None, now()).await.unwrap();
    sqlx::query(
        "UPDATE outbox_events SET processed_at = now() WHERE event_type = 'booking.confirmed'",
    )
    .execute(&db)
    .await
    .unwrap();
    // Only the pre-arrival email here (the pre-check-in reminder has its own test).
    sqlx::query("UPDATE bookings SET precheckin_completed_at = now() WHERE id = $1")
        .bind(b)
        .execute(&db)
        .await
        .unwrap();

    let planned = scheduler::plan(&db, now()).await.unwrap();
    assert_eq!(planned.pre_arrival, 1);
    assert_eq!(planned.precheckin_reminder, 0);
    assert_eq!(
        scheduler::plan(&db, now()).await.unwrap().pre_arrival,
        0,
        "planning is idempotent"
    );
    let (count, available_at): (i64, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT count(*), max(available_at) FROM outbox_events WHERE event_type = 'booking.pre_arrival_due'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(count, 1);
    // Arrival 10 Jan, 3 days before at 09:00 Casablanca (UTC+0 since September 2026).
    assert_eq!(
        available_at,
        "2027-01-07T09:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap()
    );

    let sender = Arc::new(RecordingSender::default());
    let registry = registry(&db, &mailer(&db, &sender));
    assert_eq!(
        process_batch(&db, &registry, 10, 8, now())
            .await
            .unwrap()
            .claimed,
        0,
        "not before it is due"
    );
    assert_eq!(
        process_batch(&db, &registry, 10, 8, available_at)
            .await
            .unwrap()
            .processed,
        1
    );
    let sent = sender.sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].subject.contains("M'ssici approche"));
    assert!(sent[0].text.contains("Rissani"));

    // Re-planning after it was sent never creates a second one.
    scheduler::plan(&db, now()).await.unwrap();
    assert!(
        process_batch(&db, &registry, 10, 8, available_at + Duration::days(1))
            .await
            .unwrap()
            .claimed
            == 0
    );
    assert_eq!(sender.sent().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn account_emails_carry_the_one_time_token_which_is_then_scrubbed(db: PgPool) {
    let user_id =
        crate::common::create_user(&db, "staff@mouzoun.test", "x", true, Some("manager")).await;
    let token_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth_tokens (id, user_id, purpose, token_hash, expires_at)
         VALUES ($1, $2, 'password_reset', '\\x01', now() + interval '1 hour')",
    )
    .bind(token_id)
    .bind(user_id)
    .execute(&db)
    .await
    .unwrap();
    let mut conn = db.acquire().await.unwrap();
    let id = outbox::enqueue(
        &mut conn,
        &Event::PasswordResetRequested {
            user_id,
            auth_token_id: token_id,
            token: "raw-token-123".into(),
        },
    )
    .await
    .unwrap();
    drop(conn);

    let sender = Arc::new(RecordingSender::default());
    drain(&db, &registry(&db, &mailer(&db, &sender))).await;
    let sent = sender.sent();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0]
            .text
            .contains(&format!("{SITE}/admin/reset-password?token=raw-token-123")),
        "{}",
        sent[0].text
    );
    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM outbox_events WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        payload,
        json!({ "user_id": user_id, "auth_token_id": token_id })
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn inquiries_get_an_auto_reply_and_notify_the_team(db: PgPool) {
    sqlx::query("UPDATE settings SET value = '\"ops@mouzoun.test\"' WHERE key = 'notifications.staff_email'")
        .execute(&db)
        .await
        .unwrap();
    let inquiry_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO inquiries (id, kind, name, email, locale, subject, message, group_size)
         VALUES ($1, 'group', 'Lucas', 'lucas@example.com', 'en', 'Group of 12', 'Hello <b>there</b>', 12)",
    )
    .bind(inquiry_id)
    .execute(&db)
    .await
    .unwrap();
    let mut conn = db.acquire().await.unwrap();
    outbox::enqueue(&mut conn, &Event::InquiryCreated { inquiry_id })
        .await
        .unwrap();
    drop(conn);

    let sender = Arc::new(RecordingSender::default());
    drain(&db, &registry(&db, &mailer(&db, &sender))).await;
    let sent = sender.sent();
    let reply = sent
        .iter()
        .find(|e| e.to_email == "lucas@example.com")
        .unwrap();
    assert_eq!(reply.subject, "We have received your message");
    assert!(
        reply.html.contains("Hello &lt;b&gt;there"),
        "guest input is escaped"
    );
    assert!(!reply.html.contains("<b>there"), "never injected as markup");
    let staff = sent
        .iter()
        .find(|e| e.to_email == "ops@mouzoun.test")
        .unwrap();
    assert!(staff.subject.starts_with("Nouvelle demande"));
}

#[sqlx::test(migrations = "./migrations")]
async fn precheckin_reminders_are_planned_until_the_register_is_complete(db: PgPool) {
    let b = booked(&db, "en", "register@example.com", None).await;
    booking::confirm_booking(&db, b, None, now()).await.unwrap();
    sqlx::query("UPDATE outbox_events SET processed_at = now()")
        .execute(&db)
        .await
        .unwrap();
    let planned = scheduler::plan(&db, now()).await.unwrap();
    assert_eq!(planned.precheckin_reminder, 1);
    let available_at: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT available_at FROM outbox_events WHERE event_type = 'booking.precheckin_due'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    // Arrival 10 January, 5 days before at 10:00 camp time.
    assert_eq!(
        available_at,
        "2027-01-05T10:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap()
    );

    // Completed meanwhile: the reminder is skipped when it becomes due.
    sqlx::query("UPDATE bookings SET precheckin_completed_at = now() WHERE id = $1")
        .bind(b)
        .execute(&db)
        .await
        .unwrap();
    let sender = Arc::new(RecordingSender::default());
    let registry = registry(&db, &mailer(&db, &sender));
    process_batch(&db, &registry, 10, 8, available_at)
        .await
        .unwrap();
    assert!(
        sender
            .sent()
            .iter()
            .all(|e| !e.subject.contains("register"))
    );
}
