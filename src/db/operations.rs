//! 0010_operations: inquiries, newsletter, transactional emails, outbox, audit log.

use chrono::{DateTime, Utc};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{DateRange, Locale, WithTranslations};
use crate::text_enum;

text_enum!(InquiryKind {
    Contact = "contact",
    Quote = "quote",
    Group = "group",
    Event = "event",
    Partnership = "partnership",
    Press = "press",
});
text_enum!(InquiryStatus {
    New = "new",
    InProgress = "in_progress",
    Answered = "answered",
    Converted = "converted",
    Closed = "closed",
    Spam = "spam",
});
text_enum!(SubscriberStatus {
    Pending = "pending",
    Subscribed = "subscribed",
    Unsubscribed = "unsubscribed",
    Bounced = "bounced",
});
text_enum!(EmailStatus {
    Queued = "queued",
    Sending = "sending",
    Sent = "sent",
    Delivered = "delivered",
    Bounced = "bounced",
    Failed = "failed",
});
text_enum!(ActorType { User = "user", Customer = "customer", System = "system", Webhook = "webhook" });

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Inquiry {
    pub id: Uuid,
    pub kind: InquiryKind,
    pub customer_id: Option<Uuid>,
    pub product_id: Option<Uuid>,
    pub name: String,
    pub email: String,
    pub phone: Option<String>,
    pub locale: Option<Locale>,
    pub subject: Option<String>,
    pub message: String,
    pub preferred_dates: Option<DateRange>,
    pub group_size: Option<i32>,
    pub status: InquiryStatus,
    pub assigned_to: Option<Uuid>,
    pub converted_booking_id: Option<Uuid>,
    pub ip_address: Option<IpNetwork>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NewsletterSubscriber {
    pub id: Uuid,
    pub email: String,
    pub locale: Option<Locale>,
    pub status: SubscriberStatus,
    pub source: Option<String>,
    #[serde(skip)]
    pub confirm_token_hash: Option<Vec<u8>>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub unsubscribed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EmailTemplateRecord {
    pub id: Uuid,
    pub code: String,
    pub is_active: bool,
    pub variables: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EmailTemplateTranslation {
    #[serde(skip_serializing)]
    pub email_template_id: Uuid,
    #[serde(skip_serializing)]
    pub locale: Locale,
    pub subject: String,
    pub body_html: String,
    pub body_text: String,
}

pub type EmailTemplate = WithTranslations<EmailTemplateRecord, EmailTemplateTranslation>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EmailLog {
    pub id: i64,
    pub template_code: Option<String>,
    pub to_email: String,
    pub booking_id: Option<Uuid>,
    /// The event that caused this email; `UNIQUE (outbox_event_id, to_email, template_code)`.
    pub outbox_event_id: Option<i64>,
    pub locale: Option<Locale>,
    pub subject: Option<String>,
    pub provider_message_id: Option<String>,
    pub status: EmailStatus,
    pub error: Option<String>,
    pub attempts: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OutboxEvent {
    pub id: i64,
    pub aggregate_type: String,
    pub aggregate_id: String,
    /// e.g. `booking.confirmed`
    pub event_type: String,
    pub payload: serde_json::Value,
    pub available_at: DateTime<Utc>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
    /// Dead: given up on, until replayed.
    pub failed_at: Option<DateTime<Utc>>,
    pub dedupe_key: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditLog {
    pub id: i64,
    pub actor_type: ActorType,
    pub actor_user_id: Option<Uuid>,
    /// e.g. `product.update`
    pub action: String,
    pub entity_type: String,
    pub entity_id: String,
    /// `{ "before": ..., "after": ... }`
    pub changes: serde_json::Value,
    pub ip_address: Option<IpNetwork>,
    pub user_agent: Option<String>,
    pub request_id: Option<String>,
    pub created_at: DateTime<Utc>,
}
