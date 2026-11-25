//! Payment provider webhooks (the source of truth for payment outcomes) and, in development
//! only, the fake provider's "simulate payment" endpoint.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    services::payments::{self, fake, stripe},
    state::AppState,
};

/// `POST /webhooks/stripe` (the URL given to Stripe): same as `/payments/webhooks/stripe`.
pub async fn receive_stripe(
    state: State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<Value>)> {
    receive(state, Path("stripe".to_string()), headers, body).await
}

pub async fn receive(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<Value>)> {
    let (name, gateway) = state.payments.get(&provider).ok_or(AppError::NotFound)?;
    let event = gateway.verify_webhook(&headers, &body).await.map_err(|e| {
        tracing::warn!(provider = name, error = %e, "webhook refused");
        AppError::BadRequest("invalid webhook".into())
    })?;
    // The Stripe account is shared with another site: its events (no `metadata.app` of ours on
    // the object or its payment intent) are kept as `ignored_foreign` and never processed.
    if name == "stripe"
        && let Some(config) = state.config.booking.stripe.as_ref()
    {
        let tag = stripe::event_app_tag(&event.payload, |id| async move {
            stripe::fetch_intent(config, &id).await
        })
        .await;
        if tag.as_deref() != Some(config.app_tag.as_str())
            && !known_ids(&state, &event.payload).await?
        {
            ignore_foreign(&state, &event).await?;
            return Ok((
                StatusCode::OK,
                Json(json!({ "received": true, "ignored": "foreign" })),
            ));
        }
    }
    // One Stripe endpoint for both: shop order events are stored, queued and answered at once.
    if name == "stripe"
        && crate::modules::shop::public::receive_stripe_shop_event(&state, &event.payload).await?
    {
        return Ok((StatusCode::OK, Json(json!({ "received": true }))));
    }
    // Processing errors answer 500: the provider retries, and the stored event is reprocessed.
    payments::handle_webhook(&state.db, &state.payments, name, &event, Utc::now()).await?;
    Ok((StatusCode::OK, Json(json!({ "received": true }))))
}

/// Objects created before the tag existed: ours when their metadata names one of our payments,
/// bookings or orders (UUIDs: another site cannot collide).
async fn known_ids(state: &AppState, payload: &Value) -> AppResult<bool> {
    let m = &payload["data"]["object"]["metadata"];
    let id = |k: &str| m[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
    let (payment, booking, order) = (id("payment_id"), id("booking_id"), id("order_id"));
    if payment.is_none() && booking.is_none() && order.is_none() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM payments WHERE id = $1)
               OR EXISTS (SELECT 1 FROM bookings WHERE id = $2)
               OR EXISTS (SELECT 1 FROM orders WHERE id = $3) AS "x!""#,
        payment,
        booking,
        order
    )
    .fetch_one(&state.db)
    .await?)
}

/// Keeps a trace of an event of another site (id and type only: never its customer data).
async fn ignore_foreign(
    state: &AppState,
    event: &payments::gateway::WebhookEvent,
) -> AppResult<()> {
    let object = &event.payload["data"]["object"];
    sqlx::query!(
        "INSERT INTO payment_webhook_events (provider, event_id, event_type, payload, status, processed_at)
         VALUES ('stripe', $1, $2, $3, 'ignored_foreign', now()) ON CONFLICT (provider, event_id) DO NOTHING",
        event.event_id,
        event.event_type,
        json!({ "object": object["object"], "object_id": object["id"] })
    )
    .execute(&state.db)
    .await?;
    tracing::debug!(event_id = %event.event_id, "stripe event of another site ignored");
    Ok(())
}

/// `PAYMENTS_FAKE=true` only: plays the provider's part for a fake payment
/// (`outcome` = `succeed` | `fail`), through the same webhook processing.
pub async fn fake_outcome(
    State(state): State<AppState>,
    Path((payment_id, outcome)): Path<(Uuid, String)>,
) -> AppResult<Json<Value>> {
    if state.payments.get("fake").is_none() {
        return Err(AppError::NotFound);
    }
    let payment = sqlx::query!(
        "SELECT provider_payment_id, amount_minor,
                (SELECT balance_collection FROM bookings WHERE id = booking_id) AS balance_collection
         FROM payments WHERE id = $1",
        payment_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let provider_payment_id = payment
        .provider_payment_id
        .filter(|id| id.starts_with("fake_"))
        .ok_or(AppError::NotFound)?;
    let raw = match outcome.as_str() {
        "succeed" => fake::succeeded_event(
            &provider_payment_id,
            payment.amount_minor,
            payment.balance_collection.as_deref() == Some("saved_card"),
        ),
        "fail" => fake::failed_event(&provider_payment_id),
        _ => return Err(AppError::BadRequest("succeed or fail".into())),
    };
    let body = serde_json::to_vec(&raw).unwrap_or_default();
    let event = stripe::parse_event(&body).map_err(|e| AppError::BadRequest(e.to_string()))?;
    payments::handle_webhook(&state.db, &state.payments, "fake", &event, Utc::now()).await?;
    Ok(Json(json!({ "ok": true })))
}
