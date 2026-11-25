//! One-off, idempotent set-ups before the shop opens (`mouzoun-cli`): Mouzoun's legal pages and
//! the placeholder delivery rates for mainland France.

use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::Staff,
    modules::pages::{
        model::{BlockDraft, PageText, SaveTree},
        registry::SectionRegistry,
        service::{self, PublishRequest},
    },
    services::audit::Actor,
};

use super::legal_pages::{PAGES, template};

/// What happened to one legal page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageOutcome {
    Created,
    Replaced,
    AlreadyInstalled,
}

/// The staff account the pages are written as: `email`, or the oldest super admin.
pub async fn staff_for(pool: &PgPool, email: Option<&str>) -> anyhow::Result<Staff> {
    let id: Option<Uuid> = match email {
        Some(e) => sqlx::query_scalar!("SELECT id FROM users WHERE email = $1", e)
            .fetch_optional(pool)
            .await?,
        None => sqlx::query_scalar!(
            "SELECT u.id FROM users u JOIN user_roles ur ON ur.user_id = u.id JOIN roles r ON r.id = ur.role_id
             WHERE r.code = 'super_admin' ORDER BY u.created_at LIMIT 1"
        )
        .fetch_optional(pool)
        .await?,
    };
    let id = id.ok_or_else(|| {
        anyhow::anyhow!("no staff account to write the pages as (use --as <email>)")
    })?;
    let permissions: HashSet<String> = sqlx::query_scalar!("SELECT code FROM permissions")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
    Ok(Staff { id, permissions })
}

fn rich_text(bodies: BTreeMap<String, serde_json::Value>) -> BlockDraft {
    BlockDraft {
        id: Uuid::now_v7(),
        block_type: "rich_text".into(),
        name: None,
        anchor_id: None,
        is_visible: true,
        active_period: None,
        visible_on: Default::default(),
        hidden_locales: Vec::new(),
        appearance: json!({}),
        analytics_label: None,
        settings: json!({ "max_width": "narrow" }),
        content: bodies,
        items: Vec::new(),
        shared_block_id: None,
        shared: None,
    }
}

/// Installs the six legal pages from the templates and publishes them, pointing the
/// `legal.*_page_id` settings at them (the admin then edits them in the builder). A page already
/// set up is left alone unless `force` (its content is then replaced by the template and
/// republished).
pub async fn install_legal_pages(
    pool: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    force: bool,
) -> anyhow::Result<Vec<(&'static str, PageOutcome)>> {
    let actor = Actor::system();
    let mut out = Vec::new();
    for (key, texts) in PAGES {
        let setting = format!("legal.{key}_page_id");
        let current: Option<Uuid> = sqlx::query_scalar!(
            r#"SELECT p.id FROM settings s JOIN pages p ON p.id::text = s.value #>> '{}'
               WHERE s.key = $1 AND p.deleted_at IS NULL"#,
            setting
        )
        .fetch_optional(pool)
        .await?;
        if current.is_some() && !force {
            out.push((key, PageOutcome::AlreadyInstalled));
            continue;
        }
        let bodies: BTreeMap<String, serde_json::Value> = texts
            .iter()
            .map(|(locale, _, _)| {
                let body = template(key, locale).unwrap_or_default();
                (locale.to_string(), json!({ "body": body }))
            })
            .collect();
        let (page, outcome) = match current {
            Some(id) => (id, PageOutcome::Replaced),
            None => {
                // A page of an earlier attempt with the same slug is reused rather than duplicated.
                let existing: Option<Uuid> = sqlx::query_scalar!(
                    "SELECT p.id FROM pages p JOIN page_translations t ON t.page_id = p.id
                     WHERE p.page_type = 'legal' AND p.deleted_at IS NULL AND t.locale = 'fr' AND t.slug = $1",
                    texts[0].2
                )
                .fetch_optional(pool)
                .await?;
                match existing {
                    Some(id) => (id, PageOutcome::Replaced),
                    None => {
                        let translations = texts
                            .iter()
                            .map(|(locale, title, slug)| {
                                (
                                    locale.to_string(),
                                    PageText {
                                        title: (*title).into(),
                                        slug: (*slug).into(),
                                        excerpt: None,
                                        seo_title: None,
                                        seo_description: None,
                                        og_media_id: None,
                                        noindex: false,
                                    },
                                )
                            })
                            .collect();
                        let id = service::create_page(
                            pool,
                            staff,
                            &actor,
                            service::NewPage {
                                page_type: "legal".into(),
                                translations,
                            },
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("create legal page {key}: {e:?}"))?;
                        (id, PageOutcome::Created)
                    }
                }
            }
        };
        let mut conn = pool.acquire().await?;
        let version = service::builder_tree(&mut conn, page)
            .await
            .map_err(|e| anyhow::anyhow!("legal page {key}: {e:?}"))?
            .draft_version;
        drop(conn);
        service::save_tree(
            pool,
            reg,
            staff,
            &actor,
            page,
            version,
            SaveTree {
                blocks: vec![rich_text(bodies)],
                translations: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("save legal page {key}: {e:?}"))?;
        service::publish(
            pool,
            reg,
            &actor,
            Some(staff.id),
            page,
            Utc::now(),
            PublishRequest {
                publish_at: None,
                note: Some("Mouzoun legal template".into()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("publish legal page {key}: {e:?}"))?;
        sqlx::query!(
            "UPDATE settings SET value = to_jsonb($2::text), updated_at = now() WHERE key = $1",
            setting,
            page.to_string()
        )
        .execute(pool)
        .await?;
        out.push((key, outcome));
    }
    Ok(out)
}

/// Placeholder rates: (code, carrier, delivery type, names, delays, [(max kg, price cents)]).
type Bracket = (i32, i32, i64);
const BRACKETS_COLISSIMO: [Bracket; 5] = [
    (0, 1000, 790),
    (1001, 3000, 990),
    (3001, 5000, 1290),
    (5001, 10000, 1790),
    (10001, 20000, 2690),
];
const BRACKETS_RELAY: [Bracket; 5] = [
    (0, 1000, 490),
    (1001, 3000, 650),
    (3001, 5000, 890),
    (5001, 10000, 1390),
    (10001, 20000, 1990),
];
/// (code prefix, carrier, delivery type, delay in days, brackets, names fr/en/ar).
type Carrier = (
    &'static str,
    &'static str,
    &'static str,
    (i32, i32),
    &'static [Bracket],
    [&'static str; 3],
);
/// Free delivery from this order amount (placeholder too).
pub const PLACEHOLDER_FREE_OVER_MINOR: i64 = 6000;

/// Seeds placeholder delivery rates for mainland France (Colissimo at home, Mondial Relay in a
/// pick-up point, five weight brackets each, free from 60 €), flagged `is_placeholder`: never
/// offered at checkout, not counted by the checklist, until an admin saves or confirms them.
/// Idempotent: a rate whose code exists in the zone and profile is left as it is. Returns the
/// number of rates created.
pub async fn seed_placeholder_shipping(pool: &PgPool) -> anyhow::Result<usize> {
    let mut tx = pool.begin().await?;
    let profile: Uuid = match sqlx::query_scalar!(
        "SELECT id FROM shipping_profiles ORDER BY is_default DESC, created_at LIMIT 1"
    )
    .fetch_optional(&mut *tx)
    .await?
    {
        Some(id) => id,
        None => sqlx::query_scalar!(
            "INSERT INTO shipping_profiles (code, name, is_default) VALUES ('standard', 'Standard', true) RETURNING id"
        )
        .fetch_one(&mut *tx)
        .await?,
    };
    // The French zone: an active zone covering FR, or a new "France métropolitaine" (Corsica and
    // overseas excluded by postal code).
    let zone: Uuid = match sqlx::query_scalar!(
        "SELECT id FROM shipping_zones WHERE 'FR' = ANY(country_codes) AND NOT is_export ORDER BY is_active DESC, sort_order LIMIT 1"
    )
    .fetch_optional(&mut *tx)
    .await?
    {
        Some(id) => id,
        None => {
            let patterns: Vec<String> = (1..=95).filter(|d| *d != 20).map(|d| format!("{d:02}*")).collect();
            let id = sqlx::query_scalar!(
                "INSERT INTO shipping_zones (code, country_codes, postal_code_patterns, sort_order)
                 VALUES ('france_metro', '{FR}', $1, 0) RETURNING id",
                &patterns
            )
            .fetch_one(&mut *tx)
            .await?;
            for (locale, name) in [("fr", "France métropolitaine"), ("en", "Mainland France"), ("ar", "فرنسا القارية")] {
                sqlx::query!(
                    "INSERT INTO shipping_zone_translations (zone_id, locale, name) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                    id,
                    locale,
                    name
                )
                .execute(&mut *tx)
                .await?;
            }
            id
        }
    };
    let mut created = 0;
    let carriers: [Carrier; 2] = [
        (
            "colissimo",
            "colissimo",
            "home",
            (2, 3),
            &BRACKETS_COLISSIMO,
            [
                "Colissimo à domicile",
                "Colissimo home delivery",
                "كوليسيمو إلى المنزل",
            ],
        ),
        (
            "mondial_relay",
            "mondial_relay",
            "relay_point",
            (3, 5),
            &BRACKETS_RELAY,
            [
                "Mondial Relay en point relais",
                "Mondial Relay pick-up point",
                "مونديال ريلاي في نقطة استلام",
            ],
        ),
    ];
    for (prefix, carrier, delivery, (dmin, dmax), brackets, names) in carriers {
        for (i, (min, max, price)) in brackets.iter().enumerate() {
            let code = format!("fr_{prefix}_{}_{}kg", min / 1000, max / 1000);
            let id: Option<Uuid> = sqlx::query_scalar!(
                "INSERT INTO shipping_rates (zone_id, profile_id, code, kind, carrier, amount_minor, min_weight_grams,
                                             max_weight_grams, free_over_minor, delivery_days_min, delivery_days_max,
                                             delivery_type, sort_order, is_placeholder)
                 VALUES ($1, $2, $3::text::citext, 'flat', $4, $5, $6, $7, $8, $9, $10, $11, $12, true)
                 ON CONFLICT (zone_id, profile_id, code) DO NOTHING RETURNING id",
                zone,
                profile,
                code,
                carrier,
                price,
                min,
                max,
                PLACEHOLDER_FREE_OVER_MINOR,
                dmin,
                dmax,
                delivery,
                (if delivery == "home" { 10 } else { 20 }) + i as i32
            )
            .fetch_optional(&mut *tx)
            .await?;
            let Some(id) = id else { continue };
            created += 1;
            for (locale, name) in ["fr", "en", "ar"].into_iter().zip(names) {
                sqlx::query!(
                    "INSERT INTO shipping_rate_translations (rate_id, locale, name) VALUES ($1, $2, $3)",
                    id,
                    locale,
                    name
                )
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    crate::services::audit::record(
        &mut tx,
        &Actor::system(),
        "shipping_rates.placeholders_seeded",
        "shipping_zone",
        &zone.to_string(),
        json!({ "created": created }),
    )
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// Paragraphs of the earlier templates that started with U+2696 (scales) were notes for a
/// reviewer: removed from the legal pages, the rest of the text unchanged.
const NOTE_MARK: char = '\u{2696}';

/// Removes those note paragraphs from an HTML text (`<p>` starting with the mark).
pub fn remove_note_paragraphs(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find("<p>") {
        let after = &rest[i + 3..];
        if after.trim_start().starts_with(NOTE_MARK) {
            out.push_str(&rest[..i]);
            rest = match after.find("</p>") {
                Some(end) => after[end + 4..].trim_start_matches('\n'),
                None => "",
            };
        } else {
            out.push_str(&rest[..i + 3]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn clean_strings(v: &mut serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) if s.contains(NOTE_MARK) => {
            let cleaned = remove_note_paragraphs(s);
            let changed = cleaned != *s;
            *s = cleaned;
            changed
        }
        serde_json::Value::Array(a) => a.iter_mut().fold(false, |c, x| clean_strings(x) | c),
        serde_json::Value::Object(m) => m.values_mut().fold(false, |c, x| clean_strings(x) | c),
        _ => false,
    }
}

/// What happened to one legal page during the clean-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanOutcome {
    /// Notes removed from the draft and published as a new revision (author: system).
    Republished,
    /// Notes removed from the draft; the draft had other unpublished changes, so it is left for
    /// the admin to publish.
    DraftOnly,
    Clean,
}

/// Removes the reviewer notes from every legal page's draft and publishes the cleaned text as a
/// new revision (author `system`), so the history stays intact. Idempotent.
pub async fn remove_legal_page_notes(
    pool: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
) -> anyhow::Result<Vec<(String, CleanOutcome)>> {
    let pages = sqlx::query!(
        r#"SELECT p.id, p.has_unpublished_changes, COALESCE(t.slug, p.id::text) AS "slug!",
                  EXISTS (SELECT 1 FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'published'
                          AND strpos(r.snapshot::text, $1) > 0) AS "published_has_notes!"
           FROM pages p LEFT JOIN page_translations t ON t.page_id = p.id AND t.locale = 'fr'
           WHERE p.page_type = 'legal' AND p.deleted_at IS NULL ORDER BY 3"#,
        NOTE_MARK.to_string()
    )
    .fetch_all(pool)
    .await?;
    let actor = Actor::system();
    let mut out = Vec::new();
    for p in pages {
        let mut conn = pool.acquire().await?;
        let tree = service::builder_tree(&mut conn, p.id)
            .await
            .map_err(|e| anyhow::anyhow!("legal page {}: {e:?}", p.slug))?;
        drop(conn);
        let mut blocks = serde_json::to_value(&tree.blocks)?;
        let draft_changed = clean_strings(&mut blocks);
        if !draft_changed && !p.published_has_notes {
            out.push((p.slug, CleanOutcome::Clean));
            continue;
        }
        if draft_changed {
            service::save_tree(
                pool,
                reg,
                staff,
                &actor,
                p.id,
                tree.draft_version,
                SaveTree {
                    blocks: serde_json::from_value::<Vec<BlockDraft>>(blocks)?,
                    translations: None,
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("save legal page {}: {e:?}", p.slug))?;
        }
        if p.has_unpublished_changes || tree.published_revision.is_none() {
            out.push((p.slug, CleanOutcome::DraftOnly));
            continue;
        }
        service::publish(
            pool,
            reg,
            &actor,
            None,
            p.id,
            Utc::now(),
            PublishRequest {
                publish_at: None,
                note: Some("Reviewer notes removed".into()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("publish legal page {}: {e:?}", p.slug))?;
        out.push((p.slug, CleanOutcome::Republished));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_note_paragraphs_go() {
        let html = format!(
            "<p>{NOTE_MARK}\u{fe0f} note</p>\n<h3>Titre</h3>\n<p>Texte {{{{seller.siren}}}}</p>"
        );
        assert_eq!(
            remove_note_paragraphs(&html),
            "<h3>Titre</h3>\n<p>Texte {{seller.siren}}</p>"
        );
        assert_eq!(remove_note_paragraphs("<p>a</p>"), "<p>a</p>");
    }
}
