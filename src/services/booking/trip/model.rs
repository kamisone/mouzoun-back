//! Request and quote types of a trip (one booking = stay + activities + extras + packages).

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    db::{
        bookings::PaymentOption,
        catalog::ProductKind,
        pricing::{MealPlan, PaymentMode},
        types::CurrencyCode,
    },
    services::pricing::{GuestCount, engine::PriceLine},
};

fn one() -> i32 {
    1
}

/// One thing the guest wants. Indexes (`parent`) refer to positions in `TripRequest::items`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TripItem {
    Stay {
        product_id: Uuid,
        arrival: NaiveDate,
        departure: NaiveDate,
        /// Units (tents, pitches).
        #[serde(default = "one")]
        quantity: i32,
        guests: Vec<GuestCount>,
        /// Specific pitches picked on the site map (empty = best available, assigned by the camp).
        #[serde(default)]
        unit_ids: Vec<Uuid>,
    },
    Activity {
        product_id: Uuid,
        /// Fixed-slot activities.
        #[serde(default)]
        slot_id: Option<Uuid>,
        /// `on_request` / `anytime` activities: the day, and the preferred time.
        #[serde(default)]
        date: Option<NaiveDate>,
        #[serde(default)]
        time: Option<NaiveTime>,
        guests: Vec<GuestCount>,
        #[serde(default = "one")]
        quantity: i32,
        #[serde(default)]
        language: Option<String>,
    },
    Extra {
        product_id: Uuid,
        /// Stay (or activity) this extra belongs to (index in the request).
        #[serde(default)]
        parent: Option<usize>,
        #[serde(default)]
        date: Option<NaiveDate>,
        #[serde(default = "one")]
        quantity: i32,
    },
    Package {
        product_id: Uuid,
        arrival: NaiveDate,
        guests: Vec<GuestCount>,
    },
}

impl TripItem {
    pub fn product_id(&self) -> Uuid {
        match self {
            Self::Stay { product_id, .. }
            | Self::Activity { product_id, .. }
            | Self::Extra { product_id, .. }
            | Self::Package { product_id, .. } => *product_id,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripRequest {
    pub items: Vec<TripItem>,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub promo_code: Option<String>,
    /// Ages of the children (age-dependent rules such as an activity's minimum age).
    #[serde(default)]
    pub child_ages: Vec<u8>,
}

/// Why an item cannot be sold as requested (stable codes, translated by the front).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemIssue {
    pub code: String,
    pub message: String,
    /// Extra facts for the message (`{"min_nights": 2}`, `{"left": 1}`...).
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl ItemIssue {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }

    pub fn with(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedDiscount {
    pub promotion_id: Uuid,
    pub code: Option<String>,
    pub name: Option<String>,
    pub amount_minor: i64,
}

/// One priced line of the trip, in the order of the request; derived items (package parts,
/// mandatory extras, pitch-selection fee) follow their parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotedItem {
    /// Position in `TripQuote::items`.
    pub index: usize,
    /// Position in the request (`None` for derived items).
    pub request_index: Option<usize>,
    pub parent_index: Option<usize>,
    pub kind: ProductKind,
    pub product_id: Uuid,
    pub name: String,
    pub slug: Option<String>,
    /// Added by the camp's rules (mandatory extra, pitch fee): cannot be removed.
    pub mandatory: bool,
    /// Part of a package: priced in the package.
    pub in_package: bool,
    /// Needs the camp's confirmation (product or activity on request).
    pub on_request: bool,
    pub quantity: i32,
    pub guests: Vec<GuestCount>,
    pub arrival: Option<NaiveDate>,
    pub departure: Option<NaiveDate>,
    pub date: Option<NaiveDate>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub slot_id: Option<Uuid>,
    pub time: Option<NaiveTime>,
    pub language: Option<String>,
    pub unit_ids: Vec<Uuid>,
    /// Units / seats left after this item (honest urgency, `None` = not limited).
    pub remaining: Option<i32>,
    pub lines: Vec<PriceLine>,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub discounts: Vec<AppliedDiscount>,
    /// Taxes added on top (tourist tax...).
    pub tax_minor: i64,
    /// Taxes already contained in the price.
    pub included_tax_minor: i64,
    pub taxes: Vec<crate::services::pricing::engine::TaxLine>,
    pub total_minor: i64,
    pub issue: Option<ItemIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatePlanSummary {
    pub id: Uuid,
    pub code: String,
    pub name: Option<String>,
    pub meal_plan: MealPlan,
    pub payment_mode: PaymentMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentChoice {
    pub option: PaymentOption,
    pub due_now_minor: i64,
    pub due_later_minor: i64,
    /// When the remainder is due (deposit option), `None` = at the camp.
    pub due_later_on: Option<NaiveDate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancellationStep {
    /// Refund (of the total) when cancelled before `until`; the last step has `until = None`.
    pub refund_percent: i64,
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromoStatus {
    pub code: String,
    /// `applied`, `invalid` (with `reason`) or `not_combinable`.
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripQuote {
    pub currency: CurrencyCode,
    pub items: Vec<QuotedItem>,
    pub rate_plan: Option<RatePlanSummary>,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub discounts: Vec<AppliedDiscount>,
    pub tax_minor: i64,
    pub included_tax_minor: i64,
    pub total_minor: i64,
    pub payment_options: Vec<PaymentChoice>,
    /// The booking is a request the camp must accept (no charge now).
    pub is_request: bool,
    /// Every item confirmed at once.
    pub instant_confirmation: bool,
    pub cancellation: Vec<CancellationStep>,
    pub promo: Option<PromoStatus>,
    /// Trip start (first night / first activity), Africa/Casablanca.
    pub starts_on: Option<NaiveDate>,
    pub ends_on: Option<NaiveDate>,
    /// Some item cannot be sold as requested: see `items[].issue`.
    pub has_issues: bool,
}

impl TripQuote {
    pub fn choice(&self, option: PaymentOption) -> Option<&PaymentChoice> {
        self.payment_options.iter().find(|c| c.option == option)
    }
}
