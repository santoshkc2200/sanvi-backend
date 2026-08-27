#!/usr/bin/env bash
# Long-lived Postgres + Redis for the integration suites.
#
#   ./scripts/test-db.sh up       # start (or reuse) the servers, print the env
#   ./scripts/test-db.sh env      # print the env for an already-running pair
#   ./scripts/test-db.sh down     # stop and remove them
#   ./scripts/test-db.sh prune    # + reap containers stray test runs left behind
#
# Without this, each test *process* starts its own container and leaves it
# running (nothing drops a `static`). With it, the whole workspace shares one
# pair and the migrated template database survives between runs, so a second
# `just test` skips the migrations entirely.
#
#   eval "$(./scripts/test-db.sh up)" && cargo nextest run --workspace

set -euo pipefail

PG_NAME=sanvi-test-postgres
REDIS_NAME=sanvi-test-redis
PG_IMAGE=postgres:18-alpine
REDIS_IMAGE=redis:8-alpine

port_of() {
    docker port "$1" "$2" 2>/dev/null | head -1 | sed 's/.*://'
}

start_postgres() {
    if [[ -z "$(docker ps -q --filter "name=^${PG_NAME}$")" ]]; then
        docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
        # Durability buys nothing for a throwaway database; the connection
        # ceiling has to clear every pool the workspace opens at once. The
        # data directory is a tmpfs so migrations never touch the disk.
        docker run -d --name "$PG_NAME" \
            -e POSTGRES_PASSWORD=postgres \
            -e POSTGRES_USER=postgres \
            -e POSTGRES_DB=postgres \
            -e PGDATA=/var/lib/postgresql/tmpfs \
            --tmpfs /var/lib/postgresql/tmpfs:rw,size=4g \
            -p 127.0.0.1::5432 \
            "$PG_IMAGE" \
            -c fsync=off \
            -c synchronous_commit=off \
            -c full_page_writes=off \
            -c max_connections=600 \
            -c shared_buffers=256MB >/dev/null
        until docker exec "$PG_NAME" pg_isready -q -U postgres 2>/dev/null; do sleep 0.2; done
    fi
}

start_redis() {
    if [[ -z "$(docker ps -q --filter "name=^${REDIS_NAME}$")" ]]; then
        docker rm -f "$REDIS_NAME" >/dev/null 2>&1 || true
        docker run -d --name "$REDIS_NAME" -p 127.0.0.1::6379 "$REDIS_IMAGE" >/dev/null
    fi
}

# Databases are named `sanvi_t_<pid>_<n>` and outlive the test that made them
# (a `Drop` cannot await). Clearing them here keeps the server from growing
# across runs; the migrated template is deliberately kept.
# Redis has no per-test database (a stock server has sixteen, the suite has
# hundreds of tests), so suites divide it by key name instead and every key
# they touch is uniquely named. Keys still outlive the run that made them —
# a test that is killed mid-way acks nothing — so clear the server here for
# the same reason the databases below are dropped: nothing should carry over
# between runs.
flush_redis() {
    docker exec "$REDIS_NAME" redis-cli FLUSHALL >/dev/null 2>&1 || true
}

drop_stale_databases() {
    docker exec "$PG_NAME" psql -U postgres -tAc \
        "SELECT datname FROM pg_database WHERE datname LIKE 'sanvi\\_t\\_%' OR datname LIKE 'sanvi\\_b\\_%'" \
        2>/dev/null | while read -r db; do
        [[ -n "$db" ]] && docker exec "$PG_NAME" psql -U postgres -qc "DROP DATABASE IF EXISTS \"$db\"" >/dev/null 2>&1 || true
    done
}

print_env() {
    local pg redis
    pg="$(port_of "$PG_NAME" 5432)"
    redis="$(port_of "$REDIS_NAME" 6379)"
    if [[ -z "$pg" || -z "$redis" ]]; then
        echo "test servers are not running; run: ./scripts/test-db.sh up" >&2
        exit 1
    fi
    echo "export SANVI_TEST_PG_PORT=${pg}"
    echo "export SANVI_TEST_REDIS_PORT=${redis}"
}

case "${1:-}" in
    up)
        start_postgres
        start_redis
        flush_redis
        drop_stale_databases
        print_env
        ;;
    env)
        print_env
        ;;
    down)
        docker rm -f "$PG_NAME" "$REDIS_NAME" >/dev/null 2>&1 || true
        ;;
    prune)
        docker rm -f "$PG_NAME" "$REDIS_NAME" >/dev/null 2>&1 || true
        # Containers from runs that did not use this script: testcontainers
        # labels everything it starts.
        stray="$(docker ps -aq --filter 'label=org.testcontainers=true')"
        [[ -n "$stray" ]] && docker rm -f $stray >/dev/null 2>&1 || true
        echo "test containers removed"
        ;;
    *)
        sed -n '2,9p' "$0"
        exit 0
        ;;
esac
