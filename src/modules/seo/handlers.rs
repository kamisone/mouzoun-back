//! SEO HTTP adapters: the redirect map and hit counting for the Nuxt server, sitemaps, head tags
//! (public), and Admin › SEO (redirects, 404 log, sitemap status, URL tester).

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    HEAD_CACHE_PREFIX, hits,
    path::{absolute, normalize},
    redirects::{self, check_manual, field_error, map_loop},
    routes::{self, Ctx, judge},
    sitemap,
};
use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::audit,
    state::AppState,
};

fn xml(body: String) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/xml; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=3600"),
            ),
        ],
        body,
    )
        .into_response()
}

async fn ctx(state: &AppState) -> AppResult<Ctx> {
    let mut conn = state.db.acquire().await?;
    Ok(Ctx::load(&mut conn, state.clock.now()).await?)
}

// ---- Public: the Nuxt server ------------------------------------------------------------------------

pub async fn redirects_version(State(state): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(
        json!({ "version": hits::version(&state.cache, &state.db).await? }),
    ))
}

pub async fn redirects_map(State(state): State<AppState>) -> AppResult<Json<hits::RedirectMap>> {
    Ok(Json(hits::map(&state.cache, &state.db).await?))
}

/// Batched visits from the Nuxt server (`x-internal-token`). Counting never fails a request.
pub async fn hits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<hits::Hits>,
) -> AppResult<StatusCode> {
    if let Some(token) = state.config.internal_api_token.as_deref() {
        let given = headers
            .get("x-internal-token")
            .and_then(|v| v.to_str().ok());
        if given != Some(token) {
            return Err(AppError::Forbidden);
        }
    }
    if let Err(e) = hits::add(&state.cache, &batch).await {
        tracing::warn!(error = %e, "seo: hits lost (redis)");
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn sitemap_index(State(state): State<AppState>) -> AppResult<Response> {
    Ok(xml(sitemap::index(&state).await?))
}

pub async fn sitemap_file(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> AppResult<Response> {
    let (name, page) = sitemap::parse_name(&file).ok_or(AppError::NotFound)?;
    Ok(xml(sitemap::file(&state, name, page)
        .await?
        .ok_or(AppError::NotFound)?))
}

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Head {
    /// The registry knows this path (content or listing).
    pub known: bool,
    pub canonical: Option<String>,
    pub noindex: bool,
    pub reason: Option<String>,
    /// `{hreflang, href}`: every indexable language version, then `x-default`.
    pub alternates: Vec<Value>,
}

/// Canonical, hreflang and robots for a public path, from the registry (same rule as the sitemap).
pub async fn head(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> AppResult<Json<Head>> {
    let path = normalize(&q.path).ok_or_else(|| AppError::BadRequest("path".into()))?;
    let key = format!("{HEAD_CACHE_PREFIX}{path}");
    if let Some(h) = state.cache.get_json::<Head>(&key).await {
        return Ok(Json(h));
    }
    let ctx = ctx(&state).await?;
    let base = sitemap::base_url(&state);
    let mut conn = state.db.acquire().await?;
    let found = routes::lookup(&mut conn, &ctx, &path).await?;
    let h = match found {
        None => Head {
            known: false,
            canonical: None,
            noindex: false,
            reason: None,
            alternates: vec![],
        },
        Some(f) => {
            let verdict = judge(&ctx, &f.entry, &f.locale, &base);
            let own = f.entry.versions.get(&f.locale);
            let canonical = match own {
                Some(v) => match v
                    .canonical
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                {
                    Some(c) => c.to_string(),
                    None => absolute(&base, &v.path),
                },
                None => absolute(&base, &path),
            };
            let urls = sitemap::entry_urls(&ctx, &f.entry, &base, &Default::default());
            let alternates = urls
                .first()
                .map(|u| {
                    u.alternates
                        .iter()
                        .map(|(l, href)| json!({ "hreflang": l, "href": href }))
                        .collect()
                })
                .unwrap_or_default();
            Head {
                known: true,
                canonical: Some(canonical),
                noindex: !verdict.is_indexable(),
                reason: match verdict {
                    routes::Indexability::NotIndexable(r) => Some(r.to_string()),
                    routes::Indexability::Indexable => None,
                },
                alternates,
            }
        }
    };
    state
        .cache
        .set_json(&key, &h, Duration::from_secs(3600))
        .await;
    Ok(Json(h))
}

/// `seo.robots_extra` for robots.txt (the Nuxt server writes the rest).
pub async fn robots(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let extra: Option<Value> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'seo.robots_extra'")
            .fetch_optional(&state.db)
            .await?;
    Ok(Json(
        json!({ "extra": extra.as_ref().and_then(|v| v.as_str()).unwrap_or("") }),
    ))
}

// ---- Admin: redirects ----------------------------------------------------------------------------

fn can_read(staff: &Staff) -> AppResult<()> {
    if staff.can("seo.manage") || staff.can("content.read") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RedirectQuery {
    pub q: Option<String>,
    pub source: Option<String>,
    pub code: Option<i16>,
    pub active: Option<bool>,
    /// No visit for six months.
    pub stale: Option<bool>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

pub async fn admin_redirects(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<RedirectQuery>,
) -> AppResult<Json<Value>> {
    can_read(&staff)?;
    let per_page = q.per_page.unwrap_or(25).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);
    const SORTS: [&str; 6] = [
        "from_path",
        "to_path",
        "hits",
        "last_hit_at",
        "created_at",
        "status_code",
    ];
    let sort = q
        .sort
        .as_deref()
        .filter(|s| SORTS.contains(s))
        .unwrap_or("created_at");
    let desc = q.dir.as_deref() != Some("asc");
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query!(
        r#"SELECT r.id, r.from_path, r.to_path, r.status_code, r.source, r.entity_type, r.entity_id, r.locale, r.note,
                  r.hits, r.last_hit_at, r.is_active, r.created_at, r.updated_at, count(*) OVER () AS "total!"
           FROM redirects r
           WHERE ($1::text IS NULL OR r.from_path ILIKE '%' || $1 || '%' OR r.to_path ILIKE '%' || $1 || '%' OR r.note ILIKE '%' || $1 || '%')
             AND ($2::text IS NULL OR r.source = $2)
             AND ($3::smallint IS NULL OR r.status_code = $3)
             AND ($4::bool IS NULL OR r.is_active = $4)
             AND (NOT $5 OR ((r.last_hit_at IS NULL OR r.last_hit_at < now() - interval '6 months') AND r.created_at < now() - interval '6 months'))
           ORDER BY
             CASE WHEN $6 = 'from_path' AND NOT $7 THEN r.from_path END ASC, CASE WHEN $6 = 'from_path' AND $7 THEN r.from_path END DESC,
             CASE WHEN $6 = 'to_path' AND NOT $7 THEN r.to_path END ASC, CASE WHEN $6 = 'to_path' AND $7 THEN r.to_path END DESC,
             CASE WHEN $6 = 'hits' AND NOT $7 THEN r.hits END ASC, CASE WHEN $6 = 'hits' AND $7 THEN r.hits END DESC,
             CASE WHEN $6 = 'status_code' AND NOT $7 THEN r.status_code END ASC, CASE WHEN $6 = 'status_code' AND $7 THEN r.status_code END DESC,
             CASE WHEN $6 = 'last_hit_at' AND NOT $7 THEN r.last_hit_at END ASC NULLS FIRST, CASE WHEN $6 = 'last_hit_at' AND $7 THEN r.last_hit_at END DESC NULLS LAST,
             CASE WHEN $6 = 'created_at' AND NOT $7 THEN r.created_at END ASC, CASE WHEN $6 = 'created_at' AND $7 THEN r.created_at END DESC,
             r.from_path
           LIMIT $8 OFFSET $9"#,
        search,
        q.source.as_deref().filter(|s| !s.is_empty()),
        q.code,
        q.active,
        q.stale.unwrap_or(false),
        sort,
        desc,
        per_page,
        (page - 1) * per_page
    )
    .fetch_all(&state.db)
    .await?;
    let uses_rare = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM redirects WHERE status_code IN (307, 308)) AS "x!""#
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({
        "data": rows.iter().map(|r| json!({
            "id": r.id, "from_path": r.from_path, "to_path": r.to_path, "status_code": r.status_code, "source": r.source,
            "entity_type": r.entity_type, "entity_id": r.entity_id, "locale": r.locale, "note": r.note, "hits": r.hits,
            "last_hit_at": r.last_hit_at, "is_active": r.is_active, "created_at": r.created_at, "updated_at": r.updated_at,
        })).collect::<Vec<_>>(),
        "total": rows.first().map_or(0, |r| r.total),
        "page": page,
        "per_page": per_page,
        "uses_307_308": uses_rare,
    })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RedirectInput {
    pub from_path: Option<String>,
    pub to_path: Option<String>,
    pub status_code: Option<i16>,
    pub note: Option<String>,
    pub is_active: Option<bool>,
}

fn check_code(code: i16) -> AppResult<()> {
    if [301, 302, 307, 308].contains(&code) {
        Ok(())
    } else {
        Err(field_error("status_code", "invalid"))
    }
}

pub async fn admin_create_redirect(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<RedirectInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("seo.manage")?;
    let code = b.status_code.unwrap_or(301);
    check_code(code)?;
    let ctx = ctx(&state).await?;
    let mut tx = state.db.begin().await?;
    let c = check_manual(
        &mut tx,
        &ctx,
        None,
        b.from_path.as_deref().unwrap_or(""),
        b.to_path.as_deref().unwrap_or(""),
    )
    .await?;
    let id = sqlx::query_scalar!(
        "INSERT INTO redirects (from_path, to_path, status_code, source, note, is_active, created_by)
         VALUES ($1, $2, $3, 'manual', $4, $5, $6) RETURNING id",
        c.from_path,
        c.to_path,
        code,
        b.note.as_deref().map(str::trim).filter(|n| !n.is_empty()),
        b.is_active.unwrap_or(true),
        staff.id
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(map_loop)?;
    let flattened = redirects::flatten_into(&mut tx, &c.from_path, &c.to_path)
        .await
        .map_err(map_loop)?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "redirect.created",
        "redirect",
        &id.to_string(),
        json!({ "from_path": c.from_path, "to_path": c.to_path, "status_code": code, "flattened": flattened }),
    )
    .await?;
    redirects::changed(&mut tx, true, &[]).await?;
    tx.commit().await.map_err(map_loop)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "checked": c, "flattened": flattened })),
    ))
}

pub async fn admin_update_redirect(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<RedirectInput>,
) -> AppResult<Json<Value>> {
    staff.require("seo.manage")?;
    let ctx = ctx(&state).await?;
    let mut tx = state.db.begin().await?;
    let cur = sqlx::query!(
        "SELECT from_path, to_path, status_code, note, is_active, source FROM redirects WHERE id = $1 FOR UPDATE",
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let code = b.status_code.unwrap_or(cur.status_code);
    check_code(code)?;
    let paths_changed = b
        .from_path
        .as_ref()
        .is_some_and(|f| normalize(f).as_deref() != Some(&cur.from_path))
        || b.to_path.as_ref().is_some_and(|t| t.trim() != cur.to_path);
    let (from, to) = if paths_changed {
        let c = check_manual(
            &mut tx,
            &ctx,
            Some(id),
            b.from_path.as_deref().unwrap_or(&cur.from_path),
            b.to_path.as_deref().unwrap_or(&cur.to_path),
        )
        .await?;
        (c.from_path, c.to_path)
    } else {
        (cur.from_path.clone(), cur.to_path.clone())
    };
    let note = match &b.note {
        Some(n) => Some(n.trim().to_string()).filter(|n| !n.is_empty()),
        None => cur.note.clone(),
    };
    let active = b.is_active.unwrap_or(cur.is_active);
    // Editing an automatic redirect makes it a manual one (only the toggle keeps it automatic).
    let becomes_manual = paths_changed || b.status_code.is_some() || b.note.is_some();
    sqlx::query!(
        "UPDATE redirects SET from_path = $2, to_path = $3, status_code = $4, note = $5, is_active = $6,
                source = CASE WHEN $7 THEN 'manual' ELSE source END, updated_at = now()
         WHERE id = $1",
        id,
        from,
        to,
        code,
        note,
        active,
        becomes_manual
    )
    .execute(&mut *tx)
    .await
    .map_err(map_loop)?;
    if active {
        redirects::flatten_into(&mut tx, &from, &to)
            .await
            .map_err(map_loop)?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "redirect.updated",
        "redirect",
        &id.to_string(),
        json!({ "before": { "from_path": cur.from_path, "to_path": cur.to_path, "status_code": cur.status_code, "is_active": cur.is_active, "source": cur.source },
                "after": { "from_path": from, "to_path": to, "status_code": code, "is_active": active } }),
    )
    .await?;
    redirects::changed(&mut tx, true, &[]).await?;
    tx.commit().await.map_err(map_loop)?;
    Ok(Json(json!({ "id": id })))
}

pub async fn admin_delete_redirect(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require("seo.manage")?;
    let mut tx = state.db.begin().await?;
    let r = sqlx::query!(
        "DELETE FROM redirects WHERE id = $1 RETURNING from_path, to_path, source",
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "redirect.deleted",
        "redirect",
        &id.to_string(),
        json!({ "from_path": r.from_path, "to_path": r.to_path, "source": r.source }),
    )
    .await?;
    redirects::changed(&mut tx, true, &[]).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct Bulk {
    pub ids: Vec<Uuid>,
    /// activate | deactivate | delete
    pub action: String,
}

pub async fn admin_bulk_redirects(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<Bulk>,
) -> AppResult<Json<Value>> {
    staff.require("seo.manage")?;
    let mut tx = state.db.begin().await?;
    let n = match b.action.as_str() {
        "delete" => {
            sqlx::query!("DELETE FROM redirects WHERE id = ANY($1)", &b.ids)
                .execute(&mut *tx)
                .await?
        }
        "deactivate" | "activate" => sqlx::query!(
            "UPDATE redirects SET is_active = $2, updated_at = now() WHERE id = ANY($1)",
            &b.ids,
            b.action == "activate"
        )
        .execute(&mut *tx)
        .await
        .map_err(map_loop)?,
        _ => return Err(field_error("action", "invalid")),
    }
    .rows_affected();
    audit::record(
        &mut tx,
        &staff.actor(),
        "redirect.bulk",
        "redirect",
        "bulk",
        json!({ "action": b.action, "ids": b.ids, "count": n }),
    )
    .await?;
    redirects::changed(&mut tx, true, &[]).await?;
    tx.commit().await.map_err(map_loop)?;
    Ok(Json(json!({ "count": n })))
}

// ---- Admin: live paths, 404 log --------------------------------------------------------------------

/// Every live path (served content), for autocomplete and 404 suggestions.
async fn live_paths(state: &AppState, ctx: &Ctx) -> AppResult<Vec<(String, String, String)>> {
    let mut conn = state.db.acquire().await?;
    let entries = routes::entries(&mut conn, ctx, routes::Filter::All).await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.served)
        .flat_map(|e| {
            let t = e.entity_type.to_string();
            e.versions
                .into_iter()
                .map(move |(l, v)| (v.path, l, t.clone()))
        })
        .collect())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LiveQuery {
    pub q: Option<String>,
}

pub async fn admin_live_paths(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<LiveQuery>,
) -> AppResult<Json<Value>> {
    can_read(&staff)?;
    let ctx = ctx(&state).await?;
    let needle = q.q.unwrap_or_default().to_lowercase();
    let list: Vec<Value> = live_paths(&state, &ctx)
        .await?
        .into_iter()
        .filter(|(p, _, _)| needle.is_empty() || p.to_lowercase().contains(&needle))
        .take(30)
        .map(|(path, locale, kind)| json!({ "path": path, "locale": locale, "entity_type": kind }))
        .collect();
    Ok(Json(json!({ "data": list })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct NotFoundQuery {
    pub ignored: Option<bool>,
    pub q: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

pub async fn admin_not_found(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<NotFoundQuery>,
) -> AppResult<Json<Value>> {
    can_read(&staff)?;
    let per_page = q.per_page.unwrap_or(25).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);
    let rows = sqlx::query!(
        r#"SELECT path, hits, first_seen_at, last_seen_at, last_referrer, is_ignored, count(*) OVER () AS "total!"
           FROM not_found_log
           WHERE ($1 OR NOT is_ignored) AND ($2::text IS NULL OR path ILIKE '%' || $2 || '%')
           ORDER BY hits DESC, last_seen_at DESC LIMIT $3 OFFSET $4"#,
        q.ignored.unwrap_or(false),
        q.q.as_deref().filter(|s| !s.is_empty()),
        per_page,
        (page - 1) * per_page
    )
    .fetch_all(&state.db)
    .await?;
    let ctx = ctx(&state).await?;
    let live = live_paths(&state, &ctx).await?;
    let mut data = vec![];
    for r in &rows {
        // Same language as the missing path (its prefix), compared on the last segment.
        let locale = r
            .path
            .split('/')
            .nth(1)
            .filter(|s| *s != ctx.default_locale && ctx.locales.iter().any(|l| l == s))
            .unwrap_or(&ctx.default_locale)
            .to_string();
        let candidates: Vec<String> = live
            .iter()
            .filter(|(_, l, _)| *l == locale)
            .map(|(p, _, _)| p.clone())
            .collect();
        let suggestions = sqlx::query!(
            r#"SELECT p AS "path!", similarity(regexp_replace(p, '^.*/', ''), regexp_replace($2, '^.*/', ''))::float8 AS "score!"
               FROM unnest($1::text[]) p ORDER BY 2 DESC, p LIMIT 3"#,
            &candidates,
            r.path
        )
        .fetch_all(&state.db)
        .await?;
        data.push(json!({
            "path": r.path, "hits": r.hits, "first_seen_at": r.first_seen_at, "last_seen_at": r.last_seen_at,
            "last_referrer": r.last_referrer, "is_ignored": r.is_ignored,
            "suggestions": suggestions.iter().filter(|s| s.score > 0.0).map(|s| json!({ "path": s.path, "score": s.score })).collect::<Vec<_>>(),
        }));
    }
    Ok(Json(json!({
        "data": data,
        "total": rows.first().map_or(0, |r| r.total),
        "page": page,
        "per_page": per_page,
    })))
}

#[derive(Debug, Deserialize)]
pub struct IgnoreBody {
    pub is_ignored: bool,
}

pub async fn admin_ignore_not_found(
    State(state): State<AppState>,
    staff: Staff,
    Path(path): Path<String>,
    Json(b): Json<IgnoreBody>,
) -> AppResult<StatusCode> {
    staff.require("seo.manage")?;
    let path = format!("/{}", path.trim_start_matches('/'));
    let mut tx = state.db.begin().await?;
    let n = sqlx::query!(
        "UPDATE not_found_log SET is_ignored = $2 WHERE path = $1",
        path,
        b.is_ignored
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "not_found.ignored",
        "not_found",
        &path,
        json!({ "is_ignored": b.is_ignored }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ClearBody {
    /// Rows to delete; empty: every row that is not ignored.
    pub paths: Vec<String>,
}

pub async fn admin_clear_not_found(
    State(state): State<AppState>,
    staff: Staff,
    body: axum::body::Bytes,
) -> AppResult<Json<Value>> {
    staff.require("seo.manage")?;
    let b: ClearBody = if body.is_empty() {
        ClearBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| AppError::BadRequest(e.to_string()))?
    };
    let mut tx = state.db.begin().await?;
    let n = if b.paths.is_empty() {
        sqlx::query!("DELETE FROM not_found_log WHERE NOT is_ignored")
            .execute(&mut *tx)
            .await?
    } else {
        sqlx::query!("DELETE FROM not_found_log WHERE path = ANY($1)", &b.paths)
            .execute(&mut *tx)
            .await?
    }
    .rows_affected();
    audit::record(
        &mut tx,
        &staff.actor(),
        "not_found.cleared",
        "not_found",
        "log",
        json!({ "paths": b.paths, "count": n }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "count": n })))
}

// ---- Admin: tester, sitemap ---------------------------------------------------------------------

/// What the site does with a path: normalisation, redirect, content, indexability, sitemap.
pub async fn admin_test(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<PathQuery>,
) -> AppResult<Json<Value>> {
    can_read(&staff)?;
    let raw = q.path.trim();
    let raw = if raw.starts_with('/') {
        raw.to_string()
    } else {
        format!("/{raw}")
    };
    let normalized = normalize(&raw).ok_or_else(|| field_error("path", "relative_path"))?;
    let redirect = sqlx::query!(
        "SELECT id, to_path, status_code, source, is_active FROM redirects WHERE from_path = $1",
        normalized
    )
    .fetch_optional(&state.db)
    .await?;
    let ctx = ctx(&state).await?;
    let base = sitemap::base_url(&state);
    let mut conn = state.db.acquire().await?;
    let found = routes::lookup(&mut conn, &ctx, &normalized).await?;
    let content = found.as_ref().map(|f| {
        let verdict = judge(&ctx, &f.entry, &f.locale, &base);
        let own = f.entry.versions.get(&f.locale).map(|v| v.path.clone());
        json!({
            "entity_type": f.entry.entity_type, "entity_id": f.entry.entity_id, "locale": f.locale,
            "served": f.entry.served, "indexability": verdict,
            "canonical_path": own,
            "in_sitemap": verdict.is_indexable() && own.as_deref() == Some(normalized.as_str()),
        })
    });
    Ok(Json(json!({
        "input": raw,
        "normalized": normalized,
        "normalization_redirect": normalize(&raw).as_deref() != Some(raw.split('?').next().unwrap_or("")),
        "redirect": redirect.map(|r| json!({ "id": r.id, "to_path": r.to_path, "status_code": r.status_code, "source": r.source, "is_active": r.is_active })),
        "content": content,
    })))
}

pub async fn admin_sitemap_status(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<Json<Value>> {
    can_read(&staff)?;
    let files = sitemap::files(&state).await?;
    let built: Option<Value> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'seo.sitemap_built_at'")
            .fetch_optional(&state.db)
            .await?;
    let base = sitemap::base_url(&state);
    Ok(Json(json!({
        "sitemap_url": format!("{base}/sitemap.xml"),
        "built_at": built.and_then(|v| v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok())),
        "files": files.iter().map(|(name, lastmod, count, per_locale)| json!({
            "name": name, "url": format!("{base}/sitemaps/{name}"), "lastmod": lastmod, "count": count, "per_locale": per_locale,
        })).collect::<Vec<_>>(),
    })))
}

pub async fn admin_sitemap_rebuild(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<StatusCode> {
    staff.require("seo.manage")?;
    sitemap::invalidate(&state.cache, &[])
        .await
        .map_err(AppError::from)?;
    sitemap::index(&state).await?;
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &staff.actor(),
        "sitemap.rebuilt",
        "sitemap",
        "all",
        json!({}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
