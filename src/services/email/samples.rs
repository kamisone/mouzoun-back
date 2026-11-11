//! Realistic sample variables for every template (admin preview, test sends, tests). Built with
//! the same view builders as real emails.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::views::{
    self, BookingFacts, GuestFacts, InquiryFacts, LineFacts, PaymentFacts, ViewEnv,
};

/// Every template the application sends.
pub const TEMPLATE_CODES: [&str; 15] = [
    "booking_received",
    "booking_confirmation",
    "booking_cancelled",
    "booking_modified",
    "payment_receipt",
    "pre_arrival",
    "review_request",
    "inquiry_auto_reply",
    "newsletter_confirm",
    "password_reset",
    "email_verification",
    "staff_invitation",
    "staff_new_booking",
    "staff_booking_cancelled",
    "staff_new_inquiry",
];

fn at(s: &str) -> DateTime<Utc> {
    s.parse().expect("valid sample timestamp")
}

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid sample date")
}

/// `(fr, en, ar)` product names of the sample stay.
const NAMES: [[&str; 3]; 3] = [
    ["Tente nomade", "Nomad tent", "خيمة رحّل"],
    ["Quad 1 h", "Quad 1 h", "دراجة رباعية ساعة"],
    ["Dîner berbère", "Berber dinner", "عشاء أمازيغي"],
];

fn sample_booking(status: &str, locale: &str) -> (BookingFacts, GuestFacts, Vec<LineFacts>) {
    let lang = match locale {
        "fr" => 0,
        "ar" => 2,
        _ => 1,
    };
    let name = |i: usize| NAMES[i][lang].to_string();
    let booking = BookingFacts {
        id: Uuid::from_u128(0x2027_0115),
        reference: "MZ-27-000042".into(),
        status: status.into(),
        currency: "EUR".into(),
        arrival: Some(day(2027, 1, 15)),
        departure: Some(day(2027, 1, 17)),
        adults: 2,
        children: 1,
        infants: 0,
        subtotal_minor: 31_500,
        discount_minor: 0,
        tax_minor: 1_000,
        total_minor: 32_500,
        amount_paid_minor: if status == "confirmed" { 9_750 } else { 0 },
        deposit_due_minor: 9_750,
        balance_due_on: Some(day(2027, 1, 15)),
        hold_expires_at: (status == "awaiting_payment" || status == "pending")
            .then(|| at("2027-01-02T10:30:00Z")),
        special_requests: Some("Arrivée tardive vers 19 h, un lit bébé si possible.".into()),
        estimated_arrival_time: NaiveTime::from_hms_opt(18, 30, 0),
        check_in_from: NaiveTime::from_hms_opt(14, 0, 0),
        cancelled_at: (status == "cancelled" || status == "expired")
            .then(|| at("2027-01-05T09:12:00Z")),
        cancellation_reason: (status == "cancelled").then(|| "Changement de programme".to_string()),
        created_at: at("2027-01-02T10:00:00Z"),
        policy_rules: Some(json!([
            {"min_hours_before": 168, "refund_percent": 100},
            {"min_hours_before": 48, "refund_percent": 50},
            {"min_hours_before": 0, "refund_percent": 0}
        ])),
    };
    let guest = GuestFacts {
        first_name: "Amina".into(),
        last_name: Some("Benali".into()),
        email: Some("amina.benali@example.com".into()),
        phone: Some("+33612345678".into()),
    };
    let lines = vec![
        LineFacts {
            name: name(0),
            kind: "accommodation".into(),
            quantity: 1,
            start_date: Some(day(2027, 1, 15)),
            end_date: Some(day(2027, 1, 17)),
            starts_at: None,
            participants: 3,
            total_minor: 12_000,
        },
        LineFacts {
            name: name(1),
            kind: "activity".into(),
            quantity: 1,
            start_date: Some(day(2027, 1, 16)),
            end_date: None,
            starts_at: Some(at("2027-01-16T08:00:00Z")),
            participants: 3,
            total_minor: 13_500,
        },
        LineFacts {
            name: name(2),
            kind: "extra".into(),
            quantity: 2,
            start_date: Some(day(2027, 1, 15)),
            end_date: None,
            starts_at: None,
            participants: 3,
            total_minor: 6_000,
        },
    ];
    (booking, guest, lines)
}

/// Sample variables of `code` (without `site`, added by the renderer's context).
pub fn for_template(code: &str, locale: &str, site_url: &str) -> Option<Value> {
    let env = ViewEnv {
        locale,
        site_url,
        link_secret: "sample",
    };
    let mut vars = Map::new();
    let booking_vars = |vars: &mut Map<String, Value>, status: &str| {
        let (b, g, l) = sample_booking(status, locale);
        let (booking, lines, policy) = views::booking(&b, &l, &env);
        vars.insert("guest".into(), views::guest(&g));
        vars.insert("booking".into(), booking);
        vars.insert("lines".into(), lines);
        vars.insert("policy".into(), policy);
        vars.insert("payment_instructions".into(), json!(match locale {
            "fr" => "Virement bancaire — IBAN MA64 0000 0000 0000 0000 0000 0000, en indiquant la référence de la réservation.",
            "ar" => "تحويل بنكي — IBAN MA64 0000 0000 0000 0000 0000 0000، مع ذكر مرجع الحجز.",
            _ => "Bank transfer — IBAN MA64 0000 0000 0000 0000 0000 0000, quoting the booking reference.",
        }));
        vars.insert("payment".into(), Value::Null);
        vars.insert(
            "admin_url".into(),
            json!(format!("{site_url}/admin/bookings/{}", b.id)),
        );
    };
    let inquiry = InquiryFacts {
        id: Uuid::from_u128(7),
        kind: "group".into(),
        name: "Lucas Martin".into(),
        email: "lucas.martin@example.com".into(),
        phone: Some("+33698765432".into()),
        subject: Some("Séjour pour un groupe de 12".into()),
        message: "Bonjour, nous sommes 12 randonneurs et aimerions passer deux nuits au camp en mars.\nAvez-vous des disponibilités ?".into(),
        preferred_from: Some(day(2027, 3, 12)),
        preferred_to: Some(day(2027, 3, 14)),
        group_size: Some(12),
        created_at: at("2027-01-03T15:20:00Z"),
    };
    let account = |vars: &mut Map<String, Value>, path: &str| {
        vars.insert(
            "user".into(),
            json!({"full_name": "Youssef El Amrani", "email": "youssef@example.com"}),
        );
        vars.insert(
            "action_url".into(),
            json!(format!("{site_url}/{path}?token=sample-token")),
        );
        vars.insert("expires_at".into(), json!("2027-01-03T16:20:00Z"));
        vars.insert("invited_by".into(), json!("Hassan Ait Ouarab"));
    };
    match code {
        "booking_received" | "staff_new_booking" => booking_vars(&mut vars, "awaiting_payment"),
        "booking_confirmation" | "booking_modified" | "pre_arrival" => {
            booking_vars(&mut vars, "confirmed")
        }
        "booking_cancelled" | "staff_booking_cancelled" => booking_vars(&mut vars, "cancelled"),
        "review_request" => booking_vars(&mut vars, "completed"),
        "payment_receipt" => {
            booking_vars(&mut vars, "confirmed");
            vars.insert(
                "payment".into(),
                views::payment(&PaymentFacts {
                    amount_minor: 9_750,
                    currency: "EUR".into(),
                    provider: "bank_transfer".into(),
                    purpose: "deposit".into(),
                    paid_at: Some(at("2027-01-03T09:45:00Z")),
                    provider_reference: Some("VIR-58213".into()),
                }),
            );
        }
        "inquiry_auto_reply" | "staff_new_inquiry" => {
            vars.insert("inquiry".into(), views::inquiry(&inquiry));
            vars.insert(
                "admin_url".into(),
                json!(format!("{site_url}/admin/inquiries/{}", inquiry.id)),
            );
        }
        "newsletter_confirm" => {
            vars.insert(
                "confirm_url".into(),
                json!(format!(
                    "{site_url}/{locale}/newsletter/confirm?token=sample-token"
                )),
            );
        }
        "password_reset" => account(&mut vars, &format!("{locale}/account/reset-password")),
        "email_verification" => account(&mut vars, &format!("{locale}/account/verify-email")),
        "staff_invitation" => account(&mut vars, "admin/accept-invitation"),
        _ => return None,
    }
    Some(Value::Object(vars))
}
