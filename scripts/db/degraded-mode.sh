#!/usr/bin/env bash
# Read-only recovery commands against a restored drill server only.
set -euo pipefail
export MSYS_NO_PATHCONV=1
container="${1:?restored drill container is required}"
[[ "$container" == sanvi-drill-restore* ]] || { echo "degraded: only drill restore containers allowed" >&2; exit 2; }
db=sanvi_scale_drill
psql_drill() {
    docker exec -e PGPASSWORD=drill-app -e 'PGOPTIONS=-c default_transaction_read_only=on' \
        "$container" psql -h 127.0.0.1 -U sanvi_app -d "$db" -v ON_ERROR_STOP=1 -v VERBOSITY=verbose -qtA "$@"
}
[[ "$(psql_drill -c 'SHOW transaction_read_only')" == on ]]
[[ "$(psql_drill -c 'SELECT 1')" == 1 ]]
error_file="$(mktemp)"
trap 'rm -f "$error_file"' EXIT
if psql_drill -c 'UPDATE tenancy.tenants SET display_name = display_name WHERE false' >"$error_file" 2>&1; then
    echo "degraded: read-only connection allowed a write" >&2; exit 1
fi
rg -q '25006' < "$error_file" || { echo "degraded: write failed for a reason other than read-only recovery" >&2; exit 1; }
# A fresh ordinary connection after leaving recovery permits a write command.
# WHERE false means this never changes a tenant row.
docker exec -e PGPASSWORD=drill-app "$container" \
    psql -h 127.0.0.1 -U sanvi_app -d "$db" -v ON_ERROR_STOP=1 -qtA \
    -c 'UPDATE tenancy.tenants SET display_name = display_name WHERE false' >/dev/null
echo "degraded: read-only SELECT succeeded, write rejected with 25006, fresh read-write connection recovered"
