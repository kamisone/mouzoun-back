use axum::http::StatusCode;
use serde_json::json;

use super::trip::{ItemIssue, PromoStatus};
use crate::{db::bookings::BookingStatus, error::AppError, services::pricing::PricingError};

#[derive(Debug, thiserror::Error)]
pub enum BookingError {
    /// The requested inventory cannot be sold (full, stop-sell, closed, restrictions...).
    #[error("not available: {0}")]
    NotAvailable(String),
    #[error("invalid booking request: {0}")]
    Invalid(String),
    #[error("not found")]
    NotFound,
    #[error("a {0} booking cannot be changed")]
    InvalidTransition(BookingStatus),
    #[error(transparent)]
    Pricing(#[from] PricingError),
    /// One item of a trip cannot be sold as requested (index in the quote's items).
    #[error("item {index} not available: {}", issue.message)]
    ItemUnavailable { index: usize, issue: ItemIssue },
    /// The price recomputed at checkout differs from the one the guest saw.
    #[error("price changed: {expected} → {actual}")]
    PriceChanged { expected: i64, actual: i64 },
    #[error("promo code not applicable: {}", .0.reason.clone().unwrap_or_default())]
    PromoInvalid(PromoStatus),
    #[error("payment option not offered for this booking")]
    PaymentOptionNotAllowed,
    #[error(transparent)]
    Database(sqlx::Error),
}

impl From<sqlx::Error> for BookingError {
    /// The exclusion constraints on unit allocations and resource assignments are the last line
    /// of defence against double-booking: a violation means "not available".
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::Database(db) if db.code().as_deref() == Some("23P01") => {
                BookingError::NotAvailable("already allocated for this period".into())
            }
            sqlx::Error::Database(db)
                if db.code().as_deref() == Some("23514") && db.message().contains("reserved") =>
            {
                BookingError::NotAvailable("no capacity left".into())
            }
            _ => BookingError::Database(e),
        }
    }
}

impl From<BookingError> for AppError {
    fn from(e: BookingError) -> Self {
        match e {
            BookingError::ItemUnavailable { index, issue } => {
                AppError::domain(StatusCode::CONFLICT, "not_available", issue.message.clone())
                    .with_details(json!({ "index": index, "issue": issue }))
            }
            BookingError::PriceChanged { expected, actual } => AppError::domain(
                StatusCode::CONFLICT,
                "price_changed",
                "the price changed since it was shown",
            )
            .with_details(json!({ "expected_total_minor": expected, "total_minor": actual })),
            BookingError::PromoInvalid(status) => AppError::domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "promo_invalid",
                "this promo code cannot be used",
            )
            .with_details(json!(status)),
            BookingError::PaymentOptionNotAllowed => AppError::domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "payment_option_not_allowed",
                "this payment option is not offered for this booking",
            ),
            BookingError::NotAvailable(msg) => {
                AppError::domain(StatusCode::CONFLICT, "not_available", msg)
            }
            BookingError::InvalidTransition(status) => AppError::domain(
                StatusCode::CONFLICT,
                "invalid_transition",
                format!("a {status} booking cannot be changed this way"),
            )
            .with_details(json!({ "status": status })),
            BookingError::Invalid(_) | BookingError::Pricing(_) => {
                AppError::BadRequest(e.to_string())
            }
            BookingError::NotFound => AppError::NotFound,
            BookingError::Database(db) => AppError::Database(db),
        }
    }
}
