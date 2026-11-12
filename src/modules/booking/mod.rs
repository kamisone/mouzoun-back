//! Public booking API: catalogue reads, availability, quotes, basket, checkout, payments
//! (webhooks) and the guest's own booking pages. See docs/booking/README.md.

pub mod availability;
pub mod cart;
pub mod catalog;
pub mod checkout;
pub mod manage;
pub mod webhooks;

use axum::{
    Json, Router,
    extract::State,
    routing::{get, patch, post},
};
use chrono::Utc;
use serde_json::{Value, json};

use crate::{
    db::types::Locale,
    error::AppResult,
    services::booking::{
        settings::{self, BookingSettings},
        trip::TripContext,
    },
    state::AppState,
};

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/booking/config", get(config))
        .route("/catalog/products", get(catalog::list))
        .route("/catalog/products/{slug}", get(catalog::detail))
        .route("/catalog/guest-categories", get(catalog::guest_categories))
        .route("/availability/calendar", get(availability::calendar))
        .route("/availability/search", post(availability::search))
        .route("/availability/slots", get(availability::slots))
        .route("/quote", post(checkout::quote))
        .route(
            "/cart",
            get(cart::get).put(cart::update).delete(cart::delete),
        )
        .route("/cart/items", post(cart::add))
        .route(
            "/cart/items/{id}",
            patch(cart::update_item).delete(cart::remove_item),
        )
        .route("/checkout", post(checkout::checkout))
        .route("/bookings/find", post(manage::find))
        .route(
            "/bookings/{reference}",
            get(manage::get).patch(manage::update),
        )
        .route("/bookings/{reference}/access", post(manage::redeem))
        .route("/bookings/{reference}/status", get(manage::status))
        .route("/bookings/{reference}/pay", post(manage::pay))
        .route(
            "/bookings/{reference}/cancellation",
            get(manage::cancellation_preview),
        )
        .route("/bookings/{reference}/cancel", post(manage::cancel))
        .route(
            "/bookings/{reference}/guests",
            get(manage::guests).put(manage::save_guests),
        )
        .route("/bookings/{reference}/calendar.ics", get(manage::ics))
        .route("/payments/webhooks/{provider}", post(webhooks::receive))
        .route(
            "/payments/fake/{payment_id}/{outcome}",
            post(webhooks::fake_outcome),
        )
}

pub(crate) async fn booking_settings(state: &AppState) -> AppResult<BookingSettings> {
    if let Some(cached) = state
        .cache
        .get_json::<BookingSettings>("settings:booking")
        .await
    {
        return Ok(cached);
    }
    let loaded = settings::load(&state.db).await?;
    state
        .cache
        .set_json(
            "settings:booking",
            &loaded,
            std::time::Duration::from_secs(30),
        )
        .await;
    Ok(loaded)
}

pub(crate) fn trip_context(locale: Locale, settings: BookingSettings) -> TripContext {
    TripContext {
        locale,
        public: true,
        now: Utc::now(),
        settings,
    }
}

/// What the funnel needs to know before showing anything bookable.
pub async fn config(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let s = booking_settings(&state).await?;
    let currency = crate::db::foundation::default_currency(&state.db).await?;
    Ok(Json(json!({
        "enabled": s.enabled,
        "site_live": s.site_live,
        "currency": currency.code,
        "hold_minutes": s.hold_minutes,
        "request_answer_hours": s.request_answer_hours,
        "balance_due_days": s.balance_due_days,
        "balance_auto_charge": s.balance_auto_charge,
        "max_days_ahead": s.max_days_ahead,
        "standalone_activities": s.standalone_activities,
        "calendar_show_prices": s.calendar_show_prices,
        "urgency_threshold": s.urgency_threshold,
        "self_service_changes": s.self_service_changes,
        "guest_cancellation": s.guest_cancellation,
        "pitch_selection": s.pitch_selection,
        "terms_version": s.terms_version,
        "payments": state.payments.public_config(),
        "providers": state.payments.names(),
        "captcha": state.config.booking.turnstile_secret.is_some(),
    })))
}
