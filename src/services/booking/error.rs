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
            BookingError::NotAvailable(_) | BookingError::InvalidTransition(_) => {
                AppError::Conflict(e.to_string())
            }
            BookingError::Invalid(_) | BookingError::Pricing(_) => {
                AppError::BadRequest(e.to_string())
            }
            BookingError::NotFound => AppError::NotFound,
            BookingError::Database(db) => AppError::Database(db),
        }
    }
}
