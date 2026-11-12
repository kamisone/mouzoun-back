//! Booking settings (`settings` rows `booking.*`, `legal.*`, `payments.*`), read in one query.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use sqlx::PgExecutor;

#[derive(Debug, Clone, Serialize)]
pub struct BookingSettings {
    pub enabled: bool,
    pub site_live: bool,
    pub hold_minutes: i64,
    pub request_answer_hours: i64,
    pub payment_link_hours: i64,
    pub balance_due_days: i64,
    pub balance_auto_charge: bool,
    pub balance_reminder_days: i64,
    pub balance_grace_days: i64,
    pub max_days_ahead: i64,
    pub standalone_activities: bool,
    pub calendar_show_prices: bool,
    pub urgency_threshold: i64,
    pub self_service_changes: bool,
    pub guest_cancellation: bool,
    pub request_card_authorization: bool,
    pub precheckin_reminder_days: i64,
    pub magic_link_minutes: i64,
    pub pitch_selection: bool,
    pub id_retention_days: i64,
    pub terms_version: String,
    pub online_payments: bool,
    pub providers: Vec<String>,
}

impl Default for BookingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            site_live: false,
            hold_minutes: 15,
            request_answer_hours: 24,
            payment_link_hours: 48,
            balance_due_days: 14,
            balance_auto_charge: false,
            balance_reminder_days: 3,
            balance_grace_days: 3,
            max_days_ahead: 365,
            standalone_activities: true,
            calendar_show_prices: true,
            urgency_threshold: 2,
            self_service_changes: false,
            guest_cancellation: true,
            request_card_authorization: false,
            precheckin_reminder_days: 5,
            magic_link_minutes: 60,
            pitch_selection: true,
            id_retention_days: 365,
            terms_version: "2026-11".into(),
            online_payments: false,
            providers: vec![],
        }
    }
}

impl BookingSettings {
    /// Whether the public can book right now.
    pub fn open_to_public(&self) -> bool {
        self.enabled
    }
}

pub async fn load(db: impl PgExecutor<'_>) -> sqlx::Result<BookingSettings> {
    let rows: HashMap<String, Value> = sqlx::query!(
        "SELECT key, value FROM settings
         WHERE key LIKE 'booking.%' OR key LIKE 'legal.%' OR key LIKE 'payments.%' OR key = 'site.status'"
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| (r.key, r.value))
    .collect();
    let d = BookingSettings::default();
    let num = |k: &str, def: i64| rows.get(k).and_then(Value::as_i64).unwrap_or(def);
    let flag = |k: &str, def: bool| rows.get(k).and_then(Value::as_bool).unwrap_or(def);
    Ok(BookingSettings {
        enabled: flag("booking.enabled", d.enabled),
        site_live: rows.get("site.status").and_then(Value::as_str) == Some("live"),
        hold_minutes: num("booking.hold_minutes", d.hold_minutes).clamp(5, 24 * 60),
        request_answer_hours: num("booking.request_answer_hours", d.request_answer_hours).max(1),
        payment_link_hours: num("booking.payment_link_hours", d.payment_link_hours).max(1),
        balance_due_days: num("booking.balance_due_days", d.balance_due_days).max(0),
        balance_auto_charge: flag("booking.balance_auto_charge", d.balance_auto_charge),
        balance_reminder_days: num("booking.balance_reminder_days", d.balance_reminder_days).max(0),
        balance_grace_days: num("booking.balance_grace_days", d.balance_grace_days).max(0),
        max_days_ahead: num("booking.max_days_ahead", d.max_days_ahead).max(1),
        standalone_activities: flag("booking.standalone_activities", d.standalone_activities),
        calendar_show_prices: flag("booking.calendar_show_prices", d.calendar_show_prices),
        urgency_threshold: num("booking.urgency_threshold", d.urgency_threshold).max(0),
        self_service_changes: flag("booking.self_service_changes", d.self_service_changes),
        guest_cancellation: flag("booking.guest_cancellation", d.guest_cancellation),
        request_card_authorization: flag(
            "booking.request_card_authorization",
            d.request_card_authorization,
        ),
        precheckin_reminder_days: num(
            "booking.precheckin_reminder_days",
            d.precheckin_reminder_days,
        ),
        magic_link_minutes: num("booking.magic_link_minutes", d.magic_link_minutes)
            .clamp(5, 7 * 24 * 60),
        pitch_selection: flag("booking.pitch_selection", d.pitch_selection),
        id_retention_days: num("booking.id_retention_days", d.id_retention_days).max(1),
        terms_version: rows
            .get("legal.terms_version")
            .and_then(Value::as_str)
            .unwrap_or(&d.terms_version)
            .to_string(),
        online_payments: flag("payments.online_enabled", d.online_payments),
        providers: rows
            .get("payments.providers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}
