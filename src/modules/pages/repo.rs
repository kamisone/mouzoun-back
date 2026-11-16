//! Reads and writes of the draft tables (`content_blocks`, items, translations, refs).

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::model::{BlockDraft, ItemDraft, PageText, RefEntry, VisibleOn};
use crate::db::types::TsRange;

/// Where a list of sections lives.
#[derive(Debug, Clone, Copy)]
pub enum BlockOwner {
    Page(Uuid),
    /// The shared-section library (owned by no page).
    Library,
}

impl BlockOwner {
    fn page_id(self) -> Option<Uuid> {
        match self {
            BlockOwner::Page(id) => Some(id),
            BlockOwner::Library => None,
        }
    }
    fn is_shared(self) -> bool {
        matches!(self, BlockOwner::Library)
    }
}

/// Ids of the live (not deleted) sections of a page, in order.
pub async fn page_block_ids(conn: &mut PgConnection, page_id: Uuid) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar!(
        "SELECT id FROM content_blocks WHERE page_id = $1 AND deleted_at IS NULL ORDER BY sort_order, id",
        page_id
    )
    .fetch_all(conn)
    .await
}

/// Sections by id (deleted ones included), with translations and items, in the order of `ids`.
pub async fn load_blocks(conn: &mut PgConnection, ids: &[Uuid]) -> sqlx::Result<Vec<BlockDraft>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        r#"SELECT id, block_type, name, anchor_id, is_visible, active_period AS "active_period: TsRange",
                  visible_on AS "visible_on: VisibleOn", hidden_locales, appearance, analytics_label,
                  settings, shared_block_id
           FROM content_blocks WHERE id = ANY($1)"#,
        ids
    )
    .fetch_all(&mut *conn)
    .await?;
    let translations = sqlx::query!(
        "SELECT content_block_id, locale, content FROM content_block_translations WHERE content_block_id = ANY($1)",
        ids
    )
    .fetch_all(&mut *conn)
    .await?;
    let items = sqlx::query!(
        r#"SELECT id, block_id, settings, is_visible, active_period AS "active_period: TsRange"
           FROM content_block_items WHERE block_id = ANY($1) ORDER BY block_id, sort_order, id"#,
        ids
    )
    .fetch_all(&mut *conn)
    .await?;
    let item_ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
    let item_translations = sqlx::query!(
        "SELECT item_id, locale, content FROM content_block_item_translations WHERE item_id = ANY($1)",
        &item_ids
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut item_content: HashMap<Uuid, BTreeMap<String, Value>> = HashMap::new();
    for t in item_translations {
        item_content
            .entry(t.item_id)
            .or_default()
            .insert(t.locale, t.content);
    }
    let mut block_items: HashMap<Uuid, Vec<ItemDraft>> = HashMap::new();
    for i in items {
        block_items.entry(i.block_id).or_default().push(ItemDraft {
            id: i.id,
            settings: i.settings,
            content: item_content.remove(&i.id).unwrap_or_default(),
            is_visible: i.is_visible,
            active_period: i.active_period,
        });
    }
    let mut content: HashMap<Uuid, BTreeMap<String, Value>> = HashMap::new();
    for t in translations {
        content
            .entry(t.content_block_id)
            .or_default()
            .insert(t.locale, t.content);
    }
    let mut by_id: HashMap<Uuid, BlockDraft> = rows
        .into_iter()
        .map(|r| {
            (
                r.id,
                BlockDraft {
                    id: r.id,
                    block_type: r.block_type,
                    name: r.name,
                    anchor_id: r.anchor_id,
                    is_visible: r.is_visible,
                    active_period: r.active_period,
                    visible_on: r.visible_on,
                    hidden_locales: r.hidden_locales,
                    appearance: r.appearance,
                    analytics_label: r.analytics_label,
                    settings: r.settings,
                    content: content.remove(&r.id).unwrap_or_default(),
                    items: block_items.remove(&r.id).unwrap_or_default(),
                    shared_block_id: r.shared_block_id,
                    shared: None,
                },
            )
        })
        .collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Attaches the (read-only) shared section to every placement.
pub async fn expand_shared(conn: &mut PgConnection, blocks: &mut [BlockDraft]) -> sqlx::Result<()> {
    let ids: Vec<Uuid> = blocks.iter().filter_map(|b| b.shared_block_id).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let shared = load_blocks(conn, &ids).await?;
    let by_id: HashMap<Uuid, BlockDraft> = shared.into_iter().map(|b| (b.id, b)).collect();
    for b in blocks.iter_mut() {
        if let Some(id) = b.shared_block_id {
            b.shared = by_id.get(&id).cloned().map(Box::new);
        }
    }
    Ok(())
}

/// Shared sections that exist and are not deleted: id → type.
pub async fn shared_types(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> sqlx::Result<HashMap<Uuid, String>> {
    Ok(sqlx::query!(
        "SELECT id, block_type FROM content_blocks WHERE id = ANY($1) AND is_shared AND deleted_at IS NULL",
        ids
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|r| (r.id, r.block_type))
    .collect())
}

/// Ids among `block_ids` / `item_ids` that already belong to another owner (a page cannot take
/// over the sections of another page by sending their ids).
pub async fn foreign_ids(
    conn: &mut PgConnection,
    owner: BlockOwner,
    block_ids: &[Uuid],
    items: &[(Uuid, Uuid)],
) -> sqlx::Result<Vec<Uuid>> {
    let mut foreign = sqlx::query_scalar!(
        "SELECT id FROM content_blocks
         WHERE id = ANY($1) AND (page_id IS DISTINCT FROM $2 OR is_shared <> $3 OR product_id IS NOT NULL
                                 OR place_id IS NOT NULL)",
        block_ids,
        owner.page_id(),
        owner.is_shared()
    )
    .fetch_all(&mut *conn)
    .await?;
    let (item_ids, parents): (Vec<Uuid>, Vec<Uuid>) = items.iter().copied().unzip();
    foreign.extend(
        sqlx::query_scalar!(
            "SELECT i.id FROM content_block_items i
             JOIN unnest($1::uuid[], $2::uuid[]) AS x (item_id, block_id) ON x.item_id = i.id
             WHERE i.block_id <> x.block_id",
            &item_ids,
            &parents
        )
        .fetch_all(&mut *conn)
        .await?,
    );
    Ok(foreign)
}

/// Inserts or updates one section (and revives it if it was soft-deleted).
pub async fn upsert_block(
    conn: &mut PgConnection,
    owner: BlockOwner,
    block: &BlockDraft,
    sort_order: i32,
    user: Option<Uuid>,
) -> sqlx::Result<()> {
    let placement = block.shared_block_id.is_some();
    let empty = Value::Object(Default::default());
    sqlx::query!(
        "INSERT INTO content_blocks (id, page_id, is_shared, block_type, name, anchor_id, is_visible, active_period,
                                     visible_on, hidden_locales, appearance, analytics_label, settings,
                                     shared_block_id, sort_order, created_by, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $16)
         ON CONFLICT (id) DO UPDATE SET
             block_type = EXCLUDED.block_type, name = EXCLUDED.name, anchor_id = EXCLUDED.anchor_id,
             is_visible = EXCLUDED.is_visible, active_period = EXCLUDED.active_period,
             visible_on = EXCLUDED.visible_on, hidden_locales = EXCLUDED.hidden_locales,
             appearance = EXCLUDED.appearance, analytics_label = EXCLUDED.analytics_label,
             settings = EXCLUDED.settings, shared_block_id = EXCLUDED.shared_block_id,
             sort_order = EXCLUDED.sort_order, deleted_at = NULL, updated_by = EXCLUDED.updated_by
         WHERE (content_blocks.block_type, content_blocks.name, content_blocks.anchor_id, content_blocks.is_visible,
                content_blocks.active_period, content_blocks.visible_on, content_blocks.hidden_locales,
                content_blocks.appearance, content_blocks.analytics_label, content_blocks.settings,
                content_blocks.shared_block_id, content_blocks.sort_order, content_blocks.deleted_at)
               IS DISTINCT FROM
               (EXCLUDED.block_type, EXCLUDED.name, EXCLUDED.anchor_id, EXCLUDED.is_visible, EXCLUDED.active_period,
                EXCLUDED.visible_on, EXCLUDED.hidden_locales, EXCLUDED.appearance, EXCLUDED.analytics_label,
                EXCLUDED.settings, EXCLUDED.shared_block_id, EXCLUDED.sort_order, NULL::timestamptz)",
        block.id,
        owner.page_id(),
        owner.is_shared(),
        block.block_type,
        block.name.as_deref().filter(|n| !n.trim().is_empty()),
        block.anchor_id.as_deref().filter(|a| !a.is_empty()),
        block.is_visible,
        block.active_period.filter(|p| p.start.is_some() || p.end.is_some()) as Option<TsRange>,
        block.visible_on.as_str(),
        &block.hidden_locales,
        if placement { &empty } else { &block.appearance },
        block.analytics_label.as_deref().filter(|l| !l.trim().is_empty()),
        if placement { &empty } else { &block.settings },
        block.shared_block_id,
        sort_order,
        user,
    )
    .execute(&mut *conn)
    .await?;

    let content = if placement {
        &BTreeMap::new()
    } else {
        &block.content
    };
    write_block_translations(conn, block.id, content).await?;
    let items: &[ItemDraft] = if placement { &[] } else { &block.items };
    let keep: Vec<Uuid> = items.iter().map(|i| i.id).collect();
    sqlx::query!(
        "DELETE FROM content_block_items WHERE block_id = $1 AND id <> ALL($2)",
        block.id,
        &keep
    )
    .execute(&mut *conn)
    .await?;
    for (j, item) in items.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO content_block_items (id, block_id, settings, sort_order, is_visible, active_period)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET settings = EXCLUDED.settings, sort_order = EXCLUDED.sort_order,
                 is_visible = EXCLUDED.is_visible, active_period = EXCLUDED.active_period
             WHERE (content_block_items.settings, content_block_items.sort_order, content_block_items.is_visible,
                    content_block_items.active_period)
                   IS DISTINCT FROM
                   (EXCLUDED.settings, EXCLUDED.sort_order, EXCLUDED.is_visible, EXCLUDED.active_period)",
            item.id,
            block.id,
            item.settings,
            i32::try_from(j).unwrap_or(i32::MAX),
            item.is_visible,
            item.active_period.filter(|p| p.start.is_some() || p.end.is_some()) as Option<TsRange>,
        )
        .execute(&mut *conn)
        .await?;
        let locales: Vec<String> = item.content.keys().cloned().collect();
        sqlx::query!(
            "DELETE FROM content_block_item_translations WHERE item_id = $1 AND locale <> ALL($2)",
            item.id,
            &locales
        )
        .execute(&mut *conn)
        .await?;
        for (locale, value) in &item.content {
            sqlx::query!(
                "INSERT INTO content_block_item_translations (item_id, locale, content) VALUES ($1, $2, $3)
                 ON CONFLICT (item_id, locale) DO UPDATE SET content = EXCLUDED.content
                 WHERE content_block_item_translations.content IS DISTINCT FROM EXCLUDED.content",
                item.id,
                locale,
                value
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn write_block_translations(
    conn: &mut PgConnection,
    block_id: Uuid,
    content: &BTreeMap<String, Value>,
) -> sqlx::Result<()> {
    let locales: Vec<String> = content.keys().cloned().collect();
    sqlx::query!(
        "DELETE FROM content_block_translations WHERE content_block_id = $1 AND locale <> ALL($2)",
        block_id,
        &locales
    )
    .execute(&mut *conn)
    .await?;
    for (locale, value) in content {
        sqlx::query!(
            "INSERT INTO content_block_translations (content_block_id, locale, content) VALUES ($1, $2, $3)
             ON CONFLICT (content_block_id, locale) DO UPDATE SET content = EXCLUDED.content
             WHERE content_block_translations.content IS DISTINCT FROM EXCLUDED.content",
            block_id,
            locale,
            value
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Soft-deletes the sections of `owner` that are not in `keep`.
pub async fn soft_delete_others(
    conn: &mut PgConnection,
    page_id: Uuid,
    keep: &[Uuid],
    user: Option<Uuid>,
) -> sqlx::Result<u64> {
    Ok(sqlx::query!(
        "UPDATE content_blocks SET deleted_at = now(), updated_by = $3
         WHERE page_id = $1 AND deleted_at IS NULL AND id <> ALL($2)",
        page_id,
        keep,
        user
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Replaces the reference rows of one section.
pub async fn write_refs(
    conn: &mut PgConnection,
    block_id: Uuid,
    refs: &[(Option<Uuid>, RefEntry)],
) -> sqlx::Result<()> {
    sqlx::query!(
        "DELETE FROM content_block_refs WHERE block_id = $1",
        block_id
    )
    .execute(&mut *conn)
    .await?;
    if refs.is_empty() {
        return Ok(());
    }
    let items: Vec<Option<Uuid>> = refs.iter().map(|(i, _)| *i).collect();
    let types: Vec<String> = refs
        .iter()
        .map(|(_, r)| r.ref_type.as_str().to_string())
        .collect();
    let ids: Vec<Uuid> = refs.iter().map(|(_, r)| r.ref_id).collect();
    sqlx::query!(
        "INSERT INTO content_block_refs (block_id, item_id, ref_type, ref_id)
         SELECT $1, x.item_id, x.ref_type, x.ref_id
         FROM unnest($2::uuid[], $3::text[], $4::uuid[]) AS x (item_id, ref_type, ref_id)
         ON CONFLICT DO NOTHING",
        block_id,
        &items as &[Option<Uuid>],
        &types,
        &ids
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Page texts per locale.
pub async fn page_texts(
    conn: &mut PgConnection,
    page_id: Uuid,
) -> sqlx::Result<BTreeMap<String, PageText>> {
    Ok(sqlx::query!(
        "SELECT locale, title, slug, excerpt, seo_title, seo_description, og_media_id, noindex
         FROM page_translations WHERE page_id = $1",
        page_id
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.locale,
            PageText {
                title: r.title,
                slug: r.slug,
                excerpt: r.excerpt,
                seo_title: r.seo_title,
                seo_description: r.seo_description,
                og_media_id: r.og_media_id,
                noindex: r.noindex,
            },
        )
    })
    .collect())
}

pub async fn write_page_texts(
    conn: &mut PgConnection,
    page_id: Uuid,
    texts: &BTreeMap<String, PageText>,
) -> sqlx::Result<()> {
    for (locale, t) in texts {
        sqlx::query!(
            "INSERT INTO page_translations (page_id, locale, title, slug, excerpt, seo_title, seo_description,
                                            og_media_id, noindex)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (page_id, locale) DO UPDATE SET title = EXCLUDED.title, slug = EXCLUDED.slug,
                 excerpt = EXCLUDED.excerpt, seo_title = EXCLUDED.seo_title,
                 seo_description = EXCLUDED.seo_description, og_media_id = EXCLUDED.og_media_id,
                 noindex = EXCLUDED.noindex
             WHERE (page_translations.title, page_translations.slug, page_translations.excerpt,
                    page_translations.seo_title, page_translations.seo_description, page_translations.og_media_id,
                    page_translations.noindex)
                   IS DISTINCT FROM
                   (EXCLUDED.title, EXCLUDED.slug, EXCLUDED.excerpt, EXCLUDED.seo_title, EXCLUDED.seo_description,
                    EXCLUDED.og_media_id, EXCLUDED.noindex)",
            page_id,
            locale,
            t.title.trim(),
            t.slug.trim(),
            t.excerpt.as_deref(),
            t.seo_title.as_deref(),
            t.seo_description.as_deref(),
            t.og_media_id,
            t.noindex
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The last builder save of a page (from the audit log): who and when.
pub async fn last_save(
    conn: &mut PgConnection,
    page_id: Uuid,
) -> sqlx::Result<Option<(Uuid, String, DateTime<Utc>)>> {
    Ok(sqlx::query!(
        r#"SELECT u.id AS "id!", COALESCE(NULLIF(u.full_name, ''), u.email::text) AS "name!", a.created_at
           FROM audit_logs a JOIN users u ON u.id = a.actor_user_id
           WHERE a.entity_type = 'page' AND a.entity_id = $1 AND a.action LIKE 'page.draft_%'
           ORDER BY a.id DESC LIMIT 1"#,
        page_id.to_string()
    )
    .fetch_optional(conn)
    .await?
    .map(|r| (r.id, r.name, r.created_at)))
}
