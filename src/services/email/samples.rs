//! Realistic sample variables for every template (admin preview, test sends, tests). Built with
//! the same view builders as real emails.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::views::{
    self, BookingFacts, GuestFacts, InquiryFacts, LineFacts, PaymentFacts, ViewEnv,
};

/// Every template the application sends.
pub const TEMPLATE_CODES: [&str; 53] = [
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
    "booking_declined",
    "booking_payment_link",
    "balance_reminder",
    "balance_failed",
    "staff_balance_failed",
    "precheckin_reminder",
    "booking_magic_link",
    "refund_receipt",
    "cart_abandoned",
    "sign_in_code",
    "account_welcome",
    "security_new_device",
    "security_password_changed",
    "email_change_code",
    "email_change_notice",
    "security_email_changed",
    "security_identity_linked",
    "security_passkey_added",
    "booking_claim",
    "booking_claimed",
    "account_export_ready",
    "account_deletion_scheduled",
    "account_deleted",
    "order_confirmation",
    "order_payment_pending",
    "order_confirmed_cod",
    "order_shipped",
    "order_ready_for_pickup",
    "order_delivered",
    "order_cancelled",
    "refund_issued",
    "return_update",
    "abandoned_cart",
    "back_in_stock",
    "staff_new_order",
    "staff_low_stock",
    "staff_payment_issue",
    "withdrawal_acknowledgment",
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
    if let Some(shop) = shop_sample(code, locale, site_url) {
        return Some(shop);
    }
    match code {
        "booking_received" | "staff_new_booking" => booking_vars(&mut vars, "awaiting_payment"),
        "booking_confirmation" | "booking_modified" | "pre_arrival" => {
            booking_vars(&mut vars, "confirmed")
        }
        "booking_cancelled" | "staff_booking_cancelled" => booking_vars(&mut vars, "cancelled"),
        "review_request" => booking_vars(&mut vars, "completed"),
        "booking_declined" => booking_vars(&mut vars, "cancelled"),
        "booking_payment_link" => booking_vars(&mut vars, "awaiting_payment"),
        "balance_reminder" | "balance_failed" | "staff_balance_failed" | "precheckin_reminder" => {
            booking_vars(&mut vars, "confirmed")
        }
        "booking_magic_link" => {
            booking_vars(&mut vars, "confirmed");
            vars.insert(
                "access_url".into(),
                json!(format!(
                    "{site_url}/{locale}/booking/MZ-27-000042?m=sample-token"
                )),
            );
        }
        "refund_receipt" => {
            booking_vars(&mut vars, "cancelled");
            vars.insert(
                "refund".into(),
                json!({ "amount_minor": 9_750, "currency": "EUR" }),
            );
        }
        "cart_abandoned" => booking_vars(&mut vars, "expired"),
        "sign_in_code" => {
            vars.insert("code".into(), json!("042917"));
            vars.insert(
                "link".into(),
                json!(format!(
                    "{site_url}/{locale}/account/signin/link?e=a%40x.ma&t=sample"
                )),
            );
            vars.insert("minutes".into(), json!(10));
        }
        "email_change_code" => {
            vars.insert("code".into(), json!("318204"));
        }
        "account_deleted" => {}
        code if code.starts_with("security_")
            || matches!(
                code,
                "account_welcome"
                    | "email_change_notice"
                    | "booking_claim"
                    | "booking_claimed"
                    | "account_export_ready"
                    | "account_deletion_scheduled"
            ) =>
        {
            for (k, v) in [
                ("first_name", json!("Amina")),
                ("account_url", json!(format!("{site_url}/{locale}/account"))),
                (
                    "security_url",
                    json!(format!("{site_url}/{locale}/account/security")),
                ),
                ("device", json!("Safari · iPhone")),
                ("when", json!("2027-01-03T09:45:00Z")),
                ("change", json!("added")),
                ("new_email", json!("amina.new@example.com")),
                ("old_email", json!("amina@example.com")),
                ("provider", json!("Google")),
                ("reference", json!("MZ-27-000042")),
                (
                    "link",
                    json!(format!(
                        "{site_url}/{locale}/account/trips/claim?token=sample"
                    )),
                ),
                (
                    "trip_url",
                    json!(format!("{site_url}/{locale}/booking/MZ-27-000042")),
                ),
                ("days", json!(7)),
                ("scheduled_for", json!("2027-01-17T10:00:00Z")),
                (
                    "cancel_url",
                    json!(format!(
                        "{site_url}/{locale}/account/privacy/cancel-deletion?token=sample"
                    )),
                ),
            ] {
                vars.insert(k.into(), v);
            }
        }
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

/// Sample variables of the shop templates.
fn shop_sample(code: &str, locale: &str, site_url: &str) -> Option<Value> {
    if !code.starts_with("order_")
        && !matches!(
            code,
            "refund_issued"
                | "return_update"
                | "abandoned_cart"
                | "back_in_stock"
                | "staff_new_order"
                | "staff_low_stock"
                | "staff_payment_issue"
                | "withdrawal_acknowledgment"
        )
    {
        return None;
    }
    let labels = match locale {
        "fr" => {
            json!({ "details": "Commande", "discount": "Remise", "shipping": "Livraison", "cod_fee": "Frais de paiement à la livraison", "total": "Total", "tax_included": "dont TVA" })
        }
        "ar" => {
            json!({ "details": "الطلب", "discount": "الخصم", "shipping": "التوصيل", "cod_fee": "رسوم الدفع عند الاستلام", "total": "المجموع", "tax_included": "منها الضريبة" })
        }
        _ => {
            json!({ "details": "Order", "discount": "Discount", "shipping": "Shipping", "cod_fee": "Cash on delivery fee", "total": "Total", "tax_included": "incl. VAT" })
        }
    };
    let order = json!({
        "number": "MZS-27-000042", "url": format!("{site_url}/boutique/orders/MZS-27-000042?t=sample"), "status": "open",
        "email": "amina@example.com", "phone": "+212600000000", "payment_method": "cash_on_delivery",
        "fulfillment_method": "shipping", "requires_confirmation": true, "subtotal_minor": 4_200, "discount_minor": 420,
        "shipping_minor": 450, "shipping_discount_minor": 0, "cod_fee_minor": 150, "tax_minor": 630, "total_minor": 4_380,
        "amount_paid_minor": 0, "currency": "EUR", "hold_until": "2027-01-08T10:00:00Z", "labels": labels,
    });
    let lines = json!([
        { "name": "Dattes Medjool", "options": "1 kg", "quantity": 2, "total_minor": 2_400 },
        { "name": "Huile d'olive vierge extra", "options": "1 L", "quantity": 1, "total_minor": 1_800 },
    ]);
    let mut vars = Map::new();
    vars.insert(
        "customer".into(),
        json!({ "first_name": "Amina", "full_name": "Amina Ait Ali" }),
    );
    vars.insert("order".into(), order);
    vars.insert(
        "admin_url".into(),
        json!(format!("{site_url}/admin/shop/orders/sample")),
    );
    vars.insert(
        "seller".into(),
        json!({ "legal_name": "MOUZOUN SAS", "legal_form": "SAS", "address": "1 rue Exemple, 75001 Paris, FR",
                "email": "boutique@example.com", "phone": null, "siren": "123456789", "vat_number": "FR12123456789", "rcs_city": "Paris" }),
    );
    vars.insert(
        "withdrawal".into(),
        json!({ "days": 14, "refund_days": 14, "deadline": "2027-01-22", "refund_due_by": "2027-01-24", "guarantee_years": 2 }),
    );
    vars.insert(
        "mediator".into(),
        json!({ "name": "Médiateur (exemple)", "url": "https://mediateur.example" }),
    );
    match code {
        "withdrawal_acknowledgment" => {
            vars.insert("order_return".into(), json!({ "number": "MZR-27-000004", "status": "requested", "requested_at": "2027-01-10T09:00:00Z" }));
        }
        "abandoned_cart" => {
            return Some(
                json!({ "lines": [{ "name": "Dattes Medjool", "quantity": 2 }], "cart_url": format!("{site_url}/boutique/cart") }),
            );
        }
        "back_in_stock" => {
            return Some(
                json!({ "product": { "name": "Dattes Medjool", "variant": "1 kg", "url": format!("{site_url}/boutique/p/dattes-medjool") } }),
            );
        }
        "staff_low_stock" => {
            return Some(
                json!({ "stock": { "sku": "DATES-MEDJOOL-1KG", "name": "Dattes Medjool", "location": "Boutique de la ferme",
                                           "available": 3, "threshold": 5 },
                                "admin_url": format!("{site_url}/admin/shop/inventory") }),
            );
        }
        "order_shipped" | "order_ready_for_pickup" => {
            vars.insert("fulfillment".into(), json!({ "carrier": "amana", "tracking_number": "CP123456789MA",
                "tracking_url": "https://www.poste.ma/suivi?n=CP123456789MA", "location_name": "Boutique de la ferme",
                "pickup_instructions": "À l'accueil du camp, de 9 h à 19 h.", "pickup_code": "042917" }));
        }
        "refund_issued" => {
            vars.insert(
                "refund".into(),
                json!({ "amount_minor": 1_800, "currency": "EUR" }),
            );
        }
        "return_update" => {
            vars.insert("order_return".into(), json!({ "number": "MZR-27-000003", "status": "approved", "status_label": "accepté",
                                                       "staff_note": "Renvoyez le colis avec le bon joint." }));
        }
        "staff_payment_issue" => {
            vars.insert("issue".into(), json!("paid 4200 EUR instead of 4380 EUR"));
        }
        _ => {}
    }
    vars.insert("lines".into(), lines);
    vars.insert(
        "bank_transfer_details".into(),
        json!("IBAN MA64 0000 0000 0000 0000 0000 0000"),
    );
    vars.insert("payment_url".into(), Value::Null);
    Some(Value::Object(vars))
}
