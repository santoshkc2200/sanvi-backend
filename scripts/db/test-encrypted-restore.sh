#!/usr/bin/env bash
# A correctly encrypted but invalid archive must fail before PostgreSQL starts.
set -euo pipefail
export MSYS_NO_PATHCONV=1
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
mkdir -p .drill
directory="$(mktemp -d "$root/.drill/task025-invalid-XXXXXXXX")"
session="${directory##*/}"
container="sanvi-drill-restore-${session}"
cleanup() {
    rm -f "$directory/manifest.json" "$directory/base.tar.enc" "$directory/result.log"
    rmdir "$directory"
}
trap cleanup EXIT
printf '{}\n' > "$directory/manifest.json"
export DRILL_BACKUP_PASSPHRASE=drill-invalid-fixture
printf 'This is not a tar archive.\n' | openssl enc -aes-256-cbc -pbkdf2 -salt \
    -pass env:DRILL_BACKUP_PASSPHRASE > "$directory/base.tar.enc"
if DRILL_RESTORE_CONTAINER="$container" DRILL_RESTORE_PORT=55437 \
    bash "${DRILL_RESTORE_SCRIPT:-scripts/db/restore.sh}" --session "$session" --rehearse > "$directory/result.log" 2>&1; then
    echo "FAIL: invalid archive accepted" >&2; exit 1
fi
if rg -q 'server never reached read-write service' < "$directory/result.log"; then
    echo "FAIL: extraction failure reached the PostgreSQL recovery wait" >&2; exit 1
fi
[[ -z "$(docker ps -aq --filter "name=^${container}$")" ]] || { echo "FAIL: recovery container started" >&2; exit 1; }
[[ ! -f "benchmarks/dr/restore-full-${session}.json" ]] || { echo "FAIL: invalid success report" >&2; exit 1; }
if docker volume ls -q | rg -q "$container"; then
    echo "FAIL: materialization volume survived cleanup" >&2; exit 1
fi
echo "encrypted restore negative control: invalid decrypted archive rejected before recovery; volumes cleaned"
