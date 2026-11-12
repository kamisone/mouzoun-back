//! The provider boundary. Card data never reaches the API (PCI SAQ A): the browser talks to the
//! provider (Stripe Payment Element, PayPal buttons) with a client secret / order id created
//! here, and the provider tells us the outcome through signed webhooks.

use async_trait::async_trait;
use axum::http::HeaderMap;
use serde::Serialize;
use serde_json::Value;

use crate::db::{bookings::PaymentProvider, types::CurrencyCode};

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// The provider refused (card declined, invalid request): a message for the guest.
    #[error("declined: {0}")]
    Declined(String),
    /// Network or provider outage: retry later.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("invalid webhook: {0}")]
    InvalidWebhook(String),
    #[error("not supported by this provider")]
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct CreatePayment<'a> {
    pub amount_minor: i64,
    pub currency: &'a CurrencyCode,
    /// `payments.idempotency_key`: the provider de-duplicates retries with it.
    pub idempotency_key: &'a str,
    pub reference: &'a str,
    pub description: String,
    pub booking_id: uuid::Uuid,
    pub payment_id: uuid::Uuid,
    pub email: Option<&'a str>,
    pub name: Option<&'a str>,
    /// Keep the card for an off-session balance charge (explicit consent given).
    pub save_card: bool,
    pub provider_customer_id: Option<&'a str>,
}

/// What the browser needs to complete the payment.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderPayment {
    pub provider_payment_id: String,
    /// Stripe client secret, PayPal order id, fake token...
    pub client_secret: String,
    pub provider_customer_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct RefundRequest<'a> {
    pub provider_payment_id: &'a str,
    pub amount_minor: i64,
    pub currency: &'a CurrencyCode,
    pub idempotency_key: &'a str,
}

#[derive(Debug, Clone)]
pub struct ProviderRefund {
    pub provider_refund_id: String,
    pub succeeded: bool,
}

#[derive(Debug, Clone)]
pub struct OffSessionCharge<'a> {
    pub amount_minor: i64,
    pub currency: &'a CurrencyCode,
    pub idempotency_key: &'a str,
    pub provider_customer_id: &'a str,
    pub payment_method_id: &'a str,
    pub reference: &'a str,
    pub booking_id: uuid::Uuid,
    pub payment_id: uuid::Uuid,
}

/// A verified webhook, reduced to what the booking system acts on.
#[derive(Debug, Clone)]
pub struct WebhookEvent {
    pub event_id: String,
    pub event_type: String,
    pub outcome: WebhookOutcome,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookOutcome {
    PaymentSucceeded {
        provider_payment_id: String,
        /// Saved card (only when the guest consented to it).
        payment_method_id: Option<String>,
        amount_minor: Option<i64>,
    },
    PaymentFailed {
        provider_payment_id: String,
        message: String,
    },
    /// 3-D Secure or another step is pending on the guest's side.
    RequiresAction {
        provider_payment_id: String,
    },
    RefundSucceeded {
        provider_refund_id: String,
    },
    RefundFailed {
        provider_refund_id: String,
        message: String,
    },
    /// Valid but irrelevant to us.
    Ignored,
}

#[async_trait]
pub trait PaymentGateway: Send + Sync {
    fn provider(&self) -> PaymentProvider;

    /// Public configuration for the browser (publishable key, client id...).
    fn public_config(&self) -> Value;

    async fn create_payment(
        &self,
        req: &CreatePayment<'_>,
    ) -> Result<ProviderPayment, GatewayError>;

    async fn refund(&self, req: &RefundRequest<'_>) -> Result<ProviderRefund, GatewayError>;

    async fn charge_off_session(
        &self,
        _req: &OffSessionCharge<'_>,
    ) -> Result<ProviderPayment, GatewayError> {
        Err(GatewayError::Unsupported)
    }

    /// Checks the signature and decodes the event.
    async fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<WebhookEvent, GatewayError>;
}
