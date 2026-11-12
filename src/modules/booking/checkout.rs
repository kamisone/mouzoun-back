//! Quote and checkout. Checkout = validation → bot check → hold (booking + inventory in one
//! transaction, price re-checked against the signed quote) → payment intent when something is
//! due now. Protected by an `Idempotency-Key` (a double click never books twice) and rate limits.

use std::time::Duration as StdDuration;

use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use axum_extra::extract::cookie::CookieJar;
use chrono::NaiveTime;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{booking_settings, cart, trip_context};
use crate::{
    auth::GuestUser,
    db::bookings::{BookingSource, BookingStatus, PaymentOption, PaymentPurpose},
    error::{AppError, AppResult},
    services::{
        audit::Actor,
        booking::{
            CustomerInput, CustomerRef, manage, quote_token,
            trip::{self, CheckoutInput, TripItem, TripRequest},
        },
        payments, turnstile,
    },
    state::AppState,
    web::{
        client::{ClientInfo, RequestLocale},
        limits::{idempotency_key, idempotent, rate_limit},
    },
};

#[derive(Debug, Deserialize)]
pub struct QuoteBody {
    pub items: Vec<TripItem>,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub promo_code: Option<String>,
    #[serde(default)]
    pub child_ages: Vec<u8>,
}

pub async fn quote(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    Json(body): Json<QuoteBody>,
) -> AppResult<Json<Value>> {
    rate_limit(
        &state.cache,
        "quote",
        &client.key(),
        120,
        StdDuration::from_secs(60),
    )
    .await?;
    if body
        .promo_code
        .as_deref()
        .is_some_and(|c| !c.trim().is_empty())
    {
        rate_limit(
            &state.cache,
            "promo",
            &client.key(),
            20,
            StdDuration::from_secs(600),
        )
        .await?;
    }
    let settings = booking_settings(&state).await?;
    let ctx = trip_context(locale, settings);
    let request = TripRequest {
        items: body.items,
        rate_plan_id: body.rate_plan_id,
        promo_code: body.promo_code,
        child_ages: body.child_ages,
    };
    let mut conn = state.db.acquire().await?;
    let quote = trip::quote(&mut conn, &request, &ctx).await?;
    let token = (!quote.has_issues).then(|| {
        quote_token::sign(
            &state.config.jwt_secret,
            &request,
            quote.total_minor,
            quote.currency.as_str(),
            ctx.now,
        )
    });
    Ok(Json(json!({ "quote": quote, "quote_token": token })))
}

#[derive(Debug, Deserialize)]
pub struct CustomerBody {
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub whatsapp_preferred: bool,
}

#[derive(Debug, Deserialize)]
pub struct CheckoutBody {
    /// Direct checkout; when absent, the basket (cart cookie) is booked.
    #[serde(default)]
    pub items: Option<Vec<TripItem>>,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub promo_code: Option<String>,
    #[serde(default)]
    pub child_ages: Vec<u8>,
    pub quote_token: String,
    pub customer: CustomerBody,
    pub payment_option: PaymentOption,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub save_card: bool,
    #[serde(default)]
    pub estimated_arrival_time: Option<NaiveTime>,
    #[serde(default)]
    pub special_requests: Option<String>,
    pub accept_terms: bool,
    #[serde(default)]
    pub marketing_consent: bool,
    #[serde(default)]
    pub captcha_token: Option<String>,
    /// "Create an account to manage this trip": emails a sign-in code + link to the booking
    /// address (the account exists only once that address is verified). The answer never says
    /// whether an account already exists.
    #[serde(default)]
    pub create_account: bool,
}

fn field_error(field: &str, code: &str) -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_field",
        format!("{field}: {code}"),
    )
    .with_details(json!({ "field": field, "reason": code }))
}

fn validate(body: &CheckoutBody) -> AppResult<()> {
    let c = &body.customer;
    for (field, value, max) in [
        ("first_name", &c.first_name, 80),
        ("last_name", &c.last_name, 80),
    ] {
        if value.trim().is_empty() {
            return Err(field_error(field, "required"));
        }
        if value.chars().count() > max {
            return Err(field_error(field, "too_long"));
        }
    }
    let email = c.email.trim();
    let valid_email = email.len() <= 200
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        })
        && !email.contains(char::is_whitespace);
    if !valid_email {
        return Err(field_error("email", "invalid"));
    }
    if let Some(phone) = c.phone.as_deref().filter(|p| !p.trim().is_empty()) {
        let digits = phone.chars().filter(char::is_ascii_digit).count();
        if !(6..=16).contains(&digits) || phone.len() > 30 {
            return Err(field_error("phone", "invalid"));
        }
    }
    if let Some(cc) = c.country_code.as_deref().filter(|c| !c.is_empty())
        && (cc.len() != 2 || !cc.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return Err(field_error("country_code", "invalid"));
    }
    if body
        .special_requests
        .as_deref()
        .is_some_and(|s| s.chars().count() > 2000)
    {
        return Err(field_error("special_requests", "too_long"));
    }
    if !body.accept_terms {
        return Err(field_error("accept_terms", "required"));
    }
    Ok(())
}

pub async fn checkout(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    headers: HeaderMap,
    jar: CookieJar,
    guest: Option<Extension<GuestUser>>,
    Json(body): Json<CheckoutBody>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let user = guest.map(|g| g.user_id);
    // Signed in: the booking goes to the account's customer record (details kept up to date).
    let account_customer = match user {
        Some(u) => Some(crate::services::accounts::signin::customer_of(&state.db, u).await?),
        None => None,
    };
    rate_limit(
        &state.cache,
        "checkout",
        &client.key(),
        10,
        StdDuration::from_secs(60),
    )
    .await?;
    validate(&body)?;
    let settings = booking_settings(&state).await?;
    if !settings.open_to_public() {
        return Err(AppError::domain(
            StatusCode::SERVICE_UNAVAILABLE,
            "booking_closed",
            "online booking is not open yet",
        ));
    }
    turnstile::verify(
        &state.http,
        state.config.booking.turnstile_secret.as_deref(),
        body.captcha_token.as_deref(),
        client.ip,
    )
    .await?;

    let (cart_id, basket) = cart::load(&state, &jar, user).await;
    let from_cart = body.items.is_none();
    let request = match &body.items {
        Some(items) => TripRequest {
            items: items.clone(),
            rate_plan_id: body.rate_plan_id,
            promo_code: body.promo_code.clone(),
            child_ages: body.child_ages.clone(),
        },
        None => basket.to_request().0,
    };
    if request.items.is_empty() {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_trip",
            "the trip is empty",
        ));
    }
    let ctx = trip_context(locale.clone(), settings.clone());
    let Some(expected) = quote_token::verify(
        &state.config.jwt_secret,
        &body.quote_token,
        &request,
        ctx.now,
    ) else {
        // Expired or for another basket: show the current price before taking any money.
        let mut conn = state.db.acquire().await?;
        let fresh = trip::quote(&mut conn, &request, &ctx).await?;
        let token = quote_token::sign(
            &state.config.jwt_secret,
            &request,
            fresh.total_minor,
            fresh.currency.as_str(),
            ctx.now,
        );
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "quote_expired",
            "the price must be confirmed again",
        )
        .with_details(json!({ "quote": fresh, "quote_token": token })));
    };
    let pays_online = matches!(
        body.payment_option,
        PaymentOption::Full | PaymentOption::Deposit
    );
    if pays_online && !state.payments.is_enabled() {
        return Err(AppError::domain(
            StatusCode::SERVICE_UNAVAILABLE,
            "payments_unavailable",
            "online payment is not available, choose another option",
        ));
    }
    if let Some(p) = &body.provider
        && state.payments.get(p).is_none()
    {
        return Err(AppError::BadRequest("unknown payment provider".into()));
    }

    if let Some(customer_id) = account_customer {
        let c = &body.customer;
        sqlx::query!(
            "UPDATE customers SET first_name = $2, last_name = $3, phone = COALESCE($4, phone),
                    country_code = COALESCE($5, country_code)
             WHERE id = $1",
            customer_id,
            c.first_name.trim(),
            c.last_name.trim(),
            c.phone.as_deref().map(str::trim).filter(|p| !p.is_empty()),
            c.country_code
                .as_deref()
                .map(str::to_ascii_uppercase)
                .filter(|c| c.len() == 2),
        )
        .execute(&state.db)
        .await?;
    }
    let key = idempotency_key(&headers);
    let state2 = state.clone();
    let invite = (body.create_account && user.is_none())
        .then(|| (body.customer.email.clone(), locale.clone()));
    let response = idempotent(&state.cache, "checkout", key, || async move {
        let state = state2;
        let c = &body.customer;
        let input = CheckoutInput {
            request: request.clone(),
            customer: match account_customer {
                Some(id) => CustomerRef::Existing(id),
                None => CustomerRef::New(CustomerInput {
                email: Some(c.email.trim().to_lowercase()),
                first_name: c.first_name.trim().to_string(),
                last_name: Some(c.last_name.trim().to_string()),
                phone: c.phone.clone().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()),
                country_code: c.country_code.clone().map(|c| c.to_ascii_uppercase()).filter(|c| !c.is_empty()),
                preferred_locale: Some(locale.clone()),
            }),
            },
            payment_option: body.payment_option,
            expected_total_minor: Some(expected),
            special_requests: body.special_requests.clone(),
            estimated_arrival_time: body.estimated_arrival_time,
            terms_version: Some(settings.terms_version.clone()),
            marketing_consent: body.marketing_consent,
            whatsapp_preferred: c.whatsapp_preferred,
            save_card_for_balance: body.save_card && body.payment_option == PaymentOption::Deposit,
            source: BookingSource::Website,
            actor: client.actor(Actor::customer()),
            skip_guest_emails: false,
            request_id: client.request_id.clone(),
            ip_address: client.ip_network(),
            user_agent: client.user_agent.clone(),
        };
        let created = match trip::create(&state.db, &input, &ctx).await {
            Ok(c) => c,
            Err(crate::services::booking::BookingError::PriceChanged { expected, actual }) => {
                let mut conn = state.db.acquire().await?;
                let fresh = trip::quote(&mut conn, &request, &ctx).await?;
                let token = quote_token::sign(&state.config.jwt_secret, &request, fresh.total_minor, fresh.currency.as_str(), ctx.now);
                return Err(AppError::domain(StatusCode::CONFLICT, "price_changed", "the price changed since it was shown")
                    .with_details(json!({ "expected_total_minor": expected, "total_minor": actual, "quote": fresh, "quote_token": token })));
            }
            Err(e) => return Err(e.into()),
        };
        let access_token = manage::access_token(&state.config.jwt_secret, created.booking_id);
        let payment = if created.status == BookingStatus::AwaitingPayment && created.due_now_minor > 0 {
            let purpose = if created.payment_option == PaymentOption::Deposit {
                PaymentPurpose::Deposit
            } else {
                PaymentPurpose::Full
            };
            match payments::start(
                &state.db,
                &state.payments,
                body.provider.as_deref(),
                created.booking_id,
                purpose,
                created.due_now_minor,
                input.save_card_for_balance,
            )
            .await
            {
                Ok(p) => Some(p),
                // The booking and its hold exist: the payment page retries from there.
                Err(e) => {
                    tracing::warn!(booking = %created.reference, error = %e, "payment could not start");
                    None
                }
            }
        } else {
            None
        };
        Ok(json!({
            "booking": {
                "reference": created.reference,
                "status": created.status,
                "total_minor": created.total_minor,
                "due_now_minor": created.due_now_minor,
                "payment_option": created.payment_option,
                "hold_expires_at": created.hold_expires_at,
                "currency": created.quote.currency,
            },
            "access_token": access_token,
            "payment": payment,
            "payment_config": state.payments.public_config(),
        }))
    })
    .await?;
    let jar = if from_cart && cart_id.is_some() {
        cart::clear(&state, jar, user).await
    } else {
        jar
    };
    if let Some((email, locale)) = invite
        && let Err(e) = crate::services::accounts::signin::send_code(
            &state.db,
            &state.cache,
            &email,
            &locale,
            Some("/account/trips"),
            chrono::Utc::now(),
        )
        .await
    {
        tracing::warn!(error = %e, "account invitation after checkout failed");
    }
    Ok((jar, Json(response)))
}
