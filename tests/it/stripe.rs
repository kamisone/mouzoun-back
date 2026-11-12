//! The Stripe gateway against a local mock of the Stripe API: what is sent (idempotency key on
//! every write, form fields, SCA-ready PaymentIntents) and how answers are read.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use mouzoun_api::{
    config::StripeConfig,
    db::types::CurrencyCode,
    services::payments::{
        PaymentGateway,
        gateway::{CreatePayment, GatewayError, OffSessionCharge, RefundRequest},
        stripe::{StripeGateway, parse_event},
    },
};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    idempotency_key: Option<String>,
    authorized: bool,
    form: String,
}

type Log = Arc<Mutex<Vec<Seen>>>;

async fn handle(
    State(log): State<Log>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<Value>) {
    log.lock().unwrap().push(Seen {
        path: path.clone(),
        idempotency_key: headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        authorized: headers
            .get("authorization")
            .is_some_and(|v| v.as_bytes().starts_with(b"Basic ")),
        form: body,
    });
    match path.as_str() {
        "v1/customers" => (StatusCode::OK, Json(json!({ "id": "cus_1" }))),
        "v1/payment_intents"
            if log
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.path == path)
                .count()
                == 3 =>
        {
            (
                // The off-session charge: the card needs the guest (3-D Secure).
                StatusCode::PAYMENT_REQUIRED,
                Json(
                    json!({ "error": { "type": "card_error", "code": "authentication_required",
                                    "message": "Your card requires authentication." } }),
                ),
            )
        }
        "v1/payment_intents" => (
            StatusCode::OK,
            Json(
                json!({ "id": "pi_1", "client_secret": "pi_1_secret_x", "status": "requires_payment_method" }),
            ),
        ),
        "v1/payment_intents/pi_paid/cancel" => (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": { "code": "payment_intent_unexpected_state", "message": "already succeeded" } }),
            ),
        ),
        "v1/payment_intents/pi_down/cancel" => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "message": "Stripe is having trouble" } })),
        ),
        p if p.ends_with("/cancel") => (
            StatusCode::OK,
            Json(json!({ "id": "pi_1", "status": "canceled" })),
        ),
        "v1/refunds" => (
            StatusCode::OK,
            Json(json!({ "id": "re_1", "status": "pending" })),
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "message": "down" } })),
        ),
    }
}

async fn mock() -> (StripeGateway, Log) {
    let log: Log = Arc::default();
    let app = Router::new()
        .route("/{*path}", post(handle))
        .with_state(log.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let gateway = StripeGateway::new(
        StripeConfig {
            secret_key: "sk_test_mock".into(),
            publishable_key: "pk_test_mock".into(),
            webhook_secret: "whsec_mock".into(),
            api_base: format!("http://{addr}"),
        },
        reqwest::Client::new(),
    );
    (gateway, log)
}

#[tokio::test]
async fn every_write_carries_an_idempotency_key_and_payments_are_sca_ready() {
    let (stripe, log) = mock().await;
    let eur = CurrencyCode::eur();
    let booking_id = Uuid::now_v7();

    // Deposit with consent to keep the card for the balance.
    let deposit = stripe
        .create_payment(&CreatePayment {
            amount_minor: 1_800,
            currency: &eur,
            idempotency_key: "pay-1",
            reference: "MZ-27-000001",
            description: "Mouzoun MZ-27-000001".into(),
            booking_id,
            payment_id: Uuid::now_v7(),
            email: Some("amina@example.com"),
            name: Some("Amina Test"),
            save_card: true,
            provider_customer_id: None,
        })
        .await
        .unwrap();
    assert_eq!(deposit.client_secret, "pi_1_secret_x");
    assert_eq!(deposit.provider_customer_id.as_deref(), Some("cus_1"));

    // Full payment, no card kept.
    stripe
        .create_payment(&CreatePayment {
            amount_minor: 6_000,
            currency: &eur,
            idempotency_key: "pay-2",
            reference: "MZ-27-000002",
            description: "Mouzoun MZ-27-000002".into(),
            booking_id,
            payment_id: Uuid::now_v7(),
            email: None,
            name: None,
            save_card: false,
            provider_customer_id: None,
        })
        .await
        .unwrap();

    // Balance charged off-session: Stripe asks for authentication → a decline the guest can fix.
    let balance = stripe
        .charge_off_session(&OffSessionCharge {
            amount_minor: 4_200,
            currency: &eur,
            idempotency_key: "balance-1",
            provider_customer_id: "cus_1",
            payment_method_id: "pm_1",
            reference: "MZ-27-000001",
            booking_id,
            payment_id: Uuid::now_v7(),
        })
        .await;
    assert!(
        matches!(balance, Err(GatewayError::Declined(ref m)) if m.contains("authentication")),
        "{balance:?}"
    );

    // Partial refund, then cancelling an open and an already paid intent.
    let refund = stripe
        .refund(&RefundRequest {
            provider_payment_id: "pi_1",
            amount_minor: 900,
            currency: &eur,
            idempotency_key: "refund-1",
        })
        .await
        .unwrap();
    assert_eq!(
        (refund.provider_refund_id.as_str(), refund.succeeded),
        ("re_1", false)
    );
    stripe.cancel_payment("pi_open", "cancel-1").await.unwrap();
    stripe.cancel_payment("pi_paid", "cancel-2").await.unwrap();

    let seen = log.lock().unwrap().clone();
    assert!(seen.iter().all(|s| s.authorized), "{seen:?}");
    assert!(
        seen.iter()
            .all(|s| s.idempotency_key.as_deref().is_some_and(|k| !k.is_empty())),
        "{seen:?}"
    );
    let intents: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.path == "v1/payment_intents")
        .collect();
    // Payment methods chosen by Stripe (cards, wallets) with 3-D Secure handled by Stripe.js.
    assert!(
        intents[0]
            .form
            .contains("automatic_payment_methods%5Benabled%5D=true"),
        "{}",
        intents[0].form
    );
    assert!(
        intents[0].form.contains("setup_future_usage=off_session"),
        "{}",
        intents[0].form
    );
    assert!(intents[0].form.contains("amount=1800") && intents[0].form.contains("currency=eur"));
    assert!(
        !intents[1].form.contains("setup_future_usage"),
        "{}",
        intents[1].form
    );
    assert!(
        intents[2].form.contains("off_session=true") && intents[2].form.contains("confirm=true")
    );
    let keys: Vec<&str> = seen
        .iter()
        .filter_map(|s| s.idempotency_key.as_deref())
        .collect();
    assert!(
        keys.contains(&"pay-1") && keys.contains(&"refund-1") && keys.contains(&"cancel-1"),
        "{keys:?}"
    );
    assert!(
        seen.iter()
            .any(|s| s.path == "v1/refunds" && s.form.contains("amount=900"))
    );
}

#[tokio::test]
async fn stripe_outages_are_retryable_not_declines() {
    let (stripe, _) = mock().await;
    // A 5xx is an outage: the hold job retries later instead of treating it as final.
    let err = stripe.cancel_payment("pi_down", "cancel-down").await;
    assert!(matches!(err, Err(GatewayError::Unavailable(_))), "{err:?}");
}

#[test]
fn webhook_events_map_to_outcomes() {
    use mouzoun_api::services::payments::WebhookOutcome;
    let event = |kind: &str, object: Value| {
        parse_event(
            serde_json::to_vec(&json!({ "id": "evt", "type": kind, "data": { "object": object } }))
                .unwrap()
                .as_slice(),
        )
        .unwrap()
        .outcome
    };
    assert_eq!(
        event("payment_intent.canceled", json!({ "id": "pi_1" })),
        WebhookOutcome::PaymentCanceled {
            provider_payment_id: "pi_1".into()
        }
    );
    assert_eq!(
        event(
            "charge.dispute.created",
            json!({ "id": "dp_1", "payment_intent": "pi_1", "amount": 500,
                                                "reason": "fraudulent", "evidence_details": { "due_by": 10 } })
        ),
        WebhookOutcome::DisputeOpened {
            provider_payment_id: "pi_1".into(),
            amount_minor: Some(500),
            reason: "fraudulent".into(),
            evidence_due_by: Some(10),
        }
    );
    assert_eq!(
        event(
            "payment_intent.succeeded",
            json!({ "id": "pi_2", "amount_received": 1800,
                                                  "setup_future_usage": "off_session", "payment_method": "pm_9" })
        ),
        WebhookOutcome::PaymentSucceeded {
            provider_payment_id: "pi_2".into(),
            payment_method_id: Some("pm_9".into()),
            amount_minor: Some(1800),
        }
    );
    assert_eq!(
        event("customer.created", json!({ "id": "cus" })),
        WebhookOutcome::Ignored
    );
}
