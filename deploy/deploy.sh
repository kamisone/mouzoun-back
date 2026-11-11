#!/usr/bin/env bash
# Zero-downtime blue/green deployment of the Mouzoun API on a single VPS (no Kubernetes).
#
# Called by GitHub Actions over SSH, as root:
#   <env file content> | sudo bash ~/mouzoun/back/deploy.sh <image:tag> --env-stdin
# The env content (secrets) arrives through the SSH session's stdin and is written straight to
# $ROOT/back.env (root only, 600): it never exists as a file on the runner or in the user's home.
# The other release files (this script, nginx/) are uploaded next to the script by the CI.
#
#   0. installs the release files, nightly database backup (cron)
#   1. reconciles the private network, PostgreSQL and Redis containers with their desired
#      configuration (created on first deploy, recreated on their data volume when it changed)
#   2. installs the nginx site config shipped with this release (validated, rolled back on error)
#   3. starts the new release next to the running one (other "color", other loopback port)
#   4. waits for /health/ready — the release runs its DB migrations while booting
#   5. points nginx to the new release (graceful reload: in-flight requests finish)
#   6. drains and stops the previous release, prunes old images
#
# Any failure before step 5 leaves the current release serving untouched and fails the job.
# Every path/port can be overridden through the environment (used to test the script locally).

set -Eeuo pipefail

IMAGE="${1:?usage: deploy.sh <image:tag> [--env-stdin]}"
ENV_FROM_STDIN=false
[ "${2:-}" = "--env-stdin" ] && ENV_FROM_STDIN=true
RELEASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PREFIX="${PREFIX:-mouzoun}"
ROOT="${ROOT:-/srv/mouzoun/back}"
ENV_FILE="${ENV_FILE:-$ROOT/back.env}"
NETWORK="${NETWORK:-$PREFIX}"
NGINX_DIR="${NGINX_DIR:-/etc/nginx/mouzoun}"
# nginx is owned by the back: the site files are shipped and installed by this script.
NGINX_CONF_DIR="${NGINX_CONF_DIR:-/etc/nginx/conf.d}"
# <release file>:<installed path>
SITES="web.conf:$NGINX_CONF_DIR/default.conf routes.conf:$NGINX_DIR/routes.conf"
NGINX_TEST="${NGINX_TEST:-nginx -t -q}"
NGINX_RELOAD="${NGINX_RELOAD:-systemctl reload nginx}"
BACKUP_DIR="${BACKUP_DIR:-/srv/mouzoun/backups}"
CRON_FILE="${CRON_FILE-/etc/cron.d/mouzoun-backup}" # empty = do not manage the backup cron
POSTGRES_IMAGE="${POSTGRES_IMAGE:-postgres:17-alpine}"
REDIS_IMAGE="${REDIS_IMAGE:-redis:8-alpine}"
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-90}"
DRAIN_SECONDS="${DRAIN_SECONDS:-10}"
KEEP_IMAGES="${KEEP_IMAGES:-3}"
# Loopback ports of the two colors (never exposed publicly; nginx proxies to them).
PORT_BLUE="${PORT_BLUE:-18081}"
PORT_GREEN="${PORT_GREEN:-18082}"

PG_CONTAINER="$PREFIX-postgres"
REDIS_CONTAINER="$PREFIX-redis"
STATE_FILE="$ROOT/active-color"

log() { printf '\033[1;34m[deploy]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[deploy] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# Reads KEY from the env file without sourcing it (values may contain spaces, quotes, \n...).
env_get() { grep -E "^$1=" "$ENV_FILE" | tail -n1 | cut -d= -f2- | sed -E 's/^"(.*)"$/\1/'; }

container_exists() { docker container inspect "$1" >/dev/null 2>&1; }
container_running() { [ "$(docker container inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = "true" ]; }

wait_healthy_container() {
  local name="$1" i
  for ((i = 0; i < 60; i++)); do
    [ "$(docker container inspect -f '{{.State.Health.Status}}' "$name" 2>/dev/null)" = "healthy" ] && return 0
    sleep 1
  done
  fail "$name did not become healthy"
}

# 0. Release files -----------------------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || [ -n "${ALLOW_NON_ROOT:-}" ] || fail "must run as root (sudo bash deploy.sh ...)"
command -v docker >/dev/null || fail "docker is not installed"
command -v nginx >/dev/null || [ -n "${ALLOW_NON_ROOT:-}" ] || fail "nginx is not installed"

mkdir -p "$ROOT"
chmod 700 "$ROOT"
if [ "$ENV_FROM_STDIN" = true ]; then
  (umask 077 && cat >"$ENV_FILE.new")
  [ -s "$ENV_FILE.new" ] || { rm -f "$ENV_FILE.new"; fail "--env-stdin given but nothing received"; }
  mv -f "$ENV_FILE.new" "$ENV_FILE"
fi
if [ "$RELEASE_DIR" != "$ROOT" ] && [ -d "$RELEASE_DIR/nginx" ]; then
  rm -rf "$ROOT/nginx"
  cp -r "$RELEASE_DIR/nginx" "$ROOT/nginx"
fi
[ -f "$ENV_FILE" ] || fail "missing env file $ENV_FILE"
chmod 600 "$ENV_FILE"

if [ -n "$CRON_FILE" ]; then
  mkdir -p "$BACKUP_DIR"
  chmod 700 "$BACKUP_DIR"
  # Quoted heredoc: written literally, then placeholders replaced (no escaping pitfalls).
  # $POSTGRES_* are expanded inside the container, $(date) by cron's shell; % must be escaped for cron.
  cat >"$CRON_FILE.tmp" <<'CRON'
# Nightly PostgreSQL backup of Mouzoun, kept 14 days (managed by back/deploy/deploy.sh)
30 3 * * * root f=__DIR__/mouzoun-$(date +\%F).dump; docker exec __PG__ sh -c 'pg_dump -U "$POSTGRES_USER" -Fc "$POSTGRES_DB"' > "$f.part" && mv "$f.part" "$f" && find __DIR__ -name '*.dump' -mtime +14 -delete; rm -f "$f.part"
CRON
  sed -i.bak -e "s#__PG__#$PG_CONTAINER#g" -e "s#__DIR__#$BACKUP_DIR#g" "$CRON_FILE.tmp" && rm -f "$CRON_FILE.tmp.bak"
  mv -f "$CRON_FILE.tmp" "$CRON_FILE"
  chmod 644 "$CRON_FILE"
fi

PG_USER="$(env_get POSTGRES_USER)"
PG_PASSWORD="$(env_get POSTGRES_PASSWORD)"
PG_DB="$(env_get POSTGRES_DB)"
REDIS_PASSWORD="$(env_get REDIS_PASSWORD)"
[ -n "$PG_USER" ] && [ -n "$PG_PASSWORD" ] && [ -n "$PG_DB" ] || fail "POSTGRES_USER/POSTGRES_PASSWORD/POSTGRES_DB missing in env file"
[ -n "$REDIS_PASSWORD" ] || fail "REDIS_PASSWORD missing in env file"

# Secrets are passed to containers through root-only env files, never as command-line arguments
# (those are readable by any local user in the process list).
PG_ENV_FILE="$ROOT/postgres.env"
REDIS_ENV_FILE="$ROOT/redis.env"
API_ENV_FILE="$ROOT/api.env"
(
  umask 077
  # The API gets its own settings plus derived connection URLs, not the infrastructure secrets.
  grep -vE '^(POSTGRES_USER|POSTGRES_PASSWORD|POSTGRES_DB|REDIS_PASSWORD)=' "$ENV_FILE" >"$API_ENV_FILE" || true
  printf '%s\n' "POSTGRES_USER=$PG_USER" "POSTGRES_PASSWORD=$PG_PASSWORD" "POSTGRES_DB=$PG_DB" >"$PG_ENV_FILE"
  printf '%s\n' "REDIS_PASSWORD=$REDIS_PASSWORD" "REDISCLI_AUTH=$REDIS_PASSWORD" >"$REDIS_ENV_FILE"
  printf '%s\n' "DATABASE_URL=postgres://$PG_USER:$PG_PASSWORD@$PG_CONTAINER:5432/$PG_DB" \
    "REDIS_URL=redis://:$REDIS_PASSWORD@$REDIS_CONTAINER:6379" \
    "APP_HOST=0.0.0.0" "APP_PORT=8080" >>"$API_ENV_FILE"
)

# 1. Infrastructure ----------------------------------------------------------------------------
docker network inspect "$NETWORK" >/dev/null 2>&1 || {
  log "creating network $NETWORK"
  docker network create "$NETWORK" >/dev/null
}

# Dumps the database to $BACKUP_DIR (written to .part, renamed only when complete).
backup_database() {
  local file
  file="$BACKUP_DIR/mouzoun-$1-$(date +%Y%m%d-%H%M%S).dump"
  mkdir -p "$BACKUP_DIR"
  chmod 700 "$BACKUP_DIR"
  # shellcheck disable=SC2016 # expanded inside the container
  docker exec "$PG_CONTAINER" sh -c 'pg_dump -U "$POSTGRES_USER" -Fc "$POSTGRES_DB"' >"$file.part" &&
    mv "$file.part" "$file" && log "database backup: $file"
}

# Reconciles a long-lived infrastructure container with its desired configuration.
# The image, run arguments and env file content are fingerprinted into the `mouzoun.spec`
# label. A container whose fingerprint differs (created by an older deploy, changed settings,
# rotated secret) is recreated on its existing named volume: data is kept. The database is
# backed up first, and is never recreated if that backup fails.
ensure_container() {
  local name="$1" env_file="$2"
  shift 2 # remaining arguments: `docker run` options, image and command
  local spec current
  spec="$({ printf '%s\n' "$@"; cat "$env_file"; } | sha256sum | cut -c1-16)"
  if container_exists "$name"; then
    current="$(docker container inspect -f '{{index .Config.Labels "mouzoun.spec"}}' "$name")"
    if [ "$current" = "$spec" ]; then
      container_running "$name" || docker start "$name" >/dev/null
      return 0
    fi
    log "recreating $name: configuration changed (data volume kept)"
    if [ "$name" = "$PG_CONTAINER" ]; then
      if container_running "$name"; then
        backup_database before-recreate || fail "backup failed: $name left untouched"
      else
        log "WARNING: $name is stopped, recreating without a fresh backup (volume kept)"
      fi
    fi
    docker stop -t 60 "$name" >/dev/null || true
    docker rm "$name" >/dev/null
  else
    log "creating $name"
  fi
  docker run -d --name "$name" --label "mouzoun.spec=$spec" --env-file "$env_file" "$@" >/dev/null
}

# POSTGRES_PASSWORD is only read when the data directory is first initialised. Applying the
# current secret on every deploy makes rotating it (in GitHub) effective. Sent through stdin,
# never as a command-line argument.
sync_database_password() {
  local role="\"${PG_USER//\"/\"\"}\"" password="${PG_PASSWORD//\'/\'\'}"
  printf "ALTER ROLE %s WITH PASSWORD '%s';\n" "$role" "$password" |
    docker exec -i "$PG_CONTAINER" psql -q -v ON_ERROR_STOP=1 -U "$PG_USER" -d "$PG_DB" >/dev/null ||
    fail "could not apply POSTGRES_PASSWORD to role $PG_USER"
}

ensure_container "$PG_CONTAINER" "$PG_ENV_FILE" \
  --network "$NETWORK" --restart unless-stopped \
  -v "$PREFIX-pgdata:/var/lib/postgresql/data" \
  --health-cmd "pg_isready -U $PG_USER -d $PG_DB" --health-interval 5s --health-retries 12 \
  --shm-size 256m --log-opt max-size=20m --log-opt max-file=5 \
  "$POSTGRES_IMAGE"

# The password is written to a root-only config file at start (never on redis-server's command
# line), then the image entrypoint drops privileges to the `redis` user.
# shellcheck disable=SC2016 # $REDIS_PASSWORD is expanded inside the container
ensure_container "$REDIS_CONTAINER" "$REDIS_ENV_FILE" \
  --network "$NETWORK" --restart unless-stopped \
  -v "$PREFIX-redisdata:/data" \
  --health-cmd "redis-cli ping | grep -q PONG" --health-interval 5s --health-retries 12 \
  --log-opt max-size=20m --log-opt max-file=5 \
  "$REDIS_IMAGE" sh -c '
    umask 077
    printf "requirepass %s\nappendonly yes\nmaxmemory 256mb\nmaxmemory-policy allkeys-lru\n" \
      "$REDIS_PASSWORD" >/tmp/redis.conf
    chown redis /tmp/redis.conf
    exec docker-entrypoint.sh redis-server /tmp/redis.conf'

wait_healthy_container "$PG_CONTAINER"
wait_healthy_container "$REDIS_CONTAINER"
sync_database_password

# 2. Nginx site (the back owns nginx: website + /api) ------------------------------------------
mkdir -p "$NGINX_DIR"
[ -f "$NGINX_DIR/back-upstream.conf" ] ||
  echo "upstream mouzoun_back { server 127.0.0.1:$PORT_BLUE; keepalive 64; }" >"$NGINX_DIR/back-upstream.conf"

CHANGED_SITES=()
restore_site() {
  local dst
  for dst in ${CHANGED_SITES[@]+"${CHANGED_SITES[@]}"}; do
    if [ -f "$dst.bak" ]; then cp -f "$dst.bak" "$dst"; else rm -f "$dst"; fi
  done
}
for pair in $SITES; do
  src="$ROOT/nginx/${pair%%:*}"
  dst="${pair#*:}"
  [ -f "$src" ] || continue
  cmp -s "$src" "$dst" && continue
  log "installing nginx site config $dst"
  if [ -f "$dst" ]; then cp -f "$dst" "$dst.bak"; else rm -f "$dst.bak"; fi
  cp "$src" "$dst"
  CHANGED_SITES+=("$dst")
done
if [ ${#CHANGED_SITES[@]} -gt 0 ] && ! $NGINX_TEST; then
  restore_site
  fail "new nginx config is invalid, previous config restored"
fi

# 3. Start the new release next to the current one --------------------------------------------
ACTIVE="$(cat "$STATE_FILE" 2>/dev/null || echo none)"
if [ "$ACTIVE" = "blue" ]; then NEW=green NEW_PORT=$PORT_GREEN; else NEW=blue NEW_PORT=$PORT_BLUE; fi
NEW_CONTAINER="$PREFIX-back-$NEW"
OLD_CONTAINER="$PREFIX-back-$ACTIVE"

# Release tags are immutable (v<run_number>): pull only when not already on the server.
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  log "pulling $IMAGE"
  docker pull -q "$IMAGE" >/dev/null
fi

log "starting $NEW_CONTAINER on 127.0.0.1:$NEW_PORT (active: $ACTIVE)"
docker rm -f "$NEW_CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$NEW_CONTAINER" --network "$NETWORK" --restart unless-stopped --init \
  --env-file "$API_ENV_FILE" \
  -p "127.0.0.1:$NEW_PORT:8080" \
  --stop-timeout 30 --log-opt max-size=20m --log-opt max-file=5 \
  "$IMAGE" >/dev/null

# 4. Health gate -------------------------------------------------------------------------------
log "waiting for $NEW_CONTAINER to be ready"
ready=false
for ((i = 0; i < HEALTH_TIMEOUT; i++)); do
  if curl -fsS -m 2 "http://127.0.0.1:$NEW_PORT/health/ready" >/dev/null 2>&1; then ready=true; break; fi
  container_running "$NEW_CONTAINER" || break
  sleep 1
done
if [ "$ready" != true ]; then
  docker logs --tail 80 "$NEW_CONTAINER" >&2 || true
  docker rm -f "$NEW_CONTAINER" >/dev/null 2>&1 || true
  restore_site
  fail "new release failed its health check; $ACTIVE release is still serving"
fi

# 5. Switch traffic ----------------------------------------------------------------------------
UPSTREAM="$NGINX_DIR/back-upstream.conf"
cp -f "$UPSTREAM" "$UPSTREAM.bak"
echo "upstream mouzoun_back { server 127.0.0.1:$NEW_PORT; keepalive 64; }" >"$UPSTREAM"
if ! { $NGINX_TEST && $NGINX_RELOAD; }; then
  cp -f "$UPSTREAM.bak" "$UPSTREAM"
  restore_site
  docker rm -f "$NEW_CONTAINER" >/dev/null 2>&1 || true
  fail "nginx reload failed; traffic left on the $ACTIVE release"
fi
echo "$NEW" >"$STATE_FILE"
log "traffic switched to $NEW ($IMAGE)"

# 6. Retire the previous release ---------------------------------------------------------------
if [ "$ACTIVE" != "none" ] && container_exists "$OLD_CONTAINER"; then
  log "draining $OLD_CONTAINER for ${DRAIN_SECONDS}s"
  sleep "$DRAIN_SECONDS"
  docker stop -t 30 "$OLD_CONTAINER" >/dev/null || true
  docker rm "$OLD_CONTAINER" >/dev/null || true
fi

# Keep the last KEEP_IMAGES images of this repository (fast rollback), drop the rest.
REPO="${IMAGE%:*}"
docker images "$REPO" --format '{{.Repository}}:{{.Tag}}' | tail -n +"$((KEEP_IMAGES + 1))" |
  while read -r old_image; do docker rmi "$old_image" >/dev/null 2>&1 || true; done

log "done: $NEW_CONTAINER is live"
