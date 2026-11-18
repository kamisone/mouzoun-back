//! The `gallery` section (schema version 2): a compact preview on the page, the full set of
//! photos on demand.
//!
//! - **Albums are the section's items**: each one points at a `galleries` row
//!   (`settings.gallery_id`), with an optional cover, a title and a description per locale, and
//!   the usual item `active_period`. `hidden_in_preview` keeps an album out of the preview only.
//! - The page payload carries the preview photos, the album list and the total count, never the
//!   whole set ([`preview_data`]).
//! - `GET /v1/sections/{block_id}/gallery` pages through the photos of the published snapshot (or
//!   the draft behind a preview token), 24 at a time, keyset-paginated on
//!   `(album position, gallery_items.sort_order, media id)` and cached in Redis under
//!   `gallery:{block_id}:{revision}:{locale}:{album}:{cursor}:{limit}`.
//! - Version 1 sections (one photo list or one gallery shown in full) are recognised by their
//!   settings (sections do not store the version of their type) and upgraded when the registry syncs ([`migrate_v1_drafts`]: drafts only, published snapshots are never
//!   rewritten) and in memory when an old snapshot is rendered ([`upgrade_v1`]).

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    model::{BlockDraft, FieldError, ItemDraft, Snapshot},
    public::{self, Lang, MediaView},
    registry::SectionRegistry,
    repo::{self, BlockOwner},
    service, validate,
};
use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::{audit, audit::Actor, outbox, outbox::Event},
    state::AppState,
    web::client::RequestLocale,
};

pub const CACHE_PREFIX: &str = "gallery:";
pub const PAGE_SIZE: usize = 24;
const MAX_PAGE_SIZE: usize = 48;
/// Keys of the version 1 settings.
const V1_KEYS: [&str; 6] = [
    "source",
    "media_ids",
    "gallery_id",
    "layout",
    "lightbox",
    "max_photos",
];

// ---- Preview layouts --------------------------------------------------------------------------

/// How many photos a preview layout shows: `(min, max)` for a manual pick.
pub fn preview_range(layout: &str) -> (usize, usize) {
    match layout {
        "mosaic_3" => (3, 3),
        "cover" => (1, 1),
        "filmstrip" => (3, 12),
        _ => (5, 5),
    }
}

fn setting<'a>(s: &'a Value, key: &str, default: &'a str) -> &'a str {
    s.get(key).and_then(Value::as_str).unwrap_or(default)
}

fn ids_of(v: Option<&Value>) -> Vec<Uuid> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

fn uuid_of(v: &Value, key: &str) -> Option<Uuid> {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

// ---- Version 1 → 2 ----------------------------------------------------------------------------

/// Settings still in the version 1 shape (`source`, `media_ids`, `layout`...).
pub fn is_v1(settings: &Value) -> bool {
    settings.get("preview_layout").is_none() && V1_KEYS.iter().any(|k| settings.get(k).is_some())
}

/// Version 1 → 2, without I/O:
/// - `source: gallery` + `gallery_id` → one album on that gallery;
/// - `source: media_list` → one album on `list_gallery` (the gallery made from the list by the
///   draft migration) or, when rendering an old snapshot, on the list itself (`media_ids` in the
///   album settings, never stored);
/// - a photo list long enough for the layout becomes a manual preview (its first photos);
/// - `layout` masonry / grid → `mosaic`, carousel → `filmstrip`; `lightbox` and `max_photos` go.
///
/// The album is titled with the section title, or "Photos".
pub fn upgrade_v1(
    block: &BlockDraft,
    list_gallery: Option<Uuid>,
    album_id: Uuid,
    default_locale: &str,
) -> BlockDraft {
    let s = &block.settings;
    let layout = match setting(s, "layout", "grid") {
        "carousel" => "filmstrip",
        _ => "mosaic",
    };
    let from_gallery = setting(s, "source", "media_list") == "gallery";
    let list = ids_of(s.get("media_ids"));
    let (min, max) = preview_range(layout);
    let manual = !from_gallery && list.len() >= min;
    let preview: Vec<Uuid> = if manual {
        list.iter().take(max).copied().collect()
    } else {
        Vec::new()
    };
    let album_settings = match (from_gallery, list_gallery) {
        (true, _) => match uuid_of(s, "gallery_id") {
            Some(g) => json!({ "gallery_id": g }),
            None => json!({}),
        },
        (false, Some(g)) => json!({ "gallery_id": g }),
        (false, None) => json!({ "media_ids": list }),
    };
    let mut titles: BTreeMap<String, Value> = block
        .content
        .iter()
        .filter_map(|(locale, c)| {
            let title = c.get("title")?.as_str()?.trim();
            (!title.is_empty()).then(|| (locale.clone(), json!({ "title": title })))
        })
        .collect();
    titles
        .entry(default_locale.to_string())
        .or_insert_with(|| json!({ "title": "Photos" }));
    BlockDraft {
        settings: json!({
            "preview_layout": layout,
            "preview_height": "md",
            "preview_source": if manual { "manual" } else { "auto" },
            "preview_media_ids": preview,
            "show_count": true,
            "show_album_chips": true,
            "viewer_layout": "albums",
            "open_lightbox_directly": false,
            "allow_download": false,
            "allow_share": true,
            "autoplay_videos": false,
        }),
        items: vec![ItemDraft {
            id: album_id,
            settings: album_settings,
            content: titles,
            is_visible: true,
            active_period: None,
        }],
        ..block.clone()
    }
}

/// Converts the version 1 gallery sections of the drafts (pages and shared library), called
/// after the section types are synced. A photo list becomes a new gallery. The page gets
/// `has_unpublished_changes` (and a new draft version, so an open builder reloads instead of
/// saving the old shape back); published snapshots are left as they are and rendered through
/// [`upgrade_v1`] until the next publish. Each conversion is logged and audited.
pub async fn migrate_v1_drafts(db: &PgPool, reg: &SectionRegistry) -> sqlx::Result<usize> {
    let rows = sqlx::query!(
        r#"SELECT id, page_id, is_shared, sort_order FROM content_blocks
           WHERE block_type = 'gallery' AND deleted_at IS NULL
             AND shared_block_id IS NULL AND NOT settings ? 'preview_layout'
             AND settings ?| array['source', 'media_ids', 'gallery_id', 'layout', 'lightbox', 'max_photos']
           ORDER BY created_at"#
    )
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let Some(t) = reg.get("gallery") else {
        return Ok(0);
    };
    let mut tx = db.begin().await?;
    let (default_locale, _) = service::locales(&mut tx).await?;
    for row in &rows {
        let Some(block) = repo::load_blocks(&mut tx, &[row.id]).await?.pop() else {
            continue;
        };
        let list = ids_of(block.settings.get("media_ids"));
        let from_gallery = setting(&block.settings, "source", "media_list") == "gallery";
        let list_gallery = if !from_gallery && !list.is_empty() {
            let title = block
                .content
                .get(&default_locale)
                .and_then(|c| c.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("Photos");
            let id = Uuid::now_v7();
            sqlx::query!(
                "INSERT INTO galleries (id, name) VALUES ($1, $2)",
                id,
                format!("{title} (section {})", &row.id.to_string()[..8])
            )
            .execute(&mut *tx)
            .await?;
            insert_items(&mut tx, id, &list).await?;
            Some(id)
        } else {
            None
        };
        let upgraded = upgrade_v1(&block, list_gallery, Uuid::now_v7(), &default_locale);
        let owner = match row.page_id {
            Some(page) if !row.is_shared => BlockOwner::Page(page),
            _ => BlockOwner::Library,
        };
        repo::upsert_block(&mut tx, owner, &upgraded, row.sort_order, None).await?;
        repo::write_refs(&mut tx, row.id, &validate::block_refs(t, &upgraded)).await?;
        let pages: Vec<Uuid> = sqlx::query_scalar!(
            "UPDATE pages SET has_unpublished_changes = true, draft_version = draft_version + 1
             WHERE deleted_at IS NULL AND (id = $1 OR id IN (
                 SELECT page_id FROM content_blocks WHERE shared_block_id = $2 AND deleted_at IS NULL AND page_id IS NOT NULL))
             RETURNING id",
            row.page_id,
            row.id
        )
        .fetch_all(&mut *tx)
        .await?;
        tracing::info!(block = %row.id, ?pages, gallery = ?list_gallery, "gallery section upgraded to schema version 2");
        audit::record(
            &mut tx,
            &Actor::system(),
            "section.migrated",
            "content_block",
            &row.id.to_string(),
            json!({"type": "gallery", "from": 1, "to": 2, "pages": pages, "created_gallery": list_gallery}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(rows.len())
}

async fn insert_items(conn: &mut PgConnection, gallery: Uuid, media: &[Uuid]) -> sqlx::Result<()> {
    let order: Vec<i32> = (0..media.len())
        .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
        .collect();
    sqlx::query!(
        "INSERT INTO gallery_items (gallery_id, media_id, sort_order)
         SELECT $1, x.media_id, x.sort_order FROM unnest($2::uuid[], $3::int[]) AS x (media_id, sort_order)
         ON CONFLICT DO NOTHING",
        gallery,
        media,
        &order
    )
    .execute(conn)
    .await?;
    Ok(())
}

// ---- Publish rules ------------------------------------------------------------------------------

/// One photo of a gallery, as the publish rules see it.
#[derive(Debug, Clone)]
pub struct PhotoFact {
    pub media_id: Uuid,
    pub kind: String,
    /// Locales with a non-empty alt text.
    pub alt_locales: Vec<String>,
}

/// Photos (public images and videos, not deleted) of each gallery, in order.
pub type GalleryFacts = HashMap<Uuid, Vec<PhotoFact>>;

#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<FieldError>,
    /// Missing alt texts in the other languages: shown, not blocking.
    pub warnings: Vec<FieldError>,
}

/// Gallery rules checked before publishing (they need the galleries, so they are not part of
/// the schema validation that runs on every autosave):
/// - every album has a gallery with at least one image or video;
/// - an album cover is a photo of that album;
/// - a manual preview has exactly the number of photos of its layout (3 to 12 for the
///   filmstrip), all taken from the section's albums;
/// - every image has an alt text in the default locale (others: warnings).
pub fn check(
    block: &BlockDraft,
    facts: &GalleryFacts,
    default_locale: &str,
    locales: &[String],
    path: &str,
) -> Report {
    let mut report = Report::default();
    let mut in_albums: HashSet<Uuid> = HashSet::new();
    let mut alt_checked: HashSet<Uuid> = HashSet::new();
    for (j, item) in block.items.iter().enumerate() {
        if !item.is_visible {
            continue;
        }
        let at = format!("{path}/items/{j}/settings");
        let Some(gallery) = uuid_of(&item.settings, "gallery_id") else {
            report.errors.push(FieldError::new(
                format!("{at}/gallery_id"),
                "required",
                "choose the gallery of this album",
            ));
            continue;
        };
        let photos = facts.get(&gallery).map(Vec::as_slice).unwrap_or_default();
        if photos.is_empty() {
            report.errors.push(FieldError::new(
                format!("{at}/gallery_id"),
                "empty_gallery",
                "this gallery has no photo or video",
            ));
            continue;
        }
        let ids: HashSet<Uuid> = photos.iter().map(|p| p.media_id).collect();
        if let Some(cover) = uuid_of(&item.settings, "cover_media_id")
            && !ids.contains(&cover)
        {
            report.errors.push(FieldError::new(
                format!("{at}/cover_media_id"),
                "cover_not_in_album",
                "the cover must be a photo of this album",
            ));
        }
        in_albums.extend(ids);
        for photo in photos.iter().filter(|p| p.kind == "image") {
            if !alt_checked.insert(photo.media_id) {
                continue;
            }
            for locale in locales {
                if photo.alt_locales.contains(locale) {
                    continue;
                }
                let e = FieldError::new(
                    format!("{path}/items/{j}/media/{}/alt/{locale}", photo.media_id),
                    "missing_alt",
                    format!("photo without a description in `{locale}`"),
                );
                if locale == default_locale {
                    report.errors.push(e);
                } else {
                    report.warnings.push(e);
                }
            }
        }
    }
    let s = &block.settings;
    if setting(s, "preview_source", "auto") == "manual" {
        let layout = setting(s, "preview_layout", "mosaic");
        let (min, max) = preview_range(layout);
        let picked = ids_of(s.get("preview_media_ids"));
        if picked.len() < min || picked.len() > max {
            let wanted = if min == max {
                format!("{min}")
            } else {
                format!("{min} to {max}")
            };
            report.errors.push(FieldError::new(
                format!("{path}/settings/preview_media_ids"),
                "preview_count",
                format!("the `{layout}` preview shows {wanted} photos"),
            ));
        }
        for (k, id) in picked.iter().enumerate() {
            if !in_albums.contains(id) {
                report.errors.push(FieldError::new(
                    format!("{path}/settings/preview_media_ids/{k}"),
                    "not_in_albums",
                    "this photo is not in any album of the section",
                ));
            }
        }
    }
    report
}

/// Photos of the given galleries, for [`check`].
pub async fn load_facts(conn: &mut PgConnection, galleries: &[Uuid]) -> sqlx::Result<GalleryFacts> {
    let mut facts = GalleryFacts::new();
    if galleries.is_empty() {
        return Ok(facts);
    }
    let rows = sqlx::query!(
        r#"SELECT gi.gallery_id, gi.media_id, m.kind,
                  ARRAY(SELECT t.locale FROM media_asset_translations t
                        WHERE t.media_asset_id = m.id AND btrim(coalesce(t.alt_text, '')) <> '') AS "alt_locales!"
           FROM gallery_items gi
           JOIN media_assets m ON m.id = gi.media_id AND m.deleted_at IS NULL AND m.visibility = 'public'
                               AND m.kind IN ('image', 'video')
           WHERE gi.gallery_id = ANY($1)
           ORDER BY gi.gallery_id, gi.sort_order, gi.media_id"#,
        galleries
    )
    .fetch_all(conn)
    .await?;
    for r in rows {
        facts.entry(r.gallery_id).or_default().push(PhotoFact {
            media_id: r.media_id,
            kind: r.kind,
            alt_locales: r.alt_locales,
        });
    }
    Ok(facts)
}

/// Runs [`check`] on every gallery section of a page (`blocks` as loaded, placements expanded).
pub async fn check_blocks(
    conn: &mut PgConnection,
    blocks: &[BlockDraft],
    default_locale: &str,
    locales: &[String],
) -> sqlx::Result<Report> {
    // Placements are checked with their shared section, at their own index.
    let inlined: Vec<(usize, BlockDraft)> = blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| Some((i, service::inline_shared(vec![b.clone()]).pop()?)))
        .filter(|(_, b)| b.block_type == "gallery" && b.is_visible && !is_v1(&b.settings))
        .collect();
    let galleries: Vec<Uuid> = inlined
        .iter()
        .flat_map(|(_, b)| b.items.iter())
        .filter_map(|i| uuid_of(&i.settings, "gallery_id"))
        .collect();
    let facts = load_facts(conn, &galleries).await?;
    let mut report = Report::default();
    for (i, b) in &inlined {
        let r = check(b, &facts, default_locale, locales, &format!("/blocks/{i}"));
        report.errors.extend(r.errors);
        report.warnings.extend(r.warnings);
    }
    Ok(report)
}

// ---- Albums and their photos ------------------------------------------------------------------

#[derive(Debug, Clone)]
enum AlbumSource {
    Gallery(Uuid),
    /// A version 1 photo list rendered from an old snapshot.
    List(Vec<Uuid>),
}

#[derive(Debug, Clone)]
struct Album {
    id: Uuid,
    source: AlbumSource,
    cover: Option<Uuid>,
    hidden_in_preview: bool,
    title: String,
    description: Option<String>,
}

/// One photo of the viewer, in viewer order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    position: u32,
    sort: i32,
    media_id: Uuid,
}

/// The block in the version 2 shape (old snapshots are upgraded in memory; the album then has
/// the section's id).
pub fn current_shape(block: &BlockDraft, default_locale: &str) -> BlockDraft {
    if is_v1(&block.settings) {
        upgrade_v1(block, None, block.id, default_locale)
    } else {
        block.clone()
    }
}

/// Albums of a section whose items were already filtered (visibility, period).
fn albums_of(block: &BlockDraft, lang: &Lang<'_>) -> Vec<Album> {
    block
        .items
        .iter()
        .filter_map(|item| {
            let source = match uuid_of(&item.settings, "gallery_id") {
                Some(g) => AlbumSource::Gallery(g),
                None => match item.settings.get("media_ids") {
                    Some(list) => AlbumSource::List(ids_of(Some(list))),
                    None => return None,
                },
            };
            let content = public::localize(&item.content, lang);
            Some(Album {
                id: item.id,
                source,
                cover: uuid_of(&item.settings, "cover_media_id"),
                hidden_in_preview: item
                    .settings
                    .get("hidden_in_preview")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                title: content["title"].as_str().unwrap_or_default().to_string(),
                description: content["description"]
                    .as_str()
                    .filter(|d| !d.trim().is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Public photos and videos of each album, in viewer order (album position, then gallery order).
async fn entries_of(db: &PgPool, albums: &[Album]) -> sqlx::Result<Vec<Vec<Entry>>> {
    let galleries: Vec<Uuid> = albums
        .iter()
        .filter_map(|a| match a.source {
            AlbumSource::Gallery(g) => Some(g),
            AlbumSource::List(_) => None,
        })
        .collect();
    let listed: Vec<Uuid> = albums
        .iter()
        .flat_map(|a| match &a.source {
            AlbumSource::List(ids) => ids.clone(),
            AlbumSource::Gallery(_) => Vec::new(),
        })
        .collect();
    let mut by_gallery: HashMap<Uuid, Vec<(i32, Uuid)>> = HashMap::new();
    if !galleries.is_empty() {
        for r in sqlx::query!(
            "SELECT gi.gallery_id, gi.media_id, gi.sort_order FROM gallery_items gi
             JOIN media_assets m ON m.id = gi.media_id AND m.deleted_at IS NULL AND m.visibility = 'public'
                                 AND m.kind IN ('image', 'video')
             WHERE gi.gallery_id = ANY($1)
             ORDER BY gi.gallery_id, gi.sort_order, gi.media_id",
            &galleries
        )
        .fetch_all(db)
        .await?
        {
            by_gallery
                .entry(r.gallery_id)
                .or_default()
                .push((r.sort_order, r.media_id));
        }
    }
    let usable: HashSet<Uuid> = if listed.is_empty() {
        HashSet::new()
    } else {
        sqlx::query_scalar!(
            "SELECT id FROM media_assets WHERE id = ANY($1) AND deleted_at IS NULL AND visibility = 'public'
               AND kind IN ('image', 'video')",
            &listed
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .collect()
    };
    Ok(albums
        .iter()
        .enumerate()
        .map(|(pos, a)| {
            let position = u32::try_from(pos).unwrap_or(u32::MAX);
            match &a.source {
                AlbumSource::Gallery(g) => by_gallery
                    .get(g)
                    .map(|photos| {
                        photos
                            .iter()
                            .map(|(sort, media_id)| Entry {
                                position,
                                sort: *sort,
                                media_id: *media_id,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                AlbumSource::List(ids) => {
                    let mut seen = HashSet::new();
                    ids.iter()
                        .filter(|id| usable.contains(id) && seen.insert(**id))
                        .enumerate()
                        .map(|(i, media_id)| Entry {
                            position,
                            sort: i32::try_from(i).unwrap_or(i32::MAX),
                            media_id: *media_id,
                        })
                        .collect()
                }
            }
        })
        .collect())
}

fn cover_of(album: &Album, entries: &[Entry]) -> Option<Uuid> {
    album
        .cover
        .filter(|c| entries.iter().any(|e| e.media_id == *c))
        .or_else(|| entries.first().map(|e| e.media_id))
}

/// Preview photos: the manual pick (photos still in an album), or the first photos of the
/// albums shown in the preview, one album after the other (first photo of each album, then the
/// second of each...), as many as the layout shows.
fn preview_of(settings: &Value, albums: &[Album], entries: &[Vec<Entry>]) -> Vec<(Uuid, Uuid)> {
    let layout = setting(settings, "preview_layout", "mosaic");
    let (_, max) = preview_range(layout);
    let album_of: HashMap<Uuid, Uuid> = albums
        .iter()
        .zip(entries)
        .rev()
        .flat_map(|(a, e)| e.iter().map(move |x| (x.media_id, a.id)))
        .collect();
    if setting(settings, "preview_source", "auto") == "manual" {
        return ids_of(settings.get("preview_media_ids"))
            .into_iter()
            .filter_map(|id| album_of.get(&id).map(|a| (id, *a)))
            .take(max)
            .collect();
    }
    let shown: Vec<(&Album, &Vec<Entry>)> = albums
        .iter()
        .zip(entries)
        .filter(|(a, _)| !a.hidden_in_preview)
        .collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let deepest = shown.iter().map(|(_, e)| e.len()).max().unwrap_or(0);
    'rounds: for round in 0..deepest {
        for (album, photos) in &shown {
            if let Some(e) = photos.get(round)
                && seen.insert(e.media_id)
            {
                out.push((e.media_id, album.id));
                if out.len() == max {
                    break 'rounds;
                }
            }
        }
    }
    out
}

/// `data` of a gallery section on the page: what the preview needs, never the whole set.
/// `media_id` / `cover_media_id` are resolved into `data.media` by the renderer.
pub async fn preview_data(
    state: &AppState,
    block: &BlockDraft,
    lang: &Lang<'_>,
) -> AppResult<Value> {
    let albums = albums_of(block, lang);
    let entries = entries_of(&state.db, &albums).await?;
    let total: usize = entries.iter().map(Vec::len).sum();
    let preview = preview_of(&block.settings, &albums, &entries);
    Ok(json!({
        "total_count": total,
        "preview": preview.iter().map(|(media, album)| json!({ "media_id": media, "album_id": album })).collect::<Vec<_>>(),
        "albums": albums.iter().zip(&entries).filter(|(_, e)| !e.is_empty()).map(|(a, e)| json!({
            "id": a.id, "title": a.title, "description": a.description, "count": e.len(),
            "cover_media_id": cover_of(a, e), "hidden_in_preview": a.hidden_in_preview,
        })).collect::<Vec<_>>(),
    }))
}

// ---- Public endpoint ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GalleryQuery {
    pub album: Option<Uuid>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumView {
    pub id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub count: usize,
    pub cover: Option<MediaView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoView {
    pub media: MediaView,
    pub caption: Option<String>,
    pub album_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryPage {
    /// Only on the first page (no cursor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub albums: Option<Vec<AlbumView>>,
    pub items: Vec<PhotoView>,
    pub next_cursor: Option<String>,
    pub total_count: usize,
}

fn encode_cursor(e: &Entry) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}.{}.{}", e.position, e.sort, e.media_id))
}

fn decode_cursor(raw: &str) -> Option<Entry> {
    let text = String::from_utf8(URL_SAFE_NO_PAD.decode(raw).ok()?).ok()?;
    let mut parts = text.splitn(3, '.');
    Some(Entry {
        position: parts.next()?.parse().ok()?,
        sort: parts.next()?.parse().ok()?,
        media_id: parts.next()?.parse().ok()?,
    })
}

/// The entries after `cursor` (keyset), `limit` of them, and the cursor of the next page.
fn page_of(all: &[Entry], cursor: Option<&Entry>, limit: usize) -> (Vec<Entry>, Option<String>) {
    let start = match cursor {
        Some(c) => all.partition_point(|e| e <= c),
        None => 0,
    };
    let slice: Vec<Entry> = all.iter().skip(start).take(limit).copied().collect();
    let next = (start + slice.len() < all.len())
        .then(|| slice.last().map(encode_cursor))
        .flatten();
    (slice, next)
}

/// The block in the published snapshot of its page: `(page, revision, block)`.
async fn published_block(db: &PgPool, block_id: Uuid) -> AppResult<(Uuid, i32, BlockDraft)> {
    let needle = json!([{ "id": block_id }]);
    let row = sqlx::query!(
        "SELECT page_id, revision_number, snapshot FROM page_revisions
         WHERE status = 'published' AND snapshot -> 'blocks' @> $1
         ORDER BY page_id LIMIT 1",
        needle
    )
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        let removed = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM page_revisions WHERE status = 'superseded' AND snapshot -> 'blocks' @> $1) AS "e!""#,
            needle
        )
        .fetch_one(db)
        .await?;
        return Err(if removed {
            AppError::domain(
                StatusCode::GONE,
                "section_removed",
                "this section is no longer on the page",
            )
        } else {
            AppError::NotFound
        });
    };
    let snapshot: Snapshot = serde_json::from_value(row.snapshot).map_err(anyhow::Error::from)?;
    let block = snapshot
        .blocks
        .into_iter()
        .find(|b| b.id == block_id)
        .ok_or(AppError::NotFound)?;
    Ok((row.page_id, row.revision_number, block))
}

/// `GET /v1/sections/{block_id}/gallery?locale=&album=&cursor=&limit=24&preview=`
pub async fn photos(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Path(block_id): Path<Uuid>,
    Query(q): Query<GalleryQuery>,
) -> AppResult<Response> {
    let limit = q.limit.unwrap_or(PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let cursor = match q.cursor.as_deref() {
        Some(raw) => {
            Some(decode_cursor(raw).ok_or_else(|| AppError::BadRequest("invalid cursor".into()))?)
        }
        None => None,
    };
    let locale = locale.as_str();
    let now = state.clock.now();
    let (block, key) = match q.preview.as_deref() {
        Some(token) => {
            let grant = service::preview_grant(&state.cache, token)
                .await
                .ok_or_else(|| {
                    AppError::domain(
                        StatusCode::FORBIDDEN,
                        "preview_expired",
                        "this preview link expired, open the builder again",
                    )
                })?;
            let mut conn = state.db.acquire().await?;
            let snapshot =
                service::draft_preview_snapshot(&mut conn, &state.sections, grant.page_id).await?;
            let block = snapshot
                .blocks
                .into_iter()
                .find(|b| b.id == block_id)
                .ok_or(AppError::NotFound)?;
            (block, None)
        }
        None => {
            let (_, revision, block) = published_block(&state.db, block_id).await?;
            let key = format!(
                "{CACHE_PREFIX}{block_id}:{revision}:{locale}:{}:{}:{limit}",
                q.album.map_or_else(|| "all".to_string(), |a| a.to_string()),
                q.cursor.as_deref().unwrap_or("start"),
            );
            (block, Some(key))
        }
    };
    if let Some(key) = &key
        && let Some(cached) = state.cache.get_json::<GalleryPage>(key).await
    {
        return Ok(Json(cached).into_response());
    }
    if block.block_type != "gallery" {
        return Err(AppError::NotFound);
    }
    let mut conn = state.db.acquire().await?;
    let (default, all) = service::locales(&mut conn).await?;
    drop(conn);
    let lang = Lang {
        locale,
        default: &default,
        all: &all,
    };
    let block = current_shape(&block, &default);
    let Some(block) = public::visible_blocks(std::slice::from_ref(&block), locale, now).pop()
    else {
        return Err(AppError::NotFound);
    };
    let albums = albums_of(&block, &lang);
    let entries = entries_of(&state.db, &albums).await?;
    let total_count: usize = entries.iter().map(Vec::len).sum();
    let selected: Vec<Entry> = match q.album {
        Some(album) => entries[albums
            .iter()
            .position(|a| a.id == album)
            .ok_or(AppError::NotFound)?]
        .clone(),
        None => entries.iter().flatten().copied().collect(),
    };
    let (slice, next_cursor) = page_of(&selected, cursor.as_ref(), limit);

    let mut ids: Vec<Uuid> = slice.iter().map(|e| e.media_id).collect();
    let covers: Vec<Option<Uuid>> = albums
        .iter()
        .zip(&entries)
        .map(|(a, e)| cover_of(a, e))
        .collect();
    if cursor.is_none() {
        ids.extend(covers.iter().flatten());
    }
    let views = public::media_views(&state, &ids, &lang).await?;
    let captions = captions(&state.db, &ids, &lang).await?;
    let album_ids: Vec<Uuid> = albums.iter().map(|a| a.id).collect();
    let items = slice
        .iter()
        .filter_map(|e| {
            Some(PhotoView {
                media: views.get(&e.media_id)?.clone(),
                caption: captions.get(&e.media_id).cloned(),
                album_id: album_ids[e.position as usize],
            })
        })
        .collect();
    let albums = cursor.is_none().then(|| {
        albums
            .iter()
            .zip(&entries)
            .zip(&covers)
            .filter(|((_, e), _)| !e.is_empty())
            .map(|((a, e), cover)| AlbumView {
                id: a.id,
                title: a.title.clone(),
                description: a.description.clone(),
                count: e.len(),
                cover: cover.and_then(|c| views.get(&c).cloned()),
            })
            .collect()
    });
    let body = GalleryPage {
        albums,
        items,
        next_cursor,
        total_count,
    };
    match key {
        Some(key) => {
            let ttl = public::ttl_until_next_boundary(std::slice::from_ref(&block), now);
            state.cache.set_json(&key, &body, ttl).await;
            Ok(Json(body).into_response())
        }
        None => {
            let mut res = Json(body).into_response();
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(res)
        }
    }
}

async fn captions(
    db: &PgPool,
    ids: &[Uuid],
    lang: &Lang<'_>,
) -> sqlx::Result<HashMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(sqlx::query!(
        r#"SELECT m.id, COALESCE(NULLIF(btrim(t.caption), ''), NULLIF(btrim(d.caption), '')) AS "caption?"
           FROM media_assets m
           LEFT JOIN media_asset_translations t ON t.media_asset_id = m.id AND t.locale = $2
           LEFT JOIN media_asset_translations d ON d.media_asset_id = m.id AND d.locale = $3
           WHERE m.id = ANY($1)"#,
        ids,
        lang.locale,
        lang.default
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .filter_map(|r| Some((r.id, r.caption?)))
    .collect())
}

/// Deletes the cached photo pages of the gallery sections of a page's published revision.
pub async fn invalidate_page_galleries(
    db: &PgPool,
    cache: &crate::infra::cache::Cache,
    page_id: Uuid,
) -> anyhow::Result<usize> {
    let blocks: Vec<String> = sqlx::query_scalar!(
        r#"SELECT b ->> 'id' AS "id!" FROM page_revisions r, jsonb_array_elements(r.snapshot -> 'blocks') b
           WHERE r.page_id = $1 AND r.status = 'published' AND b ->> 'type' = 'gallery'"#,
        page_id
    )
    .fetch_all(db)
    .await?;
    let mut deleted = 0;
    for id in blocks {
        deleted += cache.delete_prefix(&format!("{CACHE_PREFIX}{id}:")).await?;
    }
    Ok(deleted)
}

// ---- Back office: galleries ----------------------------------------------------------------------

const READ: &str = "content.read";
const WRITE: &str = "content.write";

#[derive(Debug, Deserialize)]
pub struct GalleryInput {
    pub name: Option<String>,
    pub media_ids: Vec<Uuid>,
}

async fn gallery_json(state: &AppState, id: Uuid) -> AppResult<Value> {
    let name = sqlx::query_scalar!("SELECT name FROM galleries WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows = sqlx::query!(
        r#"SELECT m.id, m.kind, m.storage_provider, m.object_key, m.width, m.height, m.visibility,
                  COALESCE((SELECT jsonb_object_agg(t.locale, t.alt_text) FROM media_asset_translations t
                            WHERE t.media_asset_id = m.id AND btrim(coalesce(t.alt_text, '')) <> ''), '{}') AS "alt!"
           FROM gallery_items gi JOIN media_assets m ON m.id = gi.media_id AND m.deleted_at IS NULL
           WHERE gi.gallery_id = $1 ORDER BY gi.sort_order, gi.media_id"#,
        id
    )
    .fetch_all(&state.db)
    .await?;
    let photos: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let url = if r.storage_provider == "site" {
                r.object_key.clone()
            } else {
                state.media.public_url(&r.object_key)
            };
            json!({"media_id": r.id, "kind": r.kind, "url": url, "width": r.width, "height": r.height,
                   "public": r.visibility == "public", "alt": r.alt})
        })
        .collect();
    Ok(json!({ "id": id, "name": name, "photos": photos }))
}

/// `GET /v1/admin/galleries/{id}`: the photos of a gallery with their alt texts per locale.
pub async fn admin_get(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require(READ)?;
    Ok(Json(gallery_json(&state, id).await?))
}

fn clean_name(name: Option<&str>) -> AppResult<Option<String>> {
    match name.map(str::trim) {
        Some("") => Err(AppError::BadRequest("the gallery needs a name".into())),
        Some(n) if n.chars().count() > 120 => {
            Err(AppError::BadRequest("at most 120 characters".into()))
        }
        other => Ok(other.map(str::to_string)),
    }
}

async fn existing_media(conn: &mut PgConnection, ids: &[Uuid]) -> AppResult<()> {
    let found: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM media_assets WHERE id = ANY($1) AND deleted_at IS NULL"#,
        ids
    )
    .fetch_one(conn)
    .await?;
    let unique: HashSet<&Uuid> = ids.iter().collect();
    if usize::try_from(found).unwrap_or(0) != unique.len() || unique.len() != ids.len() {
        return Err(AppError::BadRequest(
            "unknown or repeated photo in the list".into(),
        ));
    }
    Ok(())
}

/// `POST /v1/admin/galleries` `{name, media_ids}` (the builder's "New gallery" shortcut).
pub async fn admin_create(
    State(state): State<AppState>,
    staff: Staff,
    Json(input): Json<GalleryInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require(WRITE)?;
    let name = clean_name(input.name.as_deref())?
        .ok_or_else(|| AppError::BadRequest("the gallery needs a name".into()))?;
    let mut tx = state.db.begin().await?;
    existing_media(&mut tx, &input.media_ids).await?;
    let id = Uuid::now_v7();
    sqlx::query!("INSERT INTO galleries (id, name) VALUES ($1, $2)", id, name)
        .execute(&mut *tx)
        .await?;
    insert_items(&mut tx, id, &input.media_ids).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "gallery.created",
        "gallery",
        &id.to_string(),
        json!({"name": name, "photos": input.media_ids.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(gallery_json(&state, id).await?)))
}

/// `PUT /v1/admin/galleries/{id}` `{name?, media_ids}`: renames and replaces the photo list (in
/// order). Pages showing the gallery are refreshed through `content.changed`.
pub async fn admin_update(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(input): Json<GalleryInput>,
) -> AppResult<Json<Value>> {
    staff.require(WRITE)?;
    let name = clean_name(input.name.as_deref())?;
    let mut tx = state.db.begin().await?;
    let exists = sqlx::query_scalar!("SELECT id FROM galleries WHERE id = $1 FOR UPDATE", id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }
    existing_media(&mut tx, &input.media_ids).await?;
    if let Some(name) = &name {
        sqlx::query!(
            "UPDATE galleries SET name = $2, updated_at = now() WHERE id = $1",
            id,
            name
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!("DELETE FROM gallery_items WHERE gallery_id = $1", id)
        .execute(&mut *tx)
        .await?;
    insert_items(&mut tx, id, &input.media_ids).await?;
    outbox::enqueue(
        &mut tx,
        &Event::ContentChanged {
            ref_type: "gallery".into(),
            ref_id: id,
        },
    )
    .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "gallery.updated",
        "gallery",
        &id.to_string(),
        json!({"name": name, "photos": input.media_ids.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(gallery_json(&state, id).await?))
}

#[derive(Debug, Deserialize)]
pub struct MediaTexts {
    /// `{ locale: { alt_text?, title?, caption? } }`; an empty string clears the field.
    pub translations: BTreeMap<String, MediaText>,
}

#[derive(Debug, Deserialize)]
pub struct MediaText {
    pub alt_text: Option<String>,
    pub title: Option<String>,
    pub caption: Option<String>,
}

/// `PUT /v1/admin/media/{id}/texts`: alt text, title and caption per locale (the builder's
/// "describe this photo" dialog). Pages showing the photo are refreshed.
pub async fn admin_media_texts(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(input): Json<MediaTexts>,
) -> AppResult<Json<Value>> {
    staff.require(WRITE)?;
    let mut tx = state.db.begin().await?;
    let (_, locales) = service::locales(&mut tx).await?;
    sqlx::query_scalar!(
        "SELECT id FROM media_assets WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let trimmed = |v: &Option<String>, max: usize| -> AppResult<Option<String>> {
        match v.as_deref().map(str::trim) {
            Some(s) if s.chars().count() > max => {
                Err(AppError::BadRequest(format!("at most {max} characters")))
            }
            Some(s) => Ok(Some(s.to_string())),
            None => Ok(None),
        }
    };
    for (locale, t) in &input.translations {
        if !locales.contains(locale) {
            return Err(AppError::BadRequest(format!(
                "`{locale}` is not an active language"
            )));
        }
        let (alt, title, caption) = (
            trimmed(&t.alt_text, 300)?,
            trimmed(&t.title, 200)?,
            trimmed(&t.caption, 500)?,
        );
        sqlx::query!(
            "INSERT INTO media_asset_translations (media_asset_id, locale, alt_text, title, caption)
             VALUES ($1, $2, NULLIF($3, ''), NULLIF($4, ''), NULLIF($5, ''))
             ON CONFLICT (media_asset_id, locale) DO UPDATE SET
                 alt_text = CASE WHEN $3::text IS NULL THEN media_asset_translations.alt_text ELSE NULLIF($3, '') END,
                 title = CASE WHEN $4::text IS NULL THEN media_asset_translations.title ELSE NULLIF($4, '') END,
                 caption = CASE WHEN $5::text IS NULL THEN media_asset_translations.caption ELSE NULLIF($5, '') END",
            id,
            locale,
            alt,
            title,
            caption
        )
        .execute(&mut *tx)
        .await?;
    }
    outbox::enqueue(
        &mut tx,
        &Event::ContentChanged {
            ref_type: "media".into(),
            ref_id: id,
        },
    )
    .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "media.texts_updated",
        "media_asset",
        &id.to_string(),
        json!({"locales": input.translations.keys().collect::<Vec<_>>()}),
    )
    .await?;
    let texts = sqlx::query!(
        "SELECT locale, alt_text, title, caption FROM media_asset_translations WHERE media_asset_id = $1",
        id
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "id": id,
        "translations": texts.into_iter().map(|r| (r.locale, json!({"alt_text": r.alt_text, "title": r.title, "caption": r.caption}))).collect::<serde_json::Map<_, _>>()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::pages::model::VisibleOn;

    fn locales() -> Vec<String> {
        vec!["fr".into(), "en".into(), "ar".into()]
    }

    fn block(settings: Value, items: Vec<ItemDraft>) -> BlockDraft {
        BlockDraft {
            id: Uuid::now_v7(),
            block_type: "gallery".into(),
            name: None,
            anchor_id: None,
            is_visible: true,
            active_period: None,
            visible_on: VisibleOn::All,
            hidden_locales: vec![],
            appearance: json!({}),
            analytics_label: None,
            settings,
            content: BTreeMap::from([("fr".to_string(), json!({"title": "Le camp en images"}))]),
            items,
            shared_block_id: None,
            shared: None,
        }
    }

    fn album(gallery: Uuid) -> ItemDraft {
        ItemDraft {
            id: Uuid::now_v7(),
            settings: json!({ "gallery_id": gallery }),
            content: BTreeMap::from([("fr".to_string(), json!({"title": "Tentes"}))]),
            is_visible: true,
            active_period: None,
        }
    }

    /// A gallery of `n` images described in every language.
    fn gallery(facts: &mut GalleryFacts, n: usize) -> (Uuid, Vec<Uuid>) {
        let id = Uuid::now_v7();
        let photos: Vec<PhotoFact> = (0..n)
            .map(|_| PhotoFact {
                media_id: Uuid::now_v7(),
                kind: "image".into(),
                alt_locales: locales(),
            })
            .collect();
        let ids = photos.iter().map(|p| p.media_id).collect();
        facts.insert(id, photos);
        (id, ids)
    }

    fn codes(r: &Report) -> Vec<(&str, &str)> {
        r.errors
            .iter()
            .map(|e| (e.path.as_str(), e.code.as_str()))
            .collect()
    }

    #[test]
    fn a_manual_preview_needs_the_number_of_photos_of_its_layout() {
        let mut facts = GalleryFacts::new();
        let (g, photos) = gallery(&mut facts, 14);
        for (layout, ok, wrong) in [
            ("mosaic", vec![5], vec![4, 6]),
            ("mosaic_3", vec![3], vec![2, 4]),
            ("cover", vec![1], vec![0, 2]),
            ("filmstrip", vec![3, 7, 12], vec![2, 13]),
        ] {
            for n in ok {
                let b = block(
                    json!({"preview_layout": layout, "preview_source": "manual", "preview_media_ids": photos[..n]}),
                    vec![album(g)],
                );
                let r = check(&b, &facts, "fr", &locales(), "/blocks/0");
                assert!(r.errors.is_empty(), "{layout} with {n}: {:?}", r.errors);
            }
            for n in wrong {
                let b = block(
                    json!({"preview_layout": layout, "preview_source": "manual", "preview_media_ids": photos[..n]}),
                    vec![album(g)],
                );
                let r = check(&b, &facts, "fr", &locales(), "/blocks/0");
                assert_eq!(
                    codes(&r),
                    [("/blocks/0/settings/preview_media_ids", "preview_count")],
                    "{layout} with {n}"
                );
            }
        }
        // the automatic preview needs no pick
        let b = block(
            json!({"preview_layout": "mosaic", "preview_source": "auto"}),
            vec![album(g)],
        );
        assert!(check(&b, &facts, "fr", &locales(), "").errors.is_empty());
    }

    #[test]
    fn a_manual_preview_photo_must_come_from_the_albums() {
        let mut facts = GalleryFacts::new();
        let (g, photos) = gallery(&mut facts, 5);
        let (_, elsewhere) = gallery(&mut facts, 1);
        let mut pick = photos[..4].to_vec();
        pick.push(elsewhere[0]);
        let b = block(
            json!({"preview_layout": "mosaic", "preview_source": "manual", "preview_media_ids": pick}),
            vec![album(g)],
        );
        let r = check(&b, &facts, "fr", &locales(), "/blocks/2");
        assert_eq!(
            codes(&r),
            [("/blocks/2/settings/preview_media_ids/4", "not_in_albums")]
        );
    }

    #[test]
    fn albums_need_a_gallery_with_photos_and_images_need_their_alt_text() {
        let mut facts = GalleryFacts::new();
        let (g, photos) = gallery(&mut facts, 3);
        let (empty, _) = gallery(&mut facts, 0);
        facts.get_mut(&g).unwrap()[1].alt_locales = vec!["en".into()];
        facts.get_mut(&g).unwrap()[2].kind = "video".into();
        facts.get_mut(&g).unwrap()[2].alt_locales = vec![];
        let mut no_gallery = album(g);
        no_gallery.settings = json!({});
        let mut bad_cover = album(g);
        bad_cover.settings["cover_media_id"] = json!(Uuid::now_v7());
        let b = block(
            json!({"preview_layout": "mosaic"}),
            vec![album(g), no_gallery, album(empty), bad_cover],
        );
        let r = check(&b, &facts, "fr", &locales(), "/blocks/0");
        assert_eq!(
            codes(&r),
            [
                (
                    format!("/blocks/0/items/0/media/{}/alt/fr", photos[1]).as_str(),
                    "missing_alt"
                ),
                ("/blocks/0/items/1/settings/gallery_id", "required"),
                ("/blocks/0/items/2/settings/gallery_id", "empty_gallery"),
                (
                    "/blocks/0/items/3/settings/cover_media_id",
                    "cover_not_in_album"
                ),
            ]
        );
        // other languages are warnings; videos need no alt text
        assert_eq!(
            r.warnings
                .iter()
                .map(|w| w.path.as_str())
                .collect::<Vec<_>>(),
            [format!("/blocks/0/items/0/media/{}/alt/ar", photos[1])]
        );
        // a hidden album is not checked
        let mut hidden = album(empty);
        hidden.is_visible = false;
        let b = block(json!({}), vec![album(g), hidden]);
        assert_eq!(check(&b, &facts, "fr", &locales(), "").errors.len(), 1);
    }

    #[test]
    fn version_1_sections_become_one_album() {
        let g = Uuid::now_v7();
        let photos: Vec<Uuid> = (0..6).map(|_| Uuid::now_v7()).collect();
        let album_id = Uuid::now_v7();

        // a gallery shown as a masonry: one album on that gallery, automatic preview
        let v1 = block(
            json!({"source": "gallery", "gallery_id": g, "layout": "masonry", "lightbox": true, "max_photos": 12}),
            vec![],
        );
        assert!(is_v1(&v1.settings));
        let v2 = upgrade_v1(&v1, None, album_id, "fr");
        assert!(!is_v1(&v2.settings));
        assert_eq!(v2.settings["preview_layout"], "mosaic");
        assert_eq!(v2.settings["preview_source"], "auto");
        assert_eq!(v2.settings["preview_media_ids"], json!([]));
        for gone in [
            "source",
            "gallery_id",
            "media_ids",
            "layout",
            "lightbox",
            "max_photos",
        ] {
            assert!(v2.settings.get(gone).is_none(), "{gone}");
        }
        assert_eq!(v2.items.len(), 1);
        assert_eq!(v2.items[0].id, album_id);
        assert_eq!(v2.items[0].settings, json!({"gallery_id": g}));
        assert_eq!(
            v2.items[0].content["fr"],
            json!({"title": "Le camp en images"})
        );

        // a photo list on a grid: the new gallery, the first five photos as the preview
        let v1 = block(
            json!({"source": "media_list", "media_ids": photos, "layout": "grid", "lightbox": false, "max_photos": 6}),
            vec![],
        );
        let made = Uuid::now_v7();
        let v2 = upgrade_v1(&v1, Some(made), album_id, "fr");
        assert_eq!(v2.settings["preview_layout"], "mosaic");
        assert_eq!(v2.settings["preview_source"], "manual");
        assert_eq!(v2.settings["preview_media_ids"], json!(photos[..5]));
        assert_eq!(v2.items[0].settings, json!({"gallery_id": made}));

        // a carousel becomes a filmstrip (all six photos fit); rendered from an old snapshot
        // (no gallery made), the list itself is the album
        let mut v1 = block(
            json!({"source": "media_list", "media_ids": photos, "layout": "carousel"}),
            vec![],
        );
        v1.content.clear();
        let v2 = upgrade_v1(&v1, None, album_id, "fr");
        assert_eq!(v2.settings["preview_layout"], "filmstrip");
        assert_eq!(v2.settings["preview_media_ids"], json!(photos));
        assert_eq!(v2.items[0].settings, json!({"media_ids": photos}));
        assert_eq!(v2.items[0].content["fr"], json!({"title": "Photos"}));

        // too few photos for the layout: automatic preview
        let v1 = block(
            json!({"source": "media_list", "media_ids": photos[..2], "layout": "grid"}),
            vec![],
        );
        assert_eq!(
            upgrade_v1(&v1, None, album_id, "fr").settings["preview_source"],
            "auto"
        );
        // a version 2 section is left alone
        assert!(!is_v1(&json!({"preview_layout": "cover"})));
        assert!(!is_v1(&json!({})));
    }

    #[test]
    fn the_automatic_preview_takes_the_first_photo_of_each_album_in_turn() {
        let a = |n: u32| Album {
            id: Uuid::now_v7(),
            source: AlbumSource::List(vec![]),
            cover: None,
            hidden_in_preview: n == 1,
            title: String::new(),
            description: None,
        };
        let albums = vec![a(0), a(1), a(2)];
        let entries: Vec<Vec<Entry>> = (0..3)
            .map(|p| {
                (0..3)
                    .map(|s| Entry {
                        position: p,
                        sort: s,
                        media_id: Uuid::now_v7(),
                    })
                    .collect()
            })
            .collect();
        let picked = preview_of(&json!({"preview_layout": "mosaic"}), &albums, &entries);
        let expected: Vec<(Uuid, Uuid)> = [(0, 0), (2, 0), (0, 1), (2, 1), (0, 2)]
            .iter()
            .map(|(p, s)| (entries[*p][*s].media_id, albums[*p].id))
            .collect();
        assert_eq!(picked, expected, "album 1 is hidden from the preview");
        let cover = preview_of(&json!({"preview_layout": "cover"}), &albums, &entries);
        assert_eq!(cover.len(), 1);
    }

    #[test]
    fn keyset_pages_have_no_gap_and_no_duplicate() {
        let all: Vec<Entry> = (0..3)
            .flat_map(|p| {
                (0..10).map(move |s| Entry {
                    position: p,
                    sort: s,
                    media_id: Uuid::now_v7(),
                })
            })
            .collect();
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let (page, next) = page_of(&all, cursor.as_ref(), 7);
            seen.extend(page);
            match next {
                Some(c) => cursor = Some(decode_cursor(&c).unwrap()),
                None => break,
            }
        }
        assert_eq!(seen, all);
        assert!(decode_cursor("not a cursor").is_none());
    }
}
