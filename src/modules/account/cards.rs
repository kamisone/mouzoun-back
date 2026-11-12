//! "Payment methods": cards kept by the payment provider (never by us). A card is added with a
//! Stripe SetupIntent confirmed in the browser, or saved at checkout with consent.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use crate::{
    auth::GuestUser,
    error::{AppError, AppResult},
    services::{
        accounts::signin,
        audit::{self, Actor},
        payments::gateway::{GatewayError, PaymentGateway},
    },
    state::AppState,
    web::client::ClientInfo,
};

fn stripe(state: &AppState) -> AppResult<std::sync::Arc<dyn PaymentGateway>> {
    // The card provider is the default one (Stripe, or the fake one in development).
    state
        .payments
        .default_provider()
        .map(|(_, g)| g)
        .ok_or_else(|| {
            AppError::domain(
                StatusCode::SERVICE_UNAVAILABLE,
                "payments_disabled",
                "payments are not configured",
            )
        })
}

fn gateway_error(e: GatewayError) -> AppError {
    match e {
        GatewayError::Declined(_) => AppError::NotFound,
        GatewayError::Unsupported => AppError::domain(
            StatusCode::SERVICE_UNAVAILABLE,
            "payments_disabled",
            "cards cannot be saved here",
        ),
        other => AppError::domain(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            other.to_string(),
        ),
    }
}

pub async fn list(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<Value>> {
    let customer = signin::customer_of(&state.db, guest.user_id).await?;
    let provider_id = sqlx::query_scalar!(
        "SELECT stripe_customer_id FROM customers WHERE id = $1",
        customer
    )
    .fetch_one(&state.db)
    .await?;
    let cards = match provider_id {
        Some(id) => stripe(&state)?
            .list_cards(&id)
            .await
            .map_err(gateway_error)?,
        None => vec![],
    };
    // Cards a deposit booking will charge for its balance cannot be removed before it is paid.
    let in_use = sqlx::query_scalar!(
        r#"SELECT DISTINCT saved_payment_method_id AS "id!" FROM bookings
           WHERE customer_id = $1 AND saved_payment_method_id IS NOT NULL AND balance_collection = 'saved_card'
             AND status IN ('confirmed', 'awaiting_payment') AND total_minor > amount_paid_minor"#,
        customer
    )
    .fetch_all(&state.db)
    .await?;
    let cards: Vec<Value> = cards
        .into_iter()
        .map(|c| {
            json!({ "id": c.id, "brand": c.brand, "last4": c.last4, "exp_month": c.exp_month,
                         "exp_year": c.exp_year, "in_use": in_use.contains(&c.id) })
        })
        .collect();
    Ok(Json(
        json!({ "cards": cards, "provider": state.payments.public_config() }),
    ))
}

pub async fn setup(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<Value>> {
    let customer = signin::customer_of(&state.db, guest.user_id).await?;
    let c = sqlx::query!(
        r#"SELECT stripe_customer_id, email::text AS "email!", btrim(first_name || ' ' || COALESCE(last_name, '')) AS "name!"
           FROM customers WHERE id = $1"#,
        customer
    )
    .fetch_one(&state.db)
    .await?;
    let setup = stripe(&state)?
        .setup_card(
            c.stripe_customer_id.as_deref(),
            &c.email,
            &c.name,
            &format!("setup-{}", uuid::Uuid::now_v7().simple()),
        )
        .await
        .map_err(gateway_error)?;
    if c.stripe_customer_id.is_none() {
        sqlx::query!(
            "UPDATE customers SET stripe_customer_id = $2 WHERE id = $1",
            customer,
            setup.provider_customer_id
        )
        .execute(&state.db)
        .await?;
    }
    Ok(Json(
        json!({ "client_secret": setup.client_secret, "provider": state.payments.public_config() }),
    ))
}

pub async fn remove(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Path(card): Path<String>,
) -> AppResult<StatusCode> {
    let customer = signin::customer_of(&state.db, guest.user_id).await?;
    let provider_id = sqlx::query_scalar!(
        "SELECT stripe_customer_id FROM customers WHERE id = $1",
        customer
    )
    .fetch_one(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let used = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM bookings WHERE customer_id = $1 AND saved_payment_method_id = $2
             AND balance_collection = 'saved_card' AND status IN ('confirmed', 'awaiting_payment') AND total_minor > amount_paid_minor) AS "u!""#,
        customer,
        card
    )
    .fetch_one(&state.db)
    .await?;
    if used {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "card_in_use",
            "this card pays the balance of an upcoming stay",
        ));
    }
    stripe(&state)?
        .remove_card(&provider_id, &card)
        .await
        .map_err(gateway_error)?;
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "account.card_removed",
        "user",
        &guest.user_id.to_string(),
        json!({}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
