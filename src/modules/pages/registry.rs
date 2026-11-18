//! Section type registry. The repository is the source of truth: one JSON file per type in
//! `section_types/`, embedded in the binary (the Docker image ships no source files). At startup
//! the files are merged with the shared definitions, their schemas compiled once (keyed by
//! `(code, schema_version)`) and upserted into `section_types` (`sync`). Admins may deactivate
//! a type or set its preview image; everything else comes from the files.

use std::collections::{BTreeMap, HashMap};

use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgPool;

/// Every system type file. Adding a type = a JSON file + a line here (a unit test fails if the
/// directory and this list disagree) + a Vue component on the front.
const FILES: &[(&str, &str)] = &[
    ("hero", include_str!("../../../section_types/hero.json")),
    (
        "hero_slider",
        include_str!("../../../section_types/hero_slider.json"),
    ),
    (
        "booking_search",
        include_str!("../../../section_types/booking_search.json"),
    ),
    (
        "product_carousel",
        include_str!("../../../section_types/product_carousel.json"),
    ),
    (
        "category_tiles",
        include_str!("../../../section_types/category_tiles.json"),
    ),
    (
        "image_text",
        include_str!("../../../section_types/image_text.json"),
    ),
    (
        "features",
        include_str!("../../../section_types/features.json"),
    ),
    ("stats", include_str!("../../../section_types/stats.json")),
    (
        "timeline",
        include_str!("../../../section_types/timeline.json"),
    ),
    (
        "seasonal_calendar",
        include_str!("../../../section_types/seasonal_calendar.json"),
    ),
    (
        "gallery",
        include_str!("../../../section_types/gallery.json"),
    ),
    ("video", include_str!("../../../section_types/video.json")),
    ("offers", include_str!("../../../section_types/offers.json")),
    (
        "testimonials",
        include_str!("../../../section_types/testimonials.json"),
    ),
    ("logos", include_str!("../../../section_types/logos.json")),
    ("map", include_str!("../../../section_types/map.json")),
    ("faq", include_str!("../../../section_types/faq.json")),
    (
        "cta_band",
        include_str!("../../../section_types/cta_band.json"),
    ),
    (
        "newsletter",
        include_str!("../../../section_types/newsletter.json"),
    ),
    (
        "contact",
        include_str!("../../../section_types/contact.json"),
    ),
    (
        "posts_feed",
        include_str!("../../../section_types/posts_feed.json"),
    ),
    (
        "rich_text",
        include_str!("../../../section_types/rich_text.json"),
    ),
    ("html", include_str!("../../../section_types/html.json")),
    ("spacer", include_str!("../../../section_types/spacer.json")),
];
const DEFS: &str = include_str!("../../../section_types/_defs.json");
const APPEARANCE: &str = include_str!("../../../section_types/_appearance.json");

/// The file format of `section_types/<code>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionTypeFile {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub category: String,
    pub schema_version: i32,
    pub sort_order: i32,
    pub min_items: i32,
    pub max_items: Option<i32>,
    pub max_per_page: Option<i32>,
    pub allowed_owners: Vec<String>,
    pub allowed_page_types: Option<Vec<String>>,
    pub required_permission: Option<String>,
    pub settings_schema: Value,
    pub content_schema: Value,
    pub item_settings_schema: Option<Value>,
    pub item_content_schema: Option<Value>,
    pub preset: Value,
    /// false: no shared eyebrow/title/subtitle and heading settings (spacer, html).
    #[serde(default = "yes")]
    pub base_fields: bool,
}

fn yes() -> bool {
    true
}

/// A section type with its merged schemas (`$defs` and shared fields included), as served to the
/// admin and used by the validator.
#[derive(Debug, Clone, Serialize)]
pub struct SectionType {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub category: String,
    pub schema_version: i32,
    pub sort_order: i32,
    pub min_items: i32,
    pub max_items: Option<i32>,
    pub max_per_page: Option<i32>,
    pub allowed_owners: Vec<String>,
    pub allowed_page_types: Option<Vec<String>>,
    pub required_permission: Option<String>,
    pub settings_schema: Value,
    pub content_schema: Value,
    pub item_settings_schema: Option<Value>,
    pub item_content_schema: Option<Value>,
    pub preset: Value,
    /// Renders the page's `<h1>` (hero, hero_slider): at most one per page.
    pub renders_h1: bool,
}

impl SectionType {
    pub fn has_items(&self) -> bool {
        self.item_settings_schema.is_some() || self.item_content_schema.is_some()
    }
}

pub(super) struct Compiled {
    pub settings: Validator,
    pub content: Validator,
    /// Same schema without `required` (non-default locales may be partial).
    pub content_partial: Validator,
    pub item_settings: Option<Validator>,
    pub item_content: Option<Validator>,
    pub item_content_partial: Option<Validator>,
}

pub struct SectionRegistry {
    types: BTreeMap<String, SectionType>,
    compiled: HashMap<(String, i32), Compiled>,
    appearance_schema: Value,
    appearance: Validator,
}

impl std::fmt::Debug for SectionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SectionRegistry")
            .field("types", &self.types.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SectionRegistry {
    /// Loads and compiles the embedded files. Panics on an invalid file: this is a build
    /// defect, caught by the unit tests before anything ships.
    pub fn load() -> Self {
        Self::from_files(FILES.iter().map(|(code, raw)| {
            let file: SectionTypeFile = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("section_types/{code}.json: {e}"));
            assert_eq!(&file.code, code, "section_types/{code}.json: code mismatch");
            file
        }))
    }

    pub fn from_files(files: impl IntoIterator<Item = SectionTypeFile>) -> Self {
        let defs: Value = serde_json::from_str(DEFS).expect("section_types/_defs.json");
        let shared_defs = defs["$defs"].clone();
        let appearance_schema = with_defs(
            serde_json::from_str(APPEARANCE).expect("section_types/_appearance.json"),
            &shared_defs,
        );
        let appearance = compile(&appearance_schema, "_appearance");
        let mut types = BTreeMap::new();
        let mut compiled = HashMap::new();
        for file in files {
            let code = file.code.clone();
            let mut settings = file.settings_schema;
            let mut content = file.content_schema;
            if file.base_fields {
                merge_properties(&mut settings, &defs["base_settings"]);
                merge_properties(&mut content, &defs["base_content"]);
            }
            let renders_h1 = settings
                .get("x-renders-h1")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let settings = with_defs(settings, &shared_defs);
            let content = with_defs(content, &shared_defs);
            let item_settings = file
                .item_settings_schema
                .map(|s| with_defs(s, &shared_defs));
            let item_content = file.item_content_schema.map(|s| with_defs(s, &shared_defs));
            compiled.insert(
                (code.clone(), file.schema_version),
                Compiled {
                    settings: compile(&settings, &code),
                    content: compile(&content, &code),
                    content_partial: compile(&without_required(&content), &code),
                    item_settings: item_settings.as_ref().map(|s| compile(s, &code)),
                    item_content: item_content.as_ref().map(|s| compile(s, &code)),
                    item_content_partial: item_content
                        .as_ref()
                        .map(|s| compile(&without_required(s), &code)),
                },
            );
            types.insert(
                code.clone(),
                SectionType {
                    code,
                    name: file.name,
                    description: file.description,
                    icon: file.icon,
                    category: file.category,
                    schema_version: file.schema_version,
                    sort_order: file.sort_order,
                    min_items: file.min_items,
                    max_items: file.max_items,
                    max_per_page: file.max_per_page,
                    allowed_owners: file.allowed_owners,
                    allowed_page_types: file.allowed_page_types,
                    required_permission: file.required_permission,
                    settings_schema: settings,
                    content_schema: content,
                    item_settings_schema: item_settings,
                    item_content_schema: item_content,
                    preset: file.preset,
                    renders_h1,
                },
            );
        }
        Self {
            types,
            compiled,
            appearance_schema,
            appearance,
        }
    }

    pub fn get(&self, code: &str) -> Option<&SectionType> {
        self.types.get(code)
    }

    pub fn all(&self) -> impl Iterator<Item = &SectionType> {
        self.types.values()
    }

    pub fn appearance_schema(&self) -> &Value {
        &self.appearance_schema
    }

    pub(super) fn appearance_validator(&self) -> &Validator {
        &self.appearance
    }

    pub(super) fn compiled(&self, t: &SectionType) -> &Compiled {
        &self.compiled[&(t.code.clone(), t.schema_version)]
    }

    /// Upserts every system type into `section_types` (schemas, limits, preset), then upgrades
    /// the drafts saved in an older version of a type (gallery v1 → v2). `is_active` and
    /// `preview_media_id` belong to the admin and are never overwritten.
    pub async fn sync(&self, db: &PgPool) -> sqlx::Result<()> {
        let mut tx = db.begin().await?;
        for t in self.types.values() {
            sqlx::query!(
                "INSERT INTO section_types (code, name, description, icon, category, settings_schema, content_schema,
                     item_settings_schema, item_content_schema, min_items, max_items, allowed_owners,
                     allowed_page_types, max_per_page, preset, required_permission, schema_version,
                     is_system, sort_order)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, true, $18)
                 ON CONFLICT (code) DO UPDATE SET
                     name = EXCLUDED.name, description = EXCLUDED.description, icon = EXCLUDED.icon,
                     category = EXCLUDED.category, settings_schema = EXCLUDED.settings_schema,
                     content_schema = EXCLUDED.content_schema,
                     item_settings_schema = EXCLUDED.item_settings_schema,
                     item_content_schema = EXCLUDED.item_content_schema,
                     min_items = EXCLUDED.min_items, max_items = EXCLUDED.max_items,
                     allowed_owners = EXCLUDED.allowed_owners, allowed_page_types = EXCLUDED.allowed_page_types,
                     max_per_page = EXCLUDED.max_per_page, preset = EXCLUDED.preset,
                     required_permission = EXCLUDED.required_permission,
                     schema_version = EXCLUDED.schema_version, is_system = true, sort_order = EXCLUDED.sort_order
                 WHERE (section_types.name, section_types.description, section_types.icon, section_types.category,
                        section_types.settings_schema, section_types.content_schema,
                        section_types.item_settings_schema, section_types.item_content_schema,
                        section_types.min_items, section_types.max_items, section_types.allowed_owners,
                        section_types.allowed_page_types, section_types.max_per_page, section_types.preset,
                        section_types.required_permission, section_types.schema_version, section_types.is_system,
                        section_types.sort_order)
                       IS DISTINCT FROM
                       (EXCLUDED.name, EXCLUDED.description, EXCLUDED.icon, EXCLUDED.category,
                        EXCLUDED.settings_schema, EXCLUDED.content_schema, EXCLUDED.item_settings_schema,
                        EXCLUDED.item_content_schema, EXCLUDED.min_items, EXCLUDED.max_items,
                        EXCLUDED.allowed_owners, EXCLUDED.allowed_page_types, EXCLUDED.max_per_page,
                        EXCLUDED.preset, EXCLUDED.required_permission, EXCLUDED.schema_version, true,
                        EXCLUDED.sort_order)",
                t.code,
                t.name,
                t.description,
                t.icon,
                t.category,
                t.settings_schema,
                t.content_schema,
                t.item_settings_schema,
                t.item_content_schema,
                t.min_items,
                t.max_items,
                &t.allowed_owners,
                t.allowed_page_types.as_deref(),
                t.max_per_page,
                t.preset,
                t.required_permission,
                t.schema_version,
                t.sort_order,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        // Sections saved in an older shape of their type (drafts only).
        super::gallery::migrate_v1_drafts(db, self).await?;
        Ok(())
    }
}

fn compile(schema: &Value, code: &str) -> Validator {
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(schema)
        .unwrap_or_else(|e| panic!("section type {code}: invalid schema: {e}"))
}

fn with_defs(mut schema: Value, defs: &Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        obj.insert(
            "$schema".into(),
            json!("https://json-schema.org/draft/2020-12/schema"),
        );
        let entry = obj.entry("$defs").or_insert_with(|| json!({}));
        if let (Some(target), Some(source)) = (entry.as_object_mut(), defs.as_object()) {
            for (k, v) in source {
                target.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    schema
}

/// Adds the shared properties first (the type's own ones keep their order and win on a clash).
fn merge_properties(schema: &mut Value, base: &Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let own = obj
        .remove("properties")
        .and_then(|p| p.as_object().cloned())
        .unwrap_or_default();
    let mut merged: Map<String, Value> = base.as_object().cloned().unwrap_or_default();
    for (k, v) in own {
        merged.insert(k, v);
    }
    obj.insert("properties".into(), Value::Object(merged));
}

/// The schema with every `required` removed (translations other than the default locale may
/// be partial: missing fields fall back to the default locale).
pub fn without_required(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "required")
                .map(|(k, v)| (k.clone(), without_required(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_required).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_of_the_directory_is_embedded() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/section_types");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".json") && !n.starts_with('_'))
            .map(|n| n.trim_end_matches(".json").to_string())
            .collect();
        on_disk.sort();
        let mut embedded: Vec<String> = FILES.iter().map(|(c, _)| (*c).to_string()).collect();
        embedded.sort();
        assert_eq!(on_disk, embedded);
    }

    /// The sanitiser looks for rich fields at the top level of settings and content.
    #[test]
    fn rich_text_fields_are_top_level_properties() {
        fn nested_rich(v: &Value, depth: usize) -> bool {
            match v {
                Value::Object(m) => m.iter().any(|(k, child)| {
                    (k == "x-widget"
                        && depth > 2
                        && matches!(child.as_str(), Some("richtext" | "html")))
                        || nested_rich(child, depth + 1)
                }),
                Value::Array(a) => a.iter().any(|c| nested_rich(c, depth + 1)),
                _ => false,
            }
        }
        for t in SectionRegistry::load().all() {
            for s in [
                Some(&t.settings_schema),
                Some(&t.content_schema),
                t.item_settings_schema.as_ref(),
                t.item_content_schema.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                // schema → properties → field → x-widget is depth 2
                assert!(!nested_rich(s, 0), "{}: nested rich text field", t.code);
            }
        }
    }

    #[test]
    fn the_library_loads_and_only_heroes_render_the_h1() {
        let reg = SectionRegistry::load();
        assert_eq!(reg.all().count(), 24);
        let h1: Vec<&str> = reg
            .all()
            .filter(|t| t.renders_h1)
            .map(|t| t.code.as_str())
            .collect();
        assert_eq!(h1, ["hero", "hero_slider"]);
        let hero = reg.get("hero").unwrap();
        assert!(hero.content_schema["properties"]["eyebrow"].is_object());
        assert!(hero.settings_schema["properties"]["heading_align"].is_object());
        assert!(hero.settings_schema["$defs"]["link"].is_object());
        let spacer = reg.get("spacer").unwrap();
        assert!(spacer.content_schema["properties"].get("title").is_none());
        assert_eq!(
            reg.get("html").unwrap().required_permission.as_deref(),
            Some("content.html")
        );
    }
}
