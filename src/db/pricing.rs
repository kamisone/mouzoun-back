//! 0006_pricing: guest categories, cancellation policies, rate plans, seasons, prices, taxes,
//! promotions.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{CurrencyCode, DateRange, DaysOfWeek, Locale, Money, TsRange, WithTranslations};
use crate::text_enum;

text_enum!(MealPlan { None = "none", Breakfast = "breakfast", HalfBoard = "half_board", FullBoard = "full_board" });
text_enum!(DepositType { None = "none", Percent = "percent", Fixed = "fixed", Full = "full" });
text_enum!(PaymentMode {
    PayOnSite = "pay_on_site",
    OptionalOnline = "optional_online",
    OnlineRequired = "online_required",
});
text_enum!(PricingUnit {
    PerUnitPerNight = "per_unit_per_night",
    PerPersonPerNight = "per_person_per_night",
    PerPerson = "per_person",
    PerUnit = "per_unit",
    PerBooking = "per_booking",
    PerHour = "per_hour",
    PerDay = "per_day",
});
text_enum!(TaxCalcType {
    Percent = "percent",
    FixedPerPersonPerNight = "fixed_per_person_per_night",
    FixedPerBooking = "fixed_per_booking",
    FixedPerUnit = "fixed_per_unit",
});
text_enum!(DiscountType { Percent = "percent", FixedAmount = "fixed_amount", FreeNights = "free_nights" });

/// A rate plan's deposit, decoded from its typed columns (`deposit_percent`,
/// `deposit_amount_minor` + `deposit_currency`): business code never sees a bare number whose
/// unit depends on another column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Deposit {
    None,
    Full,
    Percent { percent: Decimal },
    Fixed { amount: Money },
}

impl Deposit {
    /// `None` when the columns contradict `deposit_type` (prevented by the table's CHECKs).
    pub fn from_columns(
        kind: DepositType,
        percent: Option<Decimal>,
        amount_minor: Option<i64>,
        currency: Option<CurrencyCode>,
    ) -> Option<Self> {
        Some(match kind {
            DepositType::None => Self::None,
            DepositType::Full => Self::Full,
            DepositType::Percent => Self::Percent { percent: percent? },
            DepositType::Fixed => Self::Fixed {
                amount: Money::new(amount_minor?, currency?),
            },
        })
    }
}

/// A promotion's discount, decoded from its typed columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Discount {
    Percent { percent: Decimal },
    FixedAmount { amount: Money },
    FreeNights { nights: u32 },
}

impl Discount {
    /// `None` when the columns contradict `discount_type` (prevented by the table's CHECKs).
    pub fn from_columns(
        kind: DiscountType,
        percent: Option<Decimal>,
        amount_minor: Option<i64>,
        currency: Option<CurrencyCode>,
        free_nights: Option<i32>,
    ) -> Option<Self> {
        Some(match kind {
            DiscountType::Percent => Self::Percent { percent: percent? },
            DiscountType::FixedAmount => Self::FixedAmount {
                amount: Money::new(amount_minor?, currency?),
            },
            DiscountType::FreeNights => Self::FreeNights {
                nights: u32::try_from(free_nights?).ok()?,
            },
        })
    }
}

impl PricingUnit {
    /// Price depends on the number of guests of each category.
    pub fn is_per_person(self) -> bool {
        matches!(self, Self::PerPersonPerNight | Self::PerPerson)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct GuestCategoryRecord {
    pub id: Uuid,
    pub code: String,
    pub min_age: Option<i16>,
    pub max_age: Option<i16>,
    pub counts_toward_capacity: bool,
    pub sort_order: i32,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct GuestCategoryTranslation {
    #[serde(skip_serializing)]
    pub guest_category_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub description: Option<String>,
}

pub type GuestCategory = WithTranslations<GuestCategoryRecord, GuestCategoryTranslation>;

/// One step of a cancellation policy: cancelling at least `min_hours_before` arrival refunds
/// `refund_percent` of the amount paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationRule {
    pub min_hours_before: i32,
    pub refund_percent: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CancellationPolicyRecord {
    pub id: Uuid,
    pub code: String,
    pub rules: sqlx::types::Json<Vec<CancellationRule>>,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CancellationPolicyTranslation {
    #[serde(skip_serializing)]
    pub cancellation_policy_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub description: Option<String>,
}

pub type CancellationPolicy =
    WithTranslations<CancellationPolicyRecord, CancellationPolicyTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RatePlanRecord {
    pub id: Uuid,
    pub code: String,
    pub cancellation_policy_id: Option<Uuid>,
    pub meal_plan: MealPlan,
    pub deposit_type: DepositType,
    /// Set when `deposit_type = percent` (0 < x ≤ 100).
    pub deposit_percent: Option<Decimal>,
    /// Set (with `deposit_currency`) when `deposit_type = fixed`, in minor units.
    pub deposit_amount_minor: Option<i64>,
    pub deposit_currency: Option<CurrencyCode>,
    pub payment_mode: PaymentMode,
    pub is_public: bool,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RatePlanTranslation {
    #[serde(skip_serializing)]
    pub rate_plan_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub description: Option<String>,
}

pub type RatePlan = WithTranslations<RatePlanRecord, RatePlanTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SeasonRecord {
    pub id: Uuid,
    pub code: String,
    pub color: Option<String>,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SeasonTranslation {
    #[serde(skip_serializing)]
    pub season_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
}

pub type Season = WithTranslations<SeasonRecord, SeasonTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SeasonPeriod {
    pub id: Uuid,
    pub season_id: Uuid,
    pub period: DateRange,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Price {
    pub id: Uuid,
    pub product_id: Uuid,
    pub rate_plan_id: Option<Uuid>,
    pub season_id: Option<Uuid>,
    pub guest_category_id: Option<Uuid>,
    pub pricing_unit: PricingUnit,
    pub amount_minor: i64,
    pub currency: CurrencyCode,
    pub extra_guest_amount_minor: Option<i64>,
    pub min_quantity: i32,
    pub max_quantity: Option<i32>,
    pub min_nights: Option<i32>,
    pub days_of_week: i16,
    pub valid_period: Option<DateRange>,
    pub priority: i32,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Price {
    pub fn weekdays(&self) -> DaysOfWeek {
        DaysOfWeek(self.days_of_week)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaxRecord {
    pub id: Uuid,
    pub code: String,
    pub calc_type: TaxCalcType,
    pub rate_percent: Option<Decimal>,
    pub amount_minor: Option<i64>,
    pub currency: Option<CurrencyCode>,
    pub is_inclusive: bool,
    pub exempt_guest_category_ids: Vec<Uuid>,
    pub valid_period: Option<DateRange>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaxTranslation {
    #[serde(skip_serializing)]
    pub tax_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
}

pub type Tax = WithTranslations<TaxRecord, TaxTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PromotionRecord {
    pub id: Uuid,
    /// `None` = automatic offer.
    pub code: Option<String>,
    pub discount_type: DiscountType,
    /// Exactly one of the three is set, matching `discount_type` (see [`Discount`]).
    pub discount_percent: Option<Decimal>,
    pub discount_amount_minor: Option<i64>,
    pub discount_free_nights: Option<i32>,
    /// Currency of `discount_amount_minor` and `min_subtotal_minor`.
    pub currency: Option<CurrencyCode>,
    pub min_subtotal_minor: Option<i64>,
    pub min_nights: Option<i32>,
    pub min_days_before_arrival: Option<i32>,
    pub max_redemptions: Option<i32>,
    pub max_per_customer: Option<i32>,
    pub booking_window: Option<TsRange>,
    pub stay_window: Option<DateRange>,
    pub applies_to_all: bool,
    pub is_stackable: bool,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PromotionTranslation {
    #[serde(skip_serializing)]
    pub promotion_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub name: String,
    pub description: Option<String>,
}

pub type Promotion = WithTranslations<PromotionRecord, PromotionTranslation>;
