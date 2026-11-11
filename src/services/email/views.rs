//! Template variables, built from plain facts. Used both for real emails (facts loaded from
//! the database) and for previews (sample facts), so a preview shows exactly what guests get.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{labels, links};

/// The `site` variable, available to every template.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SiteInfo {
    pub name: String,
    pub url: String,
    pub logo_url: Option<String>,
    pub tagline: Option<String>,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub whatsapp: Option<String>,
    pub whatsapp_url: Option<String>,
    pub email: Option<String>,
    /// Google Maps link to the camp.
    pub map_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GuestFacts {
    pub first_name: String,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BookingFacts {
    pub id: Uuid,
    pub reference: String,
    pub status: String,
    pub currency: String,
    pub arrival: Option<NaiveDate>,
    pub departure: Option<NaiveDate>,
    pub adults: i64,
    pub children: i64,
    pub infants: i64,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub tax_minor: i64,
    pub total_minor: i64,
    pub amount_paid_minor: i64,
    pub deposit_due_minor: i64,
    pub balance_due_on: Option<NaiveDate>,
    pub hold_expires_at: Option<DateTime<Utc>>,
    pub special_requests: Option<String>,
    pub estimated_arrival_time: Option<NaiveTime>,
    /// Earliest check-in time of the stay's accommodations.
    pub check_in_from: Option<NaiveTime>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub cancellation_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    /// `bookings.cancellation_policy_snapshot`.
    pub policy_rules: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct LineFacts {
    pub name: String,
    pub kind: String,
    pub quantity: i64,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub starts_at: Option<DateTime<Utc>>,
    pub participants: i64,
    pub total_minor: i64,
}

#[derive(Debug, Clone)]
pub struct PaymentFacts {
    pub amount_minor: i64,
    pub currency: String,
    pub provider: String,
    pub purpose: String,
    pub paid_at: Option<DateTime<Utc>>,
    pub provider_reference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InquiryFacts {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub email: String,
    pub phone: Option<String>,
    pub subject: Option<String>,
    pub message: String,
    pub preferred_from: Option<NaiveDate>,
    pub preferred_to: Option<NaiveDate>,
    pub group_size: Option<i32>,
    pub created_at: DateTime<Utc>,
}

/// Inputs shared by every view builder.
pub struct ViewEnv<'a> {
    pub locale: &'a str,
    pub site_url: &'a str,
    pub link_secret: &'a str,
}

pub fn guest(g: &GuestFacts) -> Value {
    let full_name = match &g.last_name {
        Some(last) if !last.is_empty() => format!("{} {last}", g.first_name),
        _ => g.first_name.clone(),
    };
    json!({
        "first_name": g.first_name, "last_name": g.last_name, "full_name": full_name,
        "email": g.email, "phone": g.phone,
    })
}

/// `booking`, `lines` and `policy` variables.
pub fn booking(b: &BookingFacts, lines: &[LineFacts], env: &ViewEnv<'_>) -> (Value, Value, Value) {
    let token = links::booking_token(env.link_secret, b.id);
    let nights = match (b.arrival, b.departure) {
        (Some(a), Some(d)) => (d - a).num_days(),
        _ => 0,
    };
    let balance = (b.total_minor - b.amount_paid_minor).max(0);
    let booking = json!({
        "reference": b.reference,
        "status": b.status,
        "url": links::booking_url(env.site_url, env.locale, &b.reference, &token),
        "review_url": links::review_url(env.site_url, env.locale, &b.reference, &token),
        "arrival_date": b.arrival.map(|d| d.to_string()),
        "departure_date": b.departure.map(|d| d.to_string()),
        "nights": nights,
        "nights_label": labels::nights_label(nights, env.locale),
        "adults": b.adults,
        "children": b.children,
        "infants": b.infants,
        "guests_label": labels::guests_label(b.adults, b.children, b.infants, env.locale),
        "currency": b.currency,
        "subtotal_minor": b.subtotal_minor,
        "discount_minor": b.discount_minor,
        "tax_minor": b.tax_minor,
        "total_minor": b.total_minor,
        "amount_paid_minor": b.amount_paid_minor,
        "balance_minor": balance,
        "deposit_due_minor": b.deposit_due_minor,
        "deposit_remaining_minor": (b.deposit_due_minor - b.amount_paid_minor).max(0),
        "balance_due_on": b.balance_due_on.map(|d| d.to_string()),
        "hold_expires_at": b.hold_expires_at.map(|t| t.to_rfc3339()),
        "special_requests": b.special_requests,
        "estimated_arrival_time": b.estimated_arrival_time.map(|t| t.format("%H:%M").to_string()),
        "check_in_from": b.check_in_from.map(|t| t.format("%H:%M").to_string()),
        "cancelled_at": b.cancelled_at.map(|t| t.to_rfc3339()),
        "cancellation_reason": b.cancellation_reason,
        "created_at": b.created_at.to_rfc3339(),
    });
    let lines: Vec<Value> = lines
        .iter()
        .map(|l| {
            json!({
                "name": l.name,
                "kind": l.kind,
                "quantity": l.quantity,
                "start_date": l.start_date.map(|d| d.to_string()),
                "end_date": l.end_date.map(|d| d.to_string()),
                "starts_at": l.starts_at.map(|t| t.to_rfc3339()),
                "participants": l.participants,
                "participants_label": (l.kind == "activity" && l.participants > 0)
                    .then(|| labels::participants_label(l.participants, env.locale)),
                "total_minor": l.total_minor,
            })
        })
        .collect();
    let policy = json!({
        "lines": b.policy_rules.as_ref().map(|r| labels::policy_lines(r, env.locale)).unwrap_or_default(),
    });
    (booking, Value::Array(lines), policy)
}

pub fn payment(p: &PaymentFacts) -> Value {
    json!({
        "amount_minor": p.amount_minor,
        "currency": p.currency,
        "provider": p.provider,
        "purpose": p.purpose,
        "paid_at": p.paid_at.map(|t| t.to_rfc3339()),
        "reference": p.provider_reference,
    })
}

pub fn inquiry(i: &InquiryFacts) -> Value {
    json!({
        "kind": i.kind,
        "name": i.name,
        "email": i.email,
        "phone": i.phone,
        "subject": i.subject,
        "message": i.message,
        "preferred_from": i.preferred_from.map(|d| d.to_string()),
        "preferred_to": i.preferred_to.map(|d| d.to_string()),
        "group_size": i.group_size,
        "created_at": i.created_at.to_rfc3339(),
    })
}
