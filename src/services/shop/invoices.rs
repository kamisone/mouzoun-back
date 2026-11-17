//! Invoices and credit notes of shop orders (French rules, CGI art. 242 nonies A):
//! gap-free number per legal entity and year, seller identity frozen in `seller_snapshot`,
//! VAT breakdown per rate, delivery date, credit notes pointing to the invoice they correct.
//! Issued invoices are immutable (database trigger). Each document is rendered once to a PDF,
//! stored privately in the bucket and sealed with its SHA-256 (`pdf_object_key`, `pdf_sha256`).

use bytes::Bytes;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    infra::storage::{ObjectAcl, PutOptions, SharedStorage},
    services::pdf::{Document, Style, sha256_hex},
};

/// Format of the stored invoice documents (bump when the layout or the fields change).
pub const FORMAT_VERSION: i32 = 2;

/// Identity of a legal entity, as printed on its invoices and legal pages.
pub async fn seller_snapshot(conn: &mut PgConnection, entity_id: Uuid) -> sqlx::Result<Value> {
    let e = sqlx::query!(
        r#"SELECT code::text AS "code!", legal_name, legal_form, share_capital_minor, currency::text AS "currency!", address,
                  country_code::text AS "country_code!", registration, email::text AS email, phone, publication_director, host,
                  COALESCE((SELECT jsonb_object_agg(locale, invoice_footer) FROM legal_entity_translations WHERE legal_entity_id = e.id), '{}') AS "footer!"
           FROM legal_entities e WHERE id = $1"#,
        entity_id
    )
    .fetch_one(conn)
    .await?;
    Ok(json!({
        "code": e.code, "legal_name": e.legal_name, "legal_form": e.legal_form,
        "share_capital_minor": e.share_capital_minor, "currency": e.currency, "address": e.address,
        "country_code": e.country_code, "registration": e.registration, "email": e.email, "phone": e.phone,
        "publication_director": e.publication_director, "host": e.host, "invoice_footer": e.footer,
    }))
}

/// One-line postal address of a snapshot (`line1, postal_code city, country`).
pub fn address_line(a: &Value) -> String {
    let s = |k: &str| a[k].as_str().unwrap_or("").trim().to_string();
    [
        s("line1"),
        s("line2"),
        format!("{} {}", s("postal_code"), s("city"))
            .trim()
            .to_string(),
        s("country_code"),
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join(", ")
}

/// The order's invoice (once), when it becomes paid. Returns its number when issued now.
pub async fn issue_invoice(
    conn: &mut PgConnection,
    order_id: Uuid,
    now: DateTime<Utc>,
) -> sqlx::Result<Option<String>> {
    let exists = sqlx::query_scalar!(
        "SELECT id FROM invoices WHERE order_id = $1 AND kind = 'invoice'",
        order_id
    )
    .fetch_optional(&mut *conn)
    .await?;
    if exists.is_some() {
        return Ok(None);
    }
    let o = sqlx::query!(
        r#"SELECT o.email::text AS "email!", o.phone, o.billing_address, o.shipping_address, o.subtotal_minor, o.discount_minor,
                  o.shipping_minor, o.shipping_discount_minor, o.cod_fee_minor, o.tax_minor, o.total_minor,
                  o.currency::text AS "currency!", o.number, o.shipping_lines, o.legal_entity_id, o.vat_breakdown, o.relay_point,
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('sku', sku, 'name', product_snapshot->>'name',
                            'options', product_snapshot->>'options_label', 'quantity', quantity,
                            'unit_price_minor', unit_price_minor, 'discount_minor', discount_minor,
                            'tax_minor', tax_minor, 'total_minor', total_minor, 'tax_lines', tax_lines) ORDER BY sort_order)
                            FROM order_items WHERE order_id = o.id), '[]') AS "lines!"
           FROM orders o WHERE o.id = $1"#,
        order_id
    )
    .fetch_one(&mut *conn)
    .await?;
    let local = crate::camp_time::local(now);
    let number =
        crate::services::numbering::next_invoice_number(conn, o.legal_entity_id, local.year())
            .await?;
    let seller = seller_snapshot(conn, o.legal_entity_id).await?;
    sqlx::query!(
        "INSERT INTO invoices (id, number, order_id, kind, billing_details, lines, subtotal_minor, tax_minor, total_minor, currency,
                               issued_at, legal_entity_id, seller_snapshot, vat_breakdown, delivery_date, format_version, einvoice_status)
         VALUES ($1, $2, $3, 'invoice', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 'b2c_ereporting')",
        Uuid::now_v7(),
        number,
        order_id,
        json!({
            "order_number": o.number,
            "email": o.email,
            "phone": o.phone,
            "billing_address": o.billing_address.or(o.shipping_address),
            "relay_point": o.relay_point,
            "shipping_lines": o.shipping_lines,
            "discount_minor": o.discount_minor,
            "shipping_minor": o.shipping_minor - o.shipping_discount_minor,
            "cod_fee_minor": o.cod_fee_minor,
        }),
        o.lines,
        o.subtotal_minor,
        o.tax_minor,
        o.total_minor,
        o.currency,
        now,
        o.legal_entity_id,
        seller,
        o.vat_breakdown,
        local.date(),
        FORMAT_VERSION
    )
    .execute(&mut *conn)
    .await?;
    Ok(Some(number))
}

/// Splits `amount` (incl. VAT) across an invoice's VAT breakdown in proportion to each rate's
/// gross amount (largest remainder: the parts sum to `amount`), with the VAT of each part.
pub fn prorate_breakdown(breakdown: &Value, invoice_total: i64, amount: i64) -> Vec<Value> {
    let buckets: Vec<(String, i64)> = breakdown
        .as_array()
        .map(|a| {
            a.iter()
                .map(|b| {
                    (
                        b["rate"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| b["rate"].to_string()),
                        b["gross_minor"].as_i64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let total: i64 = buckets.iter().map(|(_, g)| *g).sum();
    if buckets.is_empty() || total <= 0 || invoice_total <= 0 {
        return vec![
            json!({ "rate": "0", "base_minor": amount, "vat_minor": 0, "gross_minor": amount }),
        ];
    }
    let weights: Vec<i64> = buckets.iter().map(|(_, g)| *g).collect();
    let caps = vec![amount.abs(); weights.len()];
    let parts = super::pricing::allocate(amount.abs(), &weights, &caps);
    buckets
        .iter()
        .zip(parts)
        .map(|((rate, _), gross)| {
            let r: rust_decimal::Decimal = rate.trim_matches('"').parse().unwrap_or_default();
            let vat = (rust_decimal::Decimal::from(gross) * r / (rust_decimal::Decimal::from(100) + r)).round();
            let vat: i64 = vat.to_string().parse().unwrap_or(0);
            let sign = amount.signum();
            json!({ "rate": rate.trim_matches('"'), "base_minor": sign * (gross - vat), "vat_minor": sign * vat, "gross_minor": sign * gross })
        })
        .collect()
}

/// A credit note for a settled refund: next number of the same entity, negative amounts, VAT
/// prorated on the corrected invoice's rates.
pub async fn issue_credit_note(
    conn: &mut PgConnection,
    order_id: Uuid,
    refund_id: Uuid,
    amount_minor: i64,
    currency: &str,
    lines: &Value,
    now: DateTime<Utc>,
) -> sqlx::Result<()> {
    let Some(inv) = sqlx::query!(
        "SELECT id, number, legal_entity_id, vat_breakdown, total_minor FROM invoices WHERE order_id = $1 AND kind = 'invoice'",
        order_id
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(()); // nothing invoiced yet (e.g. cancelled before payment)
    };
    let already = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM invoices WHERE order_id = $1 AND kind = 'credit_note' AND billing_details->>'refund_id' = $2) AS \"x!\"",
        order_id,
        refund_id.to_string()
    )
    .fetch_one(&mut *conn)
    .await?;
    if already {
        return Ok(());
    }
    let breakdown = prorate_breakdown(&inv.vat_breakdown, inv.total_minor, -amount_minor);
    let vat: i64 = breakdown
        .iter()
        .filter_map(|b| b["vat_minor"].as_i64())
        .sum();
    let local = crate::camp_time::local(now);
    let number =
        crate::services::numbering::next_invoice_number(conn, inv.legal_entity_id, local.year())
            .await?;
    let seller = seller_snapshot(conn, inv.legal_entity_id).await?;
    let billing = sqlx::query_scalar!("SELECT billing_details FROM invoices WHERE id = $1", inv.id)
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query!(
        "INSERT INTO invoices (id, number, order_id, kind, billing_details, lines, subtotal_minor, tax_minor, total_minor, currency,
                               issued_at, legal_entity_id, seller_snapshot, vat_breakdown, credit_note_for_invoice_id, delivery_date,
                               format_version, einvoice_status)
         VALUES ($1, $2, $3, 'credit_note', $4, $5, $6, $7, $6, $8, $9, $10, $11, $12, $13, $14, $15, 'b2c_ereporting')",
        Uuid::now_v7(),
        number,
        order_id,
        json!({ "refund_id": refund_id.to_string(), "invoice": inv.number,
                "email": billing["email"], "billing_address": billing["billing_address"], "order_number": billing["order_number"] }),
        lines,
        -amount_minor,
        vat,
        currency,
        now,
        inv.legal_entity_id,
        seller,
        Value::Array(breakdown),
        inv.id,
        local.date(),
        FORMAT_VERSION
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// French money: `1 234,50 €`.
pub fn eur(minor: i64, currency: &str) -> String {
    let sign = if minor < 0 { "-" } else { "" };
    let m = minor.abs();
    let units = (m / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in units.chars().enumerate() {
        if i > 0 && (units.len() - i).is_multiple_of(3) {
            grouped.push(' ');
        }
        grouped.push(c);
    }
    let symbol = if currency == "EUR" { "€" } else { currency };
    format!("{sign}{grouped},{:02} {symbol}", m % 100)
}

fn rate_label(rate: &Value) -> String {
    let r = rate
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| rate.to_string());
    format!("{} %", r.trim_matches('"').replace('.', ","))
}

/// The invoice (or credit note) document. Deterministic for the same row.
pub async fn render_pdf(
    conn: &mut PgConnection,
    invoice_id: Uuid,
) -> sqlx::Result<(Vec<u8>, String)> {
    let inv = sqlx::query!(
        r#"SELECT i.number, i.kind, i.issued_at, i.delivery_date, i.seller_snapshot, i.billing_details, i.lines, i.vat_breakdown,
                  i.total_minor, i.tax_minor, i.currency::text AS "currency!", e.code::text AS "entity!",
                  (SELECT number FROM invoices c WHERE c.id = i.credit_note_for_invoice_id) AS corrected
           FROM invoices i JOIN legal_entities e ON e.id = i.legal_entity_id WHERE i.id = $1"#,
        invoice_id
    )
    .fetch_one(conn)
    .await?;
    let cur = inv.currency.as_str();
    let s = &inv.seller_snapshot;
    let reg = &s["registration"];
    let r = |k: &str| reg[k].as_str().unwrap_or("").to_string();
    let mut d = Document::new();
    let credit = inv.kind == "credit_note";
    d.text(
        Style::Title,
        format!(
            "{} n° {}",
            if credit { "Avoir" } else { "Facture" },
            inv.number
        ),
    );
    let mut dates = format!("Date d'émission : {}", inv.issued_at.format("%d/%m/%Y"));
    if let Some(dd) = inv.delivery_date {
        dates.push_str(&format!(
            " · Date de la vente / livraison : {}",
            dd.format("%d/%m/%Y")
        ));
    }
    d.text(Style::Body, dates);
    if let Some(c) = &inv.corrected {
        d.text(Style::Body, format!("Avoir sur la facture n° {c}"));
    }
    d.space(8.0).text(Style::Heading, "Vendeur");
    let capital = s["share_capital_minor"]
        .as_i64()
        .map(|c| format!(" au capital de {}", eur(c, cur)))
        .unwrap_or_default();
    d.text(
        Style::Body,
        format!(
            "{} — {}{}",
            s["legal_name"].as_str().unwrap_or(""),
            s["legal_form"].as_str().unwrap_or(""),
            capital
        ),
    );
    d.text(Style::Body, address_line(&s["address"]));
    let mut ids = Vec::new();
    for (label, key) in [
        ("SIREN", "siren"),
        ("SIRET", "siret"),
        ("TVA intracom.", "vat_number"),
        ("ICE", "ice"),
        ("IF", "if"),
    ] {
        let v = r(key);
        if !v.is_empty() {
            ids.push(format!("{label} {v}"));
        }
    }
    if !r("rcs_city").is_empty() {
        ids.push(format!("RCS {}", r("rcs_city")));
    }
    if !ids.is_empty() {
        d.text(Style::Body, ids.join(" · "));
    }
    let b = &inv.billing_details;
    let a = &b["billing_address"];
    d.space(6.0).text(Style::Heading, "Client");
    let name = format!(
        "{} {}",
        a["first_name"].as_str().unwrap_or(""),
        a["last_name"].as_str().unwrap_or("")
    );
    d.text(Style::Body, name.trim().to_string());
    d.text(Style::Body, address_line(a));
    if let Some(e) = b["email"].as_str() {
        d.text(Style::Body, e.to_string());
    }
    if let Some(n) = b["order_number"].as_str() {
        d.text(Style::Small, format!("Commande {n}"));
    }
    d.space(8.0).rule();
    d.row(
        &[
            (0.46, "Désignation"),
            (0.08, "Qté"),
            (0.16, "PU TTC"),
            (0.12, "TVA"),
            (0.18, "Total TTC"),
        ],
        true,
    );
    if let Some(lines) = inv.lines.as_array() {
        for l in lines {
            let label = match (l["name"].as_str(), l["options"].as_str()) {
                (Some(n), Some(o)) if !o.is_empty() => format!("{n} ({o})"),
                (Some(n), _) => n.to_string(),
                _ => "Article".into(),
            };
            let rates: Vec<String> = l["tax_lines"]
                .as_array()
                .map(|t| t.iter().map(|x| rate_label(&x["rate"])).collect())
                .unwrap_or_default();
            let total = l["total_minor"]
                .as_i64()
                .or_else(|| l["amount_minor"].as_i64())
                .unwrap_or(0);
            let total = if credit { -total.abs() } else { total };
            d.row(
                &[
                    (0.46, &label),
                    (
                        0.08,
                        &l["quantity"]
                            .as_i64()
                            .map(|q| q.to_string())
                            .unwrap_or_default(),
                    ),
                    (
                        0.16,
                        &l["unit_price_minor"]
                            .as_i64()
                            .map(|u| eur(u, cur))
                            .unwrap_or_default(),
                    ),
                    (0.12, &rates.join(" + ")),
                    (0.18, &eur(total, cur)),
                ],
                false,
            );
        }
    }
    if !credit {
        for (label, key, neg) in [
            ("Remise", "discount_minor", true),
            ("Livraison", "shipping_minor", false),
            ("Frais", "cod_fee_minor", false),
        ] {
            let v = b[key].as_i64().unwrap_or(0);
            if v != 0 {
                d.row(
                    &[(0.82, label), (0.18, &eur(if neg { -v } else { v }, cur))],
                    false,
                );
            }
        }
    }
    d.rule().text(Style::Heading, "Récapitulatif TVA");
    d.row(
        &[
            (0.4, "Taux"),
            (0.2, "Base HT"),
            (0.2, "TVA"),
            (0.2, "Total TTC"),
        ],
        true,
    );
    if let Some(buckets) = inv.vat_breakdown.as_array() {
        for v in buckets {
            d.row(
                &[
                    (0.4, &rate_label(&v["rate"])),
                    (0.2, &eur(v["base_minor"].as_i64().unwrap_or(0), cur)),
                    (0.2, &eur(v["vat_minor"].as_i64().unwrap_or(0), cur)),
                    (0.2, &eur(v["gross_minor"].as_i64().unwrap_or(0), cur)),
                ],
                false,
            );
        }
    }
    d.rule();
    d.row(
        &[
            (0.8, "Total HT"),
            (0.2, &eur(inv.total_minor - inv.tax_minor, cur)),
        ],
        false,
    );
    d.row(
        &[(0.8, "Total TVA"), (0.2, &eur(inv.tax_minor, cur))],
        false,
    );
    d.row(
        &[(0.8, "Total TTC"), (0.2, &eur(inv.total_minor, cur))],
        true,
    );
    d.space(10.0);
    if credit {
        d.text(
            Style::Small,
            "Remboursement effectué sur le moyen de paiement utilisé pour la commande.",
        );
    } else {
        d.text(Style::Small, format!("Facture acquittée le {} (paiement par carte). Pas d'escompte pour paiement anticipé.", inv.issued_at.format("%d/%m/%Y")));
    }
    if let Some(f) = s["invoice_footer"]["fr"]
        .as_str()
        .filter(|f| !f.trim().is_empty())
    {
        d.text(Style::Small, f.to_string());
    }
    let bytes = d.render();
    let key = format!("invoices/{}/{}.pdf", inv.entity, inv.number);
    Ok((bytes, key))
}

/// Renders and stores the PDF of every unsealed document of `order_id` (all orders when
/// `None`), then seals it. Safe to run again: sealed documents are skipped.
pub async fn seal_pending(
    pool: &PgPool,
    storage: &SharedStorage,
    order_id: Option<Uuid>,
) -> anyhow::Result<usize> {
    let ids = sqlx::query_scalar!(
        "SELECT id FROM invoices WHERE pdf_sha256 IS NULL AND order_id IS NOT NULL AND ($1::uuid IS NULL OR order_id = $1) ORDER BY issued_at LIMIT 200",
        order_id
    )
    .fetch_all(pool)
    .await?;
    let mut sealed = 0;
    for id in ids {
        let mut conn = pool.acquire().await?;
        let (bytes, key) = render_pdf(&mut conn, id).await?;
        let sha = sha256_hex(&bytes);
        storage
            .put(
                &key,
                Bytes::from(bytes),
                &PutOptions {
                    content_type: "application/pdf".into(),
                    acl: ObjectAcl::Private,
                    cache_control: Some("private, max-age=0, no-store".into()),
                },
            )
            .await?;
        sealed += sqlx::query!(
            "UPDATE invoices SET pdf_object_key = $2, pdf_sha256 = $3 WHERE id = $1 AND pdf_sha256 IS NULL",
            id,
            key,
            sha
        )
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize;
    }
    Ok(sealed)
}

/// Withdrawal deadline: `days` after delivery (or after the order when not delivered yet).
pub fn withdrawal_deadline(delivered_on: NaiveDate, days: i64) -> NaiveDate {
    delivered_on + chrono::Duration::days(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_note_vat_is_prorated_and_sums() {
        let breakdown = json!([
            { "rate": "5.5", "base_minor": 1896, "vat_minor": 104, "gross_minor": 2000 },
            { "rate": "20", "base_minor": 833, "vat_minor": 167, "gross_minor": 1000 }
        ]);
        let parts = prorate_breakdown(&breakdown, 3000, -1501);
        let gross: i64 = parts
            .iter()
            .map(|p| p["gross_minor"].as_i64().unwrap())
            .sum();
        assert_eq!(gross, -1501);
        for p in &parts {
            assert_eq!(
                p["base_minor"].as_i64().unwrap() + p["vat_minor"].as_i64().unwrap(),
                p["gross_minor"].as_i64().unwrap()
            );
        }
        assert_eq!(parts[1]["vat_minor"], json!(-83)); // 500 at 20 % → 83
    }

    #[test]
    fn euros_are_french() {
        assert_eq!(eur(123450, "EUR"), "1 234,50 €");
        assert_eq!(eur(-5, "EUR"), "-0,05 €");
    }
}
