//! Validation, sanitisation and reference extraction of section trees. Pure functions (no I/O),
//! unit tested here; the service supplies the context (locales, permissions, owner, shared
//! sections) loaded from the database.
//!
//! Errors are addressed by JSON pointer from the saved document, e.g.
//! `/blocks/3/items/1/content/fr/title`, so the admin form can highlight the field.

use std::collections::{BTreeSet, HashMap, HashSet};

use jsonschema::{Validator, error::ValidationErrorKind};
use serde_json::Value;
use uuid::Uuid;

use super::{
    model::{BlockDraft, FieldError, ItemDraft, RefEntry, RefType},
    registry::{SectionRegistry, SectionType},
};

/// Who owns the sections being validated.
#[derive(Debug, Clone, Copy)]
pub enum Owner<'a> {
    Page {
        page_type: &'a str,
    },
    Product,
    Place,
    /// A shared section of the library.
    Shared,
}

impl Owner<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Owner::Page { .. } | Owner::Shared => "page",
            Owner::Product => "product",
            Owner::Place => "place",
        }
    }
}

pub struct Context<'a> {
    pub default_locale: &'a str,
    /// Active locales.
    pub locales: &'a [String],
    pub permissions: &'a HashSet<String>,
    pub owner: Owner<'a>,
    /// Sections created or changed by this save: only those are checked against the type's
    /// `required_permission` (an editor may save a page that holds an untouched HTML section).
    pub touched: &'a HashSet<Uuid>,
    /// Existing shared sections (id → type), the only valid targets of a placement.
    pub shared: &'a HashMap<Uuid, String>,
}

/// Validates a whole list of sections (`base` is the pointer of the list, e.g. `/blocks`).
pub fn validate_blocks(
    reg: &SectionRegistry,
    blocks: &[BlockDraft],
    ctx: &Context<'_>,
    base: &str,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let mut per_type: HashMap<&str, usize> = HashMap::new();
    let mut anchors: HashMap<&str, usize> = HashMap::new();
    let mut ids = HashSet::new();
    let mut h1 = 0;
    for (i, block) in blocks.iter().enumerate() {
        let path = format!("{base}/{i}");
        if !ids.insert(block.id) {
            errors.push(FieldError::new(
                format!("{path}/id"),
                "duplicate_id",
                "two sections have the same id",
            ));
        }
        // A placement takes the type of its shared section.
        let block_type = match block.shared_block_id {
            Some(shared) => match ctx.shared.get(&shared) {
                Some(t) => t.as_str(),
                None => {
                    errors.push(FieldError::new(
                        format!("{path}/shared_block_id"),
                        "shared_not_found",
                        "this shared section does not exist",
                    ));
                    continue;
                }
            },
            None => block.block_type.as_str(),
        };
        let Some(t) = reg.get(block_type) else {
            errors.push(FieldError::new(
                format!("{path}/type"),
                "unknown_type",
                format!("unknown section type `{block_type}`"),
            ));
            continue;
        };
        if block.shared_block_id.is_some() && block.block_type != t.code {
            errors.push(FieldError::new(
                format!("{path}/type"),
                "type_mismatch",
                "a placement has the type of its shared section",
            ));
        }
        *per_type.entry(t.code.as_str()).or_default() += 1;
        if t.renders_h1 {
            h1 += 1;
            if h1 > 1 {
                errors.push(FieldError::new(
                    format!("{path}/type"),
                    "second_h1",
                    "a page has one main title: remove the other hero first",
                ));
            }
        }
        if let Some(max) = t.max_per_page
            && per_type[t.code.as_str()] > max as usize
        {
            errors.push(FieldError::new(
                format!("{path}/type"),
                "max_per_page",
                format!("at most {max} `{}` section(s) per page", t.code),
            ));
        }
        if !t.allowed_owners.iter().any(|o| o == ctx.owner.kind()) {
            errors.push(FieldError::new(
                format!("{path}/type"),
                "owner_not_allowed",
                format!("`{}` cannot be used here", t.code),
            ));
        }
        if let (Owner::Page { page_type }, Some(allowed)) = (ctx.owner, &t.allowed_page_types)
            && !allowed.iter().any(|p| p == page_type)
        {
            errors.push(FieldError::new(
                format!("{path}/type"),
                "page_type_not_allowed",
                format!("`{}` cannot be used on a {page_type} page", t.code),
            ));
        }
        if let Some(permission) = &t.required_permission
            && ctx.touched.contains(&block.id)
            && !ctx.permissions.contains(permission)
        {
            errors.push(FieldError::new(
                path.clone(),
                "forbidden_section",
                format!("you are not allowed to edit `{}` sections", t.code),
            ));
        }
        if let Some(anchor) = block.anchor_id.as_deref() {
            if !valid_anchor(anchor) {
                errors.push(FieldError::new(
                    format!("{path}/anchor_id"),
                    "pattern",
                    "lowercase letters, digits and dashes, starting with a letter",
                ));
            } else if anchors.insert(anchor, i).is_some() {
                errors.push(FieldError::new(
                    format!("{path}/anchor_id"),
                    "duplicate_anchor",
                    "another section of this page uses this anchor",
                ));
            }
        }
        validate_common(block, ctx, &path, &mut errors);
        if block.shared_block_id.is_some() {
            if !block.items.is_empty() {
                errors.push(FieldError::new(
                    format!("{path}/items"),
                    "placement_items",
                    "a placement of a shared section has no items of its own",
                ));
            }
            continue;
        }
        validate_body(reg, t, block, ctx, &path, &mut errors);
    }
    errors
}

/// Settings, appearance, content and items of one section (not a placement).
pub fn validate_body(
    reg: &SectionRegistry,
    t: &SectionType,
    block: &BlockDraft,
    ctx: &Context<'_>,
    path: &str,
    errors: &mut Vec<FieldError>,
) {
    let compiled = reg.compiled(t);
    schema_errors(
        &compiled.settings,
        &block.settings,
        &format!("{path}/settings"),
        errors,
    );
    schema_errors(
        reg.appearance_validator(),
        &block.appearance,
        &format!("{path}/appearance"),
        errors,
    );
    validate_content(
        &compiled.content,
        &compiled.content_partial,
        &block.content,
        ctx,
        &format!("{path}/content"),
        errors,
    );
    let items_path = format!("{path}/items");
    if !t.has_items() {
        if !block.items.is_empty() {
            errors.push(FieldError::new(
                items_path,
                "no_items",
                format!("`{}` sections have no items", t.code),
            ));
        }
        return;
    }
    let count = block.items.len();
    if count < t.min_items as usize {
        errors.push(FieldError::new(
            items_path.clone(),
            "too_few_items",
            format!("at least {} item(s)", t.min_items),
        ));
    }
    if let Some(max) = t.max_items
        && count > max as usize
    {
        errors.push(FieldError::new(
            items_path.clone(),
            "too_many_items",
            format!("at most {max} item(s)"),
        ));
    }
    let mut ids = HashSet::new();
    for (j, item) in block.items.iter().enumerate() {
        let item_path = format!("{items_path}/{j}");
        if !ids.insert(item.id) {
            errors.push(FieldError::new(
                format!("{item_path}/id"),
                "duplicate_id",
                "two items have the same id",
            ));
        }
        if let Some(v) = &compiled.item_settings {
            schema_errors(v, &item.settings, &format!("{item_path}/settings"), errors);
        }
        if let (Some(full), Some(partial)) =
            (&compiled.item_content, &compiled.item_content_partial)
        {
            validate_content(
                full,
                partial,
                &item.content,
                ctx,
                &format!("{item_path}/content"),
                errors,
            );
        }
        if let Some(p) = item.active_period
            && let (Some(s), Some(e)) = (p.start, p.end)
            && s >= e
        {
            errors.push(FieldError::new(
                format!("{item_path}/active_period"),
                "empty_period",
                "the end must be after the start",
            ));
        }
    }
}

fn validate_common(
    block: &BlockDraft,
    ctx: &Context<'_>,
    path: &str,
    errors: &mut Vec<FieldError>,
) {
    if let Some(p) = block.active_period
        && let (Some(s), Some(e)) = (p.start, p.end)
        && s >= e
    {
        errors.push(FieldError::new(
            format!("{path}/active_period"),
            "empty_period",
            "the end must be after the start",
        ));
    }
    for (k, locale) in block.hidden_locales.iter().enumerate() {
        if !ctx.locales.contains(locale) {
            errors.push(FieldError::new(
                format!("{path}/hidden_locales/{k}"),
                "unknown_locale",
                format!("`{locale}` is not an active language"),
            ));
        }
    }
    if block
        .name
        .as_deref()
        .is_some_and(|n| n.chars().count() > 120)
    {
        errors.push(FieldError::new(
            format!("{path}/name"),
            "maxLength",
            "at most 120 characters",
        ));
    }
    if block
        .analytics_label
        .as_deref()
        .is_some_and(|l| l.chars().count() > 80)
    {
        errors.push(FieldError::new(
            format!("{path}/analytics_label"),
            "maxLength",
            "at most 80 characters",
        ));
    }
}

/// The default locale must satisfy `required`; other locales may be partial.
fn validate_content(
    full: &Validator,
    partial: &Validator,
    content: &std::collections::BTreeMap<String, Value>,
    ctx: &Context<'_>,
    path: &str,
    errors: &mut Vec<FieldError>,
) {
    for locale in content.keys() {
        if !ctx.locales.contains(locale) {
            errors.push(FieldError::new(
                format!("{path}/{locale}"),
                "unknown_locale",
                format!("`{locale}` is not an active language"),
            ));
        }
    }
    let empty = Value::Object(Default::default());
    let default = content.get(ctx.default_locale).unwrap_or(&empty);
    schema_errors(
        full,
        default,
        &format!("{path}/{}", ctx.default_locale),
        errors,
    );
    for (locale, value) in content {
        if locale != ctx.default_locale && ctx.locales.contains(locale) {
            schema_errors(partial, value, &format!("{path}/{locale}"), errors);
        }
    }
}

fn schema_errors(v: &Validator, value: &Value, path: &str, errors: &mut Vec<FieldError>) {
    for e in v.iter_errors(value) {
        let mut pointer = format!("{path}{}", e.instance_path().as_str());
        if let ValidationErrorKind::Required { property } = e.kind()
            && let Some(p) = property.as_str()
        {
            pointer.push('/');
            pointer.push_str(&escape_pointer(p));
        }
        errors.push(FieldError::new(pointer, e.kind().keyword(), e.to_string()));
    }
}

fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn valid_anchor(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && s.len() <= 60
}

// ---- Schema walking ------------------------------------------------------------------------

/// Follows a local `$ref` (`#/$defs/<name>`).
fn target<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(r) => r
            .strip_prefix("#/$defs/")
            .and_then(|name| root.get("$defs")?.get(name))
            .unwrap_or(node),
        None => node,
    }
}

fn keyword<'a>(root: &'a Value, node: &'a Value, key: &str) -> Option<&'a str> {
    node.get(key)
        .or_else(|| target(root, node).get(key))
        .and_then(Value::as_str)
}

/// Calls `f(schema_node, value)` for every value that has a schema node, depth first.
fn walk<'a>(
    root: &'a Value,
    node: &'a Value,
    value: &'a Value,
    f: &mut impl FnMut(&'a Value, &'a Value),
) {
    f(node, value);
    let t = target(root, node);
    match value {
        Value::Object(map) => {
            if let Some(props) = t.get("properties").and_then(Value::as_object) {
                for (k, v) in map {
                    if let Some(child) = props.get(k) {
                        walk(root, child, v, f);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(child) = t.get("items") {
                for v in items {
                    walk(root, child, v, f);
                }
            }
        }
        _ => {}
    }
}

// ---- Restricted references ---------------------------------------------------------------------

/// A picked id whose field restricts what it may point to: `x-kinds` (product kinds) or
/// `x-scope` (category scope). Checked against the database when a page is saved or published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restricted {
    pub path: String,
    /// `product` or `category`.
    pub ref_type: String,
    pub id: Uuid,
    pub allowed: Vec<String>,
}

fn allowed_of(root: &Value, node: &Value) -> Option<(String, Vec<String>)> {
    let t = target(root, node);
    let get = |k: &str| node.get(k).or_else(|| t.get(k));
    if let Some(kinds) = get("x-kinds").and_then(Value::as_array) {
        return Some((
            "product".into(),
            kinds
                .iter()
                .filter_map(|k| k.as_str().map(String::from))
                .collect(),
        ));
    }
    get("x-scope")
        .and_then(Value::as_str)
        .map(|scope| ("category".to_string(), vec![scope.to_string()]))
}

fn walk_restricted(
    root: &Value,
    node: &Value,
    value: &Value,
    path: &str,
    out: &mut Vec<Restricted>,
) {
    if let Some((ref_type, allowed)) = allowed_of(root, node) {
        let ids: Vec<(String, &Value)> = match value {
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(j, v)| (format!("{path}/{j}"), v))
                .collect(),
            other => vec![(path.to_string(), other)],
        };
        for (p, v) in ids {
            if let Some(id) = v.as_str().and_then(|s| s.parse().ok()) {
                out.push(Restricted {
                    path: p,
                    ref_type: ref_type.clone(),
                    id,
                    allowed: allowed.clone(),
                });
            }
        }
        return;
    }
    let t = target(root, node);
    if let (Value::Object(map), Some(props)) =
        (value, t.get("properties").and_then(Value::as_object))
    {
        for (k, v) in map {
            if let Some(child) = props.get(k) {
                walk_restricted(
                    root,
                    child,
                    v,
                    &format!("{path}/{}", escape_pointer(k)),
                    out,
                );
            }
        }
    }
}

/// Every restricted id of a list of sections (placements of shared sections are skipped: the
/// shared section is checked when it is saved).
pub fn restricted_refs(
    reg: &SectionRegistry,
    blocks: &[BlockDraft],
    base: &str,
) -> Vec<Restricted> {
    let mut out = vec![];
    for (i, b) in blocks.iter().enumerate() {
        if b.shared_block_id.is_some() {
            continue;
        }
        let Some(t) = reg.get(&b.block_type) else {
            continue;
        };
        walk_restricted(
            &t.settings_schema,
            &t.settings_schema,
            &b.settings,
            &format!("{base}/{i}/settings"),
            &mut out,
        );
    }
    out
}

/// Errors for restricted ids that point elsewhere. `found` maps an id to its product kind or
/// category scope (ids missing from it no longer exist and are left to the other checks).
pub fn restricted_errors(refs: &[Restricted], found: &HashMap<Uuid, String>) -> Vec<FieldError> {
    refs.iter()
        .filter_map(|r| {
            let actual = found.get(&r.id)?;
            if r.allowed.iter().any(|a| a == actual) {
                return None;
            }
            Some(if r.ref_type == "product" {
                FieldError::new(
                    r.path.clone(),
                    "kind_not_allowed",
                    format!(
                        "this section only shows {} (not {actual})",
                        r.allowed.join(", ")
                    ),
                )
            } else {
                FieldError::new(
                    r.path.clone(),
                    "scope_not_allowed",
                    format!("choose a {} category (not {actual})", r.allowed.join(", ")),
                )
            })
        })
        .collect()
}

// ---- References ------------------------------------------------------------------------------

/// Ids referenced by a value (fields marked `x-ref`, and links to pages, products...).
pub fn refs_in(schema: &Value, value: &Value) -> BTreeSet<RefEntry> {
    let mut out = BTreeSet::new();
    walk(schema, schema, value, &mut |node, v| {
        if keyword(schema, node, "x-widget") == Some("link") {
            if let (Some(kind), Some(id)) = (
                v.get("type").and_then(Value::as_str),
                v.get("id")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok()),
            ) && let Some(ref_type) = RefType::parse(kind)
            {
                out.insert(RefEntry {
                    ref_type,
                    ref_id: id,
                });
            }
            return;
        }
        let Some(ref_type) = keyword(schema, node, "x-ref").and_then(RefType::parse) else {
            return;
        };
        let ids: Vec<&Value> = match v {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for id in ids {
            if let Some(ref_id) = id.as_str().and_then(|s| s.parse().ok()) {
                out.insert(RefEntry { ref_type, ref_id });
            }
        }
    });
    out
}

/// References of a section: `(None, ref)` for the section itself, `(Some(item), ref)` for items.
pub fn block_refs(t: &SectionType, block: &BlockDraft) -> Vec<(Option<Uuid>, RefEntry)> {
    let mut out: Vec<(Option<Uuid>, RefEntry)> = Vec::new();
    let mut own = refs_in(&t.settings_schema, &block.settings);
    for content in block.content.values() {
        own.extend(refs_in(&t.content_schema, content));
    }
    out.extend(own.into_iter().map(|r| (None, r)));
    if let Some(bg) = block
        .appearance
        .get("background_media_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
    {
        let r = RefEntry {
            ref_type: RefType::Media,
            ref_id: bg,
        };
        if !out.contains(&(None, r)) {
            out.push((None, r));
        }
    }
    for item in &block.items {
        let mut refs = BTreeSet::new();
        if let Some(s) = &t.item_settings_schema {
            refs.extend(refs_in(s, &item.settings));
        }
        if let Some(s) = &t.item_content_schema {
            for content in item.content.values() {
                refs.extend(refs_in(s, content));
            }
        }
        out.extend(refs.into_iter().map(|r| (Some(item.id), r)));
    }
    out
}

/// Kinds of data a section resolves by rule, at render time (invalidation hints).
pub fn rule_sources(block: &BlockDraft) -> Vec<&'static str> {
    let s = &block.settings;
    let source_mode = s.pointer("/source/mode").and_then(Value::as_str);
    match block.block_type.as_str() {
        // The shop's cards follow prices, stock and promotions of any shop product.
        "shop_products" => vec!["shop_catalog"],
        "product_carousel" if source_mode == Some("rule") => vec!["product"],
        "product_carousel" | "offers" => vec!["product", "promotion"],
        "category_tiles" if source_mode == Some("rule") => vec!["category"],
        "faq" if source_mode == Some("rule") => vec!["faq"],
        "testimonials" => vec!["review"],
        "seasonal_calendar" => vec!["crop"],
        "posts_feed" => vec!["page"],
        "features" if s.get("source").and_then(Value::as_str) == Some("amenities") => {
            vec!["amenity"]
        }
        "gallery" if s.get("source").and_then(Value::as_str) == Some("gallery") => vec!["gallery"],
        _ => vec![],
    }
}

// ---- Sanitising ------------------------------------------------------------------------------

/// Cleans every `richtext` / `html` field of a section in place. Markup that is dangerous (not
/// merely unsupported) is refused instead, so the editor knows their HTML was not kept.
pub fn sanitize_block(
    t: &SectionType,
    block: &mut BlockDraft,
    path: &str,
    errors: &mut Vec<FieldError>,
) {
    for (locale, content) in block.content.iter_mut() {
        sanitize_value(
            &t.content_schema,
            content,
            &format!("{path}/content/{locale}"),
            errors,
        );
    }
    sanitize_value(
        &t.settings_schema,
        &mut block.settings,
        &format!("{path}/settings"),
        errors,
    );
    for (j, item) in block.items.iter_mut().enumerate() {
        sanitize_item(t, item, &format!("{path}/items/{j}"), errors);
    }
}

fn sanitize_item(t: &SectionType, item: &mut ItemDraft, path: &str, errors: &mut Vec<FieldError>) {
    if let Some(s) = &t.item_content_schema {
        for (locale, content) in item.content.iter_mut() {
            sanitize_value(s, content, &format!("{path}/content/{locale}"), errors);
        }
    }
    if let Some(s) = &t.item_settings_schema {
        sanitize_value(s, &mut item.settings, &format!("{path}/settings"), errors);
    }
}

fn sanitize_value(schema: &Value, value: &mut Value, path: &str, errors: &mut Vec<FieldError>) {
    // Rich fields are top-level properties of content and settings (a unit test keeps the
    // library that way), so the pointer is the field name.
    let mut found: Vec<(String, String)> = Vec::new();
    if let Value::Object(map) = &*value {
        for (k, v) in map {
            let Some(node) = target(schema, schema)
                .get("properties")
                .and_then(|p| p.get(k))
            else {
                continue;
            };
            if let (Some(widget), Some(_)) = (keyword(schema, node, "x-widget"), v.as_str())
                && (widget == "richtext" || widget == "html")
            {
                found.push((k.clone(), widget.to_string()));
            }
        }
    }
    let Value::Object(map) = value else { return };
    for (key, widget) in found {
        let Some(Value::String(raw)) = map.get_mut(&key) else {
            continue;
        };
        if let Some(reason) = dangerous(raw) {
            errors.push(FieldError::new(
                format!("{path}/{}", escape_pointer(&key)),
                "unsafe_html",
                format!("HTML refused: {reason}"),
            ));
            continue;
        }
        *raw = if widget == "html" {
            clean_html(raw)
        } else {
            clean_richtext(raw)
        };
    }
}

/// Markup that must never be stored: scripts, frames, event handlers, script URLs.
pub fn dangerous(html: &str) -> Option<&'static str> {
    let lower = html.to_ascii_lowercase();
    for (needle, reason) in [
        ("<script", "scripts are not allowed"),
        ("<iframe", "frames are not allowed"),
        ("<frame", "frames are not allowed"),
        ("<object", "embedded objects are not allowed"),
        ("<embed", "embedded objects are not allowed"),
        ("<style", "style sheets are not allowed"),
        ("<link", "external resources are not allowed"),
        ("<meta", "meta tags are not allowed"),
        ("<base", "base tags are not allowed"),
        ("<form", "forms are not allowed"),
        ("javascript:", "script links are not allowed"),
        ("vbscript:", "script links are not allowed"),
        ("data:text/html", "data URLs are not allowed"),
    ] {
        if lower.contains(needle) {
            return Some(reason);
        }
    }
    // on*="..." event handler attributes inside a tag
    let bytes = lower.as_bytes();
    let mut in_tag = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' => in_tag = true,
            b'>' => in_tag = false,
            b'o' if in_tag
                && i > 0
                && (bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b'/')
                && bytes.get(i + 1) == Some(&b'n') =>
            {
                let name: Vec<u8> = bytes[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .copied()
                    .collect();
                let rest = &bytes[i + name.len()..];
                let next = rest.iter().find(|c| !c.is_ascii_whitespace());
                if name.len() > 2 && next == Some(&b'=') {
                    return Some("event handlers are not allowed");
                }
            }
            _ => {}
        }
    }
    None
}

/// Strict subset for editor text: paragraphs, emphasis, lists, quotes, links.
pub fn clean_richtext(html: &str) -> String {
    ammonia::Builder::default()
        .tags(HashSet::from([
            "p",
            "br",
            "strong",
            "b",
            "em",
            "i",
            "u",
            "s",
            "a",
            "ul",
            "ol",
            "li",
            "h3",
            "h4",
            "blockquote",
        ]))
        .tag_attributes(HashMap::from([("a", HashSet::from(["href", "title"]))]))
        .generic_attributes(HashSet::new())
        .url_schemes(HashSet::from(["https", "mailto", "tel"]))
        .link_rel(Some("noopener noreferrer"))
        .clean(html)
        .to_string()
}

/// Broader allowlist for the HTML section (layout markup, images, tables), never scripts.
pub fn clean_html(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags([
            "section",
            "article",
            "aside",
            "figure",
            "figcaption",
            "picture",
            "source",
            "details",
            "summary",
            "mark",
            "small",
            "sup",
            "sub",
            "time",
            "table",
            "thead",
            "tbody",
            "tr",
            "th",
            "td",
            "caption",
        ])
        .add_generic_attributes(["class", "id", "lang", "dir", "role", "aria-label"])
        .add_tag_attributes("img", ["loading", "decoding", "srcset", "sizes"])
        .add_tag_attributes("source", ["srcset", "type", "media"])
        .add_tag_attributes("time", ["datetime"])
        .url_schemes(HashSet::from(["https", "mailto", "tel"]))
        .link_rel(Some("noopener noreferrer"))
        .clean(html)
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::modules::pages::model::VisibleOn;

    fn reg() -> SectionRegistry {
        SectionRegistry::load()
    }

    fn locales() -> Vec<String> {
        vec!["fr".into(), "en".into(), "ar".into()]
    }

    /// A section built from the type's preset, as the "Add section" dialog does.
    fn from_preset(reg: &SectionRegistry, code: &str) -> BlockDraft {
        let t = reg.get(code).unwrap();
        let p = &t.preset;
        let content: BTreeMap<String, Value> = p["content"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let items = p["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|i| ItemDraft {
                        id: Uuid::now_v7(),
                        settings: i["settings"].clone(),
                        content: i["content"]
                            .as_object()
                            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                            .unwrap_or_default(),
                        is_visible: true,
                        active_period: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        BlockDraft {
            id: Uuid::now_v7(),
            block_type: code.into(),
            name: None,
            anchor_id: None,
            is_visible: true,
            active_period: None,
            visible_on: VisibleOn::All,
            hidden_locales: vec![],
            appearance: p.get("appearance").cloned().unwrap_or(json!({})),
            analytics_label: None,
            settings: p["settings"].clone(),
            content,
            items,
            shared_block_id: None,
            shared: None,
        }
    }

    fn check(blocks: &[BlockDraft], permissions: &[&str], touched_all: bool) -> Vec<FieldError> {
        let reg = reg();
        let locales = locales();
        let permissions: HashSet<String> = permissions.iter().map(|p| (*p).to_string()).collect();
        let touched: HashSet<Uuid> = if touched_all {
            blocks.iter().map(|b| b.id).collect()
        } else {
            HashSet::new()
        };
        let shared = HashMap::new();
        let ctx = Context {
            default_locale: "fr",
            locales: &locales,
            permissions: &permissions,
            owner: Owner::Page { page_type: "home" },
            touched: &touched,
            shared: &shared,
        };
        validate_blocks(&reg, blocks, &ctx, "/blocks")
    }

    #[test]
    fn every_preset_passes_its_own_schema() {
        let reg = reg();
        for t in reg.all() {
            let block = from_preset(&reg, &t.code);
            let errors = check(&[block], &["content.html"], true);
            assert!(errors.is_empty(), "{}: {errors:#?}", t.code);
        }
    }

    #[test]
    fn shop_and_bookable_product_sections_only_take_their_own_kinds() {
        let reg = reg();
        let (activity, honey, cat_shop, cat_post) = (
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
        );
        let found: HashMap<Uuid, String> = [
            (activity, "activity".to_string()),
            (honey, "shop_product".to_string()),
            (cat_shop, "shop".to_string()),
            (cat_post, "post".to_string()),
        ]
        .into();
        // An activity hand-picked in "Produits de la boutique".
        let mut shop = from_preset(&reg, "shop_products");
        shop.settings["source"]["mode"] = json!("manual");
        shop.settings["source"]["ids"] = json!([honey, activity]);
        // A shop product hand-picked in "Hébergements et activités" (product_carousel v2).
        let mut carousel = from_preset(&reg, "product_carousel");
        carousel.settings["source"] = json!({ "mode": "manual", "ids": [activity, honey] });
        let refs = restricted_refs(&reg, &[shop, carousel], "/blocks");
        let errors = restricted_errors(&refs, &found);
        let got: Vec<(&str, &str)> = errors
            .iter()
            .map(|e| (e.path.as_str(), e.code.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/blocks/0/settings/source/ids/1", "kind_not_allowed"),
                ("/blocks/1/settings/source/ids/1", "kind_not_allowed"),
            ]
        );
        // A category of another scope.
        let mut by_category = from_preset(&reg, "shop_products");
        by_category.settings["source"]["rule"]["category_id"] = json!(cat_post);
        let errors = restricted_errors(
            &restricted_refs(&reg, &[by_category.clone()], "/blocks"),
            &found,
        );
        assert_eq!(errors[0].path, "/blocks/0/settings/source/rule/category_id");
        assert_eq!(errors[0].code, "scope_not_allowed");
        by_category.settings["source"]["rule"]["category_id"] = json!(cat_shop);
        assert!(
            restricted_errors(&restricted_refs(&reg, &[by_category], "/blocks"), &found).is_empty()
        );
        // product_carousel v2 no longer offers the shop kind by rule.
        let mut old = from_preset(&reg, "product_carousel");
        old.settings["source"]["rule"]["kind"] = json!("shop_product");
        let errors = check(&[old], &[], true);
        assert!(
            errors
                .iter()
                .any(|e| e.path.starts_with("/blocks/0/settings/source")),
            "{errors:#?}"
        );
    }

    #[test]
    fn the_category_order_needs_a_category() {
        let reg = reg();
        let mut shop = from_preset(&reg, "shop_products");
        shop.settings["source"]["rule"]["sort"] = json!("category_order");
        let errors = check(std::slice::from_ref(&shop), &[], true);
        assert!(
            errors
                .iter()
                .any(|e| e.path.starts_with("/blocks/0/settings/source")),
            "{errors:#?}"
        );
        shop.settings["source"]["rule"]["category_id"] = json!(Uuid::from_u128(3));
        assert!(check(&[shop], &[], true).is_empty());
    }

    #[test]
    fn invalid_settings_are_reported_with_their_path() {
        let reg = reg();
        let mut hero = from_preset(&reg, "hero");
        hero.settings["overlay_opacity"] = json!(95);
        hero.settings["height"] = json!("giant");
        hero.settings["unknown"] = json!(1);
        let errors = check(&[hero], &[], true);
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert!(
            paths.contains(&"/blocks/0/settings/overlay_opacity"),
            "{errors:#?}"
        );
        assert!(paths.contains(&"/blocks/0/settings/height"), "{errors:#?}");
        assert!(errors.iter().any(|e| e.code == "additionalProperties"));
    }

    #[test]
    fn missing_default_locale_field_points_at_the_field() {
        let reg = reg();
        let mut hero = from_preset(&reg, "hero");
        hero.content
            .get_mut("fr")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("title");
        // a partial English translation is fine
        hero.content
            .insert("en".into(), json!({"subtitle": "Only a subtitle"}));
        let errors = check(&[hero], &[], true);
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert_eq!(errors[0].path, "/blocks/0/content/fr/title");
        assert_eq!(errors[0].code, "required");
    }

    #[test]
    fn too_many_items_are_refused() {
        let reg = reg();
        let mut slider = from_preset(&reg, "hero_slider");
        let slide = slider.items[0].clone();
        slider.items = (0..9)
            .map(|_| ItemDraft {
                id: Uuid::now_v7(),
                ..slide.clone()
            })
            .collect();
        let errors = check(&[from_preset(&reg, "spacer"), slider], &[], true);
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/blocks/1/items" && e.code == "too_many_items"),
            "{errors:#?}"
        );
        // an invalid item field is addressed inside the item
        let mut stats = from_preset(&reg, "stats");
        stats.items[0]
            .content
            .insert("fr".into(), json!({"label": "x".repeat(200)}));
        let errors = check(&[stats], &[], true);
        assert_eq!(
            errors[0].path, "/blocks/0/items/0/content/fr/label",
            "{errors:#?}"
        );
    }

    #[test]
    fn a_second_hero_is_refused() {
        let reg = reg();
        let errors = check(
            &[
                from_preset(&reg, "hero"),
                from_preset(&reg, "rich_text"),
                from_preset(&reg, "hero_slider"),
            ],
            &[],
            true,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/blocks/2/type" && e.code == "second_h1"),
            "{errors:#?}"
        );
        let errors = check(
            &[from_preset(&reg, "hero"), from_preset(&reg, "hero")],
            &[],
            true,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/blocks/1/type" && e.code == "max_per_page")
        );
    }

    #[test]
    fn an_editor_cannot_save_an_html_section_but_can_keep_one() {
        let reg = reg();
        let html = from_preset(&reg, "html");
        let editor = ["content.read", "content.write"];
        let errors = check(std::slice::from_ref(&html), &editor, true);
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert_eq!(errors[0].path, "/blocks/0");
        assert_eq!(errors[0].code, "forbidden_section");
        // untouched (made by a super admin): saving the rest of the page is fine
        assert!(check(&[html], &editor, false).is_empty());
    }

    #[test]
    fn unsafe_richtext_is_refused_and_safe_richtext_is_cleaned() {
        let reg = reg();
        let t = reg.get("image_text").unwrap();
        let mut block = from_preset(&reg, "image_text");
        block.content.insert(
            "en".into(),
            json!({"body": "<p onclick=\"steal()\">Hi</p>"}),
        );
        block.content.insert(
            "fr".into(),
            json!({"title": "T", "body": "<p>Bonjour <span class=\"x\">le</span> <a href=\"https://mouzoun.com\">camp</a></p>"}),
        );
        let mut errors = Vec::new();
        sanitize_block(t, &mut block, "/blocks/4", &mut errors);
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert_eq!(errors[0].path, "/blocks/4/content/en/body");
        assert_eq!(errors[0].code, "unsafe_html");
        assert_eq!(
            block.content["fr"]["body"],
            json!(
                "<p>Bonjour le <a href=\"https://mouzoun.com\" rel=\"noopener noreferrer\">camp</a></p>"
            )
        );
        for bad in [
            "<script>alert(1)</script>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "<img src=x onerror=alert(1)>",
            "<iframe src=\"https://evil\"></iframe>",
        ] {
            assert!(dangerous(bad).is_some(), "{bad}");
        }
        assert!(dangerous("<p>Ongoing: one onion</p>").is_none());
        assert!(dangerous("<p class=\"x\">only</p>").is_none());
    }

    #[test]
    fn references_are_extracted_from_ids_lists_and_links() {
        let reg = reg();
        let media = Uuid::now_v7();
        let product = Uuid::now_v7();
        let page = Uuid::now_v7();
        let mut hero = from_preset(&reg, "hero");
        hero.settings["media_id"] = json!(media);
        hero.settings["cta_primary"] =
            json!({"link": {"type": "page", "id": page, "new_tab": false}, "style": "primary"});
        let refs = block_refs(reg.get("hero").unwrap(), &hero);
        assert!(refs.contains(&(
            None,
            RefEntry {
                ref_type: RefType::Media,
                ref_id: media
            }
        )));
        assert!(refs.contains(&(
            None,
            RefEntry {
                ref_type: RefType::Page,
                ref_id: page
            }
        )));

        let mut carousel = from_preset(&reg, "product_carousel");
        carousel.settings["source"] = json!({"mode": "manual", "ids": [product]});
        let refs = block_refs(reg.get("product_carousel").unwrap(), &carousel);
        assert_eq!(
            refs,
            vec![(
                None,
                RefEntry {
                    ref_type: RefType::Product,
                    ref_id: product
                }
            )]
        );
        assert!(check(&[carousel], &[], true).is_empty());

        let mut logos = from_preset(&reg, "logos");
        let item = ItemDraft {
            id: Uuid::now_v7(),
            settings: json!({"media_id": media}),
            content: BTreeMap::from([("fr".to_string(), json!({"name": "Label"}))]),
            is_visible: true,
            active_period: None,
        };
        logos.items = vec![item.clone()];
        let refs = block_refs(reg.get("logos").unwrap(), &logos);
        assert_eq!(
            refs,
            vec![(
                Some(item.id),
                RefEntry {
                    ref_type: RefType::Media,
                    ref_id: media
                }
            )]
        );
    }

    #[test]
    fn manual_sources_need_ids_and_links_need_their_target() {
        let reg = reg();
        let mut carousel = from_preset(&reg, "product_carousel");
        carousel.settings["source"] = json!({"mode": "manual", "ids": []});
        assert!(!check(&[carousel], &[], true).is_empty());
        let mut band = from_preset(&reg, "cta_band");
        band.settings["cta_primary"] = json!({"link": {"type": "page"}, "style": "primary"});
        let errors = check(&[band], &[], true);
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/blocks/0/settings/cta_primary/link/id"),
            "{errors:#?}"
        );
    }

    #[test]
    fn anchors_and_locales_are_checked() {
        let reg = reg();
        let mut a = from_preset(&reg, "rich_text");
        a.anchor_id = Some("farm".into());
        let mut b = from_preset(&reg, "faq");
        b.anchor_id = Some("farm".into());
        b.hidden_locales = vec!["de".into()];
        let errors = check(&[a, b], &[], true);
        let codes: Vec<(&str, &str)> = errors
            .iter()
            .map(|e| (e.path.as_str(), e.code.as_str()))
            .collect();
        assert!(
            codes.contains(&("/blocks/1/anchor_id", "duplicate_anchor")),
            "{codes:?}"
        );
        assert!(
            codes.contains(&("/blocks/1/hidden_locales/0", "unknown_locale")),
            "{codes:?}"
        );
    }
}
