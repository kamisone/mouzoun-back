//! Redirects, written by the content that moves (`source = auto`, inside the business
//! transaction) and by staff (`manual`). Rules:
//!
//! - one hop: adding A → B rewrites every X → A into X → B (chains are flattened when written);
//! - live content wins: a path that content takes again loses its redirect;
//! - never a loop (the database trigger `redirects_no_loop` refuses it too);
//! - every change emits `seo.changed`, which bumps the redirect map version the Nuxt server
//!   checks every 15 seconds.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use serde::Serialize;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::routes::{self, Ctx};
use crate::{
    error::AppError,
    services::{
        audit::{self, Actor},
        outbox::{self, Event},
    },
};

/// Tells the cache layer what changed: the redirect map and/or sitemap files.
pub async fn changed(
    conn: &mut PgConnection,
    redirects: bool,
    sitemaps: &[&str],
) -> sqlx::Result<()> {
    outbox::enqueue(
        conn,
        &Event::SeoChanged {
            redirects,
            sitemaps: sitemaps.iter().map(|s| (*s).to_string()).collect(),
        },
    )
    .await?;
    Ok(())
}

/// Live content took `paths`: their redirects go (audit-logged).
pub async fn claim(conn: &mut PgConnection, actor: &Actor, paths: &[String]) -> sqlx::Result<u64> {
    let gone = sqlx::query!(
        "DELETE FROM redirects WHERE from_path = ANY($1) RETURNING id, from_path, to_path, source",
        paths
    )
    .fetch_all(&mut *conn)
    .await?;
    for r in &gone {
        audit::record(
            conn,
            actor,
            "redirect.claimed",
            "redirect",
            &r.id.to_string(),
            json!({ "from_path": r.from_path, "to_path": r.to_path, "source": r.source, "reason": "live_content" }),
        )
        .await?;
    }
    Ok(gone.len() as u64)
}

/// One automatic redirect `from` → `to`, after flattening the chains ending on `from`.
async fn auto(
    conn: &mut PgConnection,
    from: &str,
    to: &str,
    entity: (&str, Option<Uuid>),
    locale: Option<&str>,
    active: bool,
    note: Option<&str>,
) -> sqlx::Result<()> {
    if from == to {
        return Ok(());
    }
    // A redirect starting where we go would loop back.
    sqlx::query!(
        "DELETE FROM redirects WHERE from_path = $1 AND to_path = $2",
        to,
        from
    )
    .execute(&mut *conn)
    .await?;
    if active {
        sqlx::query!(
            "UPDATE redirects SET to_path = $2, updated_at = now() WHERE to_path = $1 AND from_path <> $2",
            from,
            to
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!(
        "INSERT INTO redirects (from_path, to_path, status_code, source, entity_type, entity_id, locale, is_active, note)
         VALUES ($1, $2, 301, 'auto', $3, $4, $5, $6, $7)
         ON CONFLICT (from_path) DO UPDATE SET to_path = EXCLUDED.to_path, status_code = 301, source = 'auto',
             entity_type = EXCLUDED.entity_type, entity_id = EXCLUDED.entity_id, locale = EXCLUDED.locale,
             is_active = EXCLUDED.is_active, note = EXCLUDED.note, updated_at = now()
         WHERE redirects.source = 'auto' OR NOT redirects.is_active",
        from,
        to,
        entity.0,
        entity.1,
        locale,
        active,
        note
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A piece of content's URLs changed (slug, parent): one 301 per language whose path changed,
/// and the new paths win over older redirects. Call inside the transaction that made the change.
pub async fn record_path_change(
    conn: &mut PgConnection,
    actor: &Actor,
    entity_type: &str,
    entity_id: Option<Uuid>,
    old_paths: &BTreeMap<String, String>,
    new_paths: &BTreeMap<String, String>,
) -> sqlx::Result<Vec<(String, String)>> {
    let new: Vec<String> = new_paths.values().cloned().collect();
    claim(conn, actor, &new).await?;
    let mut made = vec![];
    for (locale, old) in old_paths {
        if let Some(to) = new_paths.get(locale).filter(|n| *n != old) {
            auto(
                conn,
                old,
                to,
                (entity_type, entity_id),
                Some(locale),
                true,
                None,
            )
            .await?;
            made.push((old.clone(), to.clone()));
        }
    }
    if !made.is_empty() || !new.is_empty() {
        changed(conn, true, &[]).await?;
    }
    Ok(made)
}

/// Content left the site. Unpublished (`active = false`): the redirects to `targets` are kept
/// inactive (a temporary unpublish does not send visitors away for good); deleted: active.
pub async fn record_removal(
    conn: &mut PgConnection,
    entity_type: &str,
    entity_id: Uuid,
    paths: &BTreeMap<String, String>,
    targets: &BTreeMap<String, String>,
    active: bool,
) -> sqlx::Result<()> {
    for (locale, from) in paths {
        let Some(to) = targets.get(locale) else {
            continue;
        };
        let note = (!active).then_some("Contenu dépublié : activée à la suppression.");
        auto(
            conn,
            from,
            to,
            (entity_type, Some(entity_id)),
            Some(locale),
            active,
            note,
        )
        .await?;
    }
    if active {
        // Redirects created at unpublish time become live.
        sqlx::query!(
            "UPDATE redirects SET is_active = true, note = NULL, updated_at = now()
             WHERE entity_type = $1 AND entity_id = $2 AND source = 'auto' AND NOT is_active",
            entity_type,
            entity_id
        )
        .execute(&mut *conn)
        .await?;
    }
    changed(conn, true, &[]).await?;
    Ok(())
}

/// Content is public again: its own paths lose the (inactive or active) redirects.
pub async fn record_republish(
    conn: &mut PgConnection,
    actor: &Actor,
    paths: &BTreeMap<String, String>,
) -> sqlx::Result<()> {
    let p: Vec<String> = paths.values().cloned().collect();
    if claim(conn, actor, &p).await? > 0 {
        changed(conn, true, &[]).await?;
    }
    Ok(())
}

// ---- Manual redirects ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Checked {
    pub from_path: String,
    pub to_path: String,
    /// Set when `to_path` was itself redirected: we point at the final target.
    pub flattened_from: Option<String>,
    pub external: bool,
}

pub fn field_error(field: &str, code: &str) -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_fields",
        "some fields are invalid",
    )
    .with_details(json!({ "fields": { field: code } }))
}

/// Validates a manual redirect: `from_path` is a relative path that no live content answers;
/// `to_path` a relative path or an `https://` URL, not the same, not a loop. A target that is
/// itself redirected is replaced by its final target.
pub async fn check_manual(
    conn: &mut PgConnection,
    ctx: &Ctx,
    id: Option<Uuid>,
    from: &str,
    to: &str,
) -> Result<Checked, AppError> {
    let from_path = super::path::normalize(from.trim())
        .ok_or_else(|| field_error("from_path", "relative_path"))?;
    if from_path == "/" {
        return Err(field_error("from_path", "live_content"));
    }
    if routes::resolve(conn, ctx, &from_path).await?.is_some() {
        return Err(field_error("from_path", "live_content"));
    }
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM redirects WHERE from_path = $1 AND ($2::uuid IS NULL OR id <> $2)) AS "x!""#,
        from_path,
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    if taken {
        return Err(field_error("from_path", "taken"));
    }
    let to = to.trim();
    let external = to.starts_with("https://");
    if to.starts_with("http://") {
        return Err(field_error("to_path", "https_only"));
    }
    let mut to_path = if external {
        to.to_string()
    } else {
        super::path::normalize(to).ok_or_else(|| field_error("to_path", "relative_path"))?
    };
    if to_path == from_path {
        return Err(field_error("to_path", "loop"));
    }
    let mut flattened_from = None;
    if !external {
        // One hop: follow the target's own redirect (chains are already flat).
        if let Some(next) = sqlx::query_scalar!(
            "SELECT to_path FROM redirects WHERE from_path = $1 AND is_active AND ($2::uuid IS NULL OR id <> $2)",
            to_path,
            id
        )
        .fetch_optional(&mut *conn)
        .await?
        {
            if next == from_path {
                return Err(field_error("to_path", "loop"));
            }
            flattened_from = Some(std::mem::replace(&mut to_path, next));
        }
    }
    Ok(Checked {
        from_path,
        to_path,
        flattened_from,
        external,
    })
}

/// After a manual redirect A → B is written: X → A becomes X → B.
pub async fn flatten_into(conn: &mut PgConnection, from: &str, to: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query!(
        "UPDATE redirects SET to_path = $2, updated_at = now() WHERE to_path = $1 AND from_path <> $2",
        from,
        to
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Database loop refusals become the `to_path: loop` field error.
pub fn map_loop(e: sqlx::Error) -> AppError {
    if e.as_database_error()
        .is_some_and(|d| d.message().contains("redirect_loop"))
    {
        return field_error("to_path", "loop");
    }
    e.into()
}

// ---- One call per content change ------------------------------------------------------------------

/// A piece of content's URLs before a change (see [`after_change`]).
#[derive(Debug, Clone)]
pub struct Before {
    pub paths: BTreeMap<String, String>,
    pub served: bool,
}

/// Its URLs now (`None`: no such content).
pub async fn snapshot(
    conn: &mut PgConnection,
    entity_type: &str,
    id: Uuid,
) -> sqlx::Result<Option<Before>> {
    let ctx = Ctx::load(conn, chrono::Utc::now()).await?;
    Ok(
        routes::entries(conn, &ctx, routes::Filter::One(entity_type, id))
            .await?
            .into_iter()
            .next()
            .map(|e| Before {
                paths: e.versions.into_iter().map(|(l, v)| (l, v.path)).collect(),
                served: e.served,
            }),
    )
}

/// The closest public page above a piece of content, per language: the parent category or
/// page, the listing of its type, or the home page.
pub async fn parent_targets(
    conn: &mut PgConnection,
    ctx: &Ctx,
    entity_type: &str,
    id: Uuid,
) -> sqlx::Result<BTreeMap<String, String>> {
    let parent: Option<(&str, Uuid)> = match entity_type {
        routes::SHOP_PRODUCT => sqlx::query_scalar!(
            "SELECT pc.category_id FROM product_categories pc JOIN shop_visible_categories() v ON v.id = pc.category_id
             WHERE pc.product_id = $1 ORDER BY pc.is_primary DESC, v.depth DESC LIMIT 1",
            id
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|c| (routes::SHOP_CATEGORY, c)),
        routes::SHOP_CATEGORY => sqlx::query_scalar!(
            r#"WITH RECURSIVE up AS (
                   SELECT parent_id, 1 AS n FROM categories WHERE id = $1
                   UNION ALL SELECT c.parent_id, up.n + 1 FROM categories c JOIN up ON c.id = up.parent_id WHERE up.n < 20)
               SELECT up.parent_id AS "id!" FROM up JOIN shop_visible_categories() v ON v.id = up.parent_id ORDER BY up.n LIMIT 1"#,
            id
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|c| (routes::SHOP_CATEGORY, c)),
        routes::PAGE | routes::HOME => sqlx::query_scalar!("SELECT parent_id FROM pages WHERE id = $1", id)
            .fetch_optional(&mut *conn)
            .await?
            .flatten()
            .map(|p| (routes::PAGE, p)),
        _ => None,
    };
    let mut out = BTreeMap::new();
    if let Some((t, pid)) = parent
        && let Some(e) = routes::entries(conn, ctx, routes::Filter::One(t, pid))
            .await?
            .into_iter()
            .next()
        && e.served
    {
        for (l, v) in e.versions {
            out.insert(l, v.path);
        }
    }
    for l in &ctx.locales {
        if out.contains_key(l) {
            continue;
        }
        let fallback = match entity_type {
            routes::SHOP_PRODUCT | routes::SHOP_CATEGORY if ctx.shop_open => {
                ctx.path(routes::LISTING_SHOP, l, "")
            }
            routes::ACCOMMODATION => ctx.path(routes::LISTING_STAYS, l, ""),
            routes::ACTIVITY => ctx.path(routes::LISTING_ACTIVITIES, l, ""),
            _ => None,
        };
        out.insert(l.clone(), fallback.unwrap_or_else(|| ctx.home(l)));
    }
    Ok(out)
}

/// After a content change, inside its transaction: slug or parent moves give 301s, unpublishing
/// keeps inactive redirects to the closest public parent, deleting activates them, publishing
/// again (or taking a path) removes the redirects on its own paths.
pub async fn after_change(
    conn: &mut PgConnection,
    actor: &Actor,
    entity_type: &str,
    id: Uuid,
    before: Option<Before>,
) -> sqlx::Result<()> {
    let ctx = Ctx::load(conn, chrono::Utc::now()).await?;
    let entry = routes::entries(conn, &ctx, routes::Filter::One(entity_type, id))
        .await?
        .into_iter()
        .next();
    let (after_paths, served, deleted, sitemap) = match &entry {
        Some(e) => (
            e.versions
                .iter()
                .map(|(l, v)| (l.clone(), v.path.clone()))
                .collect::<BTreeMap<_, _>>(),
            e.served,
            e.blocked == Some("deleted"),
            e.sitemap,
        ),
        None => (BTreeMap::new(), false, true, ""),
    };
    let before = before.unwrap_or(Before {
        paths: BTreeMap::new(),
        served: false,
    });
    let entity_type = entry.as_ref().map_or(entity_type, |e| e.entity_type);
    if served {
        // Live (again): its paths win; moved paths redirect to the new ones.
        record_path_change(
            conn,
            actor,
            entity_type,
            Some(id),
            &before.paths,
            &after_paths,
        )
        .await?;
        // A language removed: its old address goes to the closest public parent.
        let dropped: BTreeMap<String, String> = before
            .paths
            .iter()
            .filter(|(l, _)| !after_paths.contains_key(*l))
            .map(|(l, p)| (l.clone(), p.clone()))
            .collect();
        if !dropped.is_empty() {
            let targets = parent_targets(conn, &ctx, entity_type, id).await?;
            record_removal(conn, entity_type, id, &dropped, &targets, true).await?;
        }
    } else if before.served || deleted {
        let targets = parent_targets(conn, &ctx, entity_type, id).await?;
        // Unpublished: from every path it had (old and current); deleted: active.
        let mut paths = before.paths.clone();
        for (l, p) in &after_paths {
            paths.entry(l.clone()).or_insert_with(|| p.clone());
        }
        record_removal(conn, entity_type, id, &paths, &targets, deleted).await?;
    }
    if !sitemap.is_empty() {
        changed(conn, false, &[sitemap]).await?;
    }
    Ok(())
}
