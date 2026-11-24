//! Shop categories admin (`/v1/admin/shop/categories`): the tree (one call saves a whole move),
//! the full form (translations, description, cover, icon, SEO, visibility, default sort), safe
//! delete with an impact preview and reassignment, manual product order and pins. Every write is
//! audited and clears the shop and menu caches. Rules: `services::shop::categories`.
//!
//! Also: a customer's orders (customer page, orders tab).

use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    modules::pages::validate::clean_richtext,
    modules::seo::{redirects as seo, routes::SHOP_CATEGORY},
    services::{
        audit,
        cache_invalidation::{MENUS_PREFIX, SHOP_CATALOG_PREFIX},
        outbox::{self, Event},
        shop::categories::{
            self as cat, Node, SORTS, check_tree, link_products, map_tree_error, shop_path, slugify,
        },
    },
    state::AppState,
};

fn field_errors(errors: &[(String, &str)]) -> AppError {
    let map: serde_json::Map<String, Value> = errors
        .iter()
        .map(|(f, c)| (f.clone(), Value::String((*c).to_string())))
        .collect();
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_fields",
        "some fields are invalid",
    )
    .with_details(json!({ "fields": map }))
}

/// `null` → `Some(None)`, absent → `None` (with `#[serde(default)]`).
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

async fn changed(tx: &mut PgConnection, id: Uuid) -> AppResult<()> {
    outbox::enqueue(
        tx,
        &Event::ContentChanged {
            ref_type: "category".into(),
            ref_id: id,
        },
    )
    .await?;
    Ok(())
}

/// After commit: category trees, category pages, product lists and menus read categories.
async fn invalidate(state: &AppState) {
    state.cache.delete_prefix(SHOP_CATALOG_PREFIX).await.ok();
    state.cache.delete_prefix(MENUS_PREFIX).await.ok();
}

/// Cover images as the site shows them (site photos and bucket media have different URLs).
async fn covers(
    state: &AppState,
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, crate::modules::pages::public::MediaView>> {
    let default_locale = cat::default_locale(conn).await?;
    let all = vec![default_locale.clone()];
    crate::modules::pages::public::media_views(
        state,
        ids,
        &crate::modules::pages::public::Lang {
            locale: &default_locale,
            default: &default_locale,
            all: &all,
        },
    )
    .await
}

// ---- List (tree) ---------------------------------------------------------------------------------

/// All shop categories, flat and in order (the client builds the tree), with product counts
/// (own and including subcategories, all and published), the tree version and the depth limit.
pub async fn list(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    let rows = sqlx::query!(
        r#"WITH RECURSIVE sub AS (
               SELECT id AS root, id FROM categories WHERE scope = 'shop'
               UNION ALL
               SELECT sub.root, c.id FROM categories c JOIN sub ON c.parent_id = sub.id),
           counts AS (
               SELECT sub.root,
                      count(DISTINCT pc.product_id) FILTER (WHERE pc.category_id = sub.root) AS own,
                      count(DISTINCT pc.product_id) FILTER (WHERE pc.category_id = sub.root AND p.status = 'published') AS own_published,
                      count(DISTINCT pc.product_id) AS total,
                      count(DISTINCT pc.product_id) FILTER (WHERE p.status = 'published') AS published
               FROM sub JOIN product_categories pc ON pc.category_id = sub.id
               JOIN products p ON p.id = pc.product_id AND p.deleted_at IS NULL
               GROUP BY sub.root)
           SELECT c.id, c.parent_id, c.icon, c.cover_media_id, c.sort_order, c.is_active, c.show_in_menu, c.default_sort,
                  c.first_activated_at, c.updated_at,
                  COALESCE((SELECT jsonb_object_agg(locale, jsonb_build_object('name', name, 'slug', slug, 'description', description,
                            'seo_title', seo_title, 'seo_description', seo_description)) FROM category_translations WHERE category_id = c.id), '{}') AS "translations!",
                  COALESCE(n.own, 0) AS "own!", COALESCE(n.own_published, 0) AS "own_published!",
                  COALESCE(n.total, 0) AS "total!", COALESCE(n.published, 0) AS "published!"
           FROM categories c
           LEFT JOIN counts n ON n.root = c.id
           WHERE c.scope = 'shop' ORDER BY c.sort_order, c.created_at"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let version = cat::tree_version(&mut conn).await?;
    let max_depth = cat::max_depth(&mut conn).await?;
    let covers = covers(
        &state,
        &mut conn,
        &rows
            .iter()
            .filter_map(|c| c.cover_media_id)
            .collect::<Vec<_>>(),
    )
    .await?;
    Ok(Json(json!({
        "data": rows.iter().map(|c| json!({
            "id": c.id, "parent_id": c.parent_id, "icon": c.icon, "cover_media_id": c.cover_media_id,
            "cover_url": c.cover_media_id.and_then(|m| covers.get(&m)).map(|v| v.url.clone()),
            "sort_order": c.sort_order, "is_active": c.is_active, "show_in_menu": c.show_in_menu, "default_sort": c.default_sort,
            "first_activated_at": c.first_activated_at, "updated_at": c.updated_at, "translations": c.translations,
            "product_count": c.own, "published_count": c.own_published,
            "product_count_total": c.total, "published_count_total": c.published,
        })).collect::<Vec<_>>(),
        "version": version,
        "max_depth": max_depth,
    })))
}

// ---- Detail ----------------------------------------------------------------------------------------

pub async fn detail(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    Ok(Json(detail_json(&state, &mut conn, id).await?))
}

async fn detail_json(state: &AppState, conn: &mut PgConnection, id: Uuid) -> AppResult<Value> {
    let c = sqlx::query!(
        r#"SELECT c.id, c.parent_id, c.icon, c.cover_media_id, c.sort_order, c.is_active, c.show_in_menu, c.default_sort,
                  c.first_activated_at, c.created_at, c.updated_at,
                  COALESCE((SELECT jsonb_object_agg(locale, alt_text) FROM media_asset_translations
                            WHERE media_asset_id = c.cover_media_id AND alt_text IS NOT NULL), '{}') AS "cover_alt!",
                  COALESCE((SELECT jsonb_object_agg(locale, jsonb_build_object('name', name, 'slug', slug, 'description', description,
                            'seo_title', seo_title, 'seo_description', seo_description)) FROM category_translations WHERE category_id = c.id), '{}') AS "translations!"
           FROM categories c WHERE c.id = $1 AND c.scope = 'shop'"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let last = sqlx::query!(
        r#"SELECT a.created_at, u.full_name AS "name?" FROM audit_logs a LEFT JOIN users u ON u.id = a.actor_user_id
           WHERE a.entity_type = 'category' AND a.entity_id = $1 ORDER BY a.created_at DESC, a.id DESC LIMIT 1"#,
        id.to_string()
    )
    .fetch_optional(&mut *conn)
    .await?;
    let cover = match c.cover_media_id {
        Some(m) => covers(state, conn, &[m]).await?.remove(&m),
        None => None,
    };
    let default_locale = cat::default_locale(conn).await?;
    let urls: HashMap<String, String> = c
        .translations
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(l, t)| {
                    t["slug"]
                        .as_str()
                        .map(|s| (l.clone(), shop_path(l, &default_locale, Some(s))))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "id": c.id, "parent_id": c.parent_id, "icon": c.icon, "sort_order": c.sort_order, "is_active": c.is_active,
        "show_in_menu": c.show_in_menu, "default_sort": c.default_sort, "first_activated_at": c.first_activated_at,
        "created_at": c.created_at, "updated_at": c.updated_at,
        "updated_by": last.as_ref().and_then(|l| l.name.clone()),
        "cover_media_id": c.cover_media_id,
        "cover": cover.map(|v| json!({ "url": v.url, "width": v.width, "height": v.height, "alt": c.cover_alt })),
        "translations": c.translations,
        "paths": urls,
    }))
}

// ---- Create / update -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TranslationInput {
    #[serde(default)]
    pub name: String,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
}

/// Body of `POST` and `PATCH`: absent fields keep their value (a translation is replaced as a
/// whole; an empty name removes that language, except French).
#[derive(Debug, Default, Deserialize)]
pub struct CategoryInput {
    #[serde(default, deserialize_with = "nullable")]
    pub parent_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "nullable")]
    pub icon: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub cover_media_id: Option<Option<Uuid>>,
    pub sort_order: Option<i32>,
    pub is_active: Option<bool>,
    pub show_in_menu: Option<bool>,
    pub default_sort: Option<String>,
    #[serde(default)]
    pub translations: HashMap<String, TranslationInput>,
}

struct Existing {
    parent_id: Option<Uuid>,
    icon: Option<String>,
    cover_media_id: Option<Uuid>,
    sort_order: i32,
    is_active: bool,
    show_in_menu: bool,
    default_sort: String,
}

fn clean(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

#[allow(clippy::too_many_lines)]
async fn save(
    state: &AppState,
    staff: &Staff,
    id: Option<Uuid>,
    c: CategoryInput,
) -> AppResult<Uuid> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let creating = id.is_none();
    let id = id.unwrap_or_else(Uuid::now_v7);
    // Tree changes are serialised (the tree endpoint takes the same lock).
    sqlx::query!("SELECT id FROM categories WHERE scope = 'shop' FOR UPDATE")
        .fetch_all(&mut *tx)
        .await?;
    let existing = if creating {
        None
    } else {
        Some(
            sqlx::query_as!(
                Existing,
                "SELECT parent_id, icon, cover_media_id, sort_order, is_active, show_in_menu, default_sort
                 FROM categories WHERE id = $1 AND scope = 'shop'",
                id
            )
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?,
        )
    };
    let old_slugs: HashMap<String, String> = sqlx::query!(
        "SELECT locale, slug FROM category_translations WHERE category_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| (r.locale, r.slug))
    .collect();
    let locales: Vec<String> = sqlx::query_scalar!("SELECT code FROM locales ORDER BY sort_order")
        .fetch_all(&mut *tx)
        .await?;

    let parent_id = c
        .parent_id
        .unwrap_or_else(|| existing.as_ref().and_then(|e| e.parent_id));
    let icon = c
        .icon
        .map(|i| clean(i.as_deref()))
        .unwrap_or_else(|| existing.as_ref().and_then(|e| e.icon.clone()));
    let cover = c
        .cover_media_id
        .unwrap_or_else(|| existing.as_ref().and_then(|e| e.cover_media_id));
    let is_active = c
        .is_active
        .or(existing.as_ref().map(|e| e.is_active))
        .unwrap_or(false);
    let show_in_menu = c
        .show_in_menu
        .or(existing.as_ref().map(|e| e.show_in_menu))
        .unwrap_or(true);
    let default_sort = c
        .default_sort
        .clone()
        .or(existing.as_ref().map(|e| e.default_sort.clone()))
        .unwrap_or_else(|| "manual".into());

    let mut errors: Vec<(String, &str)> = vec![];
    if !SORTS.contains(&default_sort.as_str()) {
        errors.push(("default_sort".into(), "invalid"));
    }
    if icon
        .as_deref()
        .is_some_and(|i| i.len() > 80 || !i.starts_with("i-"))
    {
        errors.push(("icon".into(), "invalid"));
    }

    // Tree: parent exists, no cycle, depth limit.
    if parent_id == Some(id) {
        return Err(cat::TreeError::Cycle(id).into());
    }
    let mut nodes = cat::nodes(&mut tx).await?;
    match nodes.iter_mut().find(|n| n.id == id) {
        Some(n) => n.parent_id = parent_id,
        None => nodes.push(Node { id, parent_id }),
    }
    check_tree(&nodes, cat::max_depth(&mut tx).await?)?;
    let sort_order = match c.sort_order.or(existing.as_ref().map(|e| e.sort_order)) {
        Some(s) => s,
        None => {
            sqlx::query_scalar!(
                r#"SELECT COALESCE(max(sort_order) + 1, 0) AS "n!" FROM categories
               WHERE scope = 'shop' AND parent_id IS NOT DISTINCT FROM $1"#,
                parent_id
            )
            .fetch_one(&mut *tx)
            .await?
        }
    };

    // Cover: an image of the library with an alt text (French at least).
    if let Some(m) = cover
        && c.cover_media_id.is_some()
    {
        let alt = sqlx::query_scalar!(
            r#"SELECT (SELECT alt_text FROM media_asset_translations WHERE media_asset_id = m.id AND locale = 'fr') AS alt
               FROM media_assets m WHERE m.id = $1 AND m.deleted_at IS NULL AND m.kind = 'image'"#,
            m
        )
        .fetch_optional(&mut *tx)
        .await?;
        match alt {
            None => errors.push(("cover_media_id".into(), "invalid")),
            Some(a) if clean(a.as_deref()).is_none() => {
                errors.push(("cover.alt".into(), "required"));
            }
            Some(_) => {}
        }
    }

    // Translations: French is required; slugs are derived from the name when not given.
    struct Tr {
        name: String,
        slug: String,
        explicit: bool,
        description: Option<String>,
        seo_title: Option<String>,
        seo_description: Option<String>,
    }
    let mut writes: Vec<(String, Option<Tr>)> = vec![];
    for (locale, t) in &c.translations {
        if !locales.contains(locale) {
            errors.push((format!("translations.{locale}"), "unknown_locale"));
            continue;
        }
        let name = t.name.trim().to_string();
        if name.is_empty() {
            if locale == "fr" {
                errors.push(("translations.fr.name".into(), "required"));
            }
            writes.push((locale.clone(), None));
            continue;
        }
        if name.chars().count() > 120 {
            errors.push((format!("translations.{locale}.name"), "too_long"));
        }
        let given = t.slug.as_deref().map(slugify).filter(|s| !s.is_empty());
        let explicit = given.is_some();
        let slug = given
            .or_else(|| old_slugs.get(locale).cloned())
            .unwrap_or_else(|| slugify(&name));
        if slug.is_empty() {
            errors.push((format!("translations.{locale}.slug"), "required"));
        } else if cat::is_reserved(&slug) {
            errors.push((format!("translations.{locale}.slug"), "reserved"));
        }
        let description = clean(t.description.as_deref())
            .map(|d| clean_richtext(&d))
            .filter(|d| !d.trim().is_empty());
        let seo_title = clean(t.seo_title.as_deref());
        let seo_description = clean(t.seo_description.as_deref());
        if seo_title.as_ref().is_some_and(|s| s.chars().count() > 120) {
            errors.push((format!("translations.{locale}.seo_title"), "too_long"));
        }
        if seo_description
            .as_ref()
            .is_some_and(|s| s.chars().count() > 320)
        {
            errors.push((format!("translations.{locale}.seo_description"), "too_long"));
        }
        writes.push((
            locale.clone(),
            Some(Tr {
                name,
                slug,
                explicit,
                description,
                seo_title,
                seo_description,
            }),
        ));
    }
    if creating && !c.translations.contains_key("fr") {
        errors.push(("translations.fr.name".into(), "required"));
    }
    // Slugs: taken by another category of the same language. Generated ones get -2, -3...
    for (locale, tr) in writes.iter_mut() {
        let Some(tr) = tr else { continue };
        if tr.slug.is_empty() {
            continue;
        }
        let base = tr.slug.clone();
        let mut n = 1;
        loop {
            let taken = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM category_translations WHERE locale = $1 AND slug = $2 AND category_id <> $3) AS "x!""#,
                locale.as_str(),
                tr.slug,
                id
            )
            .fetch_one(&mut *tx)
            .await?;
            if !taken {
                break;
            }
            if tr.explicit || n > 50 {
                errors.push((format!("translations.{locale}.slug"), "taken"));
                break;
            }
            n += 1;
            tr.slug = format!("{base}-{n}");
        }
    }
    // Activation needs the French name and address.
    if is_active {
        let fr_after = match writes.iter().find(|(l, _)| l == "fr") {
            Some((_, Some(_))) => true,
            Some((_, None)) => false,
            None => old_slugs.contains_key("fr"),
        };
        if !fr_after && !errors.iter().any(|(f, _)| f == "translations.fr.name") {
            errors.push(("translations.fr.name".into(), "required_to_activate"));
        }
    }
    if !errors.is_empty() {
        errors.sort();
        errors.dedup();
        return Err(field_errors(&errors));
    }

    let urls = if creating {
        None
    } else {
        seo::snapshot(&mut tx, SHOP_CATEGORY, id).await?
    };
    sqlx::query!(
        "INSERT INTO categories (id, scope, parent_id, icon, cover_media_id, sort_order, is_active, show_in_menu, default_sort)
         VALUES ($1, 'shop', $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (id) DO UPDATE SET parent_id = $2, icon = $3, cover_media_id = $4, sort_order = $5, is_active = $6,
                                        show_in_menu = $7, default_sort = $8
         WHERE categories.scope = 'shop'",
        id,
        parent_id,
        icon.as_deref(),
        cover,
        sort_order,
        is_active,
        show_in_menu,
        default_sort
    )
    .execute(&mut *tx)
    .await?;
    for (locale, tr) in &writes {
        match tr {
            None => {
                sqlx::query!(
                    "DELETE FROM category_translations WHERE category_id = $1 AND locale = $2",
                    id,
                    locale
                )
                .execute(&mut *tx)
                .await?;
            }
            Some(tr) => {
                sqlx::query!(
                    "INSERT INTO category_translations (category_id, locale, name, slug, description, seo_title, seo_description)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (category_id, locale) DO UPDATE SET name = $3, slug = $4, description = $5, seo_title = $6, seo_description = $7",
                    id,
                    locale,
                    tr.name,
                    tr.slug,
                    tr.description,
                    tr.seo_title,
                    tr.seo_description
                )
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        if creating { "category.created" } else { "category.updated" },
        "category",
        &id.to_string(),
        json!({
            "before": existing.as_ref().map(|e| json!({ "parent_id": e.parent_id, "is_active": e.is_active, "show_in_menu": e.show_in_menu,
                "default_sort": e.default_sort, "sort_order": e.sort_order, "cover_media_id": e.cover_media_id, "slugs": old_slugs })),
            "after": { "parent_id": parent_id, "is_active": is_active, "show_in_menu": show_in_menu, "default_sort": default_sort,
                "sort_order": sort_order, "cover_media_id": cover,
                "translations": writes.iter().map(|(l, t)| (l.clone(), t.as_ref().map(|t| json!({ "name": t.name, "slug": t.slug })))).collect::<HashMap<_, _>>() },
        }),
    )
    .await?;
    // Old addresses redirect (site-wide layer); a deactivated category keeps an inactive one.
    seo::after_change(&mut tx, &staff.actor(), SHOP_CATEGORY, id, urls).await?;
    changed(&mut tx, id).await?;
    tx.commit().await.map_err(map_tree_error)?;
    invalidate(state).await;
    Ok(id)
}

pub async fn create(
    State(state): State<AppState>,
    staff: Staff,
    Json(c): Json<CategoryInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = save(&state, &staff, None, c).await?;
    let mut conn = state.db.acquire().await?;
    Ok((
        StatusCode::CREATED,
        Json(detail_json(&state, &mut conn, id).await?),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(c): Json<CategoryInput>,
) -> AppResult<Json<Value>> {
    save(&state, &staff, Some(id), c).await?;
    let mut conn = state.db.acquire().await?;
    Ok(Json(detail_json(&state, &mut conn, id).await?))
}

// ---- Tree move -------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TreeItem {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub sort_order: i32,
}

#[derive(Debug, Deserialize)]
pub struct TreeBody {
    pub version: String,
    pub items: Vec<TreeItem>,
}

/// One drag-and-drop (or keyboard) move: new parents and orders of the rows it changed, applied
/// together. Refused with 409 `tree_changed` when the tree changed since `version` was read.
pub async fn save_tree(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<TreeBody>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    sqlx::query!("SELECT id FROM categories WHERE scope = 'shop' FOR UPDATE")
        .fetch_all(&mut *tx)
        .await?;
    if cat::tree_version(&mut tx).await? != body.version {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "tree_changed",
            "the categories changed meanwhile: reload",
        ));
    }
    let mut nodes = cat::nodes(&mut tx).await?;
    let mut seen = HashSet::new();
    for item in &body.items {
        if !seen.insert(item.id) {
            return Err(AppError::BadRequest("duplicate id".into()));
        }
        let Some(n) = nodes.iter_mut().find(|n| n.id == item.id) else {
            return Err(AppError::NotFound);
        };
        n.parent_id = item.parent_id;
    }
    check_tree(&nodes, cat::max_depth(&mut tx).await?)?;
    for item in &body.items {
        sqlx::query!(
            "UPDATE categories SET parent_id = $2, sort_order = $3 WHERE id = $1 AND scope = 'shop'",
            item.id,
            item.parent_id,
            item.sort_order
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "category.tree_saved",
        "category_tree",
        "shop",
        json!({ "items": body.items.iter().map(|i| json!({ "id": i.id, "parent_id": i.parent_id, "sort_order": i.sort_order })).collect::<Vec<_>>() }),
    )
    .await?;
    for item in &body.items {
        changed(&mut tx, item.id).await?;
    }
    let version = cat::tree_version(&mut tx).await?;
    tx.commit().await.map_err(map_tree_error)?;
    invalidate(&state).await;
    Ok(Json(json!({ "version": version })))
}

// ---- Delete ----------------------------------------------------------------------------------------

pub async fn impact(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    Ok(Json(json!(cat::impact(&mut conn, id).await?)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeleteOptions {
    /// Products move to this category; `None` just removes the link.
    pub products_to: Option<Uuid>,
    /// Subcategories move under this category; `None` moves them to the deleted one's parent.
    pub subcategories_to: Option<Uuid>,
    /// Menu links to the category: `replace` (by the replacement category, or the shop home) or
    /// `remove`.
    pub menu_links: Option<String>,
    /// The French name, typed to confirm, when products or subcategories are affected.
    pub confirm_name: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn delete(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> AppResult<StatusCode> {
    staff.require("shop.catalog")?;
    let o: DeleteOptions = if body.is_empty() {
        DeleteOptions::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| AppError::BadRequest(e.to_string()))?
    };
    let mut tx = state.db.begin().await?;
    sqlx::query!("SELECT id FROM categories WHERE scope = 'shop' FOR UPDATE")
        .fetch_all(&mut *tx)
        .await?;
    let impact = cat::impact(&mut tx, id).await?;
    if impact.blocked {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "category_in_use",
            "active promotions target this category: change them first",
        )
        .with_details(json!({ "promotions": impact.promotions.iter().filter(|p| p.active == Some(true)).collect::<Vec<_>>() })));
    }
    if impact.requires_confirmation
        && o.confirm_name.as_deref().map(str::trim) != Some(impact.name.trim())
    {
        return Err(field_errors(&[("confirm_name".into(), "mismatch")]));
    }
    let nodes = cat::nodes(&mut tx).await?;
    let doomed = cat::subtree(&nodes, id);
    let exists = |t: Uuid| nodes.iter().any(|n| n.id == t);
    if let Some(t) = o.products_to
        && (t == id || !exists(t))
    {
        return Err(field_errors(&[("products_to".into(), "invalid")]));
    }
    if let Some(t) = o.subcategories_to
        && (doomed.contains(&t) || !exists(t))
    {
        return Err(field_errors(&[("subcategories_to".into(), "invalid")]));
    }
    let menu_mode = o.menu_links.as_deref().unwrap_or("replace");
    if !["replace", "remove"].contains(&menu_mode) {
        return Err(field_errors(&[("menu_links".into(), "invalid")]));
    }

    // Subcategories: under the parent (default) or the chosen category, after its children.
    let new_parent = o.subcategories_to.or(impact.parent_id);
    let base: i32 = sqlx::query_scalar!(
        r#"SELECT COALESCE(max(sort_order) + 1, 0) AS "n!" FROM categories
           WHERE scope = 'shop' AND parent_id IS NOT DISTINCT FROM $1 AND id <> $2"#,
        new_parent,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE categories SET parent_id = $2, sort_order = $3 + sort_order WHERE parent_id = $1",
        id,
        new_parent,
        base
    )
    .execute(&mut *tx)
    .await?;

    // Products: moved (keeping "primary" on the new category) or unlinked.
    let links = sqlx::query!(
        "SELECT product_id, is_primary FROM product_categories WHERE category_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    let products: Vec<Uuid> = links.iter().map(|l| l.product_id).collect();
    if let Some(t) = o.products_to {
        link_products(&mut tx, t, &products).await?;
    }
    sqlx::query!("DELETE FROM product_categories WHERE category_id = $1", id)
        .execute(&mut *tx)
        .await?;
    if let Some(t) = o.products_to {
        for l in links.iter().filter(|l| l.is_primary) {
            cat::set_primary(&mut tx, l.product_id, t).await?;
        }
    }
    cat::ensure_primary(&mut tx, &products).await?;

    // What replaces it: the products' new category, else the parent, else the shop home.
    let replacement = o.products_to.or(impact.parent_id);
    sqlx::query!(
        "UPDATE variation_attributes SET category_id = $2 WHERE category_id = $1",
        id,
        replacement
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM promotion_categories WHERE category_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;
    let menus: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT DISTINCT menu_id FROM nav_menu_items WHERE category_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    match (menu_mode, replacement) {
        ("replace", Some(r)) => {
            sqlx::query!(
                "UPDATE nav_menu_items SET category_id = $2 WHERE category_id = $1",
                id,
                r
            )
            .execute(&mut *tx)
            .await?;
        }
        ("replace", None) => {
            // A "shop" link without a category is the shop home; other kinds need a target.
            sqlx::query!(
                "UPDATE nav_menu_items SET category_id = NULL WHERE category_id = $1 AND link_type = 'shop'",
                id
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!("DELETE FROM nav_menu_items WHERE category_id = $1", id)
                .execute(&mut *tx)
                .await?;
        }
        _ => {
            sqlx::query!("DELETE FROM nav_menu_items WHERE category_id = $1", id)
                .execute(&mut *tx)
                .await?;
        }
    }

    // Old addresses → the replacement in the same language (or that language's shop home).
    let default_locale = cat::default_locale(&mut tx).await?;
    let slugs = sqlx::query!(
        "SELECT locale, slug FROM category_translations WHERE category_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut redirects = vec![];
    let (mut paths, mut targets) = (
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    );
    for s in &slugs {
        let target =
            match replacement {
                Some(r) => sqlx::query_scalar!(
                    "SELECT slug FROM category_translations WHERE category_id = $1 AND locale = $2",
                    r,
                    s.locale
                )
                .fetch_optional(&mut *tx)
                .await?,
                None => None,
            };
        let from = shop_path(&s.locale, &default_locale, Some(&s.slug));
        let to = shop_path(&s.locale, &default_locale, target.as_deref());
        redirects.push(json!({ "from": from, "to": to }));
        paths.insert(s.locale.clone(), from);
        targets.insert(s.locale.clone(), to);
    }
    seo::record_removal(&mut tx, SHOP_CATEGORY, id, &paths, &targets, true).await?;

    sqlx::query!("DELETE FROM categories WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "category.deleted",
        "category",
        &id.to_string(),
        json!({
            "impact": impact,
            "options": { "products_to": o.products_to, "subcategories_to": new_parent, "menu_links": menu_mode },
            "redirects": redirects,
        }),
    )
    .await?;
    changed(&mut tx, id).await?;
    for m in menus {
        outbox::enqueue(&mut tx, &Event::MenuUpdated { menu_id: m }).await?;
    }
    tx.commit().await.map_err(map_tree_error)?;
    invalidate(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Products of a category ------------------------------------------------------------------------

pub async fn products(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    let c = sqlx::query_scalar!(
        "SELECT default_sort FROM categories WHERE id = $1 AND scope = 'shop'",
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let rows = sqlx::query!(
        r#"SELECT p.id, p.status, pc.sort_order, pc.is_pinned, pc.is_primary,
                  COALESCE(t.name, '') AS "name!", m.object_key AS "image_key?",
                  (SELECT COALESCE(sum(l.on_hand - l.reserved), 0) FROM product_variants v JOIN inventory_levels l ON l.variant_id = v.id
                   WHERE v.product_id = p.id AND v.deleted_at IS NULL)::bigint AS "stock!"
           FROM product_categories pc JOIN products p ON p.id = pc.product_id AND p.deleted_at IS NULL
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = 'fr'
           LEFT JOIN media_assets m ON m.id = p.cover_media_id AND m.deleted_at IS NULL
           WHERE pc.category_id = $1
           ORDER BY pc.is_pinned DESC, pc.sort_order, t.name"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(json!({
        "default_sort": c,
        "data": rows.iter().map(|r| json!({
            "product_id": r.id, "name": r.name, "status": r.status, "sort_order": r.sort_order, "is_pinned": r.is_pinned,
            "is_primary": r.is_primary, "stock": r.stock, "image_url": r.image_key.as_deref().map(|k| state.media.public_url(k)),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct ProductOrder {
    pub product_id: Uuid,
    pub sort_order: i32,
    #[serde(default)]
    pub is_pinned: bool,
}

/// Manual order and pins of the products already in the category.
pub async fn order_products(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(items): Json<Vec<ProductOrder>>,
) -> AppResult<StatusCode> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    for i in &items {
        let n = sqlx::query!(
            "UPDATE product_categories SET sort_order = $3, is_pinned = $4 WHERE category_id = $1 AND product_id = $2",
            id,
            i.product_id,
            i.sort_order,
            i.is_pinned
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(field_errors(&[(
                format!("products.{}", i.product_id),
                "not_in_category",
            )]));
        }
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "category.products_ordered",
        "category",
        &id.to_string(),
        json!({ "items": items.iter().map(|i| json!({ "product_id": i.product_id, "sort_order": i.sort_order, "is_pinned": i.is_pinned })).collect::<Vec<_>>() }),
    )
    .await?;
    changed(&mut tx, id).await?;
    tx.commit().await?;
    invalidate(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct AddProducts {
    pub product_ids: Vec<Uuid>,
}

pub async fn add_products(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<AddProducts>,
) -> AppResult<Json<Value>> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let known = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM products WHERE id = ANY($1) AND kind = 'shop_product' AND deleted_at IS NULL"#,
        &body.product_ids
    )
    .fetch_one(&mut *tx)
    .await?;
    let unique: HashSet<_> = body.product_ids.iter().collect();
    if known != i64::try_from(unique.len()).unwrap_or(i64::MAX) {
        return Err(field_errors(&[("product_ids".into(), "invalid")]));
    }
    sqlx::query_scalar!(
        "SELECT id FROM categories WHERE id = $1 AND scope = 'shop'",
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let added = link_products(&mut tx, id, &body.product_ids).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "category.products_added",
        "category",
        &id.to_string(),
        json!({ "product_ids": body.product_ids }),
    )
    .await?;
    changed(&mut tx, id).await?;
    tx.commit().await?;
    invalidate(&state).await;
    Ok(Json(json!({ "added": added })))
}

pub async fn remove_product(
    State(state): State<AppState>,
    staff: Staff,
    Path((id, product)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    staff.require("shop.catalog")?;
    let mut tx = state.db.begin().await?;
    let n = sqlx::query!(
        "DELETE FROM product_categories WHERE category_id = $1 AND product_id = $2",
        id,
        product
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    cat::ensure_primary(&mut tx, &[product]).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "category.product_removed",
        "category",
        &id.to_string(),
        json!({ "product_id": product }),
    )
    .await?;
    changed(&mut tx, id).await?;
    tx.commit().await?;
    invalidate(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Suggested categories ----------------------------------------------------------------------

/// Empty state: the usual farm shop categories, inactive (to review), skipping taken addresses.
pub async fn create_suggested(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<Json<Value>> {
    const SUGGESTED: [[&str; 3]; 5] = [
        ["Dattes", "Dates", "تمور"],
        [
            "Huile d'olive & olives",
            "Olive oil & olives",
            "زيت الزيتون والزيتون",
        ],
        ["Épices & épicerie", "Spices & pantry", "توابل وبقالة"],
        ["Artisanat", "Crafts", "صناعة تقليدية"],
        ["Coffrets cadeaux", "Gift boxes", "علب هدايا"],
    ];
    let mut created = 0;
    for names in SUGGESTED {
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM category_translations WHERE locale = 'fr' AND slug = $1) AS "x!""#,
            slugify(names[0])
        )
        .fetch_one(&state.db)
        .await?;
        if taken {
            continue;
        }
        let translations = ["fr", "en", "ar"]
            .iter()
            .zip(names)
            .map(|(l, n)| {
                (
                    (*l).to_string(),
                    TranslationInput {
                        name: n.into(),
                        slug: None,
                        description: None,
                        seo_title: None,
                        seo_description: None,
                    },
                )
            })
            .collect();
        save(
            &state,
            &staff,
            None,
            CategoryInput {
                is_active: Some(false),
                translations,
                ..CategoryInput::default()
            },
        )
        .await?;
        created += 1;
    }
    Ok(Json(json!({ "created": created })))
}

/// Orders of a customer (the customer page shows them next to the bookings).
pub async fn customer_orders(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    if !staff.can("shop.orders") && !staff.can("shop.refunds") {
        return Err(AppError::Forbidden);
    }
    let rows = sqlx::query!(
        r#"SELECT id, number, status, payment_status, fulfillment_status, total_minor, currency::text AS "currency!", placed_at
           FROM orders WHERE customer_id = $1 AND status <> 'draft' ORDER BY COALESCE(placed_at, created_at) DESC LIMIT 100"#,
        id
    )
    .fetch_all(&state.db)
    .await?;
    let addresses = sqlx::query!(
        "SELECT id, label, first_name, last_name, line1, line2, city, region, postal_code, country_code, phone
         FROM customer_addresses WHERE customer_id = $1 AND deleted_at IS NULL ORDER BY created_at",
        id
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "orders": rows.iter().map(|o| json!({ "id": o.id, "number": o.number, "status": o.status, "payment_status": o.payment_status,
            "fulfillment_status": o.fulfillment_status, "total_minor": o.total_minor, "currency": o.currency, "placed_at": o.placed_at })).collect::<Vec<_>>(),
        "addresses": addresses.iter().map(|a| json!({ "id": a.id, "label": a.label, "first_name": a.first_name, "last_name": a.last_name,
            "line1": a.line1, "line2": a.line2, "city": a.city, "region": a.region, "postal_code": a.postal_code,
            "country_code": a.country_code, "phone": a.phone })).collect::<Vec<_>>(),
    })))
}
