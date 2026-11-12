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
    // Processing errors answer 500: the provider retries, and the stored event is reprocessed.
    payments::handle_webhook(&state.db, &state.payments, name, &event, Utc::now()).await?;
    Ok((StatusCode::OK, Json(json!({ "received": true }))))
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
