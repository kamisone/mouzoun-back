//! HTTP adapters of the page builder: back office (`/v1/admin/...`) and public pages.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    model::{BlockDraft, RefType, SaveTree, Snapshot},
    public::{self, Lang, RenderedPage},
    service::{self, AddBlock, NewPage, PatchBlock, PatchSectionType, PublishRequest},
};
use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::audit::Actor,
    state::AppState,
    web::client::{ClientInfo, RequestLocale},
};

const READ: &str = "content.read";
const WRITE: &str = "content.write";
const PUBLISH: &str = "content.publish";
const TYPES: &str = "content.html";

fn actor(staff: &Staff, client: &ClientInfo) -> Actor {
    client.actor(staff.actor())
}

/// `If-Match: 12` or `If-Match: "12"` (the draft version the client edited).
fn if_match(headers: &HeaderMap) -> AppResult<Option<i64>> {
    match headers.get(header::IF_MATCH) {
        None => Ok(None),
        Some(v) => v
            .to_str()
            .ok()
            .map(|s| s.trim().trim_start_matches("W/").trim_matches('"'))
            .and_then(|s| s.parse().ok())
            .map(Some)
            .ok_or_else(|| AppError::BadRequest("If-Match must be the draft version".into())),
    }
}

fn with_etag<T: serde::Serialize>(version: i64, body: &T) -> Response {
    let mut res = Json(body).into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("\"{version}\"")) {
        res.headers_mut().insert(header::ETAG, v);
    }
    res
}

// ---- Section types and pages --------------------------------------------------------------------

pub async fn section_types(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    let types = service::section_types(&state.db, &state.sections, |key| {
        state.media.public_url(key)
    })
    .await?;
    Ok(Json(json!({
        "types": types,
        "appearance_schema": state.sections.appearance_schema(),
        "can_use_html": staff.can(TYPES),
        "can_publish": staff.can(PUBLISH),
    })))
}

pub async fn patch_section_type(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(code): Path<String>,
    Json(patch): Json<PatchSectionType>,
) -> AppResult<StatusCode> {
    staff.require(TYPES)?;
    service::patch_section_type(&state.db, &actor(&staff, &client), &code, patch).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn pages(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    Ok(Json(json!({ "pages": service::pages(&state.db).await? })))
}

pub async fn create_page(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Json(input): Json<NewPage>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require(WRITE)?;
    let id = service::create_page(&state.db, &staff, &actor(&staff, &client), input).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

// ---- Builder ------------------------------------------------------------------------------------

pub async fn builder(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    staff.require(READ)?;
    let mut conn = state.db.acquire().await?;
    let tree = service::builder_tree(&mut conn, id).await?;
    Ok(with_etag(tree.draft_version, &tree))
}

pub async fn save_builder(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(input): Json<SaveTree>,
) -> AppResult<Response> {
    staff.require(WRITE)?;
    let version = if_match(&headers)?.ok_or_else(|| {
        AppError::domain(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
            "send If-Match: <draft_version>",
        )
    })?;
    let tree = service::save_tree(
        &state.db,
        &state.sections,
        &staff,
        &actor(&staff, &client),
        id,
        version,
        input,
    )
    .await?;
    Ok(with_etag(tree.draft_version, &tree))
}

pub async fn add_block(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(input): Json<AddBlock>,
) -> AppResult<Response> {
    staff.require(WRITE)?;
    let tree = service::add_block(
        &state.db,
        &state.sections,
        &staff,
        &actor(&staff, &client),
        id,
        if_match(&headers)?,
        input,
    )
    .await?;
    Ok(with_etag(tree.draft_version, &tree))
}

pub async fn duplicate_block(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Response> {
    staff.require(WRITE)?;
    let tree = service::duplicate_block(
        &state.db,
        &state.sections,
        &staff,
        &actor(&staff, &client),
        id,
        if_match(&headers)?,
    )
    .await?;
    Ok(with_etag(tree.draft_version, &tree))
}

pub async fn patch_block(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(patch): Json<PatchBlock>,
) -> AppResult<Response> {
    staff.require(WRITE)?;
    let tree = service::patch_block(
        &state.db,
        &state.sections,
        &staff,
        &actor(&staff, &client),
        id,
        if_match(&headers)?,
        patch,
    )
    .await?;
    Ok(with_etag(tree.draft_version, &tree))
}

pub async fn delete_block(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Response> {
    staff.require(WRITE)?;
    let tree = service::delete_block(
        &state.db,
        &state.sections,
        &staff,
        &actor(&staff, &client),
        id,
        if_match(&headers)?,
    )
    .await?;
    Ok(with_etag(tree.draft_version, &tree))
}

// ---- Shared sections --------------------------------------------------------------------------

pub async fn shared_list(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    Ok(Json(
        json!({ "sections": service::shared_list(&state.db).await? }),
    ))
}

pub async fn shared_get(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<BlockDraft>> {
    staff.require(READ)?;
    Ok(Json(service::shared_get(&state.db, id).await?))
}

pub async fn shared_create(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Json(block): Json<BlockDraft>,
) -> AppResult<(StatusCode, Json<BlockDraft>)> {
    staff.require(WRITE)?;
    let saved = service::shared_save(
        &state.db,
        &state.sections,
        &staff,
        &actor(&staff, &client),
        None,
        block,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(saved)))
}

pub async fn shared_update(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(block): Json<BlockDraft>,
) -> AppResult<Json<BlockDraft>> {
    staff.require(WRITE)?;
    Ok(Json(
        service::shared_save(
            &state.db,
            &state.sections,
            &staff,
            &actor(&staff, &client),
            Some(id),
            block,
        )
        .await?,
    ))
}

pub async fn shared_delete(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    staff.require(WRITE)?;
    service::shared_delete(&state.db, &actor(&staff, &client), id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn shared_usages(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    Ok(Json(
        json!({ "usages": service::shared_usages(&state.db, id).await? }),
    ))
}

/// Where a media asset, product, FAQ... is used by sections ("used in").
pub async fn entity_usages(
    State(state): State<AppState>,
    staff: Staff,
    Path((ref_type, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    let ref_type = RefType::parse(&ref_type).ok_or(AppError::NotFound)?;
    Ok(Json(
        json!({ "usages": service::entity_usages(&state.db, ref_type, id).await? }),
    ))
}

// ---- Publishing ------------------------------------------------------------------------------

pub async fn publish(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
    Json(req): Json<PublishRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require(PUBLISH)?;
    let rev = service::publish(
        &state.db,
        &state.sections,
        &actor(&staff, &client),
        Some(staff.id),
        id,
        state.clock.now(),
        req,
    )
    .await?;
    // The published key changes with the revision; old entries simply expire.
    Ok((StatusCode::CREATED, Json(json!({ "revision": rev }))))
}

pub async fn publish_check(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    let check = service::publish_check(&state.db, &state.sections, id).await?;
    Ok(Json(json!(check)))
}

pub async fn revisions(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    Ok(Json(
        json!({ "revisions": service::revisions(&state.db, id).await? }),
    ))
}

pub async fn revision(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    Ok(Json(json!(service::revision(&state.db, id).await?)))
}

pub async fn restore_revision(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    staff.require(PUBLISH)?;
    let restored =
        service::restore_revision(&state.db, &state.sections, &actor(&staff, &client), id).await?;
    Ok(with_etag(restored.tree.draft_version, &restored))
}

pub async fn cancel_revision(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require(PUBLISH)?;
    Ok(Json(
        json!({ "revision": service::cancel_revision(&state.db, &actor(&staff, &client), id).await? }),
    ))
}

pub async fn preview_token(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    let mut conn = state.db.acquire().await?;
    service::builder_tree(&mut conn, id).await?; // 404 for an unknown page
    let (token, ttl) = service::create_preview_token(&state.cache, id, staff.id).await?;
    Ok(Json(json!({ "token": token, "expires_in": ttl })))
}

// ---- Public ---------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub preview: Option<String>,
    /// With a preview token: show this revision of the page instead of the draft.
    pub revision: Option<Uuid>,
}

pub async fn home(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    let page_id = sqlx::query_scalar!(
        "SELECT id FROM pages WHERE page_type = 'home' AND deleted_at IS NULL
         ORDER BY (status = 'published') DESC, created_at LIMIT 1"
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    serve(
        &state,
        page_id,
        locale.as_str(),
        q.preview.as_deref(),
        q.revision,
    )
    .await
}

pub async fn by_slug(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Path(slug): Path<String>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    let slug = slug.trim_matches('/');
    // A slug belongs to one language; the request's language wins when both have it.
    let page_id = sqlx::query_scalar!(
        "SELECT t.page_id FROM page_translations t JOIN pages p ON p.id = t.page_id AND p.deleted_at IS NULL
         WHERE t.slug = $1 ORDER BY (t.locale = $2) DESC LIMIT 1",
        slug,
        locale.as_str()
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    serve(
        &state,
        page_id,
        locale.as_str(),
        q.preview.as_deref(),
        q.revision,
    )
    .await
}

async fn languages(state: &AppState) -> AppResult<(String, Vec<String>)> {
    let mut conn = state.db.acquire().await?;
    Ok(service::locales(&mut conn).await?)
}

async fn serve(
    state: &AppState,
    page_id: Uuid,
    locale: &str,
    preview: Option<&str>,
    revision: Option<Uuid>,
) -> AppResult<Response> {
    let now = state.clock.now();
    let (default, all) = languages(state).await?;
    let lang = Lang {
        locale,
        default: &default,
        all: &all,
    };
    if let Some(token) = preview {
        let grant = service::preview_grant(&state.cache, token)
            .await
            .filter(|g| g.page_id == page_id)
            .ok_or_else(|| {
                AppError::domain(
                    StatusCode::FORBIDDEN,
                    "preview_expired",
                    "this preview link expired, open the builder again",
                )
            })?;
        let (snapshot, number) = match revision {
            Some(id) => {
                let rev = sqlx::query!(
                    "SELECT revision_number, snapshot FROM page_revisions WHERE id = $1 AND page_id = $2",
                    id,
                    grant.page_id
                )
                .fetch_optional(&state.db)
                .await?
                .ok_or(AppError::NotFound)?;
                let snapshot: Snapshot =
                    serde_json::from_value(rev.snapshot).map_err(anyhow::Error::from)?;
                (snapshot, Some(rev.revision_number))
            }
            None => {
                let mut conn = state.db.acquire().await?;
                (
                    service::draft_preview_snapshot(&mut conn, &state.sections, grant.page_id)
                        .await?,
                    None,
                )
            }
        };
        let page =
            public::render(state, &state.sections, &snapshot, number, &lang, now, true).await?;
        let mut res = Json(page).into_response();
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return Ok(res);
    }
    let published = sqlx::query!(
        "SELECT revision_number, snapshot FROM page_revisions WHERE page_id = $1 AND status = 'published'",
        page_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let key = format!("pages:{page_id}:{}:{locale}", published.revision_number);
    if let Some(cached) = state.cache.get_json::<RenderedPage>(&key).await {
        return Ok(Json(cached).into_response());
    }
    let snapshot: Snapshot =
        serde_json::from_value(published.snapshot).map_err(anyhow::Error::from)?;
    let page = public::render(
        state,
        &state.sections,
        &snapshot,
        Some(published.revision_number),
        &lang,
        now,
        false,
    )
    .await?;
    let ttl = public::ttl_until_next_boundary(&snapshot.blocks, now);
    state.cache.set_json(&key, &page, ttl).await;
    Ok(Json(page).into_response())
}

#[derive(Debug, Deserialize)]
pub struct PreviewSection {
    pub token: String,
    pub section: BlockDraft,
}

/// Live preview of one section being edited (not saved yet): rendered like the public page.
pub async fn preview_section(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Path(page_id): Path<Uuid>,
    Json(input): Json<PreviewSection>,
) -> AppResult<Response> {
    service::preview_grant(&state.cache, &input.token)
        .await
        .filter(|g| g.page_id == page_id)
        .ok_or_else(|| {
            AppError::domain(
                StatusCode::FORBIDDEN,
                "preview_expired",
                "this preview link expired",
            )
        })?;
    let (default, all) = languages(&state).await?;
    let lang = Lang {
        locale: locale.as_str(),
        default: &default,
        all: &all,
    };
    let mut block = input.section;
    if let Some(shared) = block.shared.take() {
        block = service::inline_shared(vec![BlockDraft {
            shared: Some(shared),
            ..block
        }])
        .pop()
        .ok_or(AppError::NotFound)?;
    }
    let block = public::visible_blocks(
        std::slice::from_ref(&block),
        locale.as_str(),
        state.clock.now(),
    )
    .pop();
    let section = match block {
        Some(b) => Some(
            public::render_section(&state, &state.sections, &b, &lang, state.clock.now())
                .await?
                .0,
        ),
        None => None,
    };
    let mut res = Json(json!({ "section": section })).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

// ---- Pickers ----------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LookupQuery {
    pub q: Option<String>,
    /// Comma-separated ids to resolve (labels of the current value).
    pub ids: Option<String>,
    /// Products: comma-separated kinds the field allows (`x-kinds`).
    pub kinds: Option<String>,
    /// Categories: the scope the field allows (`x-scope`).
    pub scope: Option<String>,
}

/// Options of the entity pickers of the form generator: `[{id, label, hint}]` in the default
/// language, 30 at most.
pub async fn lookup(
    State(state): State<AppState>,
    staff: Staff,
    Path(kind): Path<String>,
    Query(q): Query<LookupQuery>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    let ids: Vec<Uuid> = q
        .ids
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let search =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
    let by_ids = !ids.is_empty();
    let kinds: Option<Vec<String>> = q.kinds.as_deref().map(|k| {
        k.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    // Shop products also carry their cover, lowest price and stock status (the picker shows them).
    let mut extra: std::collections::HashMap<Uuid, Value> = std::collections::HashMap::new();
    let rows: Vec<(Uuid, Option<String>, Option<String>)> = match kind.as_str() {
        "product" => {
            let found = sqlx::query!(
                r#"SELECT p.id, t.name AS "label?", p.kind || ' · ' || p.status AS hint, p.kind, m.object_key AS "image_key?",
                          (SELECT min(v.price_minor) FROM product_variants v WHERE v.product_id = p.id AND v.status = 'active' AND v.deleted_at IS NULL) AS price,
                          (SELECT bool_or(NOT v.track_inventory) OR COALESCE(sum(l.on_hand - l.reserved) FILTER (WHERE v.track_inventory), 0) > 0
                           FROM product_variants v LEFT JOIN inventory_levels l ON l.variant_id = v.id
                           WHERE v.product_id = p.id AND v.status = 'active' AND v.deleted_at IS NULL) AS in_stock
                   FROM products p
                   LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
                   LEFT JOIN media_assets m ON m.id = p.cover_media_id AND m.deleted_at IS NULL
                   WHERE p.deleted_at IS NULL AND ($4::text[] IS NULL OR p.kind = ANY($4))
                     AND (CASE WHEN $1 THEN p.id = ANY($2) ELSE ($3::text IS NULL OR t.name ILIKE $3 OR p.sku ILIKE $3) END)
                   ORDER BY p.kind, p.sort_order, t.name LIMIT 30"#,
                by_ids, &ids, search, kinds.as_deref()
            ).fetch_all(&state.db).await?;
            for r in &found {
                if r.kind == "shop_product" {
                    extra.insert(r.id, json!({
                        "image_url": r.image_key.as_deref().map(|k| state.media.public_url(k)),
                        "price_minor": r.price,
                        "availability": match r.in_stock { Some(true) => "in_stock", Some(false) => "out_of_stock", None => "out_of_stock" },
                    }));
                }
            }
            found.into_iter().map(|r| (r.id, r.label, r.hint)).collect()
        }
        "category" => sqlx::query!(
            r#"SELECT c.id, t.name AS "label?", c.scope AS hint FROM categories c
               LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = (SELECT code FROM locales WHERE is_default)
               WHERE ($4::text IS NULL OR c.scope = $4)
                 AND (CASE WHEN $1 THEN c.id = ANY($2) ELSE ($3::text IS NULL OR t.name ILIKE $3) END)
               ORDER BY c.scope, c.sort_order, t.name LIMIT 30"#,
            by_ids, &ids, search, q.scope.as_deref().filter(|s| !s.is_empty())
        ).fetch_all(&state.db).await?.into_iter().map(|r| (r.id, r.label, Some(r.hint))).collect(),
        "place" => sqlx::query!(
            r#"SELECT p.id, t.name AS "label?", p.kind AS hint FROM places p
               LEFT JOIN place_translations t ON t.place_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
               WHERE p.deleted_at IS NULL AND (CASE WHEN $1 THEN p.id = ANY($2) ELSE ($3::text IS NULL OR t.name ILIKE $3) END)
               ORDER BY p.sort_order, t.name LIMIT 30"#,
            by_ids, &ids, search
        ).fetch_all(&state.db).await?.into_iter().map(|r| (r.id, r.label, Some(r.hint))).collect(),
        "faq" => sqlx::query!(
            r#"SELECT f.id, t.question AS "label?", CASE WHEN f.is_published THEN 'published' ELSE 'draft' END AS hint FROM faqs f
               LEFT JOIN faq_translations t ON t.faq_id = f.id AND t.locale = (SELECT code FROM locales WHERE is_default)
               WHERE (CASE WHEN $1 THEN f.id = ANY($2) ELSE ($3::text IS NULL OR t.question ILIKE $3) END)
               ORDER BY f.sort_order LIMIT 30"#,
            by_ids, &ids, search
        ).fetch_all(&state.db).await?.into_iter().map(|r| (r.id, r.label, r.hint)).collect(),
        "promotion" => sqlx::query!(
            r#"SELECT p.id, t.name AS "label?", COALESCE(p.code::text, 'automatic') AS hint FROM promotions p
               LEFT JOIN promotion_translations t ON t.promotion_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
               WHERE (CASE WHEN $1 THEN p.id = ANY($2) ELSE ($3::text IS NULL OR t.name ILIKE $3) END)
               ORDER BY p.created_at DESC LIMIT 30"#,
            by_ids, &ids, search
        ).fetch_all(&state.db).await?.into_iter().map(|r| (r.id, r.label, r.hint)).collect(),
        "gallery" => sqlx::query!(
            r#"SELECT g.id, g.name AS "label?", (SELECT count(*) FROM gallery_items i WHERE i.gallery_id = g.id)::text AS hint
               FROM galleries g WHERE (CASE WHEN $1 THEN g.id = ANY($2) ELSE ($3::text IS NULL OR g.name ILIKE $3) END)
               ORDER BY g.name LIMIT 30"#,
            by_ids, &ids, search
        ).fetch_all(&state.db).await?.into_iter().map(|r| (r.id, r.label, r.hint)).collect(),
        "page" => sqlx::query!(
            r#"SELECT p.id, t.title AS "label?", p.page_type AS hint FROM pages p
               LEFT JOIN page_translations t ON t.page_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
               WHERE p.deleted_at IS NULL AND (CASE WHEN $1 THEN p.id = ANY($2) ELSE ($3::text IS NULL OR t.title ILIKE $3) END)
               ORDER BY p.page_type = 'home' DESC, t.title LIMIT 30"#,
            by_ids, &ids, search
        ).fetch_all(&state.db).await?.into_iter().map(|r| (r.id, r.label, Some(r.hint))).collect(),
        _ => return Err(AppError::NotFound),
    };
    Ok(Json(
        json!({ "options": rows.into_iter().map(|(id, label, hint)| {
            let mut o = json!({ "id": id, "label": label.unwrap_or_else(|| id.to_string()), "hint": hint });
            if let (Some(extra), Some(map)) = (extra.get(&id).and_then(Value::as_object), o.as_object_mut()) {
                map.extend(extra.clone());
            }
            o
        }).collect::<Vec<_>>() }),
    ))
}
