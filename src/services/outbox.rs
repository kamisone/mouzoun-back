//! Transactional outbox.
//!
//! State changes write an `outbox_events` row **in their own transaction**, so an event exists
//! if and only if the change committed. The dispatcher (`crate::worker`) then hands each event
//! to its handlers (emails, cache invalidation...), with retries. Delivery is at-least-once:
//! handlers must be idempotent (the event id identifies the event).
//!
//! Events are produced only through [`Event`]: payloads carry **ids, never rendered data**;
//! handlers load the current state when they run. The one exception is `secret`: a raw token
//! that exists nowhere else (the database stores only its hash). The dispatcher deletes it from
//! the payload as soon as the event is processed or dead.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// Payload key removed by the dispatcher once the event is finished.
pub const SECRET_KEY: &str = "secret";

/// Every event the application produces. `event_type` is `<aggregate>.<verb_past>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    BookingCreated {
        booking_id: Uuid,
    },
    BookingConfirmed {
        booking_id: Uuid,
    },
    BookingModified {
        booking_id: Uuid,
    },
    BookingCancelled {
        booking_id: Uuid,
    },
    /// A pending hold ran out (background job).
    BookingExpired {
        booking_id: Uuid,
    },
    /// Scheduled: N days before arrival.
    BookingPreArrivalDue {
        booking_id: Uuid,
    },
    /// Scheduled: N days after the stay is completed.
    BookingReviewDue {
        booking_id: Uuid,
    },
    PaymentSucceeded {
        payment_id: Uuid,
        booking_id: Uuid,
    },
    InquiryCreated {
        inquiry_id: Uuid,
    },
    NewsletterSubscribed {
        subscriber_id: Uuid,
        token: String,
    },
    PasswordResetRequested {
        user_id: Uuid,
        auth_token_id: Uuid,
        token: String,
    },
    EmailVerificationRequested {
        user_id: Uuid,
        auth_token_id: Uuid,
        token: String,
    },
    StaffInvited {
        user_id: Uuid,
        auth_token_id: Uuid,
        token: String,
        invited_by: Option<Uuid>,
    },
    ProductUpdated {
        product_id: Uuid,
    },
    SettingsUpdated {
        key: String,
    },
    MenuUpdated {
        menu_id: Uuid,
    },
    PageUpdated {
        page_id: Uuid,
    },
}

/// Payload of every `booking.*` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookingPayload {
    pub booking_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentPayload {
    pub payment_id: Uuid,
    pub booking_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InquiryPayload {
    pub inquiry_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsletterPayload {
    pub subscriber_id: Uuid,
    #[serde(default)]
    pub secret: Option<TokenSecret>,
}

/// `auth.password_reset_requested`, `auth.email_verification_requested`, `auth.staff_invited`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokenPayload {
    pub user_id: Uuid,
    pub auth_token_id: Uuid,
    #[serde(default)]
    pub invited_by: Option<Uuid>,
    #[serde(default)]
    pub secret: Option<TokenSecret>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSecret {
    pub token: String,
}

pub mod types {
    pub const BOOKING_CREATED: &str = "booking.created";
    pub const BOOKING_CONFIRMED: &str = "booking.confirmed";
    pub const BOOKING_MODIFIED: &str = "booking.modified";
    pub const BOOKING_CANCELLED: &str = "booking.cancelled";
    pub const BOOKING_EXPIRED: &str = "booking.expired";
    pub const BOOKING_PRE_ARRIVAL_DUE: &str = "booking.pre_arrival_due";
    pub const BOOKING_REVIEW_DUE: &str = "booking.review_due";
    pub const PAYMENT_SUCCEEDED: &str = "payment.succeeded";
    pub const INQUIRY_CREATED: &str = "inquiry.created";
    pub const NEWSLETTER_SUBSCRIBED: &str = "newsletter.subscribed";
    pub const PASSWORD_RESET_REQUESTED: &str = "auth.password_reset_requested";
    pub const EMAIL_VERIFICATION_REQUESTED: &str = "auth.email_verification_requested";
    pub const STAFF_INVITED: &str = "auth.staff_invited";
    pub const PRODUCT_UPDATED: &str = "product.updated";
    pub const SETTINGS_UPDATED: &str = "settings.updated";
    pub const MENU_UPDATED: &str = "menu.updated";
    pub const PAGE_UPDATED: &str = "page.updated";
}

impl Event {
    pub fn event_type(&self) -> &'static str {
        use types::*;
        match self {
            Self::BookingCreated { .. } => BOOKING_CREATED,
            Self::BookingConfirmed { .. } => BOOKING_CONFIRMED,
            Self::BookingModified { .. } => BOOKING_MODIFIED,
            Self::BookingCancelled { .. } => BOOKING_CANCELLED,
            Self::BookingExpired { .. } => BOOKING_EXPIRED,
            Self::BookingPreArrivalDue { .. } => BOOKING_PRE_ARRIVAL_DUE,
            Self::BookingReviewDue { .. } => BOOKING_REVIEW_DUE,
            Self::PaymentSucceeded { .. } => PAYMENT_SUCCEEDED,
            Self::InquiryCreated { .. } => INQUIRY_CREATED,
            Self::NewsletterSubscribed { .. } => NEWSLETTER_SUBSCRIBED,
            Self::PasswordResetRequested { .. } => PASSWORD_RESET_REQUESTED,
            Self::EmailVerificationRequested { .. } => EMAIL_VERIFICATION_REQUESTED,
            Self::StaffInvited { .. } => STAFF_INVITED,
            Self::ProductUpdated { .. } => PRODUCT_UPDATED,
            Self::SettingsUpdated { .. } => SETTINGS_UPDATED,
            Self::MenuUpdated { .. } => MENU_UPDATED,
            Self::PageUpdated { .. } => PAGE_UPDATED,
        }
    }

    /// `(aggregate_type, aggregate_id, payload)`.
    pub fn parts(&self) -> (&'static str, String, Value) {
        let secret = |token: &str| json!({ "token": token });
        match self {
            Self::BookingCreated { booking_id }
            | Self::BookingConfirmed { booking_id }
            | Self::BookingModified { booking_id }
            | Self::BookingCancelled { booking_id }
            | Self::BookingExpired { booking_id }
            | Self::BookingPreArrivalDue { booking_id }
            | Self::BookingReviewDue { booking_id } => (
                "booking",
                booking_id.to_string(),
                json!({ "booking_id": booking_id }),
            ),
            Self::PaymentSucceeded {
                payment_id,
                booking_id,
            } => (
                "payment",
                payment_id.to_string(),
                json!({ "payment_id": payment_id, "booking_id": booking_id }),
            ),
            Self::InquiryCreated { inquiry_id } => (
                "inquiry",
                inquiry_id.to_string(),
                json!({ "inquiry_id": inquiry_id }),
            ),
            Self::NewsletterSubscribed {
                subscriber_id,
                token,
            } => (
                "newsletter_subscriber",
                subscriber_id.to_string(),
                json!({ "subscriber_id": subscriber_id, SECRET_KEY: secret(token) }),
            ),
            Self::PasswordResetRequested {
                user_id,
                auth_token_id,
                token,
            }
            | Self::EmailVerificationRequested {
                user_id,
                auth_token_id,
                token,
            } => (
                "user",
                user_id.to_string(),
                json!({ "user_id": user_id, "auth_token_id": auth_token_id, SECRET_KEY: secret(token) }),
            ),
            Self::StaffInvited {
                user_id,
                auth_token_id,
                token,
                invited_by,
            } => (
                "user",
                user_id.to_string(),
                json!({
                    "user_id": user_id, "auth_token_id": auth_token_id, "invited_by": invited_by,
                    SECRET_KEY: secret(token),
                }),
            ),
            Self::ProductUpdated { product_id } => (
                "product",
                product_id.to_string(),
                json!({ "product_id": product_id }),
            ),
            Self::SettingsUpdated { key } => ("setting", key.clone(), json!({ "key": key })),
            Self::MenuUpdated { menu_id } => (
                "nav_menu",
                menu_id.to_string(),
                json!({ "menu_id": menu_id }),
            ),
            Self::PageUpdated { page_id } => {
                ("page", page_id.to_string(), json!({ "page_id": page_id }))
            }
        }
    }
}

/// Writes `event` in the caller's transaction, available immediately.
pub async fn enqueue(conn: &mut PgConnection, event: &Event) -> sqlx::Result<i64> {
    let (aggregate_type, aggregate_id, payload) = event.parts();
    sqlx::query_scalar!(
        "INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload)
         VALUES ($1, $2, $3, $4) RETURNING id",
        aggregate_type,
        aggregate_id,
        event.event_type(),
        payload
    )
    .fetch_one(conn)
    .await
}

/// Writes `event` to be handled at `available_at`, at most once per `dedupe_key`. Returns
/// `None` when an event with that key already exists.
pub async fn enqueue_scheduled(
    conn: &mut PgConnection,
    event: &Event,
    available_at: DateTime<Utc>,
    dedupe_key: &str,
) -> sqlx::Result<Option<i64>> {
    let (aggregate_type, aggregate_id, payload) = event.parts();
    sqlx::query_scalar!(
        "INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload, available_at, dedupe_key)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (dedupe_key) DO NOTHING
         RETURNING id",
        aggregate_type,
        aggregate_id,
        event.event_type(),
        payload,
        available_at,
        dedupe_key
    )
    .fetch_optional(conn)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_carry_ids_and_secrets_are_isolated() {
        let id = Uuid::nil();
        let (aggregate, aggregate_id, payload) = Event::BookingCreated { booking_id: id }.parts();
        assert_eq!(
            (aggregate, aggregate_id.as_str()),
            ("booking", id.to_string().as_str())
        );
        assert_eq!(payload, json!({ "booking_id": id }));

        let (_, _, payload) = Event::PasswordResetRequested {
            user_id: id,
            auth_token_id: id,
            token: "raw".into(),
        }
        .parts();
        assert_eq!(payload[SECRET_KEY]["token"], "raw");
        let parsed: AuthTokenPayload = serde_json::from_value(payload).unwrap();
        assert_eq!(parsed.secret.unwrap().token, "raw");
    }

    #[test]
    fn every_event_type_matches_the_database_check() {
        let id = Uuid::nil();
        for event in [
            Event::BookingPreArrivalDue { booking_id: id },
            Event::PaymentSucceeded {
                payment_id: id,
                booking_id: id,
            },
            Event::EmailVerificationRequested {
                user_id: id,
                auth_token_id: id,
                token: String::new(),
            },
            Event::SettingsUpdated {
                key: "site.name".into(),
            },
        ] {
            let t = event.event_type();
            let (aggregate, verb) = t.split_once('.').unwrap();
            assert!(
                [aggregate, verb].iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c == '_')),
                "{t}"
            );
        }
    }
}
