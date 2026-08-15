#!/usr/bin/env bash
# Run database migrations under the migrator role.
#
#   ./scripts/migrate.sh              # apply pending migrations (default)
#   ./scripts/migrate.sh down         # revert the last migration
#   ./scripts/migrate.sh status       # list applied migrations

set -euo pipefail

cd "$(dirname "$0")/.."

ACTION="${1:-up}"
EXTRA=()

case "${ACTION}" in
    up|status)
        ;;
    down)
        EXTRA=(--target "${2:-0}")
        ;;
    *)
        echo "usage: $0 [up|down|status]" >&2
        exit 2
        ;;
esac

MIGRATOR_USER="${SANVI__DATABASE__MIGRATOR_USER:-sanvi_migrator}"
MIGRATOR_PASSWORD="${SANVI__DATABASE__MIGRATOR_PASSWORD:-migrator-local-dev}"

if [[ ${#EXTRA[@]} -gt 0 ]]; then
    cargo run -p sanvi-cli -- migrate \
        --user "${MIGRATOR_USER}" \
        --password "${MIGRATOR_PASSWORD}" \
        "${ACTION}" \
        "${EXTRA[@]}"
else
    cargo run -p sanvi-cli -- migrate \
        --user "${MIGRATOR_USER}" \
        --password "${MIGRATOR_PASSWORD}" \
        "${ACTION}"
fi
