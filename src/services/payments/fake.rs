//! A provider that "pays" without any card (`PAYMENTS_FAKE=true`): local development and
//! end-to-end tests run the whole funnel, webhooks included, without Stripe keys. Its
//! webhooks are signed with a fixed secret like Stripe's, so they take the same code path.

use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::{Value, json};

use super::{
    gateway::{
        CardSetup, CreatePayment, GatewayError, OffSessionCharge, PaymentGateway, ProviderPayment,
        ProviderRefund, RefundRequest, SavedCard, WebhookEvent,
    },
    stripe,
};
use crate::db::bookings::PaymentProvider;

/// Not a secret: the fake provider is for development and tests only.
pub const WEBHOOK_SECRET: &str = "whsec_mouzoun_fake_provider";

pub struct FakeGateway;

#[async_trait]
impl PaymentGateway for FakeGateway {
    fn provider(&self) -> PaymentProvider {
        // Recorded as "other": a fake payment can never be mistaken for a Stripe one.
        PaymentProvider::Other
    }

    fn public_config(&self) -> Value {
        json!({ "fake": true })
    }

    async fn create_payment(
        &self,
        req: &CreatePayment<'_>,
    ) -> Result<ProviderPayment, GatewayError> {
        let id = format!("fake_pi_{}", req.payment_id.simple());
        Ok(ProviderPayment {
            client_secret: format!("{id}_secret"),
            provider_payment_id: id,
            provider_customer_id: req
                .save_card
                .then(|| format!("fake_cus_{}", req.booking_id.simple())),
            payload: json!({ "status": "requires_payment_method", "fake": true }),
        })
    }

    async fn refund(&self, req: &RefundRequest<'_>) -> Result<ProviderRefund, GatewayError> {
        Ok(ProviderRefund {
            provider_refund_id: format!("fake_re_{}", req.idempotency_key),
            succeeded: true,
        })
    }

    async fn charge_off_session(
        &self,
        req: &OffSessionCharge<'_>,
    ) -> Result<ProviderPayment, GatewayError> {
        if req.payment_method_id.contains("declined") {
            return Err(GatewayError::Declined("Your card was declined.".into()));
        }
        Ok(ProviderPayment {
            provider_payment_id: format!("fake_pi_{}", req.payment_id.simple()),
            client_secret: String::new(),
            provider_customer_id: Some(req.provider_customer_id.to_string()),
            payload: json!({ "status": "succeeded", "fake": true }),
        })
    }

    async fn setup_card(
        &self,
        provider_customer_id: Option<&str>,
        _email: &str,
        _name: &str,
        idempotency_key: &str,
    ) -> Result<CardSetup, GatewayError> {
        Ok(CardSetup {
            provider_customer_id: provider_customer_id
                .map(str::to_string)
                .unwrap_or_else(|| format!("fake_cus_{idempotency_key}")),
            client_secret: format!("fake_seti_{idempotency_key}_secret"),
        })
    }

    /// Every fake customer has one test card; `remove_card` succeeds without effect.
    async fn list_cards(&self, provider_customer_id: &str) -> Result<Vec<SavedCard>, GatewayError> {
        Ok(vec![SavedCard {
            id: format!("fake_pm_{provider_customer_id}"),
            brand: "visa".into(),
            last4: "4242".into(),
            exp_month: 12,
            exp_year: 2030,
        }])
    }

    async fn remove_card(
        &self,
        provider_customer_id: &str,
        card_id: &str,
    ) -> Result<(), GatewayError> {
        if card_id == format!("fake_pm_{provider_customer_id}") {
            Ok(())
        } else {
            Err(GatewayError::Declined("unknown card".into()))
        }
    }

    async fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<WebhookEvent, GatewayError> {
        let header = headers
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| GatewayError::InvalidWebhook("missing signature".into()))?;
        stripe::verify_signature(header, body, WEBHOOK_SECRET, chrono::Utc::now().timestamp())?;
        stripe::parse_event(body)
    }
}

/// A Stripe-shaped event the fake provider "sends" when the guest clicks "Simulate payment".
pub fn succeeded_event(provider_payment_id: &str, amount_minor: i64, save_card: bool) -> Value {
    json!({
        "id": format!("evt_{}", uuid::Uuid::now_v7().simple()),
        "type": "payment_intent.succeeded",
        "data": { "object": {
            "id": provider_payment_id,
            "amount_received": amount_minor,
            "setup_future_usage": if save_card { Value::from("off_session") } else { Value::Null },
            "payment_method": if save_card { Value::from("fake_pm_card_visa") } else { Value::Null },
        }}
    })
}

pub fn failed_event(provider_payment_id: &str) -> Value {
    json!({
        "id": format!("evt_{}", uuid::Uuid::now_v7().simple()),
        "type": "payment_intent.payment_failed",
        "data": { "object": {
            "id": provider_payment_id,
            "last_payment_error": { "message": "Your card was declined." },
        }}
    })
}
