//! Public rendering of a page: published snapshot (or the draft behind a preview token) →
//! sections for one locale, with live data (products, reviews, FAQs...), media and links
//! resolved. Responses are cached in Redis under `pages:{page_id}:{revision}:{locale}` until the
//! next schedule boundary of the page (at most one hour); previews are never cached.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    model::{BlockDraft, ItemDraft, RefType, Snapshot},
    registry::SectionRegistry,
    validate,
};
use crate::{
    db::{catalog::ProductKind, types::TsRange},
    error::AppResult,
    modules::booking::catalog::{ProductCard, cards},
    state::AppState,
};

pub const MAX_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedPage {
    pub page: PageMeta,
    pub sections: Vec<RenderedSection>,
    /// JSON-LD documents (FAQPage, Campground with AggregateRating), to inject as-is.
    pub json_ld: Vec<Value>,
    /// Values that texts may reference as `{name}` (`{opening_date}`: ISO date, formatted by
    /// the front in the page language).
    pub variables: Value,
    pub preview: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMeta {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub page_type: String,
    pub locale: String,
    pub revision: Option<i32>,
    pub title: String,
    pub slug: String,
    pub path: String,
    pub seo: Seo,
    pub hreflang: Vec<Hreflang>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seo {
    pub title: String,
    pub description: Option<String>,
    pub image: Option<MediaView>,
    pub canonical: Option<String>,
    pub noindex: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hreflang {
    pub locale: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedSection {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub block_type: String,
    pub anchor_id: Option<String>,
    pub visible_on: String,
    pub analytics_label: Option<String>,
    pub appearance: Value,
    pub settings: Value,
    pub content: Value,
    pub items: Vec<RenderedItem>,
    /// Live data of the section (`products`, `reviews`...) and `media`: id → resolved image.
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedItem {
    pub id: Uuid,
    pub settings: Value,
    pub content: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaView {
    pub id: Uuid,
    pub kind: String,
    pub mime_type: String,
    pub url: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// `[{url, width}]`, smallest first (pre-generated variants).
    pub srcset: Vec<Variant>,
    pub blurhash: Option<String>,
    /// Tiny data-URI preview, when the asset has one instead of a blurhash.
    pub placeholder: Option<String>,
    /// Focal point as CSS `object-position` ("40% 60%").
    pub position: Option<String>,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub url: String,
    pub width: i64,
}

/// Locale context of one render.
pub struct Lang<'a> {
    pub locale: &'a str,
    pub default: &'a str,
    pub all: &'a [String],
}

impl Lang<'_> {
    /// URL prefix of a locale (`""` for the default one: `prefix_except_default`).
    pub fn prefix(&self, locale: &str) -> String {
        if locale == self.default {
            String::new()
        } else {
            format!("/{locale}")
        }
    }
}

// ---- Visibility -----------------------------------------------------------------------------

fn period_contains(p: Option<TsRange>, now: DateTime<Utc>) -> bool {
    p.is_none_or(|p| p.contains(now))
}

/// Sections and items shown at `now` in `locale`.
pub fn visible_blocks(blocks: &[BlockDraft], locale: &str, now: DateTime<Utc>) -> Vec<BlockDraft> {
    blocks
        .iter()
        .filter(|b| {
            b.is_visible
                && period_contains(b.active_period, now)
                && !b.hidden_locales.iter().any(|l| l == locale)
        })
        .map(|b| BlockDraft {
            items: b
                .items
                .iter()
                .filter(|i| i.is_visible && period_contains(i.active_period, now))
                .cloned()
                .collect(),
            ..b.clone()
        })
        .collect()
}

/// Time until the next start or end of a schedule of the page (capped at `MAX_TTL`).
pub fn ttl_until_next_boundary(blocks: &[BlockDraft], now: DateTime<Utc>) -> Duration {
    let periods = blocks
        .iter()
        .flat_map(|b| {
            std::iter::once(b.active_period).chain(b.items.iter().map(|i| i.active_period))
        })
        .flatten();
    let next = periods
        .flat_map(|p| [p.start, p.end])
        .flatten()
        .filter(|t| *t > now)
        .min();
    match next {
        Some(t) => (t - now)
            .to_std()
            .unwrap_or(MAX_TTL)
            .min(MAX_TTL)
            .max(Duration::from_secs(1)),
        None => MAX_TTL,
    }
}

// ---- Locale fallback -------------------------------------------------------------------------

/// Field by field: the locale's value when present and not empty, else the default locale's.
pub fn localize(content: &BTreeMap<String, Value>, lang: &Lang<'_>) -> Value {
    let empty = Map::new();
    let base = content
        .get(lang.default)
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let own = content
        .get(lang.locale)
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut out = base.clone();
    for (k, v) in own {
        if !is_blank(v) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

// ---- Rendering --------------------------------------------------------------------------------

/// Renders a snapshot (published, or built from the draft for a preview).
pub async fn render(
    state: &AppState,
    reg: &SectionRegistry,
    snapshot: &Snapshot,
    revision: Option<i32>,
    lang: &Lang<'_>,
    now: DateTime<Utc>,
    preview: bool,
) -> AppResult<RenderedPage> {
    let blocks = visible_blocks(&snapshot.blocks, lang.locale, now);
    let mut sections = Vec::with_capacity(blocks.len());
    let mut json_ld = Vec::new();
    for b in &blocks {
        let (section, ld) = render_section(state, reg, b, lang, now).await?;
        if renders_nothing(&section) {
            continue;
        }
        sections.push(section);
        json_ld.extend(ld);
    }
    let page = page_meta(state, snapshot, revision, lang).await?;
    let site = public_settings(state, &["site.opening_date", "site.status"]).await?;
    let variables = json!({ "opening_date": site.get("site.opening_date"), "site_status": site.get("site.status") });
    Ok(RenderedPage {
        page,
        sections,
        json_ld,
        variables,
        preview,
    })
}

/// Live sections with nothing to show are left out ("offers" before a promotion is active,
/// "testimonials" before the first review...), so the page never shows an empty band.
pub fn renders_nothing(s: &RenderedSection) -> bool {
    let empty = |key: &str| {
        s.data
            .get(key)
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    };
    match s.block_type.as_str() {
        "product_carousel" => empty("products"),
        "offers" => empty("promotions") && empty("products"),
        "testimonials" => empty("reviews"),
        "faq" => empty("faqs"),
        "posts_feed" => empty("posts"),
        "category_tiles" => empty("categories"),
        "seasonal_calendar" => empty("crops"),
        "gallery" => empty("photos"),
        _ => false,
    }
}

async fn page_meta(
    state: &AppState,
    snapshot: &Snapshot,
    revision: Option<i32>,
    lang: &Lang<'_>,
) -> AppResult<PageMeta> {
    let t = &snapshot.page.translations;
    let own = t.get(lang.locale);
    let base = t.get(lang.default).or(own);
    let pick = |f: fn(&super::model::PageText) -> Option<String>| {
        own.and_then(f)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| base.and_then(f))
    };
    let title = pick(|p| Some(p.title.clone())).unwrap_or_default();
    let slug = pick(|p| Some(p.slug.clone())).unwrap_or_default();
    let home = snapshot.page.page_type == "home";
    let path_of = |locale: &str, slug: &str| {
        let prefix = lang.prefix(locale);
        if home {
            if prefix.is_empty() {
                "/".to_string()
            } else {
                prefix
            }
        } else {
            format!("{prefix}/{slug}")
        }
    };
    let hreflang = lang
        .all
        .iter()
        .filter_map(|l| {
            let s = t
                .get(l)
                .map(|p| p.slug.clone())
                .or_else(|| home.then(String::new))?;
            Some(Hreflang {
                locale: l.clone(),
                path: path_of(l, &s),
            })
        })
        .collect();
    let og = own
        .and_then(|p| p.og_media_id)
        .or(base.and_then(|p| p.og_media_id))
        .or(snapshot.page.cover_media_id);
    let image = match og {
        Some(id) => media_views(state, &[id], lang).await?.remove(&id),
        None => None,
    };
    Ok(PageMeta {
        id: snapshot.page.id,
        page_type: snapshot.page.page_type.clone(),
        locale: lang.locale.to_string(),
        revision,
        path: path_of(lang.locale, &slug),
        seo: Seo {
            title: pick(|p| p.seo_title.clone()).unwrap_or_else(|| title.clone()),
            description: pick(|p| p.seo_description.clone())
                .or_else(|| pick(|p| p.excerpt.clone())),
            image,
            canonical: None,
            noindex: own.or(base).is_some_and(|p| p.noindex),
        },
        title,
        slug,
        hreflang,
    })
}

/// One section for one locale, with its live data. Also used by the live preview of a
/// section being edited (not saved yet).
pub async fn render_section(
    state: &AppState,
    reg: &SectionRegistry,
    b: &BlockDraft,
    lang: &Lang<'_>,
    now: DateTime<Utc>,
) -> AppResult<(RenderedSection, Vec<Value>)> {
    let mut settings = b.settings.clone();
    let mut items: Vec<RenderedItem> = b
        .items
        .iter()
        .map(|i: &ItemDraft| RenderedItem {
            id: i.id,
            settings: i.settings.clone(),
            content: localize(&i.content, lang),
        })
        .collect();
    let content = localize(&b.content, lang);
    let t = reg.get(&b.block_type);

    // Links → href (unpublished targets → null, the front hides the button).
    if let Some(t) = t {
        let mut targets = BTreeSet::new();
        collect_links(
            &t.settings_schema,
            &t.settings_schema,
            &settings,
            &mut targets,
        );
        if let Some(s) = &t.item_settings_schema {
            for i in &items {
                collect_links(s, s, &i.settings, &mut targets);
            }
        }
        let hrefs = link_targets(state, &targets, lang).await?;
        resolve_links(
            &t.settings_schema,
            &t.settings_schema,
            &mut settings,
            &hrefs,
            lang,
        );
        if let Some(s) = &t.item_settings_schema {
            for i in &mut items {
                resolve_links(s, s, &mut i.settings, &hrefs, lang);
            }
        }
    }

    let (mut data, ld) = section_data(state, b, &settings, lang, now).await?;
    // Media of the section, its items and its data.
    let mut media: BTreeSet<Uuid> = BTreeSet::new();
    if let Some(t) = t {
        media.extend(
            validate::block_refs(t, b)
                .into_iter()
                .filter(|(_, r)| r.ref_type == RefType::Media)
                .map(|(_, r)| r.ref_id),
        );
    }
    if let Some(bg) = b
        .appearance
        .get("background_media_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
    {
        media.insert(bg);
    }
    collect_media_ids(&data, &mut media);
    let ids: Vec<Uuid> = media.into_iter().collect();
    let views = media_views(state, &ids, lang).await?;
    data["media"] = serde_json::to_value(views).unwrap_or_else(|_| json!({}));

    Ok((
        RenderedSection {
            id: b.id,
            block_type: b.block_type.clone(),
            anchor_id: b.anchor_id.clone(),
            visible_on: b.visible_on.as_str().to_string(),
            analytics_label: b.analytics_label.clone(),
            appearance: b.appearance.clone(),
            settings,
            content,
            items,
            data,
        },
        ld,
    ))
}

/// `*_media_id` / `cover_media_id` keys inside resolved data.
fn collect_media_ids(v: &Value, out: &mut BTreeSet<Uuid>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                if (k == "cover_media_id" || k == "media_id")
                    && let Some(id) = v.as_str().and_then(|s| s.parse().ok())
                {
                    out.insert(id);
                } else if k == "photos"
                    && let Some(list) = v.as_array()
                {
                    out.extend(list.iter().filter_map(|x| x.as_str()?.parse::<Uuid>().ok()));
                } else {
                    collect_media_ids(v, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_media_ids(x, out)),
        _ => {}
    }
}

// ---- Links ------------------------------------------------------------------------------------

fn target<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(r) => r
            .strip_prefix("#/$defs/")
            .and_then(|n| root.get("$defs")?.get(n))
            .unwrap_or(node),
        None => node,
    }
}

fn is_link(root: &Value, node: &Value) -> bool {
    node.get("x-widget")
        .or_else(|| target(root, node).get("x-widget"))
        .and_then(Value::as_str)
        == Some("link")
}

fn collect_links(root: &Value, node: &Value, value: &Value, out: &mut BTreeSet<(String, Uuid)>) {
    if is_link(root, node) {
        if let (Some(kind), Some(id)) = (
            value.get("type").and_then(Value::as_str),
            value
                .get("id")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok()),
        ) {
            out.insert((kind.to_string(), id));
        }
        return;
    }
    let t = target(root, node);
    if let (Value::Object(map), Some(props)) =
        (value, t.get("properties").and_then(Value::as_object))
    {
        for (k, v) in map {
            if let Some(child) = props.get(k) {
                collect_links(root, child, v, out);
            }
        }
    }
}

fn resolve_links(
    root: &Value,
    node: &Value,
    value: &mut Value,
    hrefs: &HashMap<(String, Uuid), String>,
    lang: &Lang<'_>,
) {
    if is_link(root, node) {
        let href = link_href(value, hrefs, lang);
        if let Value::Object(m) = value {
            let external = m.get("type").and_then(Value::as_str) == Some("url");
            m.insert("href".into(), href.map_or(Value::Null, Value::String));
            m.insert("external".into(), Value::Bool(external));
        }
        return;
    }
    let t = target(root, node);
    let Some(props) = t.get("properties").and_then(Value::as_object) else {
        return;
    };
    if let Value::Object(map) = value {
        for (k, v) in map.iter_mut() {
            if let Some(child) = props.get(k) {
                resolve_links(root, child, v, hrefs, lang);
            }
        }
    }
}

fn link_href(
    link: &Value,
    hrefs: &HashMap<(String, Uuid), String>,
    lang: &Lang<'_>,
) -> Option<String> {
    let kind = link.get("type")?.as_str()?;
    match kind {
        "url" => link.get("url")?.as_str().map(str::to_string),
        "anchor" => link.get("anchor")?.as_str().map(|a| format!("#{a}")),
        "route" => {
            let route = link.get("route")?.as_str()?;
            let prefix = lang.prefix(lang.locale);
            Some(if route == "/" && !prefix.is_empty() {
                prefix
            } else {
                format!("{prefix}{route}")
            })
        }
        _ => {
            let id: Uuid = link.get("id")?.as_str()?.parse().ok()?;
            hrefs.get(&(kind.to_string(), id)).cloned()
        }
    }
}

/// Localised paths of the published targets of links.
async fn link_targets(
    state: &AppState,
    targets: &BTreeSet<(String, Uuid)>,
    lang: &Lang<'_>,
) -> AppResult<HashMap<(String, Uuid), String>> {
    let mut out = HashMap::new();
    if targets.is_empty() {
        return Ok(out);
    }
    let of = |kind: &str| -> Vec<Uuid> {
        targets
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, id)| *id)
            .collect()
    };
    let prefix = lang.prefix(lang.locale);
    let pages = of("page");
    if !pages.is_empty() {
        for r in sqlx::query!(
            r#"SELECT p.id, p.page_type, COALESCE(t.slug, d.slug) AS "slug?"
               FROM pages p
               LEFT JOIN page_translations t ON t.page_id = p.id AND t.locale = $2
               LEFT JOIN page_translations d ON d.page_id = p.id AND d.locale = $3
               WHERE p.id = ANY($1) AND p.deleted_at IS NULL
                 AND EXISTS (SELECT 1 FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'published')"#,
            &pages,
            lang.locale,
            lang.default
        )
        .fetch_all(&state.db)
        .await?
        {
            let path = if r.page_type == "home" {
                if prefix.is_empty() { "/".to_string() } else { prefix.clone() }
            } else {
                match r.slug {
                    Some(s) => format!("{prefix}/{s}"),
                    None => continue,
                }
            };
            out.insert(("page".to_string(), r.id), path);
        }
    }
    let products = of("product");
    // Shop products live under the shop prefix of each language (`shop.route_prefix`).
    let shop_prefix = if products.is_empty() {
        String::new()
    } else {
        let prefixes =
            sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'shop.route_prefix'")
                .fetch_optional(&state.db)
                .await?
                .unwrap_or_default();
        let p = crate::services::shop::settings::ShopSettings::localized(&prefixes, lang.locale);
        if p.is_empty() { "shop".to_string() } else { p }
    };
    if !products.is_empty() {
        for r in sqlx::query!(
            r#"SELECT p.id, p.kind, COALESCE(t.slug, d.slug) AS "slug?"
               FROM products p
               LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2
               LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = $3
               WHERE p.id = ANY($1) AND p.status = 'published' AND p.deleted_at IS NULL"#,
            &products,
            lang.locale,
            lang.default
        )
        .fetch_all(&state.db)
        .await?
        {
            let shop_base = format!("{shop_prefix}/p");
            let base = match r.kind.as_str() {
                "accommodation" => "stays",
                "activity" => "activities",
                "package" => "packages",
                "shop_product" => shop_base.as_str(),
                _ => continue,
            };
            if let Some(slug) = r.slug {
                out.insert(
                    ("product".to_string(), r.id),
                    format!("{prefix}/{base}/{slug}"),
                );
            }
        }
    }
    let categories = of("category");
    if !categories.is_empty() {
        for r in sqlx::query!(
            r#"SELECT c.id, COALESCE(t.slug, d.slug) AS "slug?"
               FROM categories c
               LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $2
               LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = $3
               WHERE c.id = ANY($1) AND c.is_active"#,
            &categories,
            lang.locale,
            lang.default
        )
        .fetch_all(&state.db)
        .await?
        {
            if let Some(slug) = r.slug {
                out.insert(
                    ("category".to_string(), r.id),
                    format!("{prefix}/activities?category={slug}"),
                );
            }
        }
    }
    // Places have no page of their own on the public site: such links are not rendered.
    Ok(out)
}

// ---- Media ------------------------------------------------------------------------------------

/// Resolves media ids (deleted or private ones are skipped). Storage provider `site` is used by
/// development placeholders: the key is a path served by the front itself.
pub async fn media_views(
    state: &AppState,
    ids: &[Uuid],
    lang: &Lang<'_>,
) -> AppResult<HashMap<Uuid, MediaView>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"SELECT m.id, m.kind, m.mime_type, m.storage_provider, m.object_key, m.width, m.height, m.blurhash,
                  m.focal_x::float8 AS "focal_x?", m.focal_y::float8 AS "focal_y?", m.variants,
                  COALESCE(NULLIF(t.alt_text, ''), d.alt_text) AS "alt?"
           FROM media_assets m
           LEFT JOIN media_asset_translations t ON t.media_asset_id = m.id AND t.locale = $2
           LEFT JOIN media_asset_translations d ON d.media_asset_id = m.id AND d.locale = $3
           WHERE m.id = ANY($1) AND m.deleted_at IS NULL AND m.visibility = 'public'"#,
        ids,
        lang.locale,
        lang.default
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = HashMap::new();
    for r in rows {
        let site = r.storage_provider == "site";
        let url_of = |key: &str| {
            if site {
                key.to_string()
            } else {
                state.media.public_url(key)
            }
        };
        let mut srcset: Vec<Variant> = r
            .variants
            .as_object()
            .map(|m| {
                m.values()
                    .filter_map(|v| {
                        Some(Variant {
                            url: url_of(v.get("key")?.as_str()?),
                            width: v.get("width")?.as_i64()?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        srcset.sort_by_key(|v| v.width);
        let (blurhash, placeholder) = match r.blurhash {
            Some(b) if b.starts_with("data:image/") => (None, Some(b)),
            other => (other, None),
        };
        out.insert(
            r.id,
            MediaView {
                id: r.id,
                kind: r.kind,
                mime_type: r.mime_type,
                url: url_of(&r.object_key),
                width: r.width,
                height: r.height,
                srcset,
                blurhash,
                placeholder,
                position: match (r.focal_x, r.focal_y) {
                    (Some(x), Some(y)) => {
                        Some(format!("{}% {}%", (x * 100.0).round(), (y * 100.0).round()))
                    }
                    _ => None,
                },
                alt: r.alt,
            },
        );
    }
    Ok(out)
}

// ---- Live data --------------------------------------------------------------------------------

fn ids_of(v: Option<&Value>) -> Vec<Uuid> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

fn str_of<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

fn int_of(v: &Value, pointer: &str, default: i64) -> i64 {
    v.pointer(pointer)
        .and_then(Value::as_i64)
        .unwrap_or(default)
}

/// Keeps the order of `ids` (hand-picked lists).
fn in_order<T>(ids: &[Uuid], items: Vec<T>, id: impl Fn(&T) -> Uuid) -> Vec<T> {
    let mut by_id: HashMap<Uuid, T> = items.into_iter().map(|x| (id(&x), x)).collect();
    ids.iter().filter_map(|i| by_id.remove(i)).collect()
}

async fn section_data(
    state: &AppState,
    b: &BlockDraft,
    s: &Value,
    lang: &Lang<'_>,
    now: DateTime<Utc>,
) -> AppResult<(Value, Vec<Value>)> {
    let db = &state.db;
    let locale = lang.locale;
    let mut ld = Vec::new();
    let data = match b.block_type.as_str() {
        "product_carousel" => json!({ "products": products(state, s, lang).await? }),
        "offers" => {
            if str_of(s, "/source") == Some("products") {
                let ids = ids_of(s.get("product_ids"));
                let found = cards(state, locale, None, false, Some(&ids)).await?;
                json!({ "products": in_order(&ids, found, |c| c.id), "promotions": [] })
            } else {
                json!({ "promotions": promotions(state, &ids_of(s.get("promotion_ids")), lang, now).await? })
            }
        }
        "category_tiles" => {
            let manual = str_of(s, "/source/mode") == Some("manual");
            let ids = ids_of(s.pointer("/source/ids"));
            let limit = int_of(s, "/source/rule/limit", 6);
            let rows = sqlx::query!(
                r#"SELECT c.id, c.icon, c.cover_media_id,
                          COALESCE(NULLIF(t.name, ''), d.name) AS "name?", COALESCE(t.slug, d.slug) AS "slug?",
                          COALESCE(t.description, d.description) AS "description?"
                   FROM categories c
                   LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = $1
                   LEFT JOIN category_translations d ON d.category_id = c.id AND d.locale = $2
                   WHERE c.scope = 'product' AND c.is_active AND (NOT $3 OR c.id = ANY($4))
                   ORDER BY c.sort_order, c.id LIMIT $5"#,
                locale,
                lang.default,
                manual,
                &ids,
                if manual { 24 } else { limit.clamp(1, 24) }
            )
            .fetch_all(db)
            .await?;
            let prefix = lang.prefix(locale);
            let list: Vec<Value> = rows
                .into_iter()
                .filter(|r| r.name.is_some() && r.slug.is_some())
                .map(|r| json!({"id": r.id, "name": r.name, "slug": r.slug, "description": r.description, "icon": r.icon,
                                "cover_media_id": r.cover_media_id,
                                "href": format!("{prefix}/activities?category={}", r.slug.unwrap_or_default())}))
                .collect();
            let list = if manual {
                in_order(&ids, list, |v| {
                    v["id"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_default()
                })
            } else {
                list
            };
            json!({ "categories": list })
        }
        "features" if str_of(s, "/source") == Some("amenities") => {
            let rows = sqlx::query!(
                r#"SELECT a.code::text AS "code!", a.icon, COALESCE(NULLIF(t.name, ''), d.name) AS "name?",
                          COALESCE(t.description, d.description) AS "description?"
                   FROM amenities a
                   LEFT JOIN amenity_translations t ON t.amenity_id = a.id AND t.locale = $1
                   LEFT JOIN amenity_translations d ON d.amenity_id = a.id AND d.locale = $2
                   WHERE a.is_site_wide ORDER BY a.sort_order, a.code"#,
                locale,
                lang.default
            )
            .fetch_all(db)
            .await?;
            json!({ "amenities": rows.into_iter().filter(|r| r.name.is_some())
                .map(|r| json!({"code": r.code, "icon": r.icon, "name": r.name, "description": r.description})).collect::<Vec<_>>() })
        }
        "seasonal_calendar" => {
            let rows = sqlx::query!(
                r#"SELECT c.code::text AS "code!", c.icon, c.cover_media_id, c.harvest_months,
                          COALESCE(NULLIF(t.name, ''), d.name) AS "name?", COALESCE(t.description, d.description) AS "description?"
                   FROM farm_crops c
                   LEFT JOIN farm_crop_translations t ON t.farm_crop_id = c.id AND t.locale = $1
                   LEFT JOIN farm_crop_translations d ON d.farm_crop_id = c.id AND d.locale = $2
                   WHERE c.is_published ORDER BY c.sort_order, c.code"#,
                locale,
                lang.default
            )
            .fetch_all(db)
            .await?;
            json!({
                "current_month": crate::camp_time::today(now).month(),
                "crops": rows.into_iter().filter(|r| r.name.is_some()).map(|r| json!({
                    "code": r.code, "name": r.name, "description": r.description, "icon": r.icon,
                    "cover_media_id": r.cover_media_id, "harvest_months": r.harvest_months })).collect::<Vec<_>>()
            })
        }
        "gallery" => {
            let max = int_of(s, "/max_photos", 12).clamp(1, 60) as usize;
            let mut photos = if str_of(s, "/source") == Some("gallery") {
                match str_of(s, "/gallery_id").and_then(|g| g.parse::<Uuid>().ok()) {
                    Some(g) => sqlx::query_scalar!(
                        "SELECT gi.media_id FROM gallery_items gi JOIN media_assets m ON m.id = gi.media_id AND m.deleted_at IS NULL AND m.visibility = 'public'
                         WHERE gi.gallery_id = $1 ORDER BY gi.sort_order, gi.media_id",
                        g
                    )
                    .fetch_all(db)
                    .await?,
                    None => Vec::new(),
                }
            } else {
                ids_of(s.get("media_ids"))
            };
            photos.truncate(max);
            json!({ "photos": photos })
        }
        "testimonials" => {
            let product = str_of(s, "/product_id").and_then(|p| p.parse::<Uuid>().ok());
            let featured = s
                .get("featured_only")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let min = int_of(s, "/min_rating", 4).clamp(1, 5) as i16;
            let limit = int_of(s, "/limit", 6).clamp(1, 24);
            let reviews = sqlx::query!(
                "SELECT id, author_name, author_country, rating, title, body, experience_date, locale
                 FROM reviews
                 WHERE status = 'approved' AND rating >= $1 AND (NOT $2 OR is_featured)
                   AND ($3::uuid IS NULL OR product_id = $3)
                 ORDER BY created_at DESC LIMIT $4",
                min,
                featured,
                product,
                limit
            )
            .fetch_all(db)
            .await?;
            let summary = sqlx::query!(
                r#"SELECT avg(rating)::float8 AS "average?", count(*) AS "count!" FROM reviews
                   WHERE status = 'approved' AND ($1::uuid IS NULL OR product_id = $1)"#,
                product
            )
            .fetch_one(db)
            .await?;
            if summary.count > 0 && product.is_none() {
                let site = public_settings(state, &["site.name"]).await?;
                ld.push(json!({
                    "@context": "https://schema.org",
                    "@type": "Campground",
                    "name": site.get("site.name").and_then(Value::as_str).unwrap_or("Mouzoun"),
                    "aggregateRating": {
                        "@type": "AggregateRating",
                        "ratingValue": (summary.average.unwrap_or(0.0) * 10.0).round() / 10.0,
                        "reviewCount": summary.count,
                        "bestRating": 5, "worstRating": 1
                    }
                }));
            }
            json!({
                "summary": { "average": summary.average.map(|a| (a * 10.0).round() / 10.0), "count": summary.count },
                "reviews": reviews.into_iter().map(|r| json!({
                    "id": r.id, "author_name": r.author_name, "author_country": r.author_country, "rating": r.rating,
                    "title": r.title, "body": r.body, "experience_date": r.experience_date, "locale": r.locale })).collect::<Vec<_>>()
            })
        }
        "faq" => {
            let manual = str_of(s, "/source/mode") == Some("manual");
            let ids = ids_of(s.pointer("/source/ids"));
            let category =
                str_of(s, "/source/rule/category_id").and_then(|c| c.parse::<Uuid>().ok());
            let limit = int_of(s, "/source/rule/limit", 10).clamp(1, 24);
            let rows = sqlx::query!(
                r#"SELECT f.id, COALESCE(NULLIF(t.question, ''), d.question) AS "question?",
                          COALESCE(NULLIF(t.answer, ''), d.answer) AS "answer?"
                   FROM faqs f
                   LEFT JOIN faq_translations t ON t.faq_id = f.id AND t.locale = $1
                   LEFT JOIN faq_translations d ON d.faq_id = f.id AND d.locale = $2
                   WHERE f.is_published AND (CASE WHEN $3 THEN f.id = ANY($4) ELSE ($5::uuid IS NULL OR f.category_id = $5) END)
                   ORDER BY f.sort_order, f.id LIMIT $6"#,
                locale,
                lang.default,
                manual,
                &ids,
                category,
                if manual { 24 } else { limit }
            )
            .fetch_all(db)
            .await?;
            let faqs: Vec<Value> = rows
                .into_iter()
                .filter(|r| r.question.is_some() && r.answer.is_some())
                .map(|r| json!({"id": r.id, "question": r.question, "answer": r.answer}))
                .collect();
            let faqs = if manual {
                in_order(&ids, faqs, |v| {
                    v["id"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_default()
                })
            } else {
                faqs
            };
            if s.get("emit_schema")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                && !faqs.is_empty()
            {
                ld.push(json!({
                    "@context": "https://schema.org",
                    "@type": "FAQPage",
                    "mainEntity": faqs.iter().map(|f| json!({
                        "@type": "Question", "name": f["question"],
                        "acceptedAnswer": { "@type": "Answer", "text": f["answer"] } })).collect::<Vec<_>>()
                }));
            }
            json!({ "faqs": faqs })
        }
        "map" => {
            let ids = ids_of(s.get("place_ids"));
            let rows = sqlx::query!(
                r#"SELECT p.id, p.kind, p.latitude::float8 AS "lat?", p.longitude::float8 AS "lng?",
                          p.distance_km::float8 AS "distance_km?", p.cover_media_id,
                          COALESCE(NULLIF(t.name, ''), d.name) AS "name?",
                          COALESCE(t.short_description, d.short_description) AS "description?"
                   FROM places p
                   LEFT JOIN place_translations t ON t.place_id = p.id AND t.locale = $2
                   LEFT JOIN place_translations d ON d.place_id = p.id AND d.locale = $3
                   WHERE p.id = ANY($1) AND p.is_published AND p.deleted_at IS NULL"#,
                &ids,
                locale,
                lang.default
            )
            .fetch_all(db)
            .await?;
            let places: Vec<Value> = rows
                .into_iter()
                .filter(|r| r.name.is_some())
                .map(|r| json!({"id": r.id, "kind": r.kind, "name": r.name, "description": r.description, "lat": r.lat,
                                "lng": r.lng, "distance_km": r.distance_km, "cover_media_id": r.cover_media_id}))
                .collect();
            let camp = public_settings(state, &["location.latitude", "location.longitude"]).await?;
            json!({
                "places": in_order(&ids, places, |v| v["id"].as_str().and_then(|s| s.parse().ok()).unwrap_or_default()),
                "camp": { "lat": camp.get("location.latitude"), "lng": camp.get("location.longitude") }
            })
        }
        "contact" => {
            let c = public_settings(
                state,
                &[
                    "contact.email",
                    "contact.phone",
                    "contact.whatsapp",
                    "contact.address",
                ],
            )
            .await?;
            let address = c
                .get("contact.address")
                .and_then(|a| a.get(locale).or_else(|| a.get(lang.default)))
                .cloned();
            json!({ "email": c.get("contact.email"), "phone": c.get("contact.phone"), "whatsapp": c.get("contact.whatsapp"), "address": address })
        }
        "posts_feed" => {
            let category = (str_of(s, "/source") == Some("category"))
                .then(|| str_of(s, "/category_id").and_then(|c| c.parse::<Uuid>().ok()))
                .flatten();
            let count = int_of(s, "/count", 3).clamp(3, 9);
            let prefix = lang.prefix(locale);
            let rows = sqlx::query!(
                r#"SELECT p.id, p.cover_media_id, r.published_at,
                          COALESCE(NULLIF(t.title, ''), d.title) AS "title?", COALESCE(t.slug, d.slug) AS "slug?",
                          COALESCE(t.excerpt, d.excerpt) AS "excerpt?"
                   FROM pages p
                   JOIN page_revisions r ON r.page_id = p.id AND r.status = 'published'
                   LEFT JOIN page_translations t ON t.page_id = p.id AND t.locale = $1
                   LEFT JOIN page_translations d ON d.page_id = p.id AND d.locale = $2
                   WHERE p.page_type = 'post' AND p.deleted_at IS NULL AND ($3::uuid IS NULL OR p.category_id = $3)
                   ORDER BY COALESCE(p.publish_at, r.published_at) DESC LIMIT $4"#,
                locale,
                lang.default,
                category,
                count
            )
            .fetch_all(db)
            .await?;
            json!({ "posts": rows.into_iter().filter(|r| r.slug.is_some()).map(|r| json!({
                "id": r.id, "title": r.title, "excerpt": r.excerpt, "cover_media_id": r.cover_media_id,
                "published_at": r.published_at, "href": format!("{prefix}/{}", r.slug.unwrap_or_default()) })).collect::<Vec<_>>() })
        }
        "hero" | "booking_search" => json!({}),
        _ => json!({}),
    };
    Ok((data, ld))
}

async fn products(state: &AppState, s: &Value, lang: &Lang<'_>) -> AppResult<Vec<ProductCard>> {
    let locale = lang.locale;
    if str_of(s, "/source/mode") == Some("manual") {
        let ids = ids_of(s.pointer("/source/ids"));
        let found = cards(state, locale, None, false, Some(&ids)).await?;
        return Ok(in_order(&ids, found, |c| c.id));
    }
    let kind = str_of(s, "/source/rule/kind");
    let category = str_of(s, "/source/rule/category_id").and_then(|c| c.parse::<Uuid>().ok());
    let featured = s
        .pointer("/source/rule/featured_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sort = str_of(s, "/source/rule/sort").unwrap_or("sort_order");
    let limit = int_of(s, "/source/rule/limit", 6).clamp(1, 24) as usize;
    let ids = sqlx::query_scalar!(
        "SELECT p.id FROM products p
         WHERE p.status = 'published' AND p.deleted_at IS NULL AND ($1::text IS NULL OR p.kind = $1)
           AND (NOT $2 OR p.is_featured)
           AND ($3::uuid IS NULL OR EXISTS (SELECT 1 FROM product_categories pc WHERE pc.product_id = p.id AND pc.category_id = $3))
         ORDER BY CASE WHEN $4 = 'newest' THEN p.created_at END DESC NULLS LAST, p.sort_order, p.id",
        kind,
        featured,
        category,
        sort
    )
    .fetch_all(&state.db)
    .await?;
    let kind = kind.and_then(|k| ProductKind::ALL.iter().copied().find(|x| x.as_str() == k));
    let found = cards(state, locale, kind, featured, Some(&ids)).await?;
    let mut list = in_order(&ids, found, |c| c.id);
    match sort {
        "price_asc" => list.sort_by_key(|c| c.from_price_minor.unwrap_or(i64::MAX)),
        "rating" => list.sort_by(|a, b| {
            let r = |c: &ProductCard| c.rating.as_ref().map_or(0.0, |r| r.average);
            r(b).total_cmp(&r(a))
        }),
        _ => {}
    }
    list.truncate(limit);
    Ok(list)
}

/// Automatic offers (no code: codes are never published) active at `now`.
async fn promotions(
    state: &AppState,
    only: &[Uuid],
    lang: &Lang<'_>,
    now: DateTime<Utc>,
) -> AppResult<Vec<Value>> {
    let rows = sqlx::query!(
        r#"SELECT p.id, p.discount_type, p.discount_percent::float8 AS "discount_percent?", p.discount_amount_minor,
                  p.discount_free_nights, p.currency,
                  upper(p.booking_window) AS ends_at, p.stay_window::text AS stay_window,
                  COALESCE(NULLIF(t.name, ''), d.name) AS "name?", COALESCE(t.description, d.description) AS "description?"
           FROM promotions p
           LEFT JOIN promotion_translations t ON t.promotion_id = p.id AND t.locale = $1
           LEFT JOIN promotion_translations d ON d.promotion_id = p.id AND d.locale = $2
           WHERE p.is_active AND p.code IS NULL AND (p.booking_window IS NULL OR p.booking_window @> $3::timestamptz)
             AND (cardinality($4::uuid[]) = 0 OR p.id = ANY($4))
           ORDER BY ends_at NULLS LAST, p.id"#,
        lang.locale,
        lang.default,
        now,
        only
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| r.name.is_some())
        .map(|r| json!({"id": r.id, "name": r.name, "description": r.description, "discount_type": r.discount_type,
                        "discount_percent": r.discount_percent, "discount_amount_minor": r.discount_amount_minor,
                        "discount_free_nights": r.discount_free_nights, "currency": r.currency, "ends_at": r.ends_at,
                        "stay_window": r.stay_window}))
        .collect())
}

pub async fn public_settings(state: &AppState, keys: &[&str]) -> AppResult<HashMap<String, Value>> {
    let keys: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
    Ok(sqlx::query!(
        "SELECT key, value FROM settings WHERE key = ANY($1) AND is_public",
        &keys
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .filter(|r| !r.value.is_null() && r.value != json!(""))
    .map(|r| (r.key, r.value))
    .collect())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2027, 1, 1, h, 0, 0).unwrap()
    }

    fn block(period: Option<TsRange>) -> BlockDraft {
        serde_json::from_value(
            json!({"id": Uuid::now_v7(), "type": "rich_text", "active_period": period}),
        )
        .unwrap()
    }

    #[test]
    fn fallback_is_field_by_field() {
        let content = BTreeMap::from([
            (
                "fr".to_string(),
                json!({"title": "Titre", "subtitle": "Sous-titre", "eyebrow": "Avant"}),
            ),
            (
                "ar".to_string(),
                json!({"title": "عنوان", "subtitle": "  "}),
            ),
        ]);
        let all = vec!["fr".to_string(), "en".to_string(), "ar".to_string()];
        let lang = Lang {
            locale: "ar",
            default: "fr",
            all: &all,
        };
        assert_eq!(
            localize(&content, &lang),
            json!({"title": "عنوان", "subtitle": "Sous-titre", "eyebrow": "Avant"})
        );
        let lang = Lang {
            locale: "en",
            default: "fr",
            all: &all,
        };
        assert_eq!(localize(&content, &lang)["title"], json!("Titre"));
    }

    #[test]
    fn schedules_filter_sections_and_set_the_cache_lifetime() {
        let blocks = vec![block(None), block(Some(TsRange::new(at(12), at(14))))];
        assert_eq!(visible_blocks(&blocks, "fr", at(10)).len(), 1);
        assert_eq!(visible_blocks(&blocks, "fr", at(12)).len(), 2);
        assert_eq!(visible_blocks(&blocks, "fr", at(14)).len(), 1);
        assert_eq!(
            ttl_until_next_boundary(&blocks, at(11)),
            Duration::from_secs(3600)
        );
        assert_eq!(
            ttl_until_next_boundary(&blocks, at(13) - chrono::Duration::minutes(50)),
            Duration::from_secs(3600)
        );
        assert_eq!(
            ttl_until_next_boundary(&blocks, at(13) + chrono::Duration::minutes(30)),
            Duration::from_secs(1800)
        );
        assert_eq!(ttl_until_next_boundary(&blocks, at(15)), MAX_TTL);
        let mut hidden = block(None);
        hidden.hidden_locales = vec!["ar".into()];
        assert!(visible_blocks(&[hidden.clone()], "ar", at(10)).is_empty());
        assert_eq!(visible_blocks(&[hidden], "fr", at(10)).len(), 1);
    }
}
