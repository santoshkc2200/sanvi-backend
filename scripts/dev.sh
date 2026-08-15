#!/usr/bin/env bash
# Local development stack manager.
#
#   ./scripts/dev.sh up           # start core stack (Postgres + Redis)
#   ./scripts/dev.sh up --full    # + Ory Kratos/Keto, Mailpit, MinIO, Jaeger
#   ./scripts/dev.sh down         # stop everything (keep data volumes)
#   ./scripts/dev.sh reset        # stop AND wipe data volumes
#   ./scripts/dev.sh logs [-f]    # tail logs
#   ./scripts/dev.sh ps           # status

set -euo pipefail

COMPOSE=(docker compose)

usage() {
    sed -n '2,9p' "$0"
    exit 0
}

case "${1:-}" in
    up)
        shift || true
        if [[ "${1:-}" == "--full" ]]; then
            "${COMPOSE[@]}" --profile full up -d --wait
        else
            "${COMPOSE[@]}" up -d --wait
        fi
        echo
        echo "Core stack is up:"
        echo "  Postgres  localhost:5432  (sanvi_app / NOBYPASSRLS)"
        echo "  Redis     localhost:6379"
        echo
        echo "Next: ./scripts/migrate.sh && cargo run -p sanvi-api"
        ;;
    down)
        "${COMPOSE[@]}" --profile full down
        ;;
    reset)
        "${COMPOSE[@]}" --profile full down -v
        ;;
    logs)
        "${COMPOSE[@]}" logs "${@:2}"
        ;;
    ps)
        "${COMPOSE[@]}" ps
        ;;
    *)
        usage
        ;;
esac
