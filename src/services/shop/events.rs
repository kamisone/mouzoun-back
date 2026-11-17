//! Outbox events of the shop. Payloads carry ids only; handlers reload the current state.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

pub mod types {
    pub const ORDER_PLACED: &str = "order.placed";
    pub const ORDER_PAID: &str = "order.paid";
    pub const ORDER_CONFIRMED: &str = "order.confirmed";
    pub const ORDER_CANCELLED: &str = "order.cancelled";
    pub const ORDER_FULFILLMENT_UPDATED: &str = "order.fulfillment_updated";
    pub const ORDER_REFUND_ISSUED: &str = "order.refund_issued";
    pub const ORDER_RETURN_UPDATED: &str = "order.return_updated";
    pub const ORDER_PAYMENT_ISSUE: &str = "order.payment_issue";
    pub const ORDER_EMAIL_RESENT: &str = "order.email_resent";
    pub const STOCK_LOW: &str = "stock.low";
    pub const STOCK_BACK_IN_STOCK: &str = "stock.back_in_stock";
    pub const STOCK_CHANGED: &str = "stock.changed";
    pub const VARIANT_UPDATED: &str = "variant.updated";
    pub const PROMOTION_UPDATED: &str = "promotion.updated";
    pub const SHOP_CART_ABANDONED: &str = "shop_cart.abandoned";
    pub const PAYMENT_WEBHOOK_RECEIVED: &str = "payment_webhook.received";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShopEvent {
    /// Checkout done (any payment method): confirmation / payment instructions, staff alert.
    OrderPlaced {
        order_id: Uuid,
    },
    /// The order is paid (webhook or offline payment): receipt, staff alert, invoice.
    OrderPaid {
        order_id: Uuid,
    },
    /// A cash-on-delivery order was confirmed by staff.
    OrderConfirmed {
        order_id: Uuid,
    },
    OrderCancelled {
        order_id: Uuid,
    },
    /// A fulfilment changed status (shipped, ready for pickup, delivered...).
    FulfillmentUpdated {
        order_id: Uuid,
        fulfillment_id: Uuid,
    },
    RefundIssued {
        order_id: Uuid,
        refund_id: Uuid,
    },
    ReturnUpdated {
        order_id: Uuid,
        return_id: Uuid,
    },
    /// Paid amount differs, dispute...: staff must check before shipping.
    PaymentIssue {
        order_id: Uuid,
        issue: String,
    },
    /// Staff asked to send a customer email again.
    EmailResent {
        order_id: Uuid,
        template: String,
    },
    StockLow {
        variant_id: Uuid,
        location_id: Uuid,
    },
    StockBackInStock {
        variant_id: Uuid,
    },
    /// The availability status of a variant changed (cache invalidation).
    StockChanged {
        product_id: Uuid,
    },
    VariantUpdated {
        product_id: Uuid,
    },
    PromotionUpdated {
        promotion_id: Uuid,
    },
    CartAbandoned {
        cart_id: Uuid,
    },
    /// A provider webhook was stored: the worker applies it.
    PaymentWebhookReceived {
        provider: String,
        event_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderPayload {
    pub order_id: Uuid,
    #[serde(default)]
    pub fulfillment_id: Option<Uuid>,
    #[serde(default)]
    pub refund_id: Option<Uuid>,
    #[serde(default)]
    pub return_id: Option<Uuid>,
    #[serde(default)]
    pub issue: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StockPayload {
    #[serde(default)]
    pub variant_id: Option<Uuid>,
    #[serde(default)]
    pub location_id: Option<Uuid>,
    #[serde(default)]
    pub product_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookPayload {
    pub provider: String,
    pub event_id: String,
}

impl ShopEvent {
    pub fn event_type(&self) -> &'static str {
        use types::*;
        match self {
            Self::OrderPlaced { .. } => ORDER_PLACED,
            Self::OrderPaid { .. } => ORDER_PAID,
            Self::OrderConfirmed { .. } => ORDER_CONFIRMED,
            Self::OrderCancelled { .. } => ORDER_CANCELLED,
            Self::FulfillmentUpdated { .. } => ORDER_FULFILLMENT_UPDATED,
            Self::RefundIssued { .. } => ORDER_REFUND_ISSUED,
            Self::ReturnUpdated { .. } => ORDER_RETURN_UPDATED,
            Self::PaymentIssue { .. } => ORDER_PAYMENT_ISSUE,
            Self::EmailResent { .. } => ORDER_EMAIL_RESENT,
            Self::StockLow { .. } => STOCK_LOW,
            Self::StockBackInStock { .. } => STOCK_BACK_IN_STOCK,
            Self::StockChanged { .. } => STOCK_CHANGED,
            Self::VariantUpdated { .. } => VARIANT_UPDATED,
            Self::PromotionUpdated { .. } => PROMOTION_UPDATED,
            Self::CartAbandoned { .. } => SHOP_CART_ABANDONED,
            Self::PaymentWebhookReceived { .. } => PAYMENT_WEBHOOK_RECEIVED,
        }
    }

    /// `(aggregate_type, aggregate_id, payload)`.
    pub fn parts(&self) -> (&'static str, String, Value) {
        match self {
            Self::OrderPlaced { order_id }
            | Self::OrderPaid { order_id }
            | Self::OrderConfirmed { order_id }
            | Self::OrderCancelled { order_id } => (
                "order",
                order_id.to_string(),
                json!({ "order_id": order_id }),
            ),
            Self::FulfillmentUpdated {
                order_id,
                fulfillment_id,
            } => (
                "order",
                order_id.to_string(),
                json!({ "order_id": order_id, "fulfillment_id": fulfillment_id }),
            ),
            Self::RefundIssued {
                order_id,
                refund_id,
            } => (
                "order",
                order_id.to_string(),
                json!({ "order_id": order_id, "refund_id": refund_id }),
            ),
            Self::ReturnUpdated {
                order_id,
                return_id,
            } => (
                "order",
                order_id.to_string(),
                json!({ "order_id": order_id, "return_id": return_id }),
            ),
            Self::PaymentIssue { order_id, issue } => (
                "order",
                order_id.to_string(),
                json!({ "order_id": order_id, "issue": issue }),
            ),
            Self::EmailResent { order_id, template } => (
                "order",
                order_id.to_string(),
                json!({ "order_id": order_id, "template": template }),
            ),
            Self::StockLow {
                variant_id,
                location_id,
            } => (
                "variant",
                variant_id.to_string(),
                json!({ "variant_id": variant_id, "location_id": location_id }),
            ),
            Self::StockBackInStock { variant_id } => (
                "variant",
                variant_id.to_string(),
                json!({ "variant_id": variant_id }),
            ),
            Self::StockChanged { product_id } | Self::VariantUpdated { product_id } => (
                "product",
                product_id.to_string(),
                json!({ "product_id": product_id }),
            ),
            Self::PromotionUpdated { promotion_id } => (
                "promotion",
                promotion_id.to_string(),
                json!({ "promotion_id": promotion_id }),
            ),
            Self::CartAbandoned { cart_id } => {
                ("cart", cart_id.to_string(), json!({ "cart_id": cart_id }))
            }
            Self::PaymentWebhookReceived { provider, event_id } => (
                "payment_webhook",
                format!("{provider}:{event_id}"),
                json!({ "provider": provider, "event_id": event_id }),
            ),
        }
    }
}

impl From<ShopEvent> for crate::services::outbox::Event {
    fn from(e: ShopEvent) -> Self {
        Self::Shop(e)
    }
}

/// Writes a shop event in the caller's transaction.
pub async fn enqueue(conn: &mut sqlx::PgConnection, event: ShopEvent) -> sqlx::Result<i64> {
    crate::services::outbox::enqueue(conn, &event.into()).await
}
