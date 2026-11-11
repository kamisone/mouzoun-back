# Mouzoun — back (Rust API)

Axum 0.8 · Tokio · SQLx (PostgreSQL 16+, 17 in production) · Redis 8 · Google Cloud Storage (official SDK) · JWT + Argon2

## Run locally

```bash
docker compose up -d          # PostgreSQL (:5433) + Redis (:6380)
cp .env.example .env          # then fill the GCS_* values (see "Google Cloud Storage")
cargo run                     # http://localhost:8080, migrations run at startup
cargo test                    # unit + integration tests (needs the compose services)
cargo run --bin seed_dev      # optional: fr/en/ar demo content (SEED_ADMIN_EMAIL / SEED_ADMIN_PASSWORD)
```

The first super admin is created from `ADMIN_EMAIL` / `ADMIN_PASSWORD` if none exists.
The database (schema, conventions, migrations, outbox, adding a language or a product kind) is
documented in [`docs/database/README.md`](docs/database/README.md).

## Architecture

```
src/
  main.rs            thin launcher
  app.rs             composition root: infra → services → HTTP + jobs, graceful shutdown
  config.rs          typed, validated env config (secrets never logged)
  state.rs           AppState; services exposed to handlers via FromRef
  error.rs           AppError → {"error": {"code", "details"}}
  web/               router + middleware, health probes, JSON extractor, pagination
  infra/
    db.rs            Postgres pool + migrations
    cache.rs         Redis JSON cache (namespaced, failures = cache miss)
    storage/         ObjectStorage trait · gcs.rs (production) · memory.rs (tests)
  auth/              login/logout/me, HttpOnly session cookie, JWT, Argon2, require_staff guard
  db/                data layer: one module per schema section (models, text enums, Money,
                     translations), mirrors migrations/ exactly
  services/          business rules, no HTTP: booking (accommodation, activity, lifecycle),
                     pricing (pure engine + loader), inventory & slot generators, catalog reads
                     with locale fallback, numbering, outbox, identity
  modules/           HTTP modules, each: routes · handlers · service · model
    media/           media library on GCS (+ policy.rs: allow-list, magic bytes, keys)
  bin/seed_dev.rs    development demo content
  jobs/              background workers: booking holds, outbox, inventory horizon, uploads
migrations/          reversible SQL migrations 0001–0012 (sqlx); .sqlx/ = offline query metadata
docs/database/       database guide
tests/               integration tests: real router + Postgres + Redis, in-memory storage
                     (the in-memory storage is compiled for tests only: `test-utils` feature)
deploy/             CI/CD: deploy.sh (blue/green), nginx site (web.conf + routes.conf)
```

Rules: handlers only do HTTP; business rules live in `services/` (or a module's service);
SQL is compile-time checked (`query!`/`query_as!`); modules never touch vendor SDKs directly
(storage goes through `ObjectStorage`). New HTTP module (products, bookings...): add
`modules/<name>` exposing `public_routes()` and/or `admin_routes()` on top of `services/`, and
mount them in `web/mod.rs` (admin routes are guarded automatically).

## Media uploads (Google Cloud Storage)

Files go **directly from the browser to GCS**; the API never streams bytes, so uploads don't
consume API memory, bandwidth or request time.

```
admin ── POST /v1/admin/media/uploads {filename, content_type, size_bytes} ──► API
      ◄── {media_id, expires_at, upload: {url, method: PUT, headers}} ── (pending, kept in Redis)
admin ── PUT <signed url> (file, with the returned headers) ──────────────► GCS
admin ── POST /v1/admin/media/{id}/complete ──► API checks size + type + magic bytes ► media_assets row
```

- The signed URL (V4, 15 min) pins content type, **exact size** and cache headers.
- Completion re-checks what actually landed in the bucket and sniffs the first bytes:
  a renamed `.exe`/`.html` is deleted and refused.
- Allowed: JPEG, PNG, WebP, AVIF, PDF (SVG refused: it can carry scripts). Max 15 MB.
- Keys are opaque (`media/YYYY/MM/<uuid>.<ext>`), never derived from filenames, and
  public objects are served with `Cache-Control: public, max-age=31536000, immutable`.
- Only completed uploads exist in `media_assets`; pending ones live in Redis until completed.
- Deleting media is a soft delete (`deleted_at`): references from content keep working.
- Uploads never completed are purged hourly after 24 h (`jobs/media_cleanup.rs`).
- Private media (`"visibility": "private"`) needs `GCS_PRIVATE_BUCKET_NAME`; it is only ever
  served through short-lived signed download URLs.

### Bucket setup (once per environment)

```bash
B=mouzoun-local                                   # or the production bucket
SA=mouzoun-storage-buckets@mouzoun.iam.gserviceaccount.com

# Service account: read/write objects in this bucket only
gcloud storage buckets add-iam-policy-binding gs://$B --member=serviceAccount:$SA --role=roles/storage.objectUser

# Public read for the public bucket (never do this on the private bucket)
gcloud storage buckets update gs://$B --uniform-bucket-level-access
gcloud storage buckets add-iam-policy-binding gs://$B --member=allUsers --role=roles/storage.objectViewer

# Allow browser uploads from the front origins (edit deploy/gcs-cors.json first)
gcloud storage buckets update gs://$B --cors-file=deploy/gcs-cors.json
```

Credentials: copy `project_id`, `client_email`, `private_key_id`, `private_key` from the
service-account JSON key into `GCS_*` (keep `private_key` on one line with its `\n`).
On GCP infrastructure, leave `GCS_CLIENT_EMAIL`/`GCS_PRIVATE_KEY` empty to use
Application Default Credentials.

## Authentication

- **Browser session = `HttpOnly` cookie.** Login sets `__Host-mouzoun_session` (`Secure`,
  `SameSite=Lax`, `Path=/`, no `Domain`) holding a signed JWT (`JWT_TTL_MINUTES`). JavaScript never
  sees the token, so an XSS bug cannot steal the session; logout expires the cookie.
- **CSRF**: `SameSite=Lax` keeps the cookie off cross-site writes, and every POST/PUT/PATCH/DELETE
  whose `Origin` is not in `CORS_ORIGINS` is refused (403).
- Non-browser clients may send the same JWT as `Authorization: Bearer <token>`.
- `COOKIE_SECURE=false` only for plain-HTTP local development (the cookie is then named
  `mouzoun_session`, since the `__Host-` prefix requires `Secure`).

## Endpoints (v1)

| Method | Path | Access |
|---|---|---|
| GET | `/health/live` · `/health/ready` | public |
| POST | `/v1/auth/login` · `/v1/auth/logout` | public (set / expire the session cookie) |
| GET | `/v1/auth/me` | authenticated |
| GET | `/v1/admin/media` (`?kind=&page=&per_page=`) | staff |
| POST | `/v1/admin/media/uploads` · `/v1/admin/media/{id}/complete` | staff |
| GET · DELETE | `/v1/admin/media/{id}` | staff (DELETE = soft delete) |

`/v1/auth/login` and `/v1/auth/me` return the profile: `is_staff`, `roles` and the effective
`permissions` codes. The former `/v1/activities` endpoints were removed with the old schema;
catalogue, booking and content endpoints will be built on `services/` (products, bookings, pages).

Public and back-office APIs are separate namespaces. Every route under `/v1/admin` is protected
by one `require_staff` middleware applied to the whole router: a new admin route cannot be
exposed by forgetting a check in its handler, and public slugs never collide with admin paths.

Add a migration: `sqlx migrate add -r <name>`, then `cargo sqlx prepare -- --all-targets --all-features`
(see [`docs/database/README.md`](docs/database/README.md)).

## CI/CD — push to `dev` = deployed

No Kubernetes, no compose in production: GitHub Actions builds an image, the VPS runs it with
plain Docker behind the host nginx, and every release is a **zero-downtime blue/green switch**.

```
push dev ─► test (fmt, clippy, unit + integration tests on Postgres/Redis)
         ─► build image  <DOCKER_HUB_USERNAME>/mouzoun_back:v<run_number>  (+ :dev)
         ─► deploy over SSH: deploy/deploy.sh
              1. reconciles network + mouzoun-postgres + mouzoun-redis (see below)
              2. installs the nginx site: web.conf → conf.d/default.conf, routes.conf →
                 /etc/nginx/mouzoun/routes.conf (validated, previous restored on any failure)
              3. starts the new release on the free color (blue 127.0.0.1:18081 / green :18082)
              4. waits for /health/ready (migrations run during boot)
              5. switches the nginx upstream, graceful reload
              6. drains then stops the previous release, keeps the last 3 images
```

A release that fails its health check never receives traffic: the job fails and the previous
release keeps serving. **Rollback**: Actions → "CI/CD - DEV - BACK" → Run workflow → `image_tag: v41`.

> Migrations run while the previous release is still serving, so keep them backward compatible
> (add columns/tables first, drop them in a later release).

### VPS layout

```
nginx (host)  :80/:443 ── DOMAIN/api/ ──► 127.0.0.1:1808{1,2}  mouzoun-back-{blue,green}
              │           DOMAIN/     ──► 127.0.0.1:1300{1,2}  mouzoun-front-{blue,green}
              └── 127.0.0.1:18080 (front SSR → API, loopback only)
docker network "mouzoun": back ⇄ mouzoun-postgres, mouzoun-redis (no published ports)
/srv/mouzoun/{back,front}   installed release + env (root, chmod 600)    /srv/mouzoun/backups  nightly pg_dump
/etc/nginx/conf.d/default.conf (previous kept as .bak) · /etc/nginx/mouzoun/back-upstream.conf · /etc/ssl/mouzoun/
```

**nginx is owned by the back repo**: the site file, the API upstream and every reload. The front
deploy never touches nginx and does not depend on the back being deployed.

One host. `deploy/nginx/web.conf` (→ `conf.d/default.conf`) keeps the original server structure:
an HTTP `default_server` that includes `/etc/ssl/mouzoun/ssl_redirect.conf*` (may be an
unconditional `return 301`, so it is **never** included in the HTTPS server) and an HTTPS server that
includes `/etc/ssl/mouzoun/ssl_config.conf*`. Both serve the same routes from
`deploy/nginx/routes.conf` (→ `/etc/nginx/mouzoun/routes.conf`):

| URL | Goes to |
|---|---|
| `https://DOMAIN/api/...` | API — the `/api` prefix is stripped (`/api/v1/auth/me` → `/v1/auth/me`), rate limited, login 5/min |
| `https://DOMAIN/...` | website + `/admin` (Nuxt) |
| `https://www.DOMAIN/...` | 301 → `https://DOMAIN/...` |
| `127.0.0.1:18080` (loopback only) | API, for the website's server-side rendering |

- API: blue/green upstream file switched by the back deploy.
- Website: both front ports are listed; the front deploy starts the new color, keeps both running
  15 s, then stops the old one; nginx skips the stopped port and retries requests on the live one
  (verified on Linux with host networking: 6 switches, ~59 000 requests, 0 errors).

### First-time setup

**Infrastructure containers are reconciled on every deploy.** Each one carries a fingerprint of
its desired configuration (image, options, secrets) in a `mouzoun.spec` label. When it differs
(older deploy, changed settings, rotated `POSTGRES_PASSWORD` / `REDIS_PASSWORD`), the container is
recreated on its named volume (data kept); the database is backed up first to
`/srv/mouzoun/backups/mouzoun-before-recreate-*.dump` and is never recreated if that backup fails.
The database role password is then synced from the secret, so rotating it in GitHub just works.
Secrets reach containers only through root-only env files (`/srv/mouzoun/back/*.env`, mode 600),
never as command-line arguments, and each container only receives its own: the API gets its
settings plus `DATABASE_URL`/`REDIS_URL`, Redis runs as the unprivileged `redis` user.

The deploy is self-bootstrapping: no script to run by hand on the VPS. It connects as
`SERVER_USER`, uploads the release to `~/mouzoun/back`, and runs `deploy.sh` as root through
`sudo` (password from `SERVER_USER_PASSWORD`). The first run creates `/srv/mouzoun`, the Docker
network, PostgreSQL, Redis, the nginx site and the nightly backup cron (03:30, 14 days kept in
`/srv/mouzoun/backups`). Requirements on the VPS: Docker and nginx installed (already the case for
silomis), `SERVER_USER` allowed to `sudo`.

1. **DNS**: `DOMAIN` and `www.DOMAIN` → VPS IP (Cloudflare proxied recommended).
2. **GitHub → back repo → Settings → Environments → `dev`**: one secret per value, like silomis.
   On every deploy the workflow assembles them in memory and streams them through the SSH session
   into `/srv/mouzoun/back/back.env` (root only, 600): no secret file is ever uploaded or left in a
   home directory; the domain (nginx) and CORS origins are derived from `APP_URL`.

   | Secret | Example | Status |
   |---|---|---|
   | `SERVER_HOST`, `SERVER_USER`, `SERVER_SSH_KEY`, `SERVER_USER_PASSWORD` | | existing |
   | `DOCKER_HUB_USERNAME`, `DOCKER_HUB_PASSWORD` | | existing |
   | `APP_URL` | `https://mouzoun.com` | to add |
   | `POSTGRES_USER` / `POSTGRES_DB` | `mouzoun` / `mouzoun` | to add |
   | `POSTGRES_PASSWORD`, `REDIS_PASSWORD` | `openssl rand -hex 24` (keep it URL-safe) | to add |
   | `JWT_SECRET` | `openssl rand -hex 32` | to add |
   | `ADMIN_EMAIL`, `ADMIN_PASSWORD` | first admin account | to add |
   | `GCS_BUCKET_NAME`, `GCS_PROJECT_ID`, `GCS_CLIENT_EMAIL`, `GCS_PRIVATE_KEY_ID`, `GCS_PRIVATE_KEY` | from the service-account JSON; the key can be pasted as is (multi-line) | to add |
   | `GCS_PRIVATE_BUCKET_NAME`, `GCS_PUBLIC_BASE_URL` | | optional |

   Non-secret settings (log level, DB pool size, token lifetime) are set in the workflow.
   If your secrets are repository secrets rather than `dev` environment secrets, they work too
   once the `dev` environment exists (or remove `environment: dev` from the workflow).
3. **GCS bucket**: see "Bucket setup" above, with the production origins in `deploy/gcs-cors.json`.
4. Push the **back** and the **front** to `dev`, in any order (until the back is deployed the
   website is up but shows no API data, and is only reachable once nginx is configured by the back).
5. **TLS**: the certificate files and `/etc/ssl/mouzoun/ssl_config.conf` / `ssl_redirect.conf` are
   managed on the server, by hand. The deploy never writes them, nginx only includes them
   (`ssl_redirect.conf*` in the HTTP server, `ssl_config.conf*` in the HTTPS server). Renewing the
   certificate = replacing the files they reference, then `sudo nginx -t && sudo systemctl reload nginx`.

Docker Hub repositories are public by default: make `mouzoun_back` / `mouzoun_front` private
(images contain no secrets — configuration is injected at runtime — but there is no reason to publish them).
