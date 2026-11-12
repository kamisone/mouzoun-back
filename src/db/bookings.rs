//! 0008_bookings: customers, bookings and their items, allocations, guest register, payments,
//! refunds, webhooks, invoices.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    catalog::ProductKind,
    types::{CurrencyCode, DateRange, Locale, TsRange},
};
use crate::text_enum;

text_enum!(BookingStatus {
    Draft = "draft",
    Pending = "pending",
    AwaitingPayment = "awaiting_payment",
    Confirmed = "confirmed",
    CheckedIn = "checked_in",
    Completed = "completed",
    Cancelled = "cancelled",
    NoShow = "no_show",
    Expired = "expired",
});
text_enum!(BookingSource {
    Website = "website",
    Admin = "admin",
    Phone = "phone",
    Email = "email",
    Whatsapp = "whatsapp",
    WalkIn = "walk_in",
    Ota = "ota",
    Agency = "agency",
});
text_enum!(CancelledBy { Customer = "customer", Staff = "staff", System = "system" });
text_enum!(BookingItemStatus { Active = "active", Cancelled = "cancelled" });
text_enum!(IdDocumentType {
    Passport = "passport",
    NationalId = "national_id",
    ResidencePermit = "residence_permit",
    DrivingLicence = "driving_licence",
    Other = "other",
});
text_enum!(PaymentProvider {
    Cmi = "cmi",
    Stripe = "stripe",
    Paypal = "paypal",
    Cash = "cash",
    BankTransfer = "bank_transfer",
    CardTerminal = "card_terminal",
    Other = "other",
});
text_enum!(PaymentPurpose { Deposit = "deposit", Balance = "balance", Full = "full", Extra = "extra" });
text_enum!(PaymentStatus {
    Pending = "pending",
    RequiresAction = "requires_action",
    Authorized = "authorized",
    Succeeded = "succeeded",
    Failed = "failed",
    Cancelled = "cancelled",
    Refunded = "refunded",
    PartiallyRefunded = "partially_refunded",
});
text_enum!(RefundStatus { Pending = "pending", Succeeded = "succeeded", Failed = "failed" });
text_enum!(InvoiceKind { Invoice = "invoice", CreditNote = "credit_note", Receipt = "receipt" });
text_enum!(PaymentOption { Full = "full", Deposit = "deposit", OnSite = "on_site" });
text_enum!(BalanceCollection {
    SavedCard = "saved_card",
    PaymentLink = "payment_link",
    OnSite = "on_site",
});
text_enum!(ChangeRequestKind { Dates = "dates", Guests = "guests", Items = "items", Other = "other" });
text_enum!(ChangeRequestStatus {
    Pending = "pending",
    Approved = "approved",
    Declined = "declined",
    Cancelled = "cancelled",
    Applied = "applied",
});
text_enum!(RequestedBy { Customer = "customer", Staff = "staff" });
text_enum!(StaffTaskKind {
    RefundOffline = "refund_offline",
    BalanceFailed = "balance_failed",
    NoShowReview = "no_show_review",
    ChangeRequest = "change_request",
    PaymentAfterExpiry = "payment_after_expiry",
    WebhookError = "webhook_error",
    Other = "other",
});
text_enum!(StaffTaskStatus { Open = "open", Done = "done", Dismissed = "dismissed" });

impl BookingStatus {
    /// Statuses in which the booking still holds inventory and can be cancelled.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Self::Draft | Self::Pending | Self::AwaitingPayment | Self::Confirmed | Self::CheckedIn
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub user_id: Option<Uuid>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: String,
    pub last_name: Option<String>,
    pub country_code: Option<String>,
    pub preferred_locale: Option<Locale>,
    pub marketing_consent: bool,
    pub marketing_consent_at: Option<DateTime<Utc>>,
    pub internal_notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Booking {
    pub id: Uuid,
    pub reference: String,
    pub customer_id: Uuid,
    pub status: BookingStatus,
    pub source: BookingSource,
    pub external_reference: Option<String>,
    pub locale: Locale,
    pub currency: CurrencyCode,
    pub arrival_date: Option<NaiveDate>,
    pub departure_date: Option<NaiveDate>,
    pub adults: i16,
    pub children: i16,
    pub infants: i16,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub tax_minor: i64,
    pub total_minor: i64,
    pub amount_paid_minor: i64,
    pub deposit_due_minor: i64,
    pub balance_due_on: Option<NaiveDate>,
    pub rate_plan_id: Option<Uuid>,
    pub cancellation_policy_snapshot: Option<serde_json::Value>,
    pub hold_expires_at: Option<DateTime<Utc>>,
    pub estimated_arrival_time: Option<NaiveTime>,
    pub special_requests: Option<String>,
    pub internal_notes: Option<String>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub cancellation_reason: Option<String>,
    pub cancelled_by: Option<CancelledBy>,
    pub created_by: Option<Uuid>,
    pub ip_address: Option<IpNetwork>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BookingItem {
    pub id: Uuid,
    pub booking_id: Uuid,
    pub parent_item_id: Option<Uuid>,
    pub product_id: Uuid,
    pub product_kind: ProductKind,
    pub rate_plan_id: Option<Uuid>,
    pub activity_slot_id: Option<Uuid>,
    pub service_period: Option<DateRange>,
    pub starts_at: Option<DateTime<Utc>>,
    pub quantity: i32,
    pub unit_price_minor: i64,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub tax_minor: i64,
    pub total_minor: i64,
    pub price_breakdown: serde_json::Value,
    pub product_snapshot: serde_json::Value,
    pub status: BookingItemStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BookingItemGuest {
    pub booking_item_id: Uuid,
    pub guest_category_id: Uuid,
    pub guest_count: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UnitAllocation {
    pub id: Uuid,
    pub booking_item_id: Uuid,
    pub unit_id: Uuid,
    pub stay: DateRange,
    pub released_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ResourceAssignment {
    pub id: Uuid,
    pub resource_id: Uuid,
    pub booking_item_id: Option<Uuid>,
    pub activity_slot_id: Option<Uuid>,
    pub period: TsRange,
    pub released_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Guest register ("fiche de police"). The identity document number is encrypted by the
/// application and never serialised.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BookingGuest {
    pub id: Uuid,
    pub booking_id: Uuid,
    pub guest_category_id: Option<Uuid>,
    pub is_lead: bool,
    pub first_name: String,
    pub last_name: String,
    pub nationality: Option<String>,
    pub date_of_birth: Option<NaiveDate>,
    pub id_document_type: Option<IdDocumentType>,
    #[serde(skip)]
    pub id_document_number_enc: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BookingStatusChange {
    pub id: i64,
    pub booking_id: Uuid,
    pub from_status: Option<BookingStatus>,
    pub to_status: BookingStatus,
    pub changed_by: Option<Uuid>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PromotionRedemption {
    pub id: Uuid,
    pub promotion_id: Uuid,
    pub booking_id: Uuid,
    pub customer_id: Option<Uuid>,
    pub discount_minor: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Payment {
    pub id: Uuid,
    pub booking_id: Uuid,
    pub provider: PaymentProvider,
    pub purpose: PaymentPurpose,
    pub status: PaymentStatus,
    pub amount_minor: i64,
    pub currency: CurrencyCode,
    pub provider_payment_id: Option<String>,
    pub provider_payload: serde_json::Value,
    pub idempotency_key: String,
    pub failure_reason: Option<String>,
    pub paid_at: Option<DateTime<Utc>>,
    pub received_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Refund {
    pub id: Uuid,
    pub payment_id: Uuid,
    pub amount_minor: i64,
    pub status: RefundStatus,
    pub reason: Option<String>,
    pub provider_refund_id: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PaymentWebhookEvent {
    pub id: i64,
    pub provider: String,
    pub event_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub received_at: DateTime<Utc>,
    pub processed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub number: String,
    pub booking_id: Uuid,
    pub kind: InvoiceKind,
    pub billing_details: serde_json::Value,
    pub lines: serde_json::Value,
    pub subtotal_minor: i64,
    pub tax_minor: i64,
    pub total_minor: i64,
    pub currency: CurrencyCode,
    pub pdf_media_id: Option<Uuid>,
    pub issued_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
