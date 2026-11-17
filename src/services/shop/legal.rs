//! Consumer-law documents of the shop: the terms of sale (CGV) as a PDF of the revision the
//! customer accepted (sent with the order confirmation, a durable medium: L221-13), the model
//! withdrawal form (annex to R221-1), the email variables about the seller, the withdrawal
//! right and the mediator, and the French-text completeness checks (Toubon law).

use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{invoices::address_line, settings::ShopSettings};
use crate::services::{
    email::Attachment,
    pdf::{Document, Style, html_to_text},
};

/// Strings of a block's content in `locale` (rich text bodies, titles), in order.
fn texts(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.trim().is_empty() => out.push(html_to_text(s)),
        Value::Array(a) => a.iter().for_each(|x| texts(x, out)),
        Value::Object(m) => {
            for key in [
                "title",
                "heading",
                "subtitle",
                "body",
                "text",
                "description",
                "question",
                "answer",
            ] {
                if let Some(x) = m.get(key) {
                    texts(x, out);
                }
            }
        }
        _ => {}
    }
}

/// Text of a page revision in `locale` (title, then each visible section).
pub async fn revision_text(
    conn: &mut PgConnection,
    revision_id: Uuid,
    locale: &str,
) -> sqlx::Result<Option<(String, String, i32)>> {
    let Some(r) = sqlx::query!(
        "SELECT snapshot, revision_number, published_at FROM page_revisions WHERE id = $1",
        revision_id
    )
    .fetch_optional(conn)
    .await?
    else {
        return Ok(None);
    };
    let snap = &r.snapshot;
    let title = snap["page"]["translations"][locale]["title"]
        .as_str()
        .or_else(|| snap["page"]["translations"]["fr"]["title"].as_str())
        .unwrap_or("Conditions générales de vente")
        .to_string();
    let mut parts = Vec::new();
    if let Some(blocks) = snap["blocks"].as_array() {
        for b in blocks
            .iter()
            .filter(|b| b["is_visible"].as_bool().unwrap_or(true))
        {
            let content = b["content"]
                .get(locale)
                .or_else(|| b["content"].get("fr"))
                .cloned()
                .unwrap_or(Value::Null);
            texts(&content, &mut parts);
            if let Some(items) = b["items"].as_array() {
                for it in items {
                    let c = it["content"]
                        .get(locale)
                        .or_else(|| it["content"].get("fr"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    texts(&c, &mut parts);
                }
            }
        }
    }
    Ok(Some((title, parts.join("\n\n"), r.revision_number)))
}

/// The terms of sale of a revision as a PDF (French: the contractual language).
pub async fn terms_pdf(
    conn: &mut PgConnection,
    revision_id: Uuid,
    seller: &Value,
) -> sqlx::Result<Option<Attachment>> {
    let Some((title, body, number)) = revision_text(conn, revision_id, "fr").await? else {
        return Ok(None);
    };
    let mut d = Document::new();
    d.text(Style::Title, &title);
    d.text(
        Style::Small,
        format!(
            "Version n° {number} — {}",
            seller["legal_name"].as_str().unwrap_or("")
        ),
    );
    d.space(6.0).rule();
    for para in body.split("\n\n") {
        d.text(Style::Body, para.trim()).space(4.0);
    }
    Ok(Some(Attachment {
        filename: format!("CGV-v{number}.pdf"),
        content_type: "application/pdf".into(),
        bytes: d.render(),
    }))
}

/// The model withdrawal form (Consumer Code, annex to R221-1), addressed to the seller.
pub fn withdrawal_form_pdf(seller: &Value, order_number: &str) -> Attachment {
    let mut d = Document::new();
    d.text(Style::Title, "Formulaire de rétractation");
    d.text(Style::Small, "(Veuillez compléter et renvoyer le présent formulaire uniquement si vous souhaitez vous rétracter du contrat.)");
    d.space(8.0);
    let mut to = format!(
        "À l'attention de {}, {}",
        seller["legal_name"].as_str().unwrap_or(""),
        address_line(&seller["address"])
    );
    if let Some(e) = seller["email"].as_str().filter(|e| !e.is_empty()) {
        to.push_str(&format!(", {e}"));
    }
    d.text(Style::Body, to);
    d.space(8.0);
    for line in [
        "Je/nous (*) vous notifie/notifions (*) par la présente ma/notre (*) rétractation du contrat portant sur la vente du bien (*) ci-dessous :",
        &format!("Commande n° {order_number}"),
        "Commandé le (*) / reçu le (*) : ............................................",
        "Nom du (des) consommateur(s) : ............................................",
        "Adresse du (des) consommateur(s) : ............................................",
        "Signature du (des) consommateur(s) (uniquement en cas de notification du présent formulaire sur papier) :",
        "",
        "Date : ....................",
        "(*) Rayez la mention inutile.",
    ] {
        d.text(Style::Body, line).space(6.0);
    }
    d.space(6.0).text(
        Style::Small,
        "Vous pouvez aussi exercer ce droit depuis la page de votre commande sur notre site, ou par toute autre déclaration dénuée d'ambiguïté.",
    );
    Attachment {
        filename: "formulaire-retractation.pdf".into(),
        content_type: "application/pdf".into(),
        bytes: d.render(),
    }
}

/// Email variables: `seller` (identity), `withdrawal` (rights), `mediator`.
pub fn email_vars(
    settings: &ShopSettings,
    seller: &Value,
    withdrawal_deadline: Option<chrono::NaiveDate>,
    refund_due_by: Option<chrono::NaiveDate>,
) -> [(&'static str, Value); 3] {
    let reg = &seller["registration"];
    [
        (
            "seller",
            json!({
                "legal_name": seller["legal_name"], "legal_form": seller["legal_form"],
                "address": address_line(&seller["address"]), "email": seller["email"], "phone": seller["phone"],
                "siren": reg["siren"], "vat_number": reg["vat_number"], "rcs_city": reg["rcs_city"],
            }),
        ),
        (
            "withdrawal",
            json!({
                "days": settings.withdrawal_days,
                "refund_days": settings.refund_deadline_days,
                "deadline": withdrawal_deadline.map(|d| d.to_string()),
                "refund_due_by": refund_due_by.map(|d| d.to_string()),
                "guarantee_years": settings.legal_guarantee_years,
            }),
        ),
        ("mediator", settings.consumer_mediator.clone()),
    ]
}

/// Template codes whose French version must be complete (consumer-facing shop emails).
pub const FRENCH_REQUIRED_TEMPLATES: &[&str] = &[
    "order_confirmation",
    "order_payment_pending",
    "order_shipped",
    "order_delivered",
    "order_cancelled",
    "refund_issued",
    "return_update",
    "withdrawal_acknowledgment",
];

/// Whether an email template may be saved: French subject and body are required for the
/// consumer-facing shop templates.
pub fn email_template_french_ok(
    code: &str,
    fr_subject: Option<&str>,
    fr_body: Option<&str>,
) -> bool {
    !FRENCH_REQUIRED_TEMPLATES.contains(&code)
        || (fr_subject.is_some_and(|s| !s.trim().is_empty())
            && fr_body.is_some_and(|b| !b.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn withdrawal_form_names_the_seller() {
        let seller = json!({ "legal_name": "MOUZOUN SAS", "address": { "line1": "1 rue X", "postal_code": "75001", "city": "Paris", "country_code": "FR" } });
        let a = withdrawal_form_pdf(&seller, "MZ-0001");
        let text = String::from_utf8_lossy(&a.bytes);
        assert!(text.contains("MOUZOUN SAS") && text.contains("MZ-0001"));
    }

    #[test]
    fn french_templates_are_required() {
        assert!(!email_template_french_ok(
            "order_confirmation",
            Some(""),
            Some("x")
        ));
        assert!(email_template_french_ok(
            "order_confirmation",
            Some("Merci"),
            Some("<p>x</p>")
        ));
        assert!(email_template_french_ok("staff_new_order", None, None));
    }
}
