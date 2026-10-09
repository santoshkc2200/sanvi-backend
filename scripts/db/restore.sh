#!/usr/bin/env bash
# TASK-025 (FR-1119/NFR-1107): the restore half of the restore drill.
#
# Restores a session's base backup (from `scripts/db/backup.sh`) into a
# clean container and times it:
#
#   - default:    replay the whole WAL archive to the end (the full restore)
#   - --target-time <ts>: point-in-time recovery to that instant — the PITR
#     drill. The consistency checks then read the three places a torn
#     restore shows up first: the audit chain (the real `sanvi-cli audit
#     verify` verifier against the restored database), the outbox (no row
#     carries a delivery stamp from after the target; the pre-target
#     delivery set is exactly the recorded pre-target state), and payment
#     records (count and external-id uniqueness match the recorded state).
#
# With --rehearse the measured numbers land in
# benchmarks/dr/<scenario>-<session>.json for committing — start time, end
# time, restore duration (the local RTO), data-loss window, machine
# profile — every number labelled local.
#
# Usage:
#   scripts/db/restore.sh --session <sid> [--target-time <ts>] [--rehearse] [--keep]
set -euo pipefail

# Git Bash (MSYS) rewrites absolute paths in arguments — a `-v vol:/backup`
# mount would silently land at C:/Program Files/Git/backup inside the
# container and every artifact would stay in the container layer. Disable
# the conversion for this script; harmless (unknown) under POSIX bash.
export MSYS_NO_PATHCONV=1

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

SESSION=""
TARGET_TIME=""
REHEARSE=0
KEEP=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --session) SESSION="$2"; shift 2 ;;
        --target-time) TARGET_TIME="$2"; shift 2 ;;
        --rehearse) REHEARSE=1; shift ;;
        --keep) KEEP=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

for tool in docker jq; do
    command -v "$tool" >/dev/null 2>&1 || { echo "restore: $tool is required" >&2; exit 1; }
done

[[ -n "$SESSION" ]] || { echo "restore: --session is required" >&2; exit 2; }
session_dir="$root/.drill/$SESSION"
[[ -f "$session_dir/manifest.json" ]] || { echo "restore: no manifest for session $SESSION" >&2; exit 2; }

PG_IMAGE="postgres:18"
# One restore at a time per name; a second concurrent restore (e.g. keeping
# a snapshot up while rehearsing tenant-restore against another) overrides it.
RESTORE_CONTAINER="${DRILL_RESTORE_CONTAINER:-sanvi-drill-restore}"
DB_NAME="sanvi_scale_drill"
DB_USER="postgres"
DB_PASSWORD="drill-local-only"
HOST_PORT="${DRILL_RESTORE_PORT:-55433}"

port_in_use() {
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && { exec 3>&- 3<&-; return 0; } || return 1
}
if port_in_use "$HOST_PORT"; then
    echo "restore: port $HOST_PORT is busy; set DRILL_RESTORE_PORT" >&2
    exit 1
fi

WAL_VOLUME="sanvi_drill_wal_${SESSION}"
BACKUP_VOLUME="sanvi_drill_backup_${SESSION}"
PGDATA_VOLUME="sanvi_drill_restore_pgdata_${SESSION}"
docker volume inspect "$WAL_VOLUME" >/dev/null 2>&1 || { echo "restore: volume $WAL_VOLUME missing" >&2; exit 2; }
docker volume inspect "$BACKUP_VOLUME" >/dev/null 2>&1 || { echo "restore: volume $BACKUP_VOLUME missing" >&2; exit 2; }
# The restored data directory must be born clean: a volume left by an
# earlier restore carries stale recovery configuration (a pinned target, a
# recovery.signal) that silently bends this restore. Kill it and start over.
docker volume rm "$PGDATA_VOLUME" >/dev/null 2>&1 || true
docker volume create "$PGDATA_VOLUME" >/dev/null

if [[ -n "$TARGET_TIME" ]]; then
    scenario="pitr"
else
    scenario="full"
fi

cleanup() {
    if [[ "$KEEP" == "1" ]]; then
        echo "restore: kept $RESTORE_CONTAINER up (port $HOST_PORT) for inspection"
    else
        docker rm -f "$RESTORE_CONTAINER" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT
docker rm -f "$RESTORE_CONTAINER" >/dev/null 2>&1 || true

echo "restore: session $SESSION (${scenario})"

# The recovery configuration, written before the server starts. With a
# target, recovery stops at the target and promotes; without one, it
# consumes the archive to its end and promotes there.
TARGET_BLOCK=""
if [[ -n "$TARGET_TIME" ]]; then
    TARGET_BLOCK="echo \"recovery_target_time = '$TARGET_TIME'\" >> \"\$PGDATA/postgresql.auto.conf\"
     echo \"recovery_target_action = 'promote'\" >> \"\$PGDATA/postgresql.auto.conf\""
fi

T_START="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
T_START_EPOCH="$(date -u +%s)"

docker run -d --name "$RESTORE_CONTAINER" \
    -p "127.0.0.1:${HOST_PORT}:5432" \
    -e "POSTGRES_USER=${DB_USER}" \
    -e "POSTGRES_PASSWORD=${DB_PASSWORD}" \
    -e "POSTGRES_DB=${DB_NAME}" \
    -e "TARGET_TIME=${TARGET_TIME}" \
    -v "${PGDATA_VOLUME}:/var/lib/postgresql" \
    -v "${BACKUP_VOLUME}:/backup_base:ro" \
    -v "${WAL_VOLUME}:/wal_archive:ro" \
    "$PG_IMAGE" \
    sh -c '
        set -e
        mkdir -p "$PGDATA"
        cp -a /backup_base/base/. "$PGDATA"/
        chmod 700 "$PGDATA"
        chown -R postgres:postgres "$PGDATA"
        touch "$PGDATA/recovery.signal"
        {
            echo "restore_command = '"'"'cp /wal_archive/%f %p'"'"'"
            if [ -n "$TARGET_TIME" ]; then
                echo "recovery_target_time = '"'"'$TARGET_TIME'"'"'"
                echo "recovery_target_action = '"'"'promote'"'"'"
            fi
        } >> "$PGDATA/postgresql.auto.conf"
        exec /usr/local/bin/docker-entrypoint.sh postgres
    ' >/dev/null

wait_restored() {
    # Ready for read-write service: accepting connections and out of
    # recovery. That instant is the RTO the report records.
    for _ in $(seq 1 300); do
        if docker exec "$RESTORE_CONTAINER" pg_isready -U "$DB_USER" -d "$DB_NAME" >/dev/null 2>&1; then
            if [[ "$(docker exec -e PGPASSWORD="$DB_PASSWORD" "$RESTORE_CONTAINER" \
                    psql -U "$DB_USER" -d "$DB_NAME" -tAc "SELECT pg_is_in_recovery()" 2>/dev/null)" == "f" ]]; then
                return 0
            fi
        fi
        sleep 0.25
    done
    echo "restore: server never reached read-write service" >&2
    docker logs --tail 50 "$RESTORE_CONTAINER" >&2 || true
    return 1
}
wait_restored
T_READY="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
T_READY_EPOCH="$(date -u +%s)"
RESTORE_SECONDS=$((T_READY_EPOCH - T_START_EPOCH))

psql_restore() {
    docker exec -e PGPASSWORD="$DB_PASSWORD" "$RESTORE_CONTAINER" \
        psql -U "$DB_USER" -d "$DB_NAME" -v ON_ERROR_STOP=1 -qtA "$@"
}

cli() {
    # The migrator role: the base backup carries the cluster-wide roles, so
    # the restored database answers to the same credentials as the source.
    if [[ ! -x target/debug/sanvi-cli ]]; then
        cargo build -p sanvi-cli
    fi
    SANVI_ENV=local \
    SANVI__DATABASE__HOST=127.0.0.1 \
    SANVI__DATABASE__PORT="$HOST_PORT" \
    SANVI__DATABASE__NAME="$DB_NAME" \
    SANVI__DATABASE__USER=sanvi_migrator \
    SANVI__DATABASE__PASSWORD=drill-migrator \
    target/debug/sanvi-cli "$@"
}

# --- consistency checks -------------------------------------------------------
fail() {
    echo "restore: CHECK FAILED: $1" >&2
    exit 1
}

# 1. The audit chain verifies — with the real verifier, not a script hash.
AUDIT_OUT="$(cli audit verify)" || fail "audit chain verification failed on the restored database"
echo "restore: $AUDIT_OUT"

# 2. Markers: what replayed and what did not.
marker_at() {
    psql_restore -c "SELECT at FROM drill.markers WHERE name = '$1'"
}
A_AT="$(marker_at A_pre_backup || true)"
A2_AT="$(marker_at A2_last_pre_backup || true)"
B_AT="$(marker_at B_post_backup || true)"
if [[ "$scenario" == "pitr" ]]; then
    [[ -n "$A_AT" && -n "$A2_AT" ]] || fail "pre-target markers missing from the PITR restore"
    [[ -z "$B_AT" ]] || fail "post-backup marker B replayed past the target — recovery overshot"
    EXPECTED_COUNTS="$session_dir/pre_backup_counts.json"
    WINDOW_BASIS="by design: recovery was pinned to the recorded target"
else
    [[ -n "$A_AT" && -n "$A2_AT" && -n "$B_AT" ]] || fail "a marker is missing from the full restore"
    EXPECTED_COUNTS="$session_dir/source_counts.json"
    WINDOW_BASIS="replayed to the last archived WAL record; the source was quiesced with pg_switch_wal before measuring"
fi
echo "restore: markers consistent with a ${scenario} restore"

# 3. The outbox: no delivery stamp from after the restore point, and the
#    row set matches the recorded state. Unpublished rows are the expected
#    post-restore posture (the worker re-delivers; handlers are idempotent).
read_json_field() {
    # Via stdin: jq is a native Windows binary here, and with
    # MSYS_NO_PATHCONV set its file arguments would arrive unconverted.
    jq -r "$1"
}
for field in audit_entries outbox_rows outbox_unpublished payments; do
    expected="$(jq -r ".$field" < "$EXPECTED_COUNTS")"
    case "$field" in
        audit_entries) actual="$(psql_restore -c 'SELECT count(*) FROM audit.entries')" ;;
        outbox_rows) actual="$(psql_restore -c 'SELECT count(*) FROM platform.outbox')" ;;
        outbox_unpublished) actual="$(psql_restore -c 'SELECT count(*) FROM platform.outbox WHERE published_at IS NULL')" ;;
        payments) actual="$(psql_restore -c 'SELECT count(*) FROM payments.payments')" ;;
    esac
    if [[ "$expected" != "$actual" ]]; then
        fail "$field: expected $expected, restored database has $actual"
    fi
done
if [[ "$scenario" == "pitr" ]]; then
    # No torn delivery: nothing replayed carries a publication stamp from
    # beyond the restore point (there should be no stamps at all pre-B).
    torn="$(psql_restore -c "SELECT count(*) FROM platform.outbox WHERE published_at IS NOT NULL AND published_at > '$TARGET_TIME'")"
    [[ "$torn" == "0" ]] || fail "outbox carries $torn delivery stamps from after the restore point"
fi
echo "restore: outbox and row counts match the recorded ${scenario} state"

# 4. Payment records: external ids stay unique per tenant after replay.
dupes="$(psql_restore -c 'SELECT count(*) FROM (SELECT tenant_id, external_payment_id FROM payments.payments GROUP BY 1, 2 HAVING count(*) > 1) d')"
[[ "$dupes" == "0" ]] || fail "payment records torn: $dupes duplicate external ids"
echo "restore: payment records consistent"

# --- report ---------------------------------------------------------------------
restored_at="$(psql_restore -c "SELECT max(occurred_at) FROM audit.entries")"
end="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
cores="$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 0)"
os="$(uname -s) $(uname -r)"

if [[ "$REHEARSE" == "1" ]]; then
    LOSS_SECONDS=""
    if [[ -n "$TARGET_TIME" ]]; then
        TARGET_EPOCH="$(date -u -d "$TARGET_TIME" +%s)"
        NOW_EPOCH="$(date -u +%s)"
        LOSS_SECONDS=$((NOW_EPOCH - TARGET_EPOCH))
    fi
    mkdir -p benchmarks/dr
    report="benchmarks/dr/restore-${scenario}-${SESSION}.json"
    jq -n \
        --arg scenario "$scenario" \
        --arg session "$SESSION" \
        --arg started "$T_START" \
        --arg ready "$T_READY" \
        --arg finished "$end" \
        --arg seconds "$RESTORE_SECONDS" \
        --arg target "$TARGET_TIME" \
        --arg window_basis "$WINDOW_BASIS" \
        --arg restored_audit_max "$restored_at" \
        --arg os "$os" \
        --arg cores "$cores" \
        --arg audit "$AUDIT_OUT" \
        --arg loss_seconds "${LOSS_SECONDS:-}" \
        '{scenario: $scenario,
          session: $session,
          date: ($finished | split("T")[0]),
          started_at: $started,
          ready_at: $ready,
          finished_at: $finished,
          restore_seconds: ($seconds | tonumber),
          rto_label: "local — docker on this host, not a production number",
          pitr_target: (if $target == "" then null else $target end),
          data_loss_window: (if $scenario == "pitr" then
                "target → drill end (by design; see data_loss_window_basis)"
             else "0s against the quiesced source; see data_loss_window_basis" end),
          data_loss_window_seconds: (if $loss_seconds == "" then null else ($loss_seconds | tonumber) end),
          data_loss_window_basis: $window_basis,
          restored_audit_max_occurred_at: $restored_audit_max,
          machine: {cores: ($cores | tonumber), os: $os},
          checks: {audit_chain: $audit, markers: "consistent", outbox: "consistent", payments: "consistent"},
          build: {cli: "sanvi-cli 0.10.0"}}' > "$report"
    echo "restore: report written — $report"
fi

echo "restore: ${scenario} restore of session $SESSION verified in ${RESTORE_SECONDS}s (local)"
