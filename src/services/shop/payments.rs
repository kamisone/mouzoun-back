//! Shop payment providers behind one trait (`PaymentProvider`), so PayPal or CMI can plug in later.
//!
//! - `StripeCheckout`: Stripe Checkout Sessions (hosted page, or embedded). Card data never
//!   touches our servers (PCI SAQ A); Apple Pay / Google Pay come with Checkout.
//! - `FakeCheckout`: development and tests (`PAYMENTS_FAKE=true`), same events as Stripe.
//! - offline methods (cash on delivery, pay at pickup, bank transfer) need no provider call:
//!   their payments are recorded by staff ([`super::orders::record_offline_payment`]).
//!
//! The Stripe secrets come only from the environment (`STRIPE_*`, see `config.rs`): never stored,
//! returned or logged. Without them Stripe is "unavailable" and the checkout hides it.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    config::{StripeConfig, StripeMode},
    services::payments::{
        GatewayError,
        gateway::{ProviderRefund, RefundRequest},
        stripe as stripe_gateway,
    },
};

/// Stripe API version the requests are written for.
pub const STRIPE_API_VERSION: &str = "2024-06-20";

#[derive(Debug, Clone, Serialize)]
pub struct CheckoutLine {
    pub name: String,
    pub unit_amount_minor: i64,
    pub quantity: i32,
}

#[derive(Debug, Clone)]
pub struct CheckoutRequest {
    pub order_id: Uuid,
    pub payment_id: Uuid,
    pub order_number: String,
    pub email: String,
    pub locale: String,
    pub currency: String,
    pub lines: Vec<CheckoutLine>,
    /// Shipping after its discount (one fixed-amount shipping option).
    pub shipping: Option<(String, i64)>,
    /// Order-level discount on the goods (one-time coupon).
    pub discount_minor: i64,
    /// Taxes not included in the prices (one extra line), else 0.
    pub exclusive_tax_minor: i64,
    pub total_minor: i64,
    pub idempotency_key: String,
    pub success_url: String,
    pub cancel_url: String,
    pub expires_at: DateTime<Utc>,
    pub statement_descriptor: Option<String>,
    pub embedded: bool,
}

impl CheckoutRequest {
    /// What the provider will charge: must equal `total_minor` (checked after creation too).
    pub fn computed_total(&self) -> i64 {
        self.lines
            .iter()
            .map(|l| l.unit_amount_minor * i64::from(l.quantity))
            .sum::<i64>()
            - self.discount_minor
            + self.shipping.as_ref().map_or(0, |s| s.1)
            + self.exclusive_tax_minor
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckoutSession {
    pub session_id: String,
    pub url: Option<String>,
    pub client_secret: Option<String>,
    pub amount_total: i64,
}

/// A verified provider event, reduced to what the shop acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShopOutcome {
    SessionPaid {
        session_id: String,
        payment_intent: Option<String>,
        amount_minor: i64,
        currency: String,
    },
    /// Completed but the money is not there yet (bank debit...): wait for async_payment_*.
    SessionPending {
        session_id: String,
    },
    SessionFailed {
        session_id: String,
    },
    SessionExpired {
        session_id: String,
    },
    ChargeRefunded {
        payment_intent: String,
        payment_id: Option<Uuid>,
        amount_refunded_minor: i64,
    },
    RefundUpdated {
        refund_id: String,
        status: String,
        payment_intent: Option<String>,
        amount_minor: i64,
        failure_reason: Option<String>,
    },
    Dispute {
        payment_intent: String,
        amount_minor: i64,
        reason: String,
    },
    Ignored,
}

#[async_trait]
pub trait PaymentProvider: Send + Sync {
    /// `payments.provider` value.
    fn name(&self) -> &'static str;
    async fn create_checkout(&self, req: &CheckoutRequest)
    -> Result<CheckoutSession, GatewayError>;
    /// Current state of a session (`payment_status`, `amount_total`...).
    async fn retrieve(&self, session_id: &str) -> Result<Value, GatewayError>;
    /// Ends an unpaid session (hold expired, order cancelled). Already finished is not an error.
    async fn expire(&self, session_id: &str) -> Result<(), GatewayError>;
    async fn refund(&self, req: &RefundRequest<'_>) -> Result<ProviderRefund, GatewayError>;
    /// Checks the signature; returns the event JSON.
    fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
        now: i64,
    ) -> Result<Value, GatewayError>;
    fn parse_event(&self, event: &Value) -> ShopOutcome {
        parse_stripe_event(event)
    }
    /// Payouts created since `created_gte` (unix seconds), with their settled payments: one
    /// entry per payment `{payout_id, arrival_date, payment_intent, fee, net}`. Providers
    /// without payouts return nothing.
    async fn settlements(&self, _created_gte: i64) -> Result<Vec<Settlement>, GatewayError> {
        Ok(vec![])
    }
}

/// A card payment as settled by the provider (fees and payout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub payout_id: String,
    pub arrival_date: i64,
    pub payment_intent: String,
    pub fee_minor: i64,
    pub net_minor: i64,
}

/// Stripe's Checkout locales we can map ours to (Arabic is not one: `auto`).
fn stripe_locale(locale: &str) -> &'static str {
    match locale {
        "fr" => "fr",
        "en" => "en",
        "es" => "es",
        "de" => "de",
        "it" => "it",
        "nl" => "nl",
        _ => "auto",
    }
}

pub fn parse_stripe_event(event: &Value) -> ShopOutcome {
    let object = &event["data"]["object"];
    let str_of = |v: &Value| v.as_str().map(str::to_string);
    let id = object["id"].as_str().unwrap_or_default().to_string();
    match event["type"].as_str().unwrap_or_default() {
        "checkout.session.completed"
            if object["payment_status"] == "paid"
                || object["payment_status"] == "no_payment_required" =>
        {
            ShopOutcome::SessionPaid {
                session_id: id,
                payment_intent: str_of(&object["payment_intent"]),
                amount_minor: object["amount_total"].as_i64().unwrap_or(0),
                currency: object["currency"]
                    .as_str()
                    .unwrap_or_default()
                    .to_ascii_uppercase(),
            }
        }
        "checkout.session.completed" => ShopOutcome::SessionPending { session_id: id },
        "checkout.session.async_payment_succeeded" => ShopOutcome::SessionPaid {
            session_id: id,
            payment_intent: str_of(&object["payment_intent"]),
            amount_minor: object["amount_total"].as_i64().unwrap_or(0),
            currency: object["currency"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_uppercase(),
        },
        "checkout.session.async_payment_failed" => ShopOutcome::SessionFailed { session_id: id },
        "checkout.session.expired" => ShopOutcome::SessionExpired { session_id: id },
        "charge.refunded" => ShopOutcome::ChargeRefunded {
            payment_intent: object["payment_intent"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            payment_id: object["metadata"]["payment_id"]
                .as_str()
                .and_then(|s| s.parse().ok()),
            amount_refunded_minor: object["amount_refunded"].as_i64().unwrap_or(0),
        },
        "refund.updated" | "refund.created" | "refund.failed" => ShopOutcome::RefundUpdated {
            refund_id: id,
            status: object["status"].as_str().unwrap_or_default().to_string(),
            payment_intent: str_of(&object["payment_intent"]),
            amount_minor: object["amount"].as_i64().unwrap_or(0),
            failure_reason: str_of(&object["failure_reason"]),
        },
        "charge.dispute.created" => ShopOutcome::Dispute {
            payment_intent: object["payment_intent"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            amount_minor: object["amount"].as_i64().unwrap_or(0),
            reason: object["reason"].as_str().unwrap_or("unknown").to_string(),
        },
        _ => ShopOutcome::Ignored,
    }
}

/// Is this Stripe event about a shop order (else it belongs to the booking flow)?
/// Checkout events and objects carrying our `order_id` metadata are; refunds and disputes are
/// matched on their payment intent by the caller.
pub fn event_mentions_order(event: &Value) -> bool {
    let object = &event["data"]["object"];
    event["type"]
        .as_str()
        .is_some_and(|t| t.starts_with("checkout.session."))
        || object["metadata"]["order_id"].is_string()
}

// ---- Stripe ---------------------------------------------------------------------------------

pub struct StripeCheckout {
    config: StripeConfig,
    http: reqwest::Client,
}

impl StripeCheckout {
    pub fn new(config: StripeConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    pub fn mode(&self) -> StripeMode {
        self.config.mode()
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        form: &[(String, String)],
        idempotency_key: Option<&str>,
    ) -> Result<Value, GatewayError> {
        let mut req = self
            .http
            .request(
                method.clone(),
                format!("{}/v1/{path}", self.config.api_base),
            )
            .basic_auth(&self.config.secret_key, Some(""))
            .header("Stripe-Version", STRIPE_API_VERSION);
        if let Some(key) = idempotency_key {
            req = req.header("Idempotency-Key", key);
        }
        let form = if method == reqwest::Method::POST {
            stripe_gateway::tag_form(path, form, &self.config.app_tag)
        } else {
            form.to_vec()
        };
        if !form.is_empty() {
            req = req.form(&form);
        }
        let response = req
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
            .unwrap_or("stripe error")
            .to_string();
        if status.is_server_error() || status.as_u16() == 429 {
            Err(GatewayError::Unavailable(message))
        } else {
            Err(GatewayError::Declined(message))
        }
    }
}

/// The card statement suffix Stripe accepts: Latin letters, digits and spaces, upper case,
/// accents removed, at most 22 characters (`payments.stripe.statement_descriptor`, default
/// `MOUZOUN`). Stripe prints it after the account's prefix, so customers recognise the charge.
pub fn descriptor_suffix(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' | 'À' | 'Â' | 'Ä' => 'A',
            'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'E',
            'î' | 'ï' | 'Î' | 'Ï' => 'I',
            'ô' | 'ö' | 'Ô' | 'Ö' => 'O',
            'ù' | 'û' | 'ü' | 'Ù' | 'Û' | 'Ü' => 'U',
            'ç' | 'Ç' => 'C',
            c => c,
        })
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .collect::<String>()
        .to_ascii_uppercase();
    let trimmed: String = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    let out: String = trimmed
        .chars()
        .take(22)
        .collect::<String>()
        .trim_end()
        .to_string();
    if out.is_empty() {
        "MOUZOUN".into()
    } else {
        out
    }
}

/// Form fields of a Checkout Session (public for tests).
pub fn checkout_form(req: &CheckoutRequest, coupon: Option<&str>) -> Vec<(String, String)> {
    let currency = req.currency.to_ascii_lowercase();
    let mut f: Vec<(String, String)> = vec![
        ("mode".into(), "payment".into()),
        ("customer_email".into(), req.email.clone()),
        ("client_reference_id".into(), req.order_id.to_string()),
        ("metadata[order_id]".into(), req.order_id.to_string()),
        ("metadata[payment_id]".into(), req.payment_id.to_string()),
        ("metadata[order_number]".into(), req.order_number.clone()),
        (
            "payment_intent_data[metadata][order_id]".into(),
            req.order_id.to_string(),
        ),
        (
            "payment_intent_data[metadata][payment_id]".into(),
            req.payment_id.to_string(),
        ),
        (
            "payment_intent_data[metadata][order_number]".into(),
            req.order_number.clone(),
        ),
        ("locale".into(), stripe_locale(&req.locale).into()),
        // French invoices need the buyer's address; Stripe also uses it for 3-D Secure (SCA).
        // No payment_method_types: the methods enabled in the Stripe dashboard apply.
        ("billing_address_collection".into(), "required".into()),
        ("expires_at".into(), req.expires_at.timestamp().to_string()),
    ];
    if let Some(d) = &req.statement_descriptor {
        f.push((
            "payment_intent_data[statement_descriptor_suffix]".into(),
            descriptor_suffix(d),
        ));
    }
    if req.embedded {
        f.push(("ui_mode".into(), "embedded".into()));
        f.push(("return_url".into(), req.success_url.clone()));
    } else {
        f.push(("success_url".into(), req.success_url.clone()));
        f.push(("cancel_url".into(), req.cancel_url.clone()));
    }
    let mut push_line = |i: usize, name: &str, amount: i64, qty: i32| {
        f.push((
            format!("line_items[{i}][price_data][currency]"),
            currency.clone(),
        ));
        f.push((
            format!("line_items[{i}][price_data][product_data][name]"),
            name.to_string(),
        ));
        f.push((
            format!("line_items[{i}][price_data][unit_amount]"),
            amount.to_string(),
        ));
        f.push((format!("line_items[{i}][quantity]"), qty.to_string()));
    };
    let mut i = 0;
    for l in &req.lines {
        push_line(i, &l.name, l.unit_amount_minor, l.quantity);
        i += 1;
    }
    if req.exclusive_tax_minor > 0 {
        push_line(i, "TVA", req.exclusive_tax_minor, 1);
    }
    if let Some((name, amount)) = &req.shipping {
        f.push((
            "shipping_options[0][shipping_rate_data][type]".into(),
            "fixed_amount".into(),
        ));
        f.push((
            "shipping_options[0][shipping_rate_data][display_name]".into(),
            name.clone(),
        ));
        f.push((
            "shipping_options[0][shipping_rate_data][fixed_amount][amount]".into(),
            amount.to_string(),
        ));
        f.push((
            "shipping_options[0][shipping_rate_data][fixed_amount][currency]".into(),
            currency.clone(),
        ));
    }
    if let Some(c) = coupon {
        f.push(("discounts[0][coupon]".into(), c.to_string()));
    }
    f
}

#[async_trait]
impl PaymentProvider for StripeCheckout {
    async fn settlements(&self, created_gte: i64) -> Result<Vec<Settlement>, GatewayError> {
        let mut out = Vec::new();
        let payouts = self
            .call(
                reqwest::Method::GET,
                &format!("payouts?limit=100&created[gte]={created_gte}"),
                &[],
                None,
            )
            .await?;
        for p in payouts["data"].as_array().cloned().unwrap_or_default() {
            let (Some(payout_id), arrival) =
                (p["id"].as_str(), p["arrival_date"].as_i64().unwrap_or(0))
            else {
                continue;
            };
            let mut after: Option<String> = None;
            loop {
                let mut path = format!(
                    "balance_transactions?limit=100&payout={payout_id}&expand[]=data.source"
                );
                if let Some(a) = &after {
                    path.push_str(&format!("&starting_after={a}"));
                }
                let page = self.call(reqwest::Method::GET, &path, &[], None).await?;
                let data = page["data"].as_array().cloned().unwrap_or_default();
                for bt in &data {
                    if !matches!(bt["type"].as_str(), Some("charge" | "payment")) {
                        continue;
                    }
                    let Some(pi) = bt["source"]["payment_intent"].as_str() else {
                        continue;
                    };
                    out.push(Settlement {
                        payout_id: payout_id.to_string(),
                        arrival_date: arrival,
                        payment_intent: pi.to_string(),
                        fee_minor: bt["fee"].as_i64().unwrap_or(0),
                        net_minor: bt["net"].as_i64().unwrap_or(0),
                    });
                }
                if page["has_more"].as_bool() != Some(true) {
                    break;
                }
                after = data
                    .last()
                    .and_then(|b| b["id"].as_str())
                    .map(str::to_string);
                if after.is_none() {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "stripe"
    }

    async fn create_checkout(
        &self,
        req: &CheckoutRequest,
    ) -> Result<CheckoutSession, GatewayError> {
        let coupon = if req.discount_minor > 0 {
            let c = self
                .call(
                    reqwest::Method::POST,
                    "coupons",
                    &[
                        ("amount_off".into(), req.discount_minor.to_string()),
                        ("currency".into(), req.currency.to_ascii_lowercase()),
                        ("duration".into(), "once".into()),
                        ("max_redemptions".into(), "1".into()),
                        ("name".into(), format!("{} discount", req.order_number)),
                    ],
                    Some(&format!("{}-coupon", req.idempotency_key)),
                )
                .await?;
            c["id"].as_str().map(str::to_string)
        } else {
            None
        };
        let session = self
            .call(
                reqwest::Method::POST,
                "checkout/sessions",
                &checkout_form(req, coupon.as_deref()),
                Some(&req.idempotency_key),
            )
            .await?;
        let session_id = session["id"]
            .as_str()
            .ok_or_else(|| GatewayError::Unavailable("checkout session without id".into()))?
            .to_string();
        Ok(CheckoutSession {
            session_id,
            url: session["url"].as_str().map(str::to_string),
            client_secret: session["client_secret"].as_str().map(str::to_string),
            amount_total: session["amount_total"].as_i64().unwrap_or(-1),
        })
    }

    async fn retrieve(&self, session_id: &str) -> Result<Value, GatewayError> {
        self.call(
            reqwest::Method::GET,
            &format!("checkout/sessions/{session_id}"),
            &[],
            None,
        )
        .await
    }

    async fn expire(&self, session_id: &str) -> Result<(), GatewayError> {
        match self
            .call(
                reqwest::Method::POST,
                &format!("checkout/sessions/{session_id}/expire"),
                &[],
                Some(&format!("expire-{session_id}")),
            )
            .await
        {
            Ok(_) => Ok(()),
            // Already complete or expired: nothing to do.
            Err(GatewayError::Declined(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn refund(&self, req: &RefundRequest<'_>) -> Result<ProviderRefund, GatewayError> {
        let refund = self
            .call(
                reqwest::Method::POST,
                "refunds",
                &[
                    ("payment_intent".into(), req.provider_payment_id.to_string()),
                    ("amount".into(), req.amount_minor.to_string()),
                ],
                Some(req.idempotency_key),
            )
            .await?;
        Ok(ProviderRefund {
            provider_refund_id: refund["id"].as_str().unwrap_or_default().to_string(),
            succeeded: refund["status"] == "succeeded",
        })
    }

    fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
        now: i64,
    ) -> Result<Value, GatewayError> {
        let header = headers
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| GatewayError::InvalidWebhook("missing Stripe-Signature".into()))?;
        stripe_gateway::verify_signature(header, body, &self.config.webhook_secret, now)?;
        serde_json::from_slice(body).map_err(|e| GatewayError::InvalidWebhook(e.to_string()))
    }
}

// ---- Fake (development, tests) ------------------------------------------------------------------

/// Sends the customer back to the order page with `fake_session`: in development the page
/// offers "simulate paid / failed", which signs and posts a Stripe-like event to the webhook.
pub struct FakeCheckout;

pub const FAKE_WEBHOOK_SECRET: &str = "whsec_fake_shop";

#[async_trait]
impl PaymentProvider for FakeCheckout {
    fn name(&self) -> &'static str {
        // Stored as `other` (the provider CHECK has no "fake"); session ids start with `fake_`.
        "other"
    }

    async fn create_checkout(
        &self,
        req: &CheckoutRequest,
    ) -> Result<CheckoutSession, GatewayError> {
        let session_id = format!("fake_cs_{}", req.payment_id.simple());
        Ok(CheckoutSession {
            url: Some(format!("{}&fake_session={session_id}", req.success_url)),
            client_secret: req.embedded.then(|| format!("{session_id}_secret")),
            amount_total: req.computed_total(),
            session_id,
        })
    }

    async fn retrieve(&self, session_id: &str) -> Result<Value, GatewayError> {
        Ok(json!({ "id": session_id, "payment_status": "unpaid" }))
    }

    async fn expire(&self, _session_id: &str) -> Result<(), GatewayError> {
        Ok(())
    }

    async fn refund(&self, req: &RefundRequest<'_>) -> Result<ProviderRefund, GatewayError> {
        Ok(ProviderRefund {
            provider_refund_id: format!("fake_re_{}", req.idempotency_key),
            succeeded: false,
        })
    }

    fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
        now: i64,
    ) -> Result<Value, GatewayError> {
        let header = headers
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| GatewayError::InvalidWebhook("missing signature".into()))?;
        stripe_gateway::verify_signature(header, body, FAKE_WEBHOOK_SECRET, now)?;
        serde_json::from_slice(body).map_err(|e| GatewayError::InvalidWebhook(e.to_string()))
    }
}

/// The configured providers (online only; offline methods need none).
#[derive(Clone, Default)]
pub struct ShopPayments {
    providers: HashMap<&'static str, Arc<dyn PaymentProvider>>,
    stripe_mode: Option<StripeMode>,
}

impl ShopPayments {
    pub fn from_config(config: &crate::config::Config, http: reqwest::Client) -> Self {
        let mut providers: HashMap<&'static str, Arc<dyn PaymentProvider>> = HashMap::new();
        let mut stripe_mode = None;
        match &config.booking.stripe {
            Some(stripe) => {
                stripe_mode = Some(stripe.mode());
                providers.insert(
                    "stripe",
                    Arc::new(StripeCheckout::new(stripe.clone(), http)),
                );
            }
            None => {
                tracing::warn!(
                    "shop: STRIPE_* not set, card payment is unavailable (offline methods only)"
                );
            }
        }
        if config.booking.fake_payments {
            providers.insert("fake", Arc::new(FakeCheckout));
        }
        Self {
            providers,
            stripe_mode,
        }
    }

    pub fn with(mut self, name: &'static str, provider: Arc<dyn PaymentProvider>) -> Self {
        self.providers.insert(name, provider);
        self
    }

    /// The provider for card payments: Stripe, else the fake one (development, tests).
    pub fn card(&self) -> Option<Arc<dyn PaymentProvider>> {
        self.providers
            .get("stripe")
            .or_else(|| self.providers.get("fake"))
            .cloned()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn PaymentProvider>> {
        self.providers.get(name).cloned()
    }

    /// Provider of a stored payment (`stripe`, or `other` + a `fake_` session for the fake one).
    pub fn for_payment(
        &self,
        provider: &str,
        session_id: Option<&str>,
    ) -> Option<Arc<dyn PaymentProvider>> {
        if provider == "other" && session_id.is_some_and(|s| s.starts_with("fake_")) {
            return self.providers.get("fake").cloned();
        }
        self.providers.get(provider).cloned()
    }

    pub fn card_available(&self) -> bool {
        self.card().is_some()
    }

    pub fn stripe_configured(&self) -> bool {
        self.providers.contains_key("stripe")
    }

    pub fn stripe_mode(&self) -> Option<StripeMode> {
        self.stripe_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> CheckoutRequest {
        CheckoutRequest {
            order_id: Uuid::nil(),
            payment_id: Uuid::nil(),
            order_number: "MZS-27-000001".into(),
            email: "a@b.ma".into(),
            locale: "ar".into(),
            currency: "EUR".into(),
            lines: vec![
                CheckoutLine {
                    name: "Dates 500 g".into(),
                    unit_amount_minor: 1200,
                    quantity: 2,
                },
                CheckoutLine {
                    name: "Oil 1 L".into(),
                    unit_amount_minor: 1800,
                    quantity: 1,
                },
            ],
            shipping: Some(("Amana".into(), 450)),
            discount_minor: 420,
            exclusive_tax_minor: 0,
            total_minor: 4230,
            idempotency_key: "k".into(),
            success_url: "https://x/ok".into(),
            cancel_url: "https://x/ko".into(),
            expires_at: DateTime::from_timestamp(1_900_000_000, 0).unwrap(),
            statement_descriptor: Some("MOUZOUN".into()),
            embedded: false,
        }
    }

    #[test]
    fn the_statement_suffix_is_latin_and_short() {
        assert_eq!(descriptor_suffix("Mouzoun Épicerie"), "MOUZOUN EPICERIE");
        assert_eq!(
            descriptor_suffix("Ferme M'ssici — dattes & huile!!"),
            "FERME MSSICI DATTES HU"
        );
        assert_eq!(descriptor_suffix("موزون"), "MOUZOUN");
        assert!(descriptor_suffix(&"X".repeat(40)).len() <= 22);
    }

    #[test]
    fn every_object_is_tagged_for_the_shared_account() {
        let form =
            stripe_gateway::tag_form("checkout/sessions", &checkout_form(&req(), None), "mouzoun");
        let get = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("metadata[app]"), Some("mouzoun"));
        assert_eq!(get("payment_intent_data[metadata][app]"), Some("mouzoun"));
        assert_eq!(
            get("payment_intent_data[statement_descriptor_suffix]"),
            Some("MOUZOUN")
        );
        let refund =
            stripe_gateway::tag_form("refunds", &[("amount".into(), "100".into())], "mouzoun");
        assert!(refund.contains(&("metadata[app]".into(), "mouzoun".into())));
        // Reads and actions on existing objects take no metadata.
        assert_eq!(
            stripe_gateway::tag_form("checkout/sessions/cs_1/expire", &[], "mouzoun"),
            vec![]
        );
    }

    #[test]
    fn the_session_charges_exactly_the_order_total() {
        let r = req();
        assert_eq!(r.computed_total(), r.total_minor);
        let form = checkout_form(&r, Some("co_1"));
        let get = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("locale"), Some("auto"), "Arabic falls back to auto");
        assert_eq!(get("line_items[1][price_data][unit_amount]"), Some("1800"));
        assert_eq!(
            get("shipping_options[0][shipping_rate_data][fixed_amount][amount]"),
            Some("450")
        );
        assert_eq!(get("discounts[0][coupon]"), Some("co_1"));
        assert_eq!(
            get("client_reference_id"),
            Some(Uuid::nil().to_string().as_str())
        );
        assert_eq!(
            get("payment_intent_data[metadata][order_id]"),
            Some(Uuid::nil().to_string().as_str())
        );
    }

    #[test]
    fn checkout_events_are_decoded() {
        let paid = json!({"type": "checkout.session.completed", "data": {"object": {
            "id": "cs_1", "payment_status": "paid", "amount_total": 4230, "currency": "eur", "payment_intent": "pi_1"}}});
        assert_eq!(
            parse_stripe_event(&paid),
            ShopOutcome::SessionPaid {
                session_id: "cs_1".into(),
                payment_intent: Some("pi_1".into()),
                amount_minor: 4230,
                currency: "EUR".into()
            }
        );
        let pending = json!({"type": "checkout.session.completed", "data": {"object": {"id": "cs_1", "payment_status": "unpaid"}}});
        assert!(matches!(
            parse_stripe_event(&pending),
            ShopOutcome::SessionPending { .. }
        ));
        assert!(event_mentions_order(&pending));
        let other = json!({"type": "payment_intent.succeeded", "data": {"object": {"id": "pi_9", "metadata": {}}}});
        assert!(!event_mentions_order(&other));
    }
}
