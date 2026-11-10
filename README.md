# Mouzoun — back (Rust API)

Axum 0.8 · Tokio · SQLx (PostgreSQL 17) · Redis 8 · Google Cloud Storage (official SDK) · JWT + Argon2

## Run locally

```bash
docker compose up -d          # PostgreSQL (:5433) + Redis (:6380)
cp .env.example .env          # then fill the GCS_* values (see "Google Cloud Storage")
cargo run                     # http://localhost:8080, migrations run at startup
cargo test                    # unit + integration tests (needs the compose services)
```

The first admin is created from `ADMIN_EMAIL` / `ADMIN_PASSWORD` if none exists.

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
  auth/              login, /me, JWT, Argon2, AuthUser / AdminUser extractors
  modules/           business modules, each: routes · handlers · service · repository · model
    media/           media library on GCS (+ policy.rs: allow-list, magic bytes, keys)
    activities/      catalogue, cover image from the media library
  jobs/              background workers (abandoned upload cleanup)
migrations/          SQL migrations (sqlx)
tests/               integration tests: real router + Postgres + Redis, in-memory storage
deploy/             CI/CD: deploy.sh (blue/green), nginx site, TLS snippets, env template
```

Rules: handlers only do HTTP; services hold business rules; repositories only SQL;
modules never touch vendor SDKs directly (storage goes through `ObjectStorage`).
New module (camps, bookings, products...): copy `modules/activities`.

## Media uploads (Google Cloud Storage)

Files go **directly from the browser to GCS**; the API never streams bytes, so uploads don't
consume API memory, bandwidth or request time.

```
admin ── POST /v1/media/uploads {filename, content_type, size_bytes} ──► API
      ◄── {media: {id, status: "pending"}, upload: {url, method: PUT, headers}} ──
admin ── PUT <signed url> (file, with the returned headers) ──────────────► GCS
admin ── POST /v1/media/{id}/complete ──► API checks size + type + magic bytes ► "ready"
```

- The signed URL (V4, 15 min) pins content type, **exact size** and cache headers.
- Completion re-checks what actually landed in the bucket and sniffs the first bytes:
  a renamed `.exe`/`.html` is deleted and refused.
- Allowed: JPEG, PNG, WebP, AVIF, PDF (SVG refused: it can carry scripts). Max 15 MB.
- Keys are opaque (`media/YYYY/MM/<uuid>.<ext>`), never derived from filenames, and
  public objects are served with `Cache-Control: public, max-age=31536000, immutable`.
- Media in use (e.g. an activity cover) cannot be deleted (409).
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

## Endpoints (v1)

| Method | Path | Access |
|---|---|---|
| GET | `/health/live` · `/health/ready` | public |
| POST | `/v1/auth/login` | public |
| GET | `/v1/auth/me` | authenticated |
| GET | `/v1/activities` · `/v1/activities/{slug}` | public (Redis-cached) |
| GET | `/v1/activities/admin?page=&per_page=` | staff |
| POST | `/v1/activities` | staff |
| GET · PUT · DELETE | `/v1/activities/id/{id}` | staff |
| POST | `/v1/media/uploads` | staff |
| POST | `/v1/media/{id}/complete` | staff |
| GET | `/v1/media?status=&page=&per_page=` · `/v1/media/{id}` | staff |
| DELETE | `/v1/media/{id}` | staff (409 if in use) |

Add a migration: `sqlx migrate add <name>` (`cargo install sqlx-cli --no-default-features --features postgres,rustls`).

## CI/CD — push to `dev` = deployed

No Kubernetes, no compose in production: GitHub Actions builds an image, the VPS runs it with
plain Docker behind the host nginx, and every release is a **zero-downtime blue/green switch**.

```
push dev ─► test (fmt, clippy, unit + integration tests on Postgres/Redis)
         ─► build image  <DOCKER_HUB_USERNAME>/mouzoun_back:v<run_number>  (+ :dev)
         ─► deploy over SSH: deploy/deploy.sh
              1. creates network + mouzoun-postgres + mouzoun-redis on the first run
              2. installs deploy/nginx/default.conf (validated, previous restored on any failure)
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
nginx (host)  :80/:443 ── api.DOMAIN ──► 127.0.0.1:1808{1,2}  mouzoun-back-{blue,green}
              │           DOMAIN     ──► 127.0.0.1:1300{1,2}  mouzoun-front-{blue,green}
              └── 127.0.0.1:18080 (front SSR → API, loopback only)
docker network "mouzoun": back ⇄ mouzoun-postgres, mouzoun-redis (no published ports)
/srv/mouzoun/{back,front}   installed release + env (root, chmod 600)    /srv/mouzoun/backups  nightly pg_dump
/etc/nginx/conf.d/default.conf (previous kept as .bak) · /etc/nginx/mouzoun/ (upstreams, routes, snippets) · /etc/ssl/mouzoun/
```

nginx keeps your previous structure: one HTTP `default_server` (redirects to HTTPS when `/etc/ssl/mouzoun/ssl_redirect.conf` exists) and one TLS server (`/etc/ssl/mouzoun/ssl_config.conf`), each include used once; requests are routed by host (`api.` → API, the rest → website).

### First-time setup

The deploy is self-bootstrapping: no script to run by hand on the VPS. It connects as
`SERVER_USER`, uploads the release to `~/mouzoun/back`, and runs `deploy.sh` as root through
`sudo` (password from `SERVER_USER_PASSWORD`). The first run creates `/srv/mouzoun`, the Docker
network, PostgreSQL, Redis, the nginx site and the nightly backup cron (03:30, 14 days kept in
`/srv/mouzoun/backups`). Requirements on the VPS: Docker and nginx installed (already the case for
silomis), `SERVER_USER` allowed to `sudo`.

1. **DNS**: `DOMAIN`, `www.DOMAIN`, `api.DOMAIN` → VPS IP (Cloudflare proxied recommended).
2. **GitHub → back repo → Settings → Environments → `dev`**: one secret per value, like silomis.
   The env file on the server (`/srv/mouzoun/back/back.env`, root only) is generated from them on
   every deploy; the domain, `api.` sub-domain and CORS origins are derived from `APP_URL`.

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
4. Push the **back** to `dev` first (database + nginx site), then the **front**.
5. **TLS** (Cloudflare Origin Certificate, mode "Full (strict)"): put `origin.pem` / `origin.key` in
   `/etc/ssl/mouzoun/`, copy `deploy/ssl/ssl_config.conf.example` → `/etc/ssl/mouzoun/ssl_config.conf`
   (and `ssl_redirect.conf.example` → `ssl_redirect.conf`), then `sudo nginx -t && sudo systemctl reload nginx`.
   Until then the site is served over HTTP (fine behind Cloudflare in "Flexible" mode for a first test).

Docker Hub repositories are public by default: make `mouzoun_back` / `mouzoun_front` private
(images contain no secrets — configuration is injected at runtime — but there is no reason to publish them).
