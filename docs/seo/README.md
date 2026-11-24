# SEO: route registry, redirects, sitemaps, robots.txt

Code: `back/src/modules/seo/` (API), `front/server/` (Nuxt server: middleware, sitemap and robots
routes), `front/app/composables/useSeoHead.ts` (head tags), Admin › SEO (`/admin/seo`).
Migration `0032_seo`. Permission `seo.manage` (super_admin, manager, editor); `content.read` sees
Admin › SEO read-only.

## Route registry (`modules/seo/routes.rs`)

The single place that knows every public URL. Paths follow the Nuxt routes, which are fixed in the
code (not settings): no prefix for the default language, `/en/…`, `/ar/…`.

| content | path | sitemap file |
|---|---|---|
| home (builder home page, or the static home) | `/`, `/en`, `/ar` | `pages.xml` |
| listings | `/stays`, `/activities`, `/boutique` (`/en/shop`) | `pages.xml` |
| builder pages (`page`, `landing`, `legal`) | `/{slug}` (a slug may contain `/`: nested pages) | `pages.xml` |
| posts (`page_type = 'post'`) | `/{slug}` | `posts.xml` |
| accommodations, activities, packages | `/stays/{slug}`, `/activities/{slug}`, `/packages/{slug}` | `accommodations.xml`, `activities.xml`, `packages.xml` |
| shop categories | `/boutique/{slug}` | `shop-categories.xml` |
| shop products | `/boutique/p/{slug}` | `shop-products.xml` |

Extras, places, tags and the other category scopes have no page. `shop.route_prefix` only feeds
email links: the shop's path segment is fixed in `pages/shop/*.vue`.

Functions: `entries(filter)` (every entry, one sitemap file, or one entity), `path_for`,
`paths_for`, `lookup(path)` (what a path points to, served or not), `resolve(path)` (live
content only), `indexability(...)` and the pure rule `judge(ctx, entry, locale)`.

### Indexability (one rule)

`judge` is used by the sitemap, the `<head>` tags (`GET /v1/seo/head?path=`, read by `app.vue`
through `useSeoHead`) and the address tester, so they never disagree. A language version is **not
indexable** when (reason code in brackets):

- the language is not active (`locale_inactive`);
- the content is soft-deleted (`deleted`), not published (`not_published`), published in the
  future (`scheduled`), under a page that is not public (`parent_not_public`), an inactive shop
  category (`inactive`), a shop category without a published product (`no_published_products`),
  or in a closed shop (`shop_closed`);
- there is no translation in that language (`translation_missing`: a fallback-rendered page is not
  its own URL);
- the translation says `noindex` (`noindex`), the page is out of the sitemap (`in_sitemap = false`,
  `not_in_sitemap`), or its `canonical_url` points elsewhere (`canonical_elsewhere`).

Head tags: canonical = the content's own path in that language (or its `canonical_url`; `?page=n`
kept on listings, never filters), `hreflang` to every indexable language version plus
`x-default` (default language), `<meta name="robots" content="noindex, follow">` when not
indexable. Paths the registry does not know (account, cart…) keep the i18n defaults.

## Redirects

### Created automatically (`modules/seo/redirects.rs`)

Content writers call `snapshot()` before and `after_change()` after a change, inside the same
transaction:

- **path moved** (slug change per language; nested pages follow their parent's new slug, each
  with its own redirect) → one 301 per changed language (`source = auto`, entity and locale filled);
- **language removed** → 301 to the closest public parent;
- **unpublished / archived** → 301 to the closest public parent (parent category or page, the
  listing of its type, or the home), written **inactive** with a note; **deleted** → active (the
  unpublish ones become active too). Published again → its own paths win;
- **live content wins**: a path taken by content (renamed back, published again, created) loses
  its redirect (audit `redirect.claimed`);
- **one hop**: adding A → B rewrites every X → A into X → B; a manual target that is itself
  redirected is replaced by its final target;
- **never a loop**: refused by the API (`to_path: loop`) and by the trigger `redirects_no_loop`.

Wired today: builder pages (draft save writes the slug), shop products (create, update, bulk
publish/archive/draft, delete), shop categories (save, deactivate, delete). An inactive category
still answers its own 301 to the nearest visible parent (category endpoint, computed from the tree,
not from `redirects`). Accommodations, activities and packages have no admin editor yet: the
service is ready (the seed uses it); call `snapshot`/`after_change` from that editor when it exists.

### Served (Nuxt server, `front/server/middleware/seo-redirects.ts`)

Before any page renders, for GET/HEAD outside `/api`, `/_nuxt`, `/admin`, static files:

1. **normalise** (percent-decode, NFC, no trailing slash, no double slash; case kept) — if that
   changed the path, 301 to the clean path;
2. **look up** the path in the active redirects; answer the stored code to `to_path`, query string
   kept; `Location` absolute from `NUXT_PUBLIC_I18N_BASE_URL` (never the Host header);
   `Cache-Control: public, max-age=3600` for 301/308, `no-store` for 302/307. A second hop (should
   not happen) is followed once and logged.

The map comes from `GET /v1/seo/redirects/map` (Redis `seo:redirects`, version
`seo:redirects:version`); the Nuxt server keeps a copy and re-checks `/version` at most every 15 s.
API or Redis down: the last copy keeps serving, then no redirects; never a 500. Without Redis the
API builds the map from Postgres.

### Visits and 404s

Never a Postgres write per request: the Nuxt server counts redirect visits and 404s (pages that
end in 404, minus `.php`, `/wp-`, `.env`, `/.git`, paths over 300 characters) and sends them every
10 s to `POST /v1/seo/hits` (header `x-internal-token` = `INTERNAL_API_TOKEN` / 
`NUXT_INTERNAL_API_TOKEN`; when the API has no token it accepts any caller, the counts only feed
statistics). The API adds them to Redis hashes (`HINCRBY`); the job `seo hits` writes them every
minute to `redirects.hits` / `last_hit_at` and `not_found_log`, which keeps at most 5,000 rows (the
least visited, oldest go); `seo 404 purge` deletes rows not seen for 90 days, daily.

## Caching and invalidation

| key | content | invalidated by |
|---|---|---|
| `seo:redirects`, `seo:redirects:version` | redirect map | `seo.changed` with `redirects = true` (every redirect write) |
| `seo:sitemap:{file}:{page}`, `seo:sitemap:index` | sitemap XML (24 h) | `product.updated` (its kind's file; shop products also `shop-categories`), `page.updated` (`pages` or `posts`), `content.changed` of a category (`shop-categories`), `seo.changed` (listed files), `settings.updated` (all) |
| `seo:head:{path}` | head tags (1 h) | with any sitemap invalidation |

Handler: `SeoCacheHandler` (outbox worker). Files are rebuilt lazily on the next request;
"Reconstruire" in Admin › SEO drops everything and builds the index.

## Sitemaps

Built by the API from the registry, served by Nuxt server routes as is:
`/sitemap.xml` (index of the non-empty files) and `/sitemaps/{file}.xml`, split past 10,000 URLs
(`activities-2.xml`…). One `<url>` per indexable language version, with `xhtml:link
rel="alternate"` to every indexable version and `x-default` (default language); `<lastmod>` = the
latest content or translation change (publish date for pages), never the build time; no
`priority` / `changefreq`; up to 11 public images (`image:image`, cover then gallery). Absolute
URLs from `PUBLIC_SITE_URL`, XML escaped, non-ASCII percent-encoded.
`application/xml`, `Cache-Control: public, max-age=3600`.

Tests validate the XML against the sitemaps.org and Google image schemas
(`back/tests/it/seo_xsd/`, with `xmllint` when installed) and the full-stack suite requests every
listed URL: 200, self-canonical, no `noindex`.

## robots.txt (per environment)

`front/server/routes/robots.txt.ts`. `NUXT_SITE_ENV=production` (the deploy script's default):
`Allow: /`, `Disallow:` the back office, `/api`, cart / checkout / orders of the shop, booking,
checkout, trip, account and wishlist pages in every language, internal search results (`?q=`),
then `Sitemap: {site}/sitemap.xml` and the setting `seo.robots_extra`. Any other value (local
development, staging): `Disallow: /` and `X-Robots-Tag: noindex, nofollow` on every response.

## Submitting the sitemap

- **Google Search Console**: add the property (domain `mouzoun.com`, DNS verification), then
  Indexing › Sitemaps › enter `sitemap.xml` › Submit. Google re-reads it on its own afterwards
  (the old "ping" endpoint is retired).
- **Bing Webmaster Tools**: add the site (or import it from Search Console), then Sitemaps ›
  Submit sitemap › `https://mouzoun.com/sitemap.xml`.

Both also find it through the `Sitemap:` line of robots.txt.
