//! Stripe over its REST API (PaymentIntents + Payment Element in the browser, which handles
//! cards, Apple Pay, Google Pay and 3-D Secure / SCA).

use async_trait::async_trait;
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use super::gateway::{
    CreatePayment, GatewayError, OffSessionCharge, PaymentGateway, ProviderPayment, ProviderRefund,
    RefundRequest, WebhookEvent, WebhookOutcome,
};
use crate::{config::StripeConfig, db::bookings::PaymentProvider};

/// Webhooks older than this are refused (replay protection, Stripe's recommended tolerance).
const TOLERANCE_SECS: i64 = 300;

pub struct StripeGateway {
    config: StripeConfig,
    http: reqwest::Client,
}

impl StripeGateway {
    pub fn new(config: StripeConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    async fn post(
        &self,
        path: &str,
        form: &[(String, String)],
        idempotency_key: &str,
    ) -> Result<Value, GatewayError> {
        let response = self
            .http
            .post(format!("{}/v1/{path}", self.config.api_base))
            .basic_auth(&self.config.secret_key, Some(""))
            .header("Idempotency-Key", idempotency_key)
            .form(form)
            .send()
            .await
            .map_err(|e| GatewayError::Unavailable(e.to_string()))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|e| GatewayError::Unavailable(e.to_string()))?;
        if status.is_success() {
            return Ok(body);
        }
        let message = body["error"]["message"]
            .as_str()
            .unwrap_or("payment refused")
            .to_string();
        if status.is_server_error() || status.as_u16() == 429 {
            Err(GatewayError::Unavailable(message))
        } else {
            Err(GatewayError::Declined(message))
        }
    }

    async fn customer(&self, req: &CreatePayment<'_>) -> Result<String, GatewayError> {
        if let Some(id) = req.provider_customer_id {
            return Ok(id.to_string());
        }
        let mut form = vec![(
            "metadata[booking_reference]".into(),
            req.reference.to_string(),
        )];
        if let Some(email) = req.email {
            form.push(("email".into(), email.to_string()));
        }
        if let Some(name) = req.name {
            form.push(("name".into(), name.to_string()));
        }
        let customer = self
            .post(
                "customers",
                &form,
                &format!("{}-customer", req.idempotency_key),
            )
            .await?;
        customer["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| GatewayError::Unavailable("customer without id".into()))
    }
}

fn currency(c: &crate::db::types::CurrencyCode) -> String {
    c.as_str().to_ascii_lowercase()
}

#[async_trait]
impl PaymentGateway for StripeGateway {
    fn provider(&self) -> PaymentProvider {
        PaymentProvider::Stripe
    }

    fn public_config(&self) -> Value {
        json!({ "publishable_key": self.config.publishable_key })
    }

    async fn create_payment(
        &self,
        req: &CreatePayment<'_>,
    ) -> Result<ProviderPayment, GatewayError> {
        let customer = if req.save_card {
            Some(self.customer(req).await?)
        } else {
            None
        };
        let mut form: Vec<(String, String)> = vec![
            ("amount".into(), req.amount_minor.to_string()),
            ("currency".into(), currency(req.currency)),
            ("automatic_payment_methods[enabled]".into(), "true".into()),
            ("description".into(), req.description.clone()),
            ("metadata[booking_id]".into(), req.booking_id.to_string()),
            ("metadata[payment_id]".into(), req.payment_id.to_string()),
            ("metadata[reference]".into(), req.reference.to_string()),
        ];
        if let Some(email) = req.email {
            form.push(("receipt_email".into(), email.to_string()));
        }
        if let Some(customer) = &customer {
            form.push(("customer".into(), customer.clone()));
            form.push(("setup_future_usage".into(), "off_session".into()));
        }
        let intent = self
            .post("payment_intents", &form, req.idempotency_key)
            .await?;
        Ok(ProviderPayment {
            provider_payment_id: intent["id"].as_str().unwrap_or_default().to_string(),
            client_secret: intent["client_secret"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            provider_customer_id: customer,
            payload: json!({ "status": intent["status"] }),
        })
    }

    async fn refund(&self, req: &RefundRequest<'_>) -> Result<ProviderRefund, GatewayError> {
        let refund = self
            .post(
                "refunds",
                &[
                    ("payment_intent".into(), req.provider_payment_id.to_string()),
                    ("amount".into(), req.amount_minor.to_string()),
                ],
                req.idempotency_key,
            )
            .await?;
        Ok(ProviderRefund {
            provider_refund_id: refund["id"].as_str().unwrap_or_default().to_string(),
            succeeded: refund["status"] == "succeeded",
        })
    }

    async fn charge_off_session(
        &self,
        req: &OffSessionCharge<'_>,
    ) -> Result<ProviderPayment, GatewayError> {
        let intent = self
            .post(
                "payment_intents",
                &[
                    ("amount".into(), req.amount_minor.to_string()),
                    ("currency".into(), currency(req.currency)),
                    ("customer".into(), req.provider_customer_id.to_string()),
                    ("payment_method".into(), req.payment_method_id.to_string()),
                    ("off_session".into(), "true".into()),
                    ("confirm".into(), "true".into()),
                    ("metadata[booking_id]".into(), req.booking_id.to_string()),
                    ("metadata[payment_id]".into(), req.payment_id.to_string()),
                    ("metadata[reference]".into(), req.reference.to_string()),
                ],
                req.idempotency_key,
            )
            .await?;
        Ok(ProviderPayment {
            provider_payment_id: intent["id"].as_str().unwrap_or_default().to_string(),
            client_secret: String::new(),
            provider_customer_id: Some(req.provider_customer_id.to_string()),
            payload: json!({ "status": intent["status"] }),
        })
    }

    async fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<WebhookEvent, GatewayError> {
        let header = headers
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| GatewayError::InvalidWebhook("missing Stripe-Signature".into()))?;
        verify_signature(
            header,
            body,
            &self.config.webhook_secret,
            chrono::Utc::now().timestamp(),
        )?;
        parse_event(body)
    }
}

/// `Stripe-Signature: t=<unix>,v1=<hex hmac of "t.body">[,v1=...]`.
pub fn verify_signature(
    header: &str,
    body: &[u8],
    secret: &str,
    now: i64,
) -> Result<(), GatewayError> {
    let invalid = |m: &str| GatewayError::InvalidWebhook(m.into());
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", v)) => timestamp = v.parse::<i64>().ok(),
            Some(("v1", v)) => signatures.push(v.to_string()),
            _ => {}
        }
    }
    let timestamp = timestamp.ok_or_else(|| invalid("no timestamp"))?;
    if (now - timestamp).abs() > TOLERANCE_SECS {
        return Err(invalid("timestamp outside the tolerance"));
    }
    for signature in signatures {
        let Ok(expected) = hex_decode(&signature) else {
            continue;
        };
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        if mac.verify_slice(&expected).is_ok() {
            return Ok(());
        }
    }
    Err(invalid("signature mismatch"))
}

/// Signature header for `body` (tests and the Stripe CLI-like fixtures).
pub fn sign(body: &[u8], secret: &str, timestamp: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={timestamp},v1={hex}")
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

pub fn parse_event(body: &[u8]) -> Result<WebhookEvent, GatewayError> {
    let event: Value =
        serde_json::from_slice(body).map_err(|e| GatewayError::InvalidWebhook(e.to_string()))?;
    let event_id = event["id"]
        .as_str()
        .ok_or_else(|| GatewayError::InvalidWebhook("event without id".into()))?
        .to_string();
    let event_type = event["type"].as_str().unwrap_or_default().to_string();
    let object = &event["data"]["object"];
    let id = object["id"].as_str().unwrap_or_default().to_string();
    let outcome = match event_type.as_str() {
        "payment_intent.succeeded" => WebhookOutcome::PaymentSucceeded {
            provider_payment_id: id,
            payment_method_id: object["setup_future_usage"]
                .as_str()
                .and_then(|_| object["payment_method"].as_str())
                .map(str::to_string),
            amount_minor: object["amount_received"].as_i64(),
        },
        "payment_intent.payment_failed" => WebhookOutcome::PaymentFailed {
            provider_payment_id: id,
            message: object["last_payment_error"]["message"]
                .as_str()
                .unwrap_or("payment failed")
                .to_string(),
        },
        "payment_intent.requires_action" => WebhookOutcome::RequiresAction {
            provider_payment_id: id,
        },
        "refund.updated" | "refund.created" if object["status"] == "succeeded" => {
            WebhookOutcome::RefundSucceeded {
                provider_refund_id: id,
            }
        }
        "refund.updated" | "refund.failed" if object["status"] == "failed" => {
            WebhookOutcome::RefundFailed {
                provider_refund_id: id,
                message: object["failure_reason"]
                    .as_str()
                    .unwrap_or("refund failed")
                    .to_string(),
            }
        }
        _ => WebhookOutcome::Ignored,
    };
    Ok(WebhookEvent {
        event_id,
        event_type,
        outcome,
        payload: event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_checked_with_tolerance() {
        let body =
            br#"{"id":"evt_1","type":"payment_intent.succeeded","data":{"object":{"id":"pi_1"}}}"#;
        let header = sign(body, "whsec_test", 1_000);
        assert!(verify_signature(&header, body, "whsec_test", 1_100).is_ok());
        assert!(verify_signature(&header, body, "whsec_other", 1_100).is_err());
        assert!(verify_signature(&header, b"{}", "whsec_test", 1_100).is_err());
        assert!(
            verify_signature(&header, body, "whsec_test", 2_000).is_err(),
            "too old"
        );
        let event = parse_event(body).unwrap();
        assert_eq!(
            event.outcome,
            WebhookOutcome::PaymentSucceeded {
                provider_payment_id: "pi_1".into(),
                payment_method_id: None,
                amount_minor: None
            }
        );
    }
}
