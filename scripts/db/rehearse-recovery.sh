#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
session="${1:-}"
if [[ -z "$session" ]]; then
    session="$(bash scripts/db/backup.sh --retention-days 0 | tee .recovery-backup.log | sed -n 's/^backup: session \(.*\) complete$/\1/p')"
fi
DRILL_RESTORE_CONTAINER=sanvi-drill-restore-task025-full DRILL_RESTORE_PORT=55435 \
    bash scripts/db/restore.sh --session "$session" --rehearse
target="$(jq -r '.pitr_target' < ".drill/$session/markers.json")"
DRILL_RESTORE_CONTAINER=sanvi-drill-restore-task025-pitr DRILL_RESTORE_PORT=55436 \
    bash scripts/db/restore.sh --session "$session" --target-time "$target" --rehearse
