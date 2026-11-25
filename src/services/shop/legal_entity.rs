//! The French seller's identity (`legal_entities` row of `shop.legal_entity`, `mouzoun_fr`) and
//! the consumer mediator (setting `shop.consumer_mediator`): structured data edited in Admin ›
//! Settings › Legal entity, read by invoices (`seller_snapshot`), checkout, emails, the footer,
//! the legal pages (tokens) and the compliance checklist.
//!
//! Validation (SIREN and SIRET Luhn keys, the French VAT key, the postal code) answers with
//! **field codes and rule codes**, never the values; the audit row lists the changed field names.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::services::audit::{self, Actor};

// ---- Rules ----------------------------------------------------------------------------------------

/// Luhn checksum over ASCII digits.
pub fn luhn(digits: &str) -> bool {
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let sum: u32 = digits
        .chars()
        .rev()
        .enumerate()
        .map(|(i, c)| {
            let d = c.to_digit(10).unwrap_or(0);
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// SIREN: 9 digits, Luhn key. Errors are rule codes (`digits_9`, `luhn`).
pub fn siren_rule(siren: &str) -> Result<(), &'static str> {
    if siren.len() != 9 || !siren.chars().all(|c| c.is_ascii_digit()) {
        return Err("digits_9");
    }
    if !luhn(siren) {
        return Err("luhn");
    }
    Ok(())
}

/// SIRET: 14 digits starting with the SIREN, Luhn key (La Poste's establishments, SIREN
/// 356000000, use a sum multiple of 5 instead).
pub fn siret_rule(siret: &str, siren: Option<&str>) -> Result<(), &'static str> {
    if siret.len() != 14 || !siret.chars().all(|c| c.is_ascii_digit()) {
        return Err("digits_14");
    }
    if let Some(siren) = siren
        && !siret.starts_with(siren)
    {
        return Err("siren_prefix");
    }
    let la_poste = siret.starts_with("356000000")
        && siret
            .chars()
            .filter_map(|c| c.to_digit(10))
            .sum::<u32>()
            .is_multiple_of(5);
    if !luhn(siret) && !la_poste {
        return Err("luhn");
    }
    Ok(())
}

/// French VAT number: `FR` + 2-digit key + SIREN, key = (12 + 3 × (SIREN mod 97)) mod 97.
pub fn fr_vat_rule(vat: &str, siren: Option<&str>) -> Result<(), &'static str> {
    if vat.len() != 13 || !vat.starts_with("FR") || !vat[2..].chars().all(|c| c.is_ascii_digit()) {
        return Err("vat_format");
    }
    let (key, number) = vat[2..].split_at(2);
    if let Some(siren) = siren
        && number != siren
    {
        return Err("vat_siren");
    }
    let n: u64 = number.parse().map_err(|_| "vat_format")?;
    if key.parse::<u64>().ok() != Some((12 + 3 * (n % 97)) % 97) {
        return Err("vat_key");
    }
    Ok(())
}

/// French postal code: 5 digits.
pub fn postal_code_rule(code: &str) -> Result<(), &'static str> {
    if code.len() == 5 && code.chars().all(|c| c.is_ascii_digit()) {
        Ok(())
    } else {
        Err("postal_code")
    }
}

// ---- The form ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Address {
    pub line1: String,
    pub line2: String,
    pub postal_code: String,
    pub city: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Registration {
    pub siren: String,
    pub siret: String,
    pub rcs_city: String,
    pub vat_number: String,
    pub ape_code: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Mediator {
    pub name: String,
    pub url: String,
    pub address: String,
}

/// What Admin › Settings › Legal entity edits.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LegalEntityForm {
    pub legal_name: String,
    pub legal_form: String,
    pub share_capital_minor: Option<i64>,
    pub address: Address,
    pub country_code: String,
    pub email: String,
    pub phone: String,
    pub registration: Registration,
    pub publication_director: String,
    pub mediator: Mediator,
}

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn no_spaces(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

impl LegalEntityForm {
    /// Trims and normalises (identifiers without spaces, VAT and country in capitals).
    pub fn normalized(mut self) -> Self {
        for s in [
            &mut self.legal_name,
            &mut self.legal_form,
            &mut self.address.line1,
            &mut self.address.line2,
            &mut self.address.city,
            &mut self.phone,
            &mut self.registration.rcs_city,
            &mut self.publication_director,
            &mut self.mediator.name,
            &mut self.mediator.address,
        ] {
            *s = squeeze(s);
        }
        self.address.postal_code = no_spaces(&self.address.postal_code);
        self.country_code = self.country_code.trim().to_ascii_uppercase();
        self.email = self.email.trim().to_string();
        self.mediator.url = self.mediator.url.trim().to_string();
        let r = &mut self.registration;
        r.siren = no_spaces(&r.siren);
        r.siret = no_spaces(&r.siret);
        r.vat_number = no_spaces(&r.vat_number).to_ascii_uppercase();
        r.ape_code = no_spaces(&r.ape_code).replace('.', "").to_ascii_uppercase();
        if r.ape_code.len() == 5 {
            r.ape_code = format!("{}.{}", &r.ape_code[..2], &r.ape_code[2..]);
        }
        self
    }

    /// Field → rule code of every value that is set but wrong (empty optional fields pass: the
    /// checklist lists what is still missing). Never contains a value.
    pub fn errors(&self) -> BTreeMap<&'static str, &'static str> {
        let mut e = BTreeMap::new();
        if self.legal_name.is_empty() {
            e.insert("legal_name", "required");
        }
        if self.legal_form.is_empty() {
            e.insert("legal_form", "required");
        }
        if self.share_capital_minor.is_some_and(|c| c < 0) {
            e.insert("share_capital_minor", "positive");
        }
        if !(self.country_code.len() == 2
            && self.country_code.chars().all(|c| c.is_ascii_uppercase()))
        {
            e.insert("country_code", "country");
        }
        let fr = self.country_code == "FR";
        if fr
            && !self.address.postal_code.is_empty()
            && let Err(rule) = postal_code_rule(&self.address.postal_code)
        {
            e.insert("address.postal_code", rule);
        }
        if !self.email.is_empty() && !(self.email.contains('@') && !self.email.contains(' ')) {
            e.insert("email", "email");
        }
        let r = &self.registration;
        let siren_ok = if r.siren.is_empty() {
            None
        } else {
            match siren_rule(&r.siren) {
                Ok(()) => Some(r.siren.as_str()),
                Err(rule) => {
                    e.insert("registration.siren", rule);
                    None
                }
            }
        };
        if !r.siret.is_empty()
            && let Err(rule) = siret_rule(&r.siret, siren_ok)
        {
            e.insert("registration.siret", rule);
        }
        if fr
            && !r.vat_number.is_empty()
            && let Err(rule) = fr_vat_rule(&r.vat_number, siren_ok)
        {
            e.insert("registration.vat_number", rule);
        }
        if !r.ape_code.is_empty() {
            let a = r.ape_code.replace('.', "");
            let ok = a.len() == 5
                && a[..4].chars().all(|c| c.is_ascii_digit())
                && a[4..].chars().all(|c| c.is_ascii_uppercase());
            if !ok {
                e.insert("registration.ape_code", "ape");
            }
        }
        let m = &self.mediator;
        if !m.url.is_empty() && !m.url.starts_with("https://") {
            e.insert("mediator.url", "https");
        }
        if (m.name.is_empty()) != (m.url.is_empty()) {
            e.insert(
                if m.name.is_empty() {
                    "mediator.name"
                } else {
                    "mediator.url"
                },
                "required",
            );
        }
        e
    }

    /// Fields the checklist still needs (field codes; the screen shows their labels).
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let ph = |s: &str| {
            let l = s.to_lowercase();
            s.trim().is_empty() || l.contains("compléter") || l.contains("à confirmer")
        };
        if ph(&self.legal_name) {
            out.push("legal_name");
        }
        if ph(&self.address.line1) {
            out.push("address.line1");
        }
        if self.address.postal_code.trim().is_empty() {
            out.push("address.postal_code");
        }
        if ph(&self.address.city) {
            out.push("address.city");
        }
        if !self.email.contains('@') {
            out.push("email");
        }
        if self.country_code == "FR" {
            let r = &self.registration;
            if siren_rule(&r.siren).is_err() {
                out.push("registration.siren");
            }
            if r.siret.len() != 14 || !r.siret.chars().all(|c| c.is_ascii_digit()) {
                out.push("registration.siret");
            }
            if r.rcs_city.trim().is_empty() {
                out.push("registration.rcs_city");
            }
            if !(r.vat_number.starts_with("FR") && r.vat_number.len() == 13) {
                out.push("registration.vat_number");
            }
        }
        out
    }

    /// Mediator fields the checklist still needs.
    pub fn mediator_missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.mediator.name.trim().is_empty() {
            out.push("mediator.name");
        }
        if self.mediator.url.trim().is_empty() {
            out.push("mediator.url");
        }
        out
    }

    fn flat(&self) -> BTreeMap<&'static str, Value> {
        let r = &self.registration;
        BTreeMap::from([
            ("legal_name", json!(self.legal_name)),
            ("legal_form", json!(self.legal_form)),
            ("share_capital_minor", json!(self.share_capital_minor)),
            ("address.line1", json!(self.address.line1)),
            ("address.line2", json!(self.address.line2)),
            ("address.postal_code", json!(self.address.postal_code)),
            ("address.city", json!(self.address.city)),
            ("country_code", json!(self.country_code)),
            ("email", json!(self.email)),
            ("phone", json!(self.phone)),
            ("registration.siren", json!(r.siren)),
            ("registration.siret", json!(r.siret)),
            ("registration.rcs_city", json!(r.rcs_city)),
            ("registration.vat_number", json!(r.vat_number)),
            ("registration.ape_code", json!(r.ape_code)),
            ("publication_director", json!(self.publication_director)),
            ("mediator.name", json!(self.mediator.name)),
            ("mediator.url", json!(self.mediator.url)),
            ("mediator.address", json!(self.mediator.address)),
        ])
    }

    /// Names of the fields that differ from `before`.
    pub fn changed_fields(&self, before: &Self) -> Vec<&'static str> {
        let (a, b) = (self.flat(), before.flat());
        a.iter()
            .filter(|(k, v)| b.get(*k) != Some(*v))
            .map(|(k, _)| *k)
            .collect()
    }
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

/// The current identity of the shop's entity and the mediator.
pub async fn load(conn: &mut PgConnection) -> sqlx::Result<(Uuid, LegalEntityForm)> {
    let e = sqlx::query!(
        r#"SELECT id, legal_name, legal_form, share_capital_minor, address, country_code::text AS "country_code!",
                  registration, email::text AS email, phone, publication_director
           FROM legal_entities WHERE id = shop_entity_id()"#
    )
    .fetch_one(&mut *conn)
    .await?;
    let m: Value =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'shop.consumer_mediator'")
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(Value::Null);
    let (a, r) = (&e.address, &e.registration);
    Ok((
        e.id,
        LegalEntityForm {
            legal_name: e.legal_name,
            legal_form: e.legal_form,
            share_capital_minor: e.share_capital_minor,
            address: Address {
                line1: s(a, "line1"),
                line2: s(a, "line2"),
                postal_code: s(a, "postal_code"),
                city: s(a, "city"),
            },
            country_code: e.country_code,
            email: e.email.unwrap_or_default(),
            phone: e.phone.unwrap_or_default(),
            registration: Registration {
                siren: s(r, "siren"),
                siret: s(r, "siret"),
                rcs_city: s(r, "rcs_city"),
                vat_number: s(r, "vat_number"),
                ape_code: s(r, "ape_code"),
            },
            publication_director: e.publication_director.unwrap_or_default(),
            mediator: Mediator {
                name: s(&m, "name"),
                url: s(&m, "url"),
                address: s(&m, "address"),
            },
        },
    ))
}

/// Saves a validated form (call [`LegalEntityForm::errors`] first). Keeps the other keys of
/// `address` / `registration` (country, Moroccan identifiers...). Audits the changed field
/// names; returns them. Issued invoices keep their own `seller_snapshot`.
pub async fn save(
    conn: &mut PgConnection,
    actor: &Actor,
    form: &LegalEntityForm,
) -> sqlx::Result<Vec<&'static str>> {
    let (id, before) = load(&mut *conn).await?;
    let changed = form.changed_fields(&before);
    if changed.is_empty() {
        return Ok(changed);
    }
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let a = &form.address;
    let r = &form.registration;
    sqlx::query!(
        "UPDATE legal_entities SET legal_name = $2, legal_form = $3, share_capital_minor = $4,
                address = address || $5, country_code = $6, registration = registration || $7,
                email = $8::text::citext, phone = $9, publication_director = $10
         WHERE id = $1",
        id,
        form.legal_name,
        form.legal_form,
        form.share_capital_minor,
        json!({ "line1": a.line1, "line2": a.line2, "postal_code": a.postal_code, "city": a.city }),
        form.country_code,
        json!({ "siren": r.siren, "siret": r.siret, "rcs_city": r.rcs_city, "vat_number": r.vat_number, "ape_code": r.ape_code }),
        opt(&form.email),
        opt(&form.phone),
        opt(&form.publication_director)
    )
    .execute(&mut *conn)
    .await?;
    if changed.iter().any(|f| f.starts_with("mediator.")) {
        let m = &form.mediator;
        sqlx::query!(
            "UPDATE settings SET value = $1, updated_at = now() WHERE key = 'shop.consumer_mediator'",
            json!({ "name": m.name, "url": m.url, "address": m.address })
        )
        .execute(&mut *conn)
        .await?;
    }
    audit::record(
        conn,
        actor,
        "legal_entity.updated",
        "legal_entity",
        &id.to_string(),
        json!({ "fields": changed }),
    )
    .await?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> LegalEntityForm {
        LegalEntityForm {
            legal_name: "ESSAI SAS".into(),
            legal_form: "SAS".into(),
            share_capital_minor: Some(100_000),
            address: Address {
                line1: "1 rue".into(),
                postal_code: "75011".into(),
                city: "Paris".into(),
                ..Default::default()
            },
            country_code: "FR".into(),
            email: "a@b.fr".into(),
            registration: Registration {
                siren: "732829320".into(),
                siret: "73282932000074".into(),
                rcs_city: "Paris".into(),
                vat_number: "FR44732829320".into(),
                ape_code: "0125z".into(),
            },
            ..Default::default()
        }
        .normalized()
    }

    #[test]
    fn identifiers_follow_their_keys() {
        assert!(valid().errors().is_empty(), "{:?}", valid().errors());
        assert_eq!(valid().registration.ape_code, "01.25Z");
        assert_eq!(siren_rule("732829321"), Err("luhn"));
        assert_eq!(siren_rule("12345"), Err("digits_9"));
        assert_eq!(siret_rule("73282932000075", Some("732829320")), Err("luhn"));
        assert_eq!(
            siret_rule("40483304800022", Some("732829320")),
            Err("siren_prefix")
        );
        assert_eq!(
            fr_vat_rule("FR45732829320", Some("732829320")),
            Err("vat_key")
        );
        assert_eq!(
            fr_vat_rule("FR44123456789", Some("732829320")),
            Err("vat_siren")
        );
        assert_eq!(postal_code_rule("7501"), Err("postal_code"));
        let mut f = valid();
        f.registration.siren = "123456789".into();
        f.mediator.url = "http://x".into();
        let e = f.errors();
        assert_eq!(e.get("registration.siren"), Some(&"luhn"));
        assert_eq!(e.get("mediator.url"), Some(&"https"));
        assert!(
            !format!("{e:?}").contains("123456789"),
            "no value in errors"
        );
    }

    #[test]
    fn changed_fields_are_names_only() {
        let mut f = valid();
        f.phone = "+33 1 00 00 00 00".into();
        assert_eq!(f.changed_fields(&valid()), vec!["phone"]);
        assert!(valid().missing().is_empty());
        assert_eq!(
            valid().mediator_missing(),
            vec!["mediator.name", "mediator.url"]
        );
    }
}
