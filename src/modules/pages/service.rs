//! Page builder rules: draft saves (optimistic locking), small section operations, shared
//! sections, publishing (now or scheduled), revisions, restore and preview tokens.
//!
//! Every write takes the page row lock first (`SELECT … FOR UPDATE`), so saves, publishes and the
//! scheduled-publishing job of one page are serialised and revision numbers stay gap-free.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    model::{
        BlockDraft, BuilderTree, Editor, FieldError, ItemDraft, PageHead, PageText, RefEntry,
        RevisionStatus, SaveTree, Snapshot, SnapshotPage,
    },
    registry::SectionRegistry,
    repo::{self, BlockOwner},
    validate::{self, Context, Owner},
};
use crate::{
    auth::Staff,
    db::types::TsRange,
    error::{AppError, AppResult},
    infra::cache::Cache,
    services::{audit, audit::Actor, outbox, outbox::Event},
};

pub const SNAPSHOT_FORMAT: i32 = 1;
const PREVIEW_TTL_SECS: u64 = 30 * 60;

#[derive(Debug, Clone)]
pub struct PageRow {
    pub id: Uuid,
    pub page_type: String,
    pub status: String,
    pub cover_media_id: Option<Uuid>,
    pub draft_version: i64,
    pub has_unpublished_changes: bool,
}

async fn lock_page(conn: &mut PgConnection, id: Uuid) -> AppResult<PageRow> {
    sqlx::query_as!(
        PageRow,
        "SELECT id, page_type, status, cover_media_id, draft_version, has_unpublished_changes
         FROM pages WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

async fn read_page(conn: &mut PgConnection, id: Uuid) -> AppResult<PageRow> {
    sqlx::query_as!(
        PageRow,
        "SELECT id, page_type, status, cover_media_id, draft_version, has_unpublished_changes
         FROM pages WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

/// `(default locale, active locales)`.
pub async fn locales(conn: &mut PgConnection) -> sqlx::Result<(String, Vec<String>)> {
    let all = crate::db::foundation::active_locales(&mut *conn).await?;
    let default = all
        .iter()
        .find(|l| l.is_default)
        .map(|l| l.code.as_str().to_string())
        .unwrap_or_else(|| "fr".into());
    Ok((default, all.into_iter().map(|l| l.code.0).collect()))
}

fn invalid(errors: Vec<FieldError>) -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_sections",
        "some sections are not valid",
    )
    .with_details(json!({ "errors": errors }))
}

// ---- Builder tree -----------------------------------------------------------------------------

pub async fn builder_tree(conn: &mut PgConnection, page_id: Uuid) -> AppResult<BuilderTree> {
    let page = read_page(conn, page_id).await?;
    tree_of(conn, &page).await
}

async fn tree_of(conn: &mut PgConnection, page: &PageRow) -> AppResult<BuilderTree> {
    let ids = repo::page_block_ids(conn, page.id).await?;
    let mut blocks = repo::load_blocks(conn, &ids).await?;
    repo::expand_shared(conn, &mut blocks).await?;
    let translations = repo::page_texts(conn, page.id).await?;
    let published_revision = sqlx::query_scalar!(
        "SELECT revision_number FROM page_revisions WHERE page_id = $1 AND status = 'published'",
        page.id
    )
    .fetch_optional(&mut *conn)
    .await?;
    let last = repo::last_save(conn, page.id).await?;
    Ok(BuilderTree {
        page: PageHead {
            id: page.id,
            page_type: page.page_type.clone(),
            status: page.status.clone(),
            cover_media_id: page.cover_media_id,
            translations,
        },
        draft_version: page.draft_version,
        has_unpublished_changes: page.has_unpublished_changes,
        published_revision,
        blocks,
        last_saved_by: last.as_ref().map(|(id, name, _)| Editor {
            id: *id,
            name: name.clone(),
        }),
        last_saved_at: last.map(|(_, _, at)| at),
    })
}

/// How much checking a write gets.
enum Mode<'a> {
    /// An editor's change: sanitised, fully validated, permissions of touched sections checked.
    Edit(&'a Staff),
    /// Restoring a revision: written as it was (it was valid when published).
    Restore,
}

/// Writes `blocks` as the complete draft of the page (insert/update/soft-delete, contiguous
/// sort order, refs rebuilt), then bumps `draft_version`.
async fn apply(
    conn: &mut PgConnection,
    reg: &SectionRegistry,
    page: &PageRow,
    mut blocks: Vec<BlockDraft>,
    texts: Option<BTreeMap<String, PageText>>,
    mode: Mode<'_>,
) -> AppResult<i64> {
    let user = match mode {
        Mode::Edit(staff) => Some(staff.id),
        Mode::Restore => None,
    };
    let (default_locale, locales) = locales(conn).await?;
    let before_ids = repo::page_block_ids(conn, page.id).await?;
    let before: HashMap<Uuid, BlockDraft> = repo::load_blocks(conn, &before_ids)
        .await?
        .into_iter()
        .map(|b| (b.id, b))
        .collect();
    let shared_ids: Vec<Uuid> = blocks.iter().filter_map(|b| b.shared_block_id).collect();
    let shared = repo::shared_types(conn, &shared_ids).await?;
    // A placement always carries the type of its shared section.
    for b in blocks.iter_mut() {
        if let Some(t) = b.shared_block_id.and_then(|id| shared.get(&id)) {
            b.block_type = t.clone();
        }
    }

    let mut errors = Vec::new();
    if let Mode::Edit(staff) = mode {
        for (i, b) in blocks.iter_mut().enumerate() {
            if b.shared_block_id.is_none()
                && let Some(t) = reg.get(&b.block_type)
            {
                validate::sanitize_block(t, b, &format!("/blocks/{i}"), &mut errors);
            }
        }
        let touched: HashSet<Uuid> = blocks
            .iter()
            .filter(|b| before.get(&b.id).is_none_or(|old| changed(old, b)))
            .map(|b| b.id)
            .collect();
        let ctx = Context {
            default_locale: &default_locale,
            locales: &locales,
            permissions: &staff.permissions,
            owner: Owner::Page {
                page_type: &page.page_type,
            },
            touched: &touched,
            shared: &shared,
        };
        errors.extend(validate::validate_blocks(reg, &blocks, &ctx, "/blocks"));
        let kept: HashSet<Uuid> = blocks.iter().map(|b| b.id).collect();
        for old in before.values().filter(|b| !kept.contains(&b.id)) {
            if let Some(permission) = reg
                .get(&old.block_type)
                .and_then(|t| t.required_permission.as_ref())
                && !staff.can(permission)
            {
                errors.push(FieldError::new(
                    "/blocks",
                    "forbidden_section",
                    format!(
                        "you are not allowed to delete `{}` sections",
                        old.block_type
                    ),
                ));
            }
        }
        if let Some(texts) = &texts {
            for (locale, t) in texts {
                if !locales.contains(locale) {
                    errors.push(FieldError::new(
                        format!("/translations/{locale}"),
                        "unknown_locale",
                        "not an active language",
                    ));
                }
                if t.title.trim().is_empty() {
                    errors.push(FieldError::new(
                        format!("/translations/{locale}/title"),
                        "required",
                        "a title is required",
                    ));
                }
                let slug = t.slug.trim();
                if slug.is_empty()
                    || slug.starts_with('/')
                    || slug.ends_with('/')
                    || slug.contains("//")
                    || slug
                        .chars()
                        .any(|c| c.is_whitespace() || c == '?' || c == '#')
                {
                    errors.push(FieldError::new(
                        format!("/translations/{locale}/slug"),
                        "pattern",
                        "letters, digits, dashes and /",
                    ));
                }
            }
        }
    }
    let block_ids: Vec<Uuid> = blocks.iter().map(|b| b.id).collect();
    let item_pairs: Vec<(Uuid, Uuid)> = blocks
        .iter()
        .flat_map(|b| b.items.iter().map(move |i| (i.id, b.id)))
        .collect();
    let foreign: HashSet<Uuid> =
        repo::foreign_ids(conn, BlockOwner::Page(page.id), &block_ids, &item_pairs)
            .await?
            .into_iter()
            .collect();
    for (i, b) in blocks.iter().enumerate() {
        if foreign.contains(&b.id) {
            errors.push(FieldError::new(
                format!("/blocks/{i}/id"),
                "foreign_id",
                "this id belongs to another section",
            ));
        }
        for (j, item) in b.items.iter().enumerate() {
            if foreign.contains(&item.id) {
                errors.push(FieldError::new(
                    format!("/blocks/{i}/items/{j}/id"),
                    "foreign_id",
                    "this id belongs to another item",
                ));
            }
        }
    }
    if !errors.is_empty() {
        return Err(invalid(errors));
    }

    // Free anchors of sections that are about to move or disappear before reusing them.
    sqlx::query!(
        "UPDATE content_blocks SET anchor_id = NULL WHERE page_id = $1 AND deleted_at IS NULL AND anchor_id IS NOT NULL",
        page.id
    )
    .execute(&mut *conn)
    .await?;
    for (i, b) in blocks.iter().enumerate() {
        repo::upsert_block(
            conn,
            BlockOwner::Page(page.id),
            b,
            i32::try_from(i).unwrap_or(i32::MAX),
            user,
        )
        .await?;
        let refs = match (b.shared_block_id, reg.get(&b.block_type)) {
            (None, Some(t)) => validate::block_refs(t, b),
            _ => Vec::new(),
        };
        repo::write_refs(conn, b.id, &refs).await?;
    }
    repo::soft_delete_others(conn, page.id, &block_ids, user).await?;
    if let Some(texts) = &texts {
        repo::write_page_texts(conn, page.id, texts).await?;
    }
    let version = sqlx::query_scalar!(
        "UPDATE pages SET draft_version = draft_version + 1, has_unpublished_changes = true
         WHERE id = $1 RETURNING draft_version",
        page.id
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(version)
}

/// Content-level change (the permission of a section type is checked only for those).
fn changed(old: &BlockDraft, new: &BlockDraft) -> bool {
    old.block_type != new.block_type
        || old.settings != new.settings
        || old.content != new.content
        || old.items != new.items
        || old.appearance != new.appearance
        || old.shared_block_id != new.shared_block_id
}

fn conflict(page: &PageRow, last: Option<(Uuid, String, DateTime<Utc>)>) -> AppError {
    AppError::domain(
        StatusCode::CONFLICT,
        "draft_conflict",
        "the page was changed by someone else since you loaded it",
    )
    .with_details(json!({
        "current_version": page.draft_version,
        "saved_by": last.as_ref().map(|(id, name, _)| json!({"id": id, "name": name})),
        "saved_at": last.map(|(_, _, at)| at),
    }))
}

/// `PUT /pages/{id}/builder` (If-Match: the draft version the client edited).
pub async fn save_tree(
    db: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    actor: &Actor,
    page_id: Uuid,
    if_match: i64,
    input: SaveTree,
) -> AppResult<BuilderTree> {
    let mut tx = db.begin().await?;
    let page = lock_page(&mut tx, page_id).await?;
    if page.draft_version != if_match {
        let last = repo::last_save(&mut tx, page_id).await?;
        return Err(conflict(&page, last));
    }
    let count = input.blocks.len();
    let version = apply(
        &mut tx,
        reg,
        &page,
        input.blocks,
        input.translations,
        Mode::Edit(staff),
    )
    .await?;
    audit::record(
        &mut tx,
        actor,
        "page.draft_saved",
        "page",
        &page_id.to_string(),
        json!({"version": version, "sections": count}),
    )
    .await?;
    let tree = builder_tree(&mut tx, page_id).await?;
    tx.commit().await?;
    Ok(tree)
}

/// Runs a small operation on the current draft (under the page lock) and saves the result.
#[allow(clippy::too_many_arguments)] // the shared plumbing of the five small operations
async fn edit_draft(
    db: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    actor: &Actor,
    page_id: Uuid,
    if_match: Option<i64>,
    action: &str,
    change: impl FnOnce(&mut Vec<BlockDraft>) -> AppResult<Value>,
) -> AppResult<BuilderTree> {
    let mut tx = db.begin().await?;
    let page = lock_page(&mut tx, page_id).await?;
    if let Some(v) = if_match
        && v != page.draft_version
    {
        let last = repo::last_save(&mut tx, page_id).await?;
        return Err(conflict(&page, last));
    }
    let ids = repo::page_block_ids(&mut tx, page_id).await?;
    let mut blocks = repo::load_blocks(&mut tx, &ids).await?;
    let changes = change(&mut blocks)?;
    apply(&mut tx, reg, &page, blocks, None, Mode::Edit(staff)).await?;
    audit::record(
        &mut tx,
        actor,
        action,
        "page",
        &page_id.to_string(),
        changes,
    )
    .await?;
    let tree = builder_tree(&mut tx, page_id).await?;
    tx.commit().await?;
    Ok(tree)
}

/// A new section of `code` built from the type's preset.
pub fn from_preset(reg: &SectionRegistry, code: &str) -> Option<BlockDraft> {
    let t = reg.get(code)?;
    let preset = &t.preset;
    let map = |v: &Value| -> BTreeMap<String, Value> {
        v.as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    };
    Some(BlockDraft {
        id: Uuid::now_v7(),
        block_type: code.to_string(),
        name: None,
        anchor_id: None,
        is_visible: true,
        active_period: None,
        visible_on: Default::default(),
        hidden_locales: Vec::new(),
        appearance: preset
            .get("appearance")
            .cloned()
            .unwrap_or_else(|| json!({})),
        analytics_label: None,
        settings: preset.get("settings").cloned().unwrap_or_else(|| json!({})),
        content: map(&preset["content"]),
        items: preset["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|i| ItemDraft {
                        id: Uuid::now_v7(),
                        settings: i.get("settings").cloned().unwrap_or_else(|| json!({})),
                        content: map(&i["content"]),
                        is_visible: true,
                        active_period: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        shared_block_id: None,
        shared: None,
    })
}

#[derive(Debug, Deserialize)]
pub struct AddBlock {
    /// Section type (ignored for a shared placement).
    #[serde(rename = "type")]
    pub block_type: Option<String>,
    pub shared_block_id: Option<Uuid>,
    /// Index in the list (default: at the end).
    pub position: Option<usize>,
}

pub async fn add_block(
    db: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    actor: &Actor,
    page_id: Uuid,
    if_match: Option<i64>,
    input: AddBlock,
) -> AppResult<BuilderTree> {
    let active = match &input.block_type {
        Some(code) if input.shared_block_id.is_none() => {
            sqlx::query_scalar!("SELECT is_active FROM section_types WHERE code = $1", code)
                .fetch_optional(db)
                .await?
        }
        _ => Some(true),
    };
    if active != Some(true) {
        return Err(invalid(vec![FieldError::new(
            "/type",
            "unknown_type",
            "this section type is not available",
        )]));
    }
    let block = match (input.shared_block_id, input.block_type.as_deref()) {
        (Some(shared), _) => BlockDraft {
            shared_block_id: Some(shared),
            block_type: String::new(),
            ..from_preset(reg, "spacer").expect("spacer type")
        },
        (None, Some(code)) => from_preset(reg, code).ok_or_else(|| {
            invalid(vec![FieldError::new(
                "/type",
                "unknown_type",
                "unknown section type",
            )])
        })?,
        (None, None) => {
            return Err(AppError::BadRequest(
                "type or shared_block_id is required".into(),
            ));
        }
    };
    let block = BlockDraft {
        settings: if block.shared_block_id.is_some() {
            json!({})
        } else {
            block.settings
        },
        content: if block.shared_block_id.is_some() {
            BTreeMap::new()
        } else {
            block.content
        },
        ..block
    };
    let id = block.id;
    edit_draft(
        db,
        reg,
        staff,
        actor,
        page_id,
        if_match,
        "page.draft_section_added",
        move |blocks| {
            let at = input.position.unwrap_or(blocks.len()).min(blocks.len());
            let code = block.block_type.clone();
            blocks.insert(at, block);
            Ok(json!({"block_id": id, "type": code, "position": at}))
        },
    )
    .await
}

async fn page_of_block(db: &PgPool, block_id: Uuid) -> AppResult<Uuid> {
    sqlx::query_scalar!(
        r#"SELECT page_id AS "page_id!" FROM content_blocks WHERE id = $1 AND page_id IS NOT NULL AND deleted_at IS NULL"#,
        block_id
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn duplicate_block(
    db: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    actor: &Actor,
    block_id: Uuid,
    if_match: Option<i64>,
) -> AppResult<BuilderTree> {
    let page_id = page_of_block(db, block_id).await?;
    edit_draft(
        db,
        reg,
        staff,
        actor,
        page_id,
        if_match,
        "page.draft_section_duplicated",
        |blocks| {
            let at = blocks
                .iter()
                .position(|b| b.id == block_id)
                .ok_or(AppError::NotFound)?;
            let mut copy = blocks[at].clone();
            copy.id = Uuid::now_v7();
            copy.anchor_id = None;
            copy.name = copy.name.map(|n| format!("{n} (copy)"));
            for item in &mut copy.items {
                item.id = Uuid::now_v7();
            }
            let new_id = copy.id;
            blocks.insert(at + 1, copy);
            Ok(json!({"block_id": block_id, "copy_id": new_id}))
        },
    )
    .await
}

/// `PATCH /blocks/{id}`: visibility and schedule only.
#[derive(Debug, Deserialize)]
pub struct PatchBlock {
    pub is_visible: Option<bool>,
    /// `null` clears the schedule.
    #[serde(default, with = "double_option")]
    pub active_period: Option<Option<TsRange>>,
    pub visible_on: Option<super::model::VisibleOn>,
    pub hidden_locales: Option<Vec<String>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

pub async fn patch_block(
    db: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    actor: &Actor,
    block_id: Uuid,
    if_match: Option<i64>,
    patch: PatchBlock,
) -> AppResult<BuilderTree> {
    let page_id = page_of_block(db, block_id).await?;
    edit_draft(db, reg, staff, actor, page_id, if_match, "page.draft_section_updated", move |blocks| {
        let b = blocks.iter_mut().find(|b| b.id == block_id).ok_or(AppError::NotFound)?;
        if let Some(v) = patch.is_visible {
            b.is_visible = v;
        }
        if let Some(p) = patch.active_period {
            b.active_period = p;
        }
        if let Some(v) = patch.visible_on {
            b.visible_on = v;
        }
        if let Some(l) = patch.hidden_locales {
            b.hidden_locales = l;
        }
        Ok(json!({"block_id": block_id, "is_visible": b.is_visible, "active_period": b.active_period,
                  "visible_on": b.visible_on, "hidden_locales": b.hidden_locales}))
    })
    .await
}

pub async fn delete_block(
    db: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    actor: &Actor,
    block_id: Uuid,
    if_match: Option<i64>,
) -> AppResult<BuilderTree> {
    let page_id = page_of_block(db, block_id).await?;
    edit_draft(
        db,
        reg,
        staff,
        actor,
        page_id,
        if_match,
        "page.draft_section_deleted",
        |blocks| {
            let at = blocks
                .iter()
                .position(|b| b.id == block_id)
                .ok_or(AppError::NotFound)?;
            let removed = blocks.remove(at);
            Ok(json!({"block_id": block_id, "type": removed.block_type}))
        },
    )
    .await
}

// ---- Snapshots and publishing ---------------------------------------------------------------

/// Replaces every placement by the content of its shared section (the placement keeps its own
/// id, visibility, schedule, devices, languages and anchor).
pub fn inline_shared(blocks: Vec<BlockDraft>) -> Vec<BlockDraft> {
    blocks
        .into_iter()
        .filter_map(|b| match (b.shared_block_id, b.shared.clone()) {
            (None, _) => Some(BlockDraft { shared: None, ..b }),
            (Some(_), None) => None, // the shared section is gone: nothing to show
            (Some(shared_id), Some(s)) => Some(BlockDraft {
                id: b.id,
                block_type: s.block_type,
                name: b.name.or(s.name),
                anchor_id: b.anchor_id,
                is_visible: b.is_visible,
                active_period: b.active_period,
                visible_on: b.visible_on,
                hidden_locales: b.hidden_locales,
                appearance: s.appearance,
                analytics_label: b.analytics_label.or(s.analytics_label),
                settings: s.settings,
                content: s.content,
                items: s.items,
                shared_block_id: Some(shared_id),
                shared: None,
            }),
        })
        .collect()
}

pub fn snapshot_of(
    reg: &SectionRegistry,
    page: &PageRow,
    texts: BTreeMap<String, PageText>,
    blocks: Vec<BlockDraft>,
) -> Snapshot {
    let blocks = inline_shared(blocks);
    let mut refs: BTreeSet<RefEntry> = BTreeSet::new();
    let mut rule_sources: BTreeSet<String> = BTreeSet::new();
    for b in &blocks {
        if let Some(t) = reg.get(&b.block_type) {
            refs.extend(validate::block_refs(t, b).into_iter().map(|(_, r)| r));
        }
        rule_sources.extend(validate::rule_sources(b).into_iter().map(str::to_string));
    }
    for media in texts
        .values()
        .filter_map(|t| t.og_media_id)
        .chain(page.cover_media_id)
    {
        refs.insert(RefEntry {
            ref_type: super::model::RefType::Media,
            ref_id: media,
        });
    }
    Snapshot {
        format: SNAPSHOT_FORMAT,
        page: SnapshotPage {
            id: page.id,
            page_type: page.page_type.clone(),
            cover_media_id: page.cover_media_id,
            translations: texts,
        },
        blocks,
        refs: refs.into_iter().collect(),
        rule_sources: rule_sources.into_iter().collect(),
    }
}

/// The current draft as a snapshot, plus its validation errors (shared sections inlined).
async fn draft_snapshot(
    conn: &mut PgConnection,
    reg: &SectionRegistry,
    page: &PageRow,
) -> AppResult<(Snapshot, Vec<FieldError>)> {
    let ids = repo::page_block_ids(conn, page.id).await?;
    let mut blocks = repo::load_blocks(conn, &ids).await?;
    let shared_ids: Vec<Uuid> = blocks.iter().filter_map(|b| b.shared_block_id).collect();
    let shared = repo::shared_types(conn, &shared_ids).await?;
    repo::expand_shared(conn, &mut blocks).await?;
    let (default_locale, locales) = locales(conn).await?;
    let no_permissions = HashSet::new();
    let untouched = HashSet::new();
    let ctx = Context {
        default_locale: &default_locale,
        locales: &locales,
        permissions: &no_permissions,
        owner: Owner::Page {
            page_type: &page.page_type,
        },
        touched: &untouched,
        shared: &shared,
    };
    let mut errors = validate::validate_blocks(reg, &blocks, &ctx, "/blocks");
    let texts = repo::page_texts(conn, page.id).await?;
    if !texts.contains_key(&default_locale) {
        errors.push(FieldError::new(
            format!("/translations/{default_locale}"),
            "required",
            "the page needs a title and a slug in the default language",
        ));
    }
    Ok((snapshot_of(reg, page, texts, blocks), errors))
}

pub async fn draft_preview_snapshot(
    conn: &mut PgConnection,
    reg: &SectionRegistry,
    page_id: Uuid,
) -> AppResult<Snapshot> {
    let page = read_page(conn, page_id).await?;
    Ok(draft_snapshot(conn, reg, &page).await?.0)
}

#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    pub publish_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevisionSummary {
    pub id: Uuid,
    pub revision_number: i32,
    pub status: RevisionStatus,
    pub publish_at: Option<DateTime<Utc>>,
    pub published_at: Option<DateTime<Utc>>,
    pub published_by: Option<Editor>,
    pub note: Option<String>,
    pub created_by: Option<Editor>,
    pub created_at: DateTime<Utc>,
}

pub async fn publish(
    db: &PgPool,
    reg: &SectionRegistry,
    actor: &Actor,
    user: Option<Uuid>,
    page_id: Uuid,
    now: DateTime<Utc>,
    req: PublishRequest,
) -> AppResult<RevisionSummary> {
    let mut tx = db.begin().await?;
    let page = lock_page(&mut tx, page_id).await?;
    let (snapshot, errors) = draft_snapshot(&mut tx, reg, &page).await?;
    if !errors.is_empty() {
        return Err(invalid(errors));
    }
    let number = next_revision_number(&mut tx, page_id).await?;
    let scheduled = req.publish_at.filter(|at| *at > now);
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let id = Uuid::now_v7();
    let snapshot = serde_json::to_value(&snapshot).map_err(anyhow::Error::from)?;
    if let Some(at) = scheduled {
        sqlx::query!(
            "INSERT INTO page_revisions (id, page_id, revision_number, status, snapshot, publish_at, note, created_by)
             VALUES ($1, $2, $3, 'scheduled', $4, $5, $6, $7)",
            id, page_id, number, snapshot, at, note, user
        )
        .execute(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            actor,
            "page.publish_scheduled",
            "page",
            &page_id.to_string(),
            json!({"revision": number, "publish_at": at}),
        )
        .await?;
    } else {
        go_live(&mut tx, page_id, now).await?;
        sqlx::query!(
            "INSERT INTO page_revisions (id, page_id, revision_number, status, snapshot, published_at, published_by,
                                         note, created_by)
             VALUES ($1, $2, $3, 'published', $4, $5, $6, $7, $6)",
            id, page_id, number, snapshot, now, user, note
        )
        .execute(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            actor,
            "page.published",
            "page",
            &page_id.to_string(),
            json!({"revision": number}),
        )
        .await?;
    }
    sqlx::query!(
        "UPDATE pages SET has_unpublished_changes = false WHERE id = $1",
        page_id
    )
    .execute(&mut *tx)
    .await?;
    let summary = revision_summary(&mut tx, id).await?;
    tx.commit().await?;
    Ok(summary)
}

/// Gap-free per page: the counter lives on the page row, locked by the caller.
async fn next_revision_number(conn: &mut PgConnection, page_id: Uuid) -> sqlx::Result<i32> {
    sqlx::query_scalar!(
        "UPDATE pages SET revision_counter = revision_counter + 1 WHERE id = $1 RETURNING revision_counter",
        page_id
    )
    .fetch_one(conn)
    .await
}

/// Supersedes the published revision and marks the page published (caller holds the page lock
/// and inserts or promotes the new published revision right after).
async fn go_live(conn: &mut PgConnection, page_id: Uuid, now: DateTime<Utc>) -> sqlx::Result<()> {
    sqlx::query!("UPDATE page_revisions SET status = 'superseded' WHERE page_id = $1 AND status = 'published'", page_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "UPDATE pages SET status = 'published', publish_at = COALESCE(publish_at, $2) WHERE id = $1",
        page_id,
        now
    )
    .execute(&mut *conn)
    .await?;
    outbox::enqueue(conn, &Event::PageUpdated { page_id }).await?;
    Ok(())
}

/// Scheduled publishing (every minute): promotes due revisions. A page busy with another
/// write is skipped (`SKIP LOCKED`) and picked up on the next run.
pub async fn promote_due(db: &PgPool, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let due = sqlx::query!(
        "SELECT id, page_id FROM page_revisions WHERE status = 'scheduled' AND publish_at <= $1
         ORDER BY publish_at, revision_number LIMIT 50",
        now
    )
    .fetch_all(db)
    .await?;
    let mut promoted = 0;
    for rev in due {
        let mut tx = db.begin().await?;
        let locked = sqlx::query_scalar!(
            "SELECT id FROM pages WHERE id = $1 FOR UPDATE SKIP LOCKED",
            rev.page_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        if locked.is_none() {
            continue;
        }
        let still_due = sqlx::query_scalar!(
            "SELECT revision_number FROM page_revisions WHERE id = $1 AND status = 'scheduled' AND publish_at <= $2
             FOR UPDATE SKIP LOCKED",
            rev.id,
            now
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(number) = still_due else { continue };
        go_live(&mut tx, rev.page_id, now).await?;
        sqlx::query!(
            "UPDATE page_revisions SET status = 'published', published_at = $2, published_by = created_by WHERE id = $1",
            rev.id,
            now
        )
        .execute(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            &Actor::system(),
            "page.published",
            "page",
            &rev.page_id.to_string(),
            json!({"revision": number, "scheduled": true}),
        )
        .await?;
        tx.commit().await?;
        promoted += 1;
    }
    Ok(promoted)
}

async fn revision_summary(conn: &mut PgConnection, id: Uuid) -> AppResult<RevisionSummary> {
    revisions_where(conn, None, Some(id))
        .await?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
}

async fn revisions_where(
    conn: &mut PgConnection,
    page_id: Option<Uuid>,
    id: Option<Uuid>,
) -> AppResult<Vec<RevisionSummary>> {
    let rows = sqlx::query!(
        r#"SELECT r.id, r.revision_number, r.status AS "status: RevisionStatus", r.publish_at, r.published_at, r.note,
                  r.created_at, r.published_by, pu.full_name AS "published_by_name?", r.created_by,
                  cu.full_name AS "created_by_name?"
           FROM page_revisions r
           LEFT JOIN users pu ON pu.id = r.published_by
           LEFT JOIN users cu ON cu.id = r.created_by
           WHERE ($1::uuid IS NULL OR r.page_id = $1) AND ($2::uuid IS NULL OR r.id = $2)
           ORDER BY r.revision_number DESC"#,
        page_id,
        id
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RevisionSummary {
            id: r.id,
            revision_number: r.revision_number,
            status: r.status,
            publish_at: r.publish_at,
            published_at: r.published_at,
            published_by: r.published_by.map(|id| Editor {
                id,
                name: r.published_by_name.unwrap_or_default(),
            }),
            note: r.note,
            created_by: r.created_by.map(|id| Editor {
                id,
                name: r.created_by_name.unwrap_or_default(),
            }),
            created_at: r.created_at,
        })
        .collect())
}

pub async fn revisions(db: &PgPool, page_id: Uuid) -> AppResult<Vec<RevisionSummary>> {
    let mut conn = db.acquire().await?;
    read_page(&mut conn, page_id).await?;
    revisions_where(&mut conn, Some(page_id), None).await
}

#[derive(Debug, Serialize)]
pub struct RevisionDetail {
    #[serde(flatten)]
    pub summary: RevisionSummary,
    pub page_id: Uuid,
    pub snapshot: Value,
}

pub async fn revision(db: &PgPool, id: Uuid) -> AppResult<RevisionDetail> {
    let mut conn = db.acquire().await?;
    let summary = revision_summary(&mut conn, id).await?;
    let row = sqlx::query!(
        "SELECT page_id, snapshot FROM page_revisions WHERE id = $1",
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(RevisionDetail {
        summary,
        page_id: row.page_id,
        snapshot: row.snapshot,
    })
}

pub async fn cancel_revision(db: &PgPool, actor: &Actor, id: Uuid) -> AppResult<RevisionSummary> {
    let mut tx = db.begin().await?;
    let page_id = sqlx::query_scalar!("SELECT page_id FROM page_revisions WHERE id = $1", id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    lock_page(&mut tx, page_id).await?;
    let cancelled = sqlx::query!(
        "UPDATE page_revisions SET status = 'cancelled' WHERE id = $1 AND status = 'scheduled'",
        id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if cancelled == 0 {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "not_scheduled",
            "only a scheduled revision can be cancelled",
        ));
    }
    sqlx::query!(
        "UPDATE pages SET has_unpublished_changes = true WHERE id = $1",
        page_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        "page.publish_cancelled",
        "page",
        &page_id.to_string(),
        json!({"revision_id": id}),
    )
    .await?;
    let summary = revision_summary(&mut tx, id).await?;
    tx.commit().await?;
    Ok(summary)
}

#[derive(Debug, Serialize)]
pub struct Restored {
    pub tree: BuilderTree,
    /// Sections of the restored revision that no longer pass today's rules (to fix before
    /// publishing).
    pub errors: Vec<FieldError>,
}

/// Copies a revision back into the draft tables. Does not publish.
pub async fn restore_revision(
    db: &PgPool,
    reg: &SectionRegistry,
    actor: &Actor,
    revision_id: Uuid,
) -> AppResult<Restored> {
    let mut tx = db.begin().await?;
    let row = sqlx::query!(
        "SELECT page_id, snapshot, revision_number FROM page_revisions WHERE id = $1",
        revision_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let page = lock_page(&mut tx, row.page_id).await?;
    let snapshot: Snapshot = serde_json::from_value(row.snapshot).map_err(anyhow::Error::from)?;
    let shared_ids: Vec<Uuid> = snapshot
        .blocks
        .iter()
        .filter_map(|b| b.shared_block_id)
        .collect();
    let alive = repo::shared_types(&mut tx, &shared_ids).await?;
    let blocks: Vec<BlockDraft> = snapshot
        .blocks
        .into_iter()
        .map(|b| match b.shared_block_id {
            // still in the library: back to a placement
            Some(id) if alive.contains_key(&id) => BlockDraft {
                settings: json!({}),
                content: BTreeMap::new(),
                items: Vec::new(),
                appearance: json!({}),
                ..b
            },
            // gone: keep the content as an ordinary section
            Some(_) => BlockDraft {
                shared_block_id: None,
                ..b
            },
            None => b,
        })
        .collect();
    apply(
        &mut tx,
        reg,
        &page,
        blocks,
        Some(snapshot.page.translations),
        Mode::Restore,
    )
    .await?;
    audit::record(
        &mut tx,
        actor,
        "page.draft_restored",
        "page",
        &page.id.to_string(),
        json!({"revision": row.revision_number}),
    )
    .await?;
    let page = read_page(&mut tx, page.id).await?;
    let (_, errors) = draft_snapshot(&mut tx, reg, &page).await?;
    let tree = tree_of(&mut tx, &page).await?;
    tx.commit().await?;
    Ok(Restored { tree, errors })
}

#[derive(Debug, Serialize)]
pub struct SectionRef {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub block_type: String,
    pub name: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct Diff {
    pub added: Vec<SectionRef>,
    pub removed: Vec<SectionRef>,
    pub changed: Vec<SectionRef>,
    pub moved: bool,
}

#[derive(Debug, Serialize)]
pub struct PublishCheck {
    pub errors: Vec<FieldError>,
    pub published_revision: Option<i32>,
    pub diff: Diff,
}

/// What publishing now would do: validation errors, and the sections added, removed and changed
/// since the published revision.
pub async fn publish_check(
    db: &PgPool,
    reg: &SectionRegistry,
    page_id: Uuid,
) -> AppResult<PublishCheck> {
    let mut conn = db.acquire().await?;
    let page = read_page(&mut conn, page_id).await?;
    let (default_locale, _) = locales(&mut conn).await?;
    let (draft, errors) = draft_snapshot(&mut conn, reg, &page).await?;
    let published = sqlx::query!(
        "SELECT revision_number, snapshot FROM page_revisions WHERE page_id = $1 AND status = 'published'",
        page_id
    )
    .fetch_optional(&mut *conn)
    .await?;
    let (published_revision, before) = match published {
        Some(p) => (
            Some(p.revision_number),
            serde_json::from_value::<Snapshot>(p.snapshot)
                .map(|s| s.blocks)
                .unwrap_or_default(),
        ),
        None => (None, Vec::new()),
    };
    Ok(PublishCheck {
        errors,
        published_revision,
        diff: diff(&before, &draft.blocks, &default_locale),
    })
}

pub fn diff(before: &[BlockDraft], after: &[BlockDraft], locale: &str) -> Diff {
    let describe = |b: &BlockDraft| SectionRef {
        id: b.id,
        block_type: b.block_type.clone(),
        name: b.name.clone(),
        title: b
            .content
            .get(locale)
            .and_then(|c| c.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let old: HashMap<Uuid, &BlockDraft> = before.iter().map(|b| (b.id, b)).collect();
    let new: HashMap<Uuid, &BlockDraft> = after.iter().map(|b| (b.id, b)).collect();
    let mut d = Diff::default();
    for b in after {
        match old.get(&b.id) {
            None => d.added.push(describe(b)),
            Some(o) if *o != b => d.changed.push(describe(b)),
            Some(_) => {}
        }
    }
    d.removed = before
        .iter()
        .filter(|b| !new.contains_key(&b.id))
        .map(describe)
        .collect();
    let order_before: Vec<Uuid> = before
        .iter()
        .map(|b| b.id)
        .filter(|id| new.contains_key(id))
        .collect();
    let order_after: Vec<Uuid> = after
        .iter()
        .map(|b| b.id)
        .filter(|id| old.contains_key(id))
        .collect();
    d.moved = order_before != order_after;
    d
}

// ---- Shared sections ------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SharedSummary {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub block_type: String,
    pub name: Option<String>,
    pub usages: i64,
    pub updated_at: DateTime<Utc>,
}

pub async fn shared_list(db: &PgPool) -> AppResult<Vec<SharedSummary>> {
    Ok(sqlx::query_as!(
        SharedSummary,
        r#"SELECT b.id, b.block_type, b.name, b.updated_at,
                  (SELECT count(*) FROM content_blocks p WHERE p.shared_block_id = b.id AND p.deleted_at IS NULL) AS "usages!"
           FROM content_blocks b WHERE b.is_shared AND b.deleted_at IS NULL ORDER BY b.updated_at DESC"#
    )
    .fetch_all(db)
    .await?)
}

pub async fn shared_get(db: &PgPool, id: Uuid) -> AppResult<BlockDraft> {
    let mut conn = db.acquire().await?;
    shared_types(&mut conn, id).await?;
    repo::load_blocks(&mut conn, &[id])
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}

async fn shared_types(conn: &mut PgConnection, id: Uuid) -> AppResult<String> {
    repo::shared_types(conn, &[id])
        .await?
        .remove(&id)
        .ok_or(AppError::NotFound)
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub page_id: Uuid,
    pub page_title: Option<String>,
    pub block_id: Uuid,
}

pub async fn shared_usages(db: &PgPool, id: Uuid) -> AppResult<Vec<Usage>> {
    Ok(sqlx::query_as!(
        Usage,
        r#"SELECT b.page_id AS "page_id!", b.id AS block_id,
                  (SELECT t.title FROM page_translations t JOIN locales l ON l.code = t.locale
                   WHERE t.page_id = b.page_id ORDER BY l.is_default DESC LIMIT 1) AS page_title
           FROM content_blocks b WHERE b.shared_block_id = $1 AND b.deleted_at IS NULL AND b.page_id IS NOT NULL
           ORDER BY b.page_id"#,
        id
    )
    .fetch_all(db)
    .await?)
}

/// Creates (`existing = None`) or replaces a shared section of the library.
pub async fn shared_save(
    db: &PgPool,
    reg: &SectionRegistry,
    staff: &Staff,
    actor: &Actor,
    existing: Option<Uuid>,
    mut block: BlockDraft,
) -> AppResult<BlockDraft> {
    let mut tx = db.begin().await?;
    let before = match existing {
        Some(id) => {
            sqlx::query_scalar!("SELECT id FROM content_blocks WHERE id = $1 AND is_shared AND deleted_at IS NULL FOR UPDATE", id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
            block.id = id;
            repo::load_blocks(&mut tx, &[id]).await?.pop()
        }
        None => {
            block.id = Uuid::now_v7();
            for item in &mut block.items {
                item.id = Uuid::now_v7();
            }
            None
        }
    };
    block.shared_block_id = None;
    block.anchor_id = None;
    let (default_locale, locales) = locales(&mut tx).await?;
    let mut errors = Vec::new();
    let t = reg.get(&block.block_type).ok_or_else(|| {
        invalid(vec![FieldError::new(
            "/type",
            "unknown_type",
            "unknown section type",
        )])
    })?;
    validate::sanitize_block(t, &mut block, "", &mut errors);
    let touched: HashSet<Uuid> = if before.as_ref().is_none_or(|b| changed(b, &block)) {
        HashSet::from([block.id])
    } else {
        HashSet::new()
    };
    let no_shared = HashMap::new();
    let ctx = Context {
        default_locale: &default_locale,
        locales: &locales,
        permissions: &staff.permissions,
        owner: Owner::Shared,
        touched: &touched,
        shared: &no_shared,
    };
    // Validated as a one-section list, then paths are made relative to the section.
    errors.extend(
        validate::validate_blocks(reg, std::slice::from_ref(&block), &ctx, "")
            .into_iter()
            .map(|e| FieldError {
                path: e.path.trim_start_matches("/0").to_string(),
                ..e
            }),
    );
    let item_pairs: Vec<(Uuid, Uuid)> = block.items.iter().map(|i| (i.id, block.id)).collect();
    if !repo::foreign_ids(&mut tx, BlockOwner::Library, &[block.id], &item_pairs)
        .await?
        .is_empty()
    {
        errors.push(FieldError::new(
            "/items",
            "foreign_id",
            "an item id belongs to another section",
        ));
    }
    if !errors.is_empty() {
        return Err(invalid(errors));
    }
    repo::upsert_block(&mut tx, BlockOwner::Library, &block, 0, Some(staff.id)).await?;
    repo::write_refs(&mut tx, block.id, &validate::block_refs(t, &block)).await?;
    // Every page that shows it now differs from its published revision.
    let pages = sqlx::query_scalar!(
        "UPDATE pages SET has_unpublished_changes = true
         WHERE id IN (SELECT page_id FROM content_blocks WHERE shared_block_id = $1 AND deleted_at IS NULL)
         RETURNING id",
        block.id
    )
    .fetch_all(&mut *tx)
    .await?;
    let action = if existing.is_some() {
        "shared_section.updated"
    } else {
        "shared_section.created"
    };
    audit::record(
        &mut tx,
        actor,
        action,
        "content_block",
        &block.id.to_string(),
        json!({"type": block.block_type, "pages": pages}),
    )
    .await?;
    let saved = repo::load_blocks(&mut tx, &[block.id])
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    tx.commit().await?;
    Ok(saved)
}

pub async fn shared_delete(db: &PgPool, actor: &Actor, id: Uuid) -> AppResult<()> {
    let mut tx = db.begin().await?;
    sqlx::query_scalar!("SELECT id FROM content_blocks WHERE id = $1 AND is_shared AND deleted_at IS NULL FOR UPDATE", id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let usages = shared_usages(db, id).await?;
    if !usages.is_empty() {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "shared_section_in_use",
            "remove it from these pages first",
        )
        .with_details(json!({ "usages": usages })));
    }
    sqlx::query!(
        "UPDATE content_blocks SET deleted_at = now() WHERE id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        "shared_section.deleted",
        "content_block",
        &id.to_string(),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---- Preview tokens ------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct PreviewGrant {
    pub page_id: Uuid,
    pub user_id: Uuid,
}

pub async fn create_preview_token(
    cache: &Cache,
    page_id: Uuid,
    user_id: Uuid,
) -> AppResult<(String, u64)> {
    let mut raw = [0u8; 24];
    rand::rng().fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let value =
        serde_json::to_string(&PreviewGrant { page_id, user_id }).map_err(anyhow::Error::from)?;
    let mut conn = cache.connection();
    redis::cmd("SET")
        .arg(cache.key(&format!("preview:{token}")))
        .arg(value)
        .arg("EX")
        .arg(PREVIEW_TTL_SECS)
        .query_async::<()>(&mut conn)
        .await?;
    Ok((token, PREVIEW_TTL_SECS))
}

/// The grant behind a preview token (`None` when unknown or expired).
pub async fn preview_grant(cache: &Cache, token: &str) -> Option<PreviewGrant> {
    if token.len() != 48 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut conn = cache.connection();
    let raw: Option<String> = redis::cmd("GET")
        .arg(cache.key(&format!("preview:{token}")))
        .query_async(&mut conn)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "preview token lookup failed"))
        .ok()?;
    serde_json::from_str(&raw?).ok()
}

// ---- Pages and section types ------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PageSummary {
    pub id: Uuid,
    pub page_type: String,
    pub status: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub has_unpublished_changes: bool,
    pub published_revision: Option<i32>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

pub async fn pages(db: &PgPool) -> AppResult<Vec<PageSummary>> {
    Ok(sqlx::query_as!(
        PageSummary,
        r#"SELECT p.id, p.page_type, p.status, p.has_unpublished_changes, p.updated_at,
                  t.title AS "title?", t.slug AS "slug?",
                  (SELECT r.revision_number FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'published') AS published_revision,
                  (SELECT min(r.publish_at) FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'scheduled') AS scheduled_at
           FROM pages p
           LEFT JOIN page_translations t ON t.page_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
           WHERE p.deleted_at IS NULL
           ORDER BY p.page_type = 'home' DESC, p.sort_order, t.title"#
    )
    .fetch_all(db)
    .await?)
}

#[derive(Debug, Deserialize)]
pub struct NewPage {
    pub page_type: String,
    pub translations: BTreeMap<String, PageText>,
}

pub async fn create_page(
    db: &PgPool,
    staff: &Staff,
    actor: &Actor,
    input: NewPage,
) -> AppResult<Uuid> {
    if !["home", "page", "post", "legal", "landing"].contains(&input.page_type.as_str()) {
        return Err(invalid(vec![FieldError::new(
            "/page_type",
            "enum",
            "unknown page type",
        )]));
    }
    let mut tx = db.begin().await?;
    let (default_locale, _) = locales(&mut tx).await?;
    if !input.translations.contains_key(&default_locale) {
        return Err(invalid(vec![FieldError::new(
            format!("/translations/{default_locale}"),
            "required",
            "a title and a slug are required",
        )]));
    }
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO pages (id, page_type, author_id) VALUES ($1, $2, $3)",
        id,
        input.page_type,
        staff.id
    )
    .execute(&mut *tx)
    .await?;
    repo::write_page_texts(&mut tx, id, &input.translations).await?;
    audit::record(
        &mut tx,
        actor,
        "page.created",
        "page",
        &id.to_string(),
        json!({"page_type": input.page_type}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

#[derive(Debug, Serialize)]
pub struct SectionTypeView {
    #[serde(flatten)]
    pub definition: super::registry::SectionType,
    pub is_active: bool,
    pub is_system: bool,
    pub preview_media_id: Option<Uuid>,
    pub preview_url: Option<String>,
}

pub async fn section_types(
    db: &PgPool,
    reg: &SectionRegistry,
    media_url: impl Fn(&str) -> String,
) -> AppResult<Vec<SectionTypeView>> {
    let rows = sqlx::query!(
        "SELECT s.code, s.is_active, s.is_system, s.preview_media_id, m.object_key AS \"object_key?\"
         FROM section_types s LEFT JOIN media_assets m ON m.id = s.preview_media_id AND m.deleted_at IS NULL"
    )
    .fetch_all(db)
    .await?;
    let by_code: HashMap<String, _> = rows.into_iter().map(|r| (r.code.clone(), r)).collect();
    let mut out: Vec<SectionTypeView> = reg
        .all()
        .map(|t| {
            let row = by_code.get(&t.code);
            SectionTypeView {
                definition: t.clone(),
                is_active: row.is_none_or(|r| r.is_active),
                is_system: row.is_none_or(|r| r.is_system),
                preview_media_id: row.and_then(|r| r.preview_media_id),
                preview_url: row.and_then(|r| r.object_key.as_deref()).map(&media_url),
            }
        })
        .collect();
    out.sort_by_key(|t| t.definition.sort_order);
    Ok(out)
}

#[derive(Debug, Deserialize)]
pub struct PatchSectionType {
    pub is_active: Option<bool>,
    #[serde(default, with = "double_option")]
    pub preview_media_id: Option<Option<Uuid>>,
}

pub async fn patch_section_type(
    db: &PgPool,
    actor: &Actor,
    code: &str,
    patch: PatchSectionType,
) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let updated = sqlx::query!(
        "UPDATE section_types SET is_active = COALESCE($2, is_active),
             preview_media_id = CASE WHEN $3 THEN $4 ELSE preview_media_id END
         WHERE code = $1",
        code,
        patch.is_active,
        patch.preview_media_id.is_some(),
        patch.preview_media_id.flatten()
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        actor,
        "section_type.updated",
        "section_type",
        code,
        json!({"is_active": patch.is_active}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Where an entity (media, product...) is used by sections: draft pages and shared sections.
#[derive(Debug, Serialize)]
pub struct EntityUsage {
    pub page_id: Option<Uuid>,
    pub page_title: Option<String>,
    pub block_id: Uuid,
    pub block_type: String,
    pub shared: bool,
    /// Used by the published revision of that page.
    pub live: bool,
}

pub async fn entity_usages(
    db: &PgPool,
    ref_type: super::model::RefType,
    id: Uuid,
) -> AppResult<Vec<EntityUsage>> {
    let needle = json!([{ "ref_type": ref_type, "ref_id": id }]);
    Ok(sqlx::query_as!(
        EntityUsage,
        r#"SELECT DISTINCT b.page_id, b.id AS block_id, b.block_type, b.is_shared AS shared,
                  (SELECT t.title FROM page_translations t JOIN locales l ON l.code = t.locale
                   WHERE t.page_id = b.page_id ORDER BY l.is_default DESC LIMIT 1) AS page_title,
                  EXISTS (SELECT 1 FROM page_revisions r WHERE r.page_id = b.page_id AND r.status = 'published'
                          AND r.snapshot -> 'refs' @> $3) AS "live!"
           FROM content_block_refs x JOIN content_blocks b ON b.id = x.block_id
           WHERE x.ref_type = $1 AND x.ref_id = $2 AND b.deleted_at IS NULL"#,
        ref_type.as_str(),
        id,
        needle
    )
    .fetch_all(db)
    .await?)
}

/// True when a published (or scheduled) revision uses the entity: deleting it must be refused.
pub async fn used_by_live_page(
    conn: &mut PgConnection,
    ref_type: super::model::RefType,
    id: Uuid,
) -> sqlx::Result<bool> {
    let needle = json!([{ "ref_type": ref_type, "ref_id": id }]);
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM page_revisions WHERE status IN ('published', 'scheduled')
                          AND snapshot -> 'refs' @> $1) AS "used!""#,
        needle
    )
    .fetch_one(conn)
    .await
}
