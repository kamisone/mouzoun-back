# Page builder

The back office composes the home page (and any other page) from **sections** picked in a
library of **section types**: add, fill, reorder, hide, schedule, translate, preview the draft,
then publish it. The public site only ever reads **published snapshots**.

| Where | What |
|---|---|
| `migrations/0023_page_builder.*.sql` | `section_types`, `content_blocks` extensions, `content_block_items` (+ translations), `content_block_refs`, `page_revisions`, permissions `content.publish` / `content.html`, setting `site.opening_date` |
| `section_types/*.json` | one file per type (source of truth), plus `_defs.json` (shared definitions and fields) and `_appearance.json` |
| `src/modules/pages/` | `registry` (load + compile + sync types), `validate` (schemas, rules, sanitising, refs), `repo`, `service` (draft, publish, revisions, shared sections, preview), `public` (rendering), `cache` (invalidation), `site` (banners, newsletter, contact) |
| `src/jobs/page_publishing.rs` | promotes scheduled revisions every minute |
| `src/bin/seed_dev/pages.rs` | the demo "coming soon" home page and the About page |
| front `app/components/sections/` | one Vue component per type; `components/page/PageSections.vue` maps type → component |
| front `app/pages/admin/pages/` | page list and the builder (`[id]/builder.vue`), components in `components/admin/builder/` |

## Model: page → sections → items

- A **page** (`pages`) owns an ordered list of **sections** (`content_blocks`, `sort_order` 0..n).
- A section has a **type** (`block_type` → `section_types.code`) whose JSON Schemas say which
  **settings** (`content_blocks.settings`, not translated: layout, ids, numbers) and which
  **content** (`content_block_translations.content`, one row per locale: titles, texts, button
  labels) it accepts, and whether it has repeatable **items** (`content_block_items` +
  `content_block_item_translations`: slides, features, stats, logos, steps).
- Every section also has `appearance` (one shared schema: colour scheme, background, spacing,
  width, dividers), `visible_on` (all / desktop / mobile), `hidden_locales`, `active_period`
  (`tstzrange`, null = always), `anchor_id` (unique per page) and an internal `name`.
- A **shared section** (`is_shared = true`, no owner) lives in the library. A page shows it
  through a **placement**: a row with `shared_block_id` whose own settings, content and items are
  ignored; the placement keeps its own visibility, period, devices, languages and anchor.
  Deleting a shared section that is placed anywhere answers `409` with the usages.
- The working tables are the **draft**. Publishing writes an immutable **snapshot** in
  `page_revisions`.

## Adding a section type

1. **JSON file**: `section_types/<code>.json` (copy a close one). Keys: `code`, `name`,
   `description`, `icon` (Lucide name), `category` (hero, listing, content, media, social_proof,
   conversion, layout, utility), `schema_version`, `sort_order`, `min_items`, `max_items`,
   `max_per_page`, `allowed_owners` (page, product, place), `allowed_page_types` (null = all),
   `required_permission`, `settings_schema`, `content_schema`, `item_settings_schema` /
   `item_content_schema` (null = no items), `preset` (`settings`, `content` per locale, `items`,
   `appearance`: what "Add section" inserts) and optionally `"base_fields": false`.
2. **Register it** in `FILES` in `src/modules/pages/registry.rs` (the binary embeds the files;
   a unit test fails when the directory and the list disagree). At startup the API compiles the
   schemas and upserts the row in `section_types` (admins can only deactivate a type or set its
   preview image, `PATCH /v1/admin/section-types/{code}`, `content.html`).
3. **Vue component**: `front/app/components/sections/<PascalCode>.vue` with props
   `{ section: RenderedSection, index: number }`, and one line in the `registry` of
   `components/page/PageSections.vue`. Use `MzSectionContainer`, `MzSectionHeading`,
   `MzSectionCtas`, `MzMedia` and `useSectionContext()` (texts with `{opening_date}`, media by id,
   heading level). An unknown type renders nothing, so deploy order does not matter.
4. **Data resolver** (optional, for live data): a branch in `section_data()` in
   `src/modules/pages/public.rs` that fills `data` (products, reviews...). Add the kind of data
   to `validate::rule_sources()` if it is resolved by rule, so cache invalidation reaches it, and
   to `public::renders_nothing()` if the section should disappear when the list is empty.
5. Tests: `validate::tests::every_preset_passes_its_own_schema` covers the preset; add a case to
   `tests/it/pages.rs` for the data resolver.

The admin form is generated from the schemas (no admin code for a new type), and the titles of
fields are translated in `admin.builder.fields.<key>` (fr, ar) with the schema `title` as the
English fallback.

### Schemas and custom keywords

JSON Schema draft 2020-12, compiled once per `(code, schema_version)` with the `jsonschema`
crate. Every schema gets the `$defs` of `_defs.json` (`media_id`, `link`, `cta`); unless
`base_fields` is false, content gets `eyebrow`, `title`, `subtitle` and settings get
`heading_align` (start | center) and `title_level` (h2 | h3).

| Keyword | Meaning |
|---|---|
| `x-widget` | form widget: `text`, `textarea`, `richtext`, `html`, `number`, `toggle`, `select` (multi for arrays), `color_scheme`, `icon`, `media`, `media_list`, `link`, `cta`, `source`, `product_picker`, `category_picker`, `place_picker`, `faq_picker`, `promotion_picker`, `gallery_picker`, `datetime` |
| `x-ref` | `content_block_refs.ref_type` of the id(s) the field holds: media, product, category, place, page, faq, gallery, promotion, review |
| `x-group` | form tab: `content`, `layout`, `source`, `advanced` |
| `x-media-kinds` | media picker filter (`image`, `video`) |
| `x-renders-h1` | (root of settings) the type renders the page's `<h1>`: at most one per page |

- A **link** is `{ type: page|product|category|place|route|url|anchor, id?, route?, url?, anchor?, new_tab }`
  (its label is translated content). The public API adds `href` (localised path, or `null` when
  the target is not published: the button is not shown) and `external`.
- A **CTA** is `{ link, style: primary|secondary|ghost|link }` in settings
  (`cta_primary`, `cta_secondary`: two at most) with `cta_primary_label` / `cta_secondary_label`
  in content.
- A **source** (listings) is `{ mode: manual|rule, ids: [...], rule: { kind?, category_id?, featured_only?, sort, limit 1..24 } }`.
- `richtext` and `html` fields must be top-level properties (a unit test enforces it): the API
  refuses dangerous markup (scripts, frames, event handlers, `javascript:` URLs) with a field
  error, then cleans the rest with `ammonia` (richtext: p, br, strong/b, em/i, u, s, a, ul/ol/li,
  h3/h4, blockquote; html: a broader layout allowlist, never scripts).

## Validation (every save)

`validate::validate_blocks()` checks settings, appearance, content (the default locale must
satisfy `required`; other locales may be partial and fall back field by field), each item,
`min_items` / `max_items`, `max_per_page`, `allowed_owners` / `allowed_page_types`, one H1 per
page, unique anchors, active locales, schedules, and the type's `required_permission` for the
sections **created or changed** by the save (an editor can save a page that holds an HTML
section made by a super admin, not change or delete it). Errors come back as
`422 invalid_sections` with `details.errors: [{ path: "/blocks/3/items/1/content/fr/title", code, message }]`;
the builder highlights the field.

## Draft → publish → snapshot

```
builder (autosave, If-Match: draft_version)
   │  PUT /v1/admin/pages/{id}/builder ─► content_blocks, items, translations, refs (draft)
   │                                      draft_version + 1, has_unpublished_changes = true
   ▼
POST /pages/{id}/publish { publish_at?, note? }      (content.publish)
   │  validate the whole draft, inline shared sections, collect refs
   ├─ now       ─► page_revisions(status = published, revision n), previous → superseded
   └─ scheduled ─► page_revisions(status = scheduled); the job promotes it at publish_at
public GET /v1/pages/home | /v1/pages/by-slug/{slug}  ─► the published snapshot only
```

- Saves, publishes and the job take the **page row lock** first; `revision_number` comes from
  `pages.revision_counter` incremented under that lock (gap-free), and a partial unique index
  keeps at most one `published` revision per page.
- `PUT …/builder` without `If-Match` is `428`; with a stale version `409 draft_conflict` with
  `current_version` and who saved last (from the audit log). Small operations exist too
  (`POST /pages/{id}/blocks` from a preset, `POST /blocks/{id}/duplicate`, `PATCH /blocks/{id}`
  visibility and schedule, `DELETE /blocks/{id}`); they accept `If-Match` and return the tree.
  Deleted sections are soft-deleted; sending one again (undo) revives it.
- `GET /pages/{id}/publish-check`: validation errors and the diff (added, removed, changed,
  reordered) against the published revision.
- `POST /revisions/{id}/restore` copies a snapshot back into the draft (placements whose shared
  section still exists become placements again) and **does not publish**; it returns the
  sections that no longer pass today's rules. `POST /revisions/{id}/cancel` cancels a scheduled
  revision.
- The snapshot stores the page texts, the sections with shared ones inlined (ids kept), and
  `refs` / `rule_sources` used for invalidation and for refusing to delete a media a live page
  shows (`DELETE /v1/admin/media/{id}` → `409 media_in_use` with the pages).
- **Preview**: `POST /pages/{id}/preview-token` stores `{page_id, user_id}` in Redis for 30
  minutes. `GET /v1/pages/…?preview=<token>` renders the draft (or `&revision=<id>`), never
  cached, `Cache-Control: no-store`. The builder iframe loads the public page with the token; on
  every form change it posts the section to the iframe, which renders it through
  `POST /v1/pages/{id}/preview/section` (same data and links) without saving; a click in the
  preview selects the section in the builder.

## Public rendering

`GET /v1/pages/home`, `GET /v1/pages/by-slug/{*slug}` (`?locale=` or `Accept-Language`):

1. published snapshot (or draft with a valid preview token);
2. drop hidden sections and items, those outside their `active_period` (injectable clock,
   `state.clock`), and those hidden in the locale;
3. content per locale with field-by-field fallback to the default locale;
4. live data: product cards (same "from" price as the catalogue), categories, amenities, farm
   crops, galleries, automatic promotions active now (codes are never published), reviews and
   their average, FAQs, places with distances, posts; unpublished or deleted references are
   skipped. Listing sections with nothing to show are left out (offers before a promotion,
   testimonials before the first review);
5. media ids → URL, variants (`srcset`), blurhash or tiny placeholder, focal point, alt in the
   locale; links → localised paths;
6. `{ page: {title, slug, path, seo, hreflang}, sections: [{id, type, anchor_id, visible_on, appearance, settings, content, items, data}], json_ld, variables }`.
   `json_ld` holds `FAQPage` (faq sections with `emit_schema`) and a `Campground` with its
   `AggregateRating` (testimonials). `variables.opening_date` fills `{opening_date}` in texts
   (formatted by the front in the page language).

Media with `storage_provider = 'site'` (development placeholders) have a key that is a path
served by the front; `variants` is `{ name: { key, width } }`.

## Caching and invalidation

- Key `pages:{page_id}:{revision}:{locale}` (under the `pages:` prefix of
  `services::cache_invalidation`). Publishing changes the revision, hence the key: nothing to
  delete. Previews are never cached.
- TTL = time until the next start or end of a section/item period of the page, at most 1 hour.
- Outbox events: `product.updated` and `content.changed { ref_type, ref_id }` delete the pages
  whose published snapshot references the entity (`snapshot.refs`) or resolves that kind of data
  by rule (`snapshot.rule_sources`); `settings.updated` deletes every page (contact details,
  opening date, location). `content.changed` is the event to emit when media, reviews, FAQs,
  promotions, categories, places, crops or galleries change (their admin screens do not exist
  yet; until then the 1-hour TTL bounds staleness).

## Permissions

| Permission | Who (seeded) | Allows |
|---|---|---|
| `content.read` | editor, manager, super_admin | builder, revisions, previews, lookups |
| `content.write` | editor, manager, super_admin | edit drafts and shared sections |
| `content.publish` | manager, super_admin | publish, schedule, restore, cancel |
| `content.html` | super_admin | create/change/delete `html` sections, activate types |

Every write is in the audit log (`page.draft_saved`, `page.published`, `page.draft_restored`,
`shared_section.updated`...).

## Demo content

`cargo run --bin seed_dev` builds, through the same service (so every section is validated), a
"coming soon" home page (hero with booking search, facilities, stays, activities, the
traditional fields, seasonal calendar, gallery, offers, testimonials, map with M'ssici, Alnif and
Rissani, FAQ, and two shared sections, pre-booking call to action and opening newsletter, also on
the About page), the top bar "Opening on {opening_date}: pre-book now", sets
`site.opening_date`, and publishes revision 1 of both pages. Placeholder photos are the front's
graded Commons pictures (credits in the site footer).
