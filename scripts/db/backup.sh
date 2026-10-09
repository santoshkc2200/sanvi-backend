#!/usr/bin/env bash
# TASK-025 (FR-1119): the backup half of the restore drill.
#
# Brings up a dedicated drill Postgres (WAL archiving on, per-session
# volumes), loads the phase-11 schema and the production-shaped scale data,
# takes a `pg_basebackup`, and records a manifest the restore half
# (`scripts/db/restore.sh`) replays against.
#
# The drill is self-contained: it never touches the dev stack's `postgres`
# service or the shared test server. The scale data is what makes the timed
# restore a restore of something, and the two entitlement grants — one
# before the base backup, one after, each writing real audit-chain and
# outbox rows — are what the PITR consistency checks read.
#
# Everything heavy stays inside the session's docker volumes (no host bind
# mounts — the same Windows rule `scripts/test-db.sh` follows); the session
# directory holds only small text artifacts the host writes itself:
#
#   .drill/<session>/manifest.json     timestamps, markers, counts, profile
#   .drill/<session>/base.tar.enc      the backup set, encrypted at rest
#                                      (AES-256-CBC/PBKDF2), when openssl
#                                      is available
#
# WAL archives and the base backup live in the per-session docker volumes
# `sanvi_drill_wal_<sid>` / `sanvi_drill_backup_<sid>`; the restore half
# mounts them. Old session directories are pruned by --retention-days — the
# local drill's stand-in for the phase-05 backup policy (the managed policy
# is parked in docs/release/needs-humans.md).
#
# Usage:
#   scripts/db/backup.sh [--scale-seed N] [--retention-days N]
set -euo pipefail

# Git Bash (MSYS) rewrites absolute paths in arguments — a `-v vol:/backup`
# mount would silently land at C:/Program Files/Git/backup inside the
# container and every artifact would stay in the container layer. Disable
# the conversion for this script; harmless (unknown) under POSIX bash.
export MSYS_NO_PATHCONV=1

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

SCALE_SEED=1
RETENTION_DAYS=30
while [[ $# -gt 0 ]]; do
    case "$1" in
        --scale-seed) SCALE_SEED="$2"; shift 2 ;;
        --retention-days) RETENTION_DAYS="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

for tool in docker jq; do
    command -v "$tool" >/dev/null 2>&1 || { echo "backup: $tool is required" >&2; exit 1; }
done

PG_IMAGE="postgres:18"
SRC_CONTAINER="sanvi-drill-src"
DB_NAME="sanvi_scale_drill"
DB_USER="postgres"
DB_PASSWORD="drill-local-only"
HOST_PORT="${DRILL_PG_PORT:-55432}"

session="$(date -u +%Y%m%dT%H%M%SZ)"
session_dir="$root/.drill/$session"
mkdir -p "$session_dir"

port_in_use() {
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && { exec 3>&- 3<&-; return 0; } || return 1
}
if port_in_use "$HOST_PORT"; then
    echo "backup: port $HOST_PORT is busy; set DRILL_PG_PORT" >&2
    exit 1
fi

echo "backup: session $session"

WAL_VOLUME="sanvi_drill_wal_${session}"
BACKUP_VOLUME="sanvi_drill_backup_${session}"
docker volume create "$WAL_VOLUME" >/dev/null
docker volume create "$BACKUP_VOLUME" >/dev/null

cleanup() {
    docker rm -f "$SRC_CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --name "$SRC_CONTAINER" \
    -p "127.0.0.1:${HOST_PORT}:5432" \
    -e "POSTGRES_USER=${DB_USER}" \
    -e "POSTGRES_PASSWORD=${DB_PASSWORD}" \
    -v "${WAL_VOLUME}:/wal_archive" \
    -v "${BACKUP_VOLUME}:/backup" \
    "$PG_IMAGE" \
    postgres -c archive_mode=on \
             -c "archive_command=test ! -f /wal_archive/%f && cp %p /wal_archive/%f" \
             -c archive_timeout=15s \
             -c shared_preload_libraries=pg_stat_statements >/dev/null

wait_ready() {
    local container="$1" tries="${2:-60}"
    for _ in $(seq 1 "$tries"); do
        if docker exec "$container" pg_isready -U "$DB_USER" -d postgres >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    echo "backup: $container never became ready" >&2
    return 1
}
wait_ready "$SRC_CONTAINER"

# The named-volume mountpoints arrive root-owned; the postgres user writes
# both the archive (archive_command) and the base backup, so hand the
# mountpoints over before either runs. A silent archive failure here would
# only surface later, as a restore missing every post-backup transaction.
docker exec -u root "$SRC_CONTAINER" chown postgres:postgres /wal_archive /backup

# The production role split (scripts/db/init/01_roles.sql on the dev stack):
# migrations run under the schema owner, the application role stays
# least-privilege. The roles are cluster-wide, so the base backup carries
# them to the restore side unchanged. The drill database is created here —
# owned by the migrator — because a POSTGRES_DB bootstrap would make it
# postgres-owned and every subsequent migration would fail on schema public.
psql_src() {
    docker exec -e PGPASSWORD="$DB_PASSWORD" "$SRC_CONTAINER" \
        psql -U "$DB_USER" -d "$DB_NAME" -v ON_ERROR_STOP=1 -qtA "$@"
}
psql_superuser() {
    docker exec -e PGPASSWORD="$DB_PASSWORD" "$SRC_CONTAINER" \
        psql -U "$DB_USER" -d postgres -v ON_ERROR_STOP=1 -qtA "$@"
}
MIGRATOR_PASSWORD="drill-migrator"
psql_superuser -c "CREATE ROLE sanvi_migrator LOGIN PASSWORD '$MIGRATOR_PASSWORD' NOSUPERUSER NOCREATEDB NOCREATEROLE" 2>/dev/null || true
psql_superuser -c "CREATE ROLE sanvi_app LOGIN PASSWORD 'drill-app' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS" 2>/dev/null || true
psql_superuser -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements" >/dev/null
psql_superuser -c '\c template1' -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements" >/dev/null
psql_superuser -c "CREATE DATABASE $DB_NAME OWNER sanvi_migrator" 2>/dev/null || true
psql_superuser -c "GRANT CONNECT ON DATABASE $DB_NAME TO sanvi_app" >/dev/null

cli() {
    # The sanvi-cli binary under the migrator role, pointed at the drill
    # source by env overrides. Built once here so the drill fails fast if
    # the workspace does not compile.
    if [[ ! -x target/debug/sanvi-cli ]]; then
        cargo build -p sanvi-cli
    fi
    SANVI_ENV=local \
    SANVI__DATABASE__HOST=127.0.0.1 \
    SANVI__DATABASE__PORT="$HOST_PORT" \
    SANVI__DATABASE__NAME="$DB_NAME" \
    SANVI__DATABASE__USER=sanvi_migrator \
    SANVI__DATABASE__PASSWORD="$MIGRATOR_PASSWORD" \
    target/debug/sanvi-cli "$@"
}

# --- schema + the data the restore is a restore of ---------------------------
cli migrate up >/dev/null
cli seed roles >/dev/null
cli seed features >/dev/null
echo "backup: loading scale data (seed ${SCALE_SEED}) — the slow step"
cli seed-scale --seed "$SCALE_SEED" | jq -c '.'

# A real audit-chain segment and real outbox rows: `entitlement grant`
# writes both (the GrantEntitlement use case publishes through the outbox).
HOT_TENANT_SLUG="$(psql_src -c "SELECT slug FROM tenancy.tenants WHERE slug LIKE 'scale-%' ORDER BY slug LIMIT 1")"
FEATURE_KEY="$(psql_src -c "SELECT key FROM access.features WHERE kind = 'boolean' ORDER BY key LIMIT 1")"
if [[ -z "$HOT_TENANT_SLUG" || -z "$FEATURE_KEY" ]]; then
    echo "backup: seed data incomplete (tenant or feature missing)" >&2
    exit 1
fi
cli entitlement grant --tenant "$HOT_TENANT_SLUG" --feature "$FEATURE_KEY" \
    --enabled --reason "drill: pre-backup marker transaction" >/dev/null

# Markers: the PITR target lands just after A2, before B.
psql_src -c "CREATE SCHEMA IF NOT EXISTS drill" >/dev/null
psql_src -c "CREATE TABLE IF NOT EXISTS drill.markers(name text PRIMARY KEY, at timestamptz NOT NULL DEFAULT clock_timestamp())" >/dev/null
psql_src -c "INSERT INTO drill.markers(name) VALUES ('A_pre_backup'), ('A2_last_pre_backup') ON CONFLICT DO NOTHING" >/dev/null
psql_src -c "SELECT pg_switch_wal()" >/dev/null

# The pre-B state: what a PITR to just after A2 must land on.
psql_src -c "SELECT json_build_object(
    'audit_entries', (SELECT count(*) FROM audit.entries),
    'outbox_rows', (SELECT count(*) FROM platform.outbox),
    'outbox_unpublished', (SELECT count(*) FROM platform.outbox WHERE published_at IS NULL),
    'payments', (SELECT count(*) FROM payments.payments),
    'tenants', (SELECT count(*) FROM tenancy.tenants),
    'entitlement_overrides', (SELECT count(*) FROM access.entitlements)
)::text" | jq '.' > "$session_dir/pre_backup_counts.json"

T_BACKUP_START="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
docker exec -u root "$SRC_CONTAINER" mkdir -p /backup/base
docker exec -u root "$SRC_CONTAINER" chown postgres:postgres /backup/base
docker exec -e PGPASSWORD="$DB_PASSWORD" -u postgres "$SRC_CONTAINER" \
    pg_basebackup -h 127.0.0.1 -U "$DB_USER" -D /backup/base -Fp -Xs --checkpoint=fast -w >/dev/null
T_BACKUP_END="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# The post-backup marker transaction: committed WAL sits after the base
# backup, so a restore that replays the whole archive carries it and a
# restore pinned before it does not.
psql_src -c "INSERT INTO drill.markers(name) VALUES ('B_post_backup') ON CONFLICT DO NOTHING" >/dev/null
cli entitlement grant --tenant "$HOT_TENANT_SLUG" --feature "$FEATURE_KEY" \
    --enabled --reason "drill: post-backup marker transaction" >/dev/null
psql_src -c "SELECT pg_switch_wal()" >/dev/null

# The archive must now hold the segments the restore will replay — an empty
# archive means archive_command has been failing (permissions, path) and
# the drill would prove nothing past the base backup.
ARCHIVE_FILES="$(docker exec "$SRC_CONTAINER" sh -c 'ls /wal_archive | grep -c "^[0-9A-F]\{24\}$" || true')"
if [[ "${ARCHIVE_FILES:-0}" -eq 0 ]]; then
    echo "backup: WAL archive is empty — archive_command is failing; check server logs" >&2
    docker logs --tail 20 "$SRC_CONTAINER" >&2 || true
    exit 1
fi
echo "backup: WAL archive holds $ARCHIVE_FILES segment(s)"

# --- manifest -----------------------------------------------------------------
psql_src -c "SELECT json_build_object(
    'markers', (SELECT json_object_agg(name, at) FROM drill.markers),
    'pitr_target', ((SELECT at FROM drill.markers WHERE name = 'A2_last_pre_backup') + interval '1 second')
)::text" | jq '.' > "$session_dir/markers.json"

psql_src -c "SELECT json_build_object(
    'audit_entries', (SELECT count(*) FROM audit.entries),
    'outbox_rows', (SELECT count(*) FROM platform.outbox),
    'outbox_unpublished', (SELECT count(*) FROM platform.outbox WHERE published_at IS NULL),
    'payments', (SELECT count(*) FROM payments.payments),
    'tenants', (SELECT count(*) FROM tenancy.tenants),
    'entitlement_overrides', (SELECT count(*) FROM access.entitlements)
)::text" | jq '.' > "$session_dir/source_counts.json"

machine_os="$(uname -s) $(uname -r)"
cores="$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 0)"
jq -n \
    --arg session "$session" \
    --arg tbs "$T_BACKUP_START" \
    --arg tbe "$T_BACKUP_END" \
    --arg seed "$SCALE_SEED" \
    --arg hot "$HOT_TENANT_SLUG" \
    --arg key "$FEATURE_KEY" \
    --arg os "$machine_os" \
    --arg cores "$cores" \
    --arg wal "$WAL_VOLUME" \
    --arg backup "$BACKUP_VOLUME" \
    '{session: $session,
      pg_image: "postgres:18",
      base_backup: {started_at: $tbs, finished_at: $tbe},
      scale_seed: ($seed | tonumber),
      hot_tenant: $hot,
      feature_key: $key,
      machine: {cores: ($cores | tonumber), os: $os},
      volumes: {wal_archive: $wal, base_backup: $backup}}' > "$session_dir/manifest.json"

# --- the encrypted artifact ---------------------------------------------------
if command -v openssl >/dev/null 2>&1; then
    docker exec "$SRC_CONTAINER" tar -C /backup -cf - base \
        | openssl enc -aes-256-cbc -pbkdf2 -salt \
            -pass "pass:${DRILL_BACKUP_PASSPHRASE:-drill-local-only}" \
        > "$session_dir/base.tar.enc"
    echo "backup: encrypted artifact base.tar.enc written ($(du -h "$session_dir/base.tar.enc" | cut -f1))"
else
    echo "backup: openssl not found; kept the base in volume $BACKUP_VOLUME only (local-only)" >&2
fi

# Retention: prune session directories older than the window.
if [[ "$RETENTION_DAYS" != "0" ]]; then
    find "$root/.drill" -mindepth 1 -maxdepth 1 -type d -name '2*' -mtime "+$RETENTION_DAYS" \
        -exec rm -rf {} \; 2>/dev/null || true
fi

echo "backup: session $session complete"
echo "backup: next — scripts/db/restore.sh --session $session [--target-time <ts>] [--rehearse]"
