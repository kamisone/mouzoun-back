//! Whether online payment works, for the readiness probe, the back office and the go-live
//! checklist. Configuration only, plus the last webhook received: no call to the provider.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::config::{BookingConfig, StripeMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMode {
    /// Stripe test keys: no real money.
    Test,
    /// Stripe live keys.
    Live,
    /// `PAYMENTS_FAKE` (development only, refused in production).
    Simulated,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaymentStatus {
    /// A guest can pay online now.
    pub online: bool,
    /// `stripe`, `fake`, or none.
    pub provider: Option<&'static str>,
    pub mode: Option<PaymentMode>,
    /// The webhook signing secret is set (payments are confirmed by webhooks only).
    pub webhook_configured: bool,
    /// Last webhook received from the provider (proves the dashboard endpoint is right).
    pub last_webhook_at: Option<DateTime<Utc>>,
    /// Webhooks received but not processed (errors to look at).
    pub webhook_errors: i64,
}

impl PaymentStatus {
    pub fn from_config(config: &BookingConfig) -> Self {
        match (&config.stripe, config.fake_payments) {
            (Some(stripe), _) => Self {
                online: true,
                provider: Some("stripe"),
                mode: Some(match stripe.mode() {
                    StripeMode::Live => PaymentMode::Live,
                    StripeMode::Test => PaymentMode::Test,
                }),
                webhook_configured: !stripe.webhook_secret.is_empty(),
                last_webhook_at: None,
                webhook_errors: 0,
            },
            (None, true) => Self {
                online: true,
                provider: Some("fake"),
                mode: Some(PaymentMode::Simulated),
                webhook_configured: true,
                last_webhook_at: None,
                webhook_errors: 0,
            },
            (None, false) => Self {
                online: false,
                provider: None,
                mode: None,
                webhook_configured: false,
                last_webhook_at: None,
                webhook_errors: 0,
            },
        }
    }

    /// With the webhook history of the active provider.
    pub async fn load(pool: &PgPool, config: &BookingConfig) -> sqlx::Result<Self> {
        let mut status = Self::from_config(config);
        // Webhook events are stored under the provider name of their URL (`/webhooks/{provider}`).
        let Some(provider) = status.provider else {
            return Ok(status);
        };
        let row = sqlx::query!(
            r#"SELECT max(received_at) AS "last?",
                      count(*) FILTER (WHERE processed_at IS NULL AND error IS NOT NULL) AS "errors!"
               FROM payment_webhook_events WHERE provider = $1"#,
            provider
        )
        .fetch_one(pool)
        .await?;
        status.last_webhook_at = row.last;
        status.webhook_errors = row.errors;
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StripeConfig;

    #[test]
    fn no_provider_means_no_online_payment() {
        let s = PaymentStatus::from_config(&BookingConfig::default());
        assert!(!s.online);
        assert_eq!(s.provider, None);
    }

    #[test]
    fn stripe_mode_follows_the_keys() {
        let config = BookingConfig {
            stripe: Some(StripeConfig {
                secret_key: "sk_live_x".into(),
                publishable_key: "pk_live_x".into(),
                webhook_secret: "whsec_x".into(),
                api_base: "https://api.stripe.com".into(),
            }),
            ..BookingConfig::default()
        };
        let s = PaymentStatus::from_config(&config);
        assert_eq!(
            (s.online, s.provider, s.mode),
            (true, Some("stripe"), Some(PaymentMode::Live))
        );
        assert!(s.webhook_configured);
    }
}
