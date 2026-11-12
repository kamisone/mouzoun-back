//! Payments: provider gateways behind one trait, and the flows built on them.
//!
//! - `start`: one `payments` row (idempotency key) + one provider intent per attempt;
//! - webhooks are the source of truth: stored first in `payment_webhook_events`
//!   (unique provider + event id), then processed idempotently in one transaction
//!   (payment status, `amount_paid_minor`, booking confirmation, outbox events);
//! - refunds through the original provider; offline payments (cash, transfer, card terminal)
//!   are recorded by staff and their refunds become staff tasks.

pub mod fake;
pub mod gateway;
mod service;
pub mod stripe;

use std::{collections::HashMap, sync::Arc};

use serde_json::{Map, Value};

pub use gateway::{GatewayError, PaymentGateway, WebhookEvent, WebhookOutcome};
pub use service::*;

use crate::config::BookingConfig;

/// The configured online providers, by name (`stripe`, `paypal`, `fake`).
#[derive(Clone, Default)]
pub struct Payments {
    gateways: HashMap<&'static str, Arc<dyn PaymentGateway>>,
}

impl Payments {
    pub fn from_config(config: &BookingConfig, http: reqwest::Client) -> Self {
        let mut gateways: HashMap<&'static str, Arc<dyn PaymentGateway>> = HashMap::new();
        if let Some(stripe) = &config.stripe {
            gateways.insert(
                "stripe",
                Arc::new(stripe::StripeGateway::new(stripe.clone(), http.clone())),
            );
        }
        if config.fake_payments {
            tracing::warn!(
                "PAYMENTS_FAKE=true: payments are simulated, never enable this on a public server"
            );
            gateways.insert("fake", Arc::new(fake::FakeGateway));
        }
        Self { gateways }
    }

    pub fn with(mut self, name: &'static str, gateway: Arc<dyn PaymentGateway>) -> Self {
        self.gateways.insert(name, gateway);
        self
    }

    pub fn get(&self, name: &str) -> Option<(&'static str, Arc<dyn PaymentGateway>)> {
        self.gateways
            .get_key_value(name)
            .map(|(k, v)| (*k, v.clone()))
    }

    /// The provider used when the guest does not choose: Stripe, else the fake one.
    pub fn default_provider(&self) -> Option<(&'static str, Arc<dyn PaymentGateway>)> {
        ["stripe", "paypal", "fake"]
            .iter()
            .find_map(|name| self.get(name))
    }

    /// Provider for a stored payment (`payments.provider` + its id prefix for the fake one).
    pub fn for_payment(
        &self,
        provider: &str,
        provider_payment_id: Option<&str>,
    ) -> Option<Arc<dyn PaymentGateway>> {
        if provider == "other" && provider_payment_id.is_some_and(|id| id.starts_with("fake_")) {
            return self.gateways.get("fake").cloned();
        }
        self.gateways.get(provider).cloned()
    }

    pub fn is_enabled(&self) -> bool {
        !self.gateways.is_empty()
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.gateways.keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// `{ "stripe": { "publishable_key": ... }, "fake": {...} }` for the browser.
    pub fn public_config(&self) -> Value {
        let mut map = Map::new();
        for (name, gateway) in &self.gateways {
            map.insert((*name).to_string(), gateway.public_config());
        }
        Value::Object(map)
    }
}
