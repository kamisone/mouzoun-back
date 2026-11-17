//! Shop and shop-payment settings (`settings` rows `shop.*`, `payments.*`), read in one query.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::db::shop::ShopPaymentMethod;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShopStatus {
    Closed,
    CatalogOnly,
    Open,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentSettings {
    pub stripe_enabled: bool,
    pub stripe_checkout_mode: String,
    pub statement_descriptor: String,
    pub cod_enabled: bool,
    pub cod_max_order_minor: Option<i64>,
    pub pay_at_pickup_enabled: bool,
    pub bank_transfer_enabled: bool,
    /// Localized bank details `{fr, en, ar}`.
    pub bank_transfer_details: Value,
    pub bank_transfer_hold_days: i64,
    /// Super admin override of the French cash-register rule (NF525).
    pub allow_cash_fr: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShopSettings {
    pub status: ShopStatus,
    pub currency: String,
    pub prices_include_tax: bool,
    pub guest_checkout: bool,
    pub checkout_hold_minutes: i64,
    pub cart_ttl_days: i64,
    pub low_stock_threshold: i64,
    pub min_order_minor: i64,
    pub max_quantity_per_line: i64,
    pub ships_to_countries: Vec<String>,
    pub carriers: Vec<String>,
    pub withdrawal_days: i64,
    pub stay_delivery_enabled: bool,
    pub stay_delivery_fee_minor: i64,
    pub abandoned_cart_hours: i64,
    pub abandoned_cart_requires_consent: bool,
    pub notify_emails: Vec<String>,
    /// `{fr: "boutique", en: "shop", ...}`.
    pub route_prefix: Value,
    pub payments: PaymentSettings,
    /// `legal_entities.code` of the seller (`mouzoun_fr`).
    pub legal_entity: String,
    pub refund_deadline_days: i64,
    pub legal_guarantee_years: i64,
    /// `{name, url}`.
    pub consumer_mediator: Value,
    pub checkout_button_label: Value,
    pub eu_countries: Vec<String>,
    pub tax: TaxSettings,
    pub customs: CustomsSettings,
    /// `legal.*_page_id` (cgv, notice, privacy, cookies, withdrawal, shipping).
    pub legal_pages: HashMap<String, Uuid>,
    pub data_retention: Value,
    /// Country of the selling entity (`FR`): decides the payment locks and fulfilment methods.
    pub entity_country: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaxSettings {
    pub oss_enabled: bool,
    pub oss_threshold_minor: i64,
    pub keep_gross_price_constant: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomsSettings {
    pub ioss_enabled: bool,
    pub ioss_number: String,
}

impl ShopSettings {
    pub fn is_open(&self) -> bool {
        self.status == ShopStatus::Open
    }

    pub fn is_visible(&self) -> bool {
        self.status != ShopStatus::Closed
    }

    /// Cash methods are locked for a French seller (NF525 cash-register rules) unless a super
    /// admin set `payments.allow_cash_fr`.
    pub fn cash_allowed(&self, entity_country: &str) -> bool {
        entity_country != "FR" || self.payments.allow_cash_fr
    }

    pub fn is_eu(&self, country: &str) -> bool {
        self.eu_countries
            .iter()
            .any(|c| c.eq_ignore_ascii_case(country))
    }

    /// Payment methods switched on by the admin. Stripe also needs its keys: `stripe_available`.
    pub fn enabled_methods(&self, stripe_available: bool) -> Vec<ShopPaymentMethod> {
        self.enabled_methods_for(stripe_available, &self.entity_country)
    }

    pub fn enabled_methods_for(
        &self,
        stripe_available: bool,
        entity_country: &str,
    ) -> Vec<ShopPaymentMethod> {
        let p = &self.payments;
        let mut out = Vec::new();
        if p.stripe_enabled && stripe_available {
            out.push(ShopPaymentMethod::Stripe);
        }
        let cash = self.cash_allowed(entity_country);
        if p.cod_enabled && cash {
            out.push(ShopPaymentMethod::CashOnDelivery);
        }
        if p.pay_at_pickup_enabled && cash {
            out.push(ShopPaymentMethod::PayAtPickup);
        }
        if p.bank_transfer_enabled {
            out.push(ShopPaymentMethod::BankTransfer);
        }
        out
    }

    /// Text of a localized setting in `locale`, falling back to the first non-empty value.
    pub fn localized(value: &Value, locale: &str) -> String {
        value
            .get(locale)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                value.as_object().and_then(|m| {
                    m.values()
                        .filter_map(Value::as_str)
                        .find(|s| !s.trim().is_empty())
                })
            })
            .unwrap_or_default()
            .to_string()
    }
}

pub async fn load(db: impl PgExecutor<'_>) -> sqlx::Result<ShopSettings> {
    let rows = sqlx::query!(
        "SELECT key, value FROM settings WHERE key LIKE 'shop.%' OR key LIKE 'payments.%' OR key LIKE 'tax.%'
             OR key LIKE 'customs.%' OR key LIKE 'legal.%' OR key = 'privacy.data_retention'
         UNION ALL
         SELECT '__entity_country', to_jsonb(country_code::text) FROM legal_entities WHERE id = shop_entity_id()"
    )
    .fetch_all(db)
    .await?;
    let map: HashMap<String, Value> = rows
        .into_iter()
        .filter_map(|r| Some((r.key?, r.value?)))
        .collect();
    Ok(from_map(&map))
}

fn from_map(map: &HashMap<String, Value>) -> ShopSettings {
    let get = |k: &str| map.get(k).cloned().unwrap_or(Value::Null);
    let int = |k: &str, d: i64| get(k).as_i64().unwrap_or(d);
    let boolean = |k: &str, d: bool| get(k).as_bool().unwrap_or(d);
    let string = |k: &str, d: &str| get(k).as_str().unwrap_or(d).to_string();
    let strings = |k: &str| -> Vec<String> {
        get(k)
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    ShopSettings {
        status: match get("shop.status").as_str() {
            Some("open") => ShopStatus::Open,
            Some("closed") => ShopStatus::Closed,
            _ => ShopStatus::CatalogOnly,
        },
        currency: string("shop.currency", "EUR"),
        prices_include_tax: boolean("shop.prices_include_tax", true),
        guest_checkout: boolean("shop.guest_checkout", true),
        checkout_hold_minutes: int("shop.checkout_hold_minutes", 30).max(30),
        cart_ttl_days: int("shop.cart_ttl_days", 30).max(1),
        low_stock_threshold: int("shop.low_stock_threshold", 5).max(0),
        min_order_minor: int("shop.min_order_minor", 0).max(0),
        max_quantity_per_line: int("shop.max_quantity_per_line", 20).clamp(1, 999),
        ships_to_countries: strings("shop.ships_to_countries"),
        carriers: strings("shop.carriers"),
        withdrawal_days: int("shop.withdrawal_days", 7).max(0),
        stay_delivery_enabled: boolean("shop.stay_delivery_enabled", true),
        stay_delivery_fee_minor: int("shop.stay_delivery_fee_minor", 0).max(0),
        abandoned_cart_hours: int("shop.abandoned_cart_hours", 4).max(1),
        abandoned_cart_requires_consent: boolean("shop.abandoned_cart_requires_consent", true),
        notify_emails: strings("shop.notify_emails"),
        route_prefix: get("shop.route_prefix"),
        legal_entity: string("shop.legal_entity", "mouzoun_fr"),
        refund_deadline_days: int("shop.refund_deadline_days", 14).max(1),
        legal_guarantee_years: int("shop.legal_guarantee_years", 2).max(1),
        consumer_mediator: get("shop.consumer_mediator"),
        checkout_button_label: get("shop.checkout_button_label"),
        eu_countries: strings("shop.eu_countries"),
        tax: TaxSettings {
            oss_enabled: boolean("tax.oss_enabled", false),
            oss_threshold_minor: int("tax.oss_threshold_minor", 1_000_000),
            keep_gross_price_constant: boolean("tax.keep_gross_price_constant", true),
        },
        customs: CustomsSettings {
            ioss_enabled: boolean("customs.ioss_enabled", false),
            ioss_number: string("customs.ioss_number", ""),
        },
        legal_pages: [
            "cgv",
            "notice",
            "privacy",
            "cookies",
            "withdrawal",
            "shipping",
        ]
        .iter()
        .filter_map(|k| {
            get(&format!("legal.{k}_page_id"))
                .as_str()
                .and_then(|s| s.parse().ok())
                .map(|id| ((*k).to_string(), id))
        })
        .collect(),
        data_retention: get("privacy.data_retention"),
        entity_country: string("__entity_country", "FR"),
        payments: PaymentSettings {
            stripe_enabled: boolean("payments.stripe.enabled", false),
            stripe_checkout_mode: string("payments.stripe.checkout_mode", "hosted"),
            statement_descriptor: string("payments.stripe.statement_descriptor", "MOUZOUN"),
            cod_enabled: boolean("payments.cod.enabled", false),
            cod_max_order_minor: get("payments.cod.max_order_minor").as_i64(),
            pay_at_pickup_enabled: boolean("payments.pay_at_pickup.enabled", true),
            bank_transfer_enabled: boolean("payments.bank_transfer.enabled", false),
            bank_transfer_details: get("payments.bank_transfer.details"),
            bank_transfer_hold_days: int("payments.bank_transfer.hold_days", 5).max(1),
            allow_cash_fr: boolean("payments.allow_cash_fr", false),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn missing_or_invalid_values_fall_back_to_safe_defaults() {
        let mut map = HashMap::new();
        map.insert("shop.status".to_string(), json!("open"));
        map.insert("shop.checkout_hold_minutes".to_string(), json!(5));
        map.insert("payments.stripe.enabled".to_string(), json!(true));
        let s = from_map(&map);
        assert!(s.is_open());
        assert_eq!(s.checkout_hold_minutes, 30, "Stripe minimum");
        assert_eq!(
            s.enabled_methods(false),
            vec![],
            "Stripe hidden without keys, no cash for a French seller"
        );
        assert_eq!(s.enabled_methods(true)[0], ShopPaymentMethod::Stripe);
        // A French seller cannot take cash unless a super admin allowed it.
        assert_eq!(s.enabled_methods_for(false, "FR"), vec![]);
        assert_eq!(
            s.enabled_methods_for(false, "MA"),
            vec![ShopPaymentMethod::PayAtPickup]
        );
        assert_eq!(from_map(&HashMap::new()).status, ShopStatus::CatalogOnly);
    }
}
