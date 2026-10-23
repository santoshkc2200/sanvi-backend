#!/usr/bin/env bash
# Two API slots behind one stable listener. The active config is the state:
# never replace its backend, and never reuse a slot while old proxy workers
# can still send it requests. Use the same script for deploy and revert.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
compose=(docker compose -f "$root/docker-compose.staging.yml")
export MSYS_NO_PATHCONV=1
export SANVI_RUNTIME_ENV_FILE="${SANVI_RUNTIME_ENV_FILE:-$root/secrets/runtime.env}"
runtime="$root/.deploy/proxy"
active="$runtime/active.conf"
pending="$root/.deploy/switch.pending"

for image in "${API_IMAGE:-}" "${WORKER_IMAGE:-}"; do
    if [[ ! "$image" =~ ^[^[:space:]]+@sha256:[a-f0-9]{64}$ ]]; then
        echo 'refusing: API_IMAGE and WORKER_IMAGE must be immutable sha256 image digests' >&2
        exit 2
    fi
done
[[ "$API_IMAGE" == "$WORKER_IMAGE" ]] || { echo 'API and worker must use the same image digest and compatibility contract' >&2; exit 2; }
[[ -f "$SANVI_RUNTIME_ENV_FILE" ]] || { echo 'private runtime environment file is missing' >&2; exit 2; }
# Secret files are provisioned by the host, never copied from CI. Linux is
# the deployment platform; Windows is supported only by the command tests.
if [[ "$(uname -s)" == Linux ]]; then
    [[ "$(stat -c '%a' "$SANVI_RUNTIME_ENV_FILE")" =~ ^[46]00$ ]] || { echo 'runtime environment file must be owner-only (0400 or 0600)' >&2; exit 2; }
fi
mkdir -p "$runtime"
drain_timeout="${DEPLOY_DRAIN_TIMEOUT_SECS:-120}"
[[ "$drain_timeout" =~ ^[1-9][0-9]*$ ]] || { echo 'invalid drain timeout' >&2; exit 2; }
# Serialize all deploys/drills on this host; a concurrent switch would
# otherwise recreate the backend another deployment just made active.
exec 9>"$root/.deploy/deploy.lock"
flock -n 9 || { echo 'another staging deploy is running' >&2; exit 1; }
mode="${DEPLOYMENT_MODE:-rolling}"
case "$mode" in rolling|maintenance) ;; *) echo 'invalid deployment mode' >&2; exit 2 ;; esac
# Compatibility checks cannot be disabled for manual deploys or rollback.
{
    export SANVI_MIGRATOR_ENV_FILE="${SANVI_MIGRATOR_ENV_FILE:-$root/secrets/migrator.env}"
    [[ -f "$SANVI_MIGRATOR_ENV_FILE" ]] || { echo 'private migrator environment file is missing' >&2; exit 2; }
    if [[ "$(uname -s)" == Linux ]]; then
        [[ "$(stat -c '%a' "$SANVI_MIGRATOR_ENV_FILE")" =~ ^[46]00$ ]] || { echo 'migrator environment file must be owner-only' >&2; exit 2; }
    fi
    docker pull "$API_IMAGE"
    stage="$(mktemp -d)"
    image_container=""
    cleanup_migration_stage() {
        [[ -z "$image_container" ]] || docker rm "$image_container" >/dev/null
        rm -rf "$stage"
    }
    trap cleanup_migration_stage EXIT
    image_container="$(docker create "$API_IMAGE")"
    unsupported_image() {
        echo 'unsupported image: this controller requires sanvi-hardened-v1 (migration policy and worker management on 9091).' >&2
        echo 'Use the first hardened image or newer. For pre-upgrade recovery restore the matching legacy controller, Compose, image and database snapshot; do not bypass probes or migration checks.' >&2
        exit 2
    }
    # Capability floor is carried by the image, not guessed from mutable tags.
    # No serving configuration, schema or process changes precede this check.
    docker cp "$image_container:/app/deployment/image-contract" "$stage/image-contract" 2>/dev/null || unsupported_image
    image_contract="$(cat "$stage/image-contract")"
    image_contract="${image_contract%$'\r'}"
    [[ "$image_contract" == sanvi-hardened-v1 ]] || unsupported_image
    mkdir -p "$stage/migrations"
    docker cp "$image_container:/app/migrations/." "$stage/migrations" 2>/dev/null || unsupported_image
    docker cp "$image_container:/app/deployment/migration-policy.json" "$stage/policy.json" 2>/dev/null || unsupported_image
    docker run --rm --env-file "$SANVI_MIGRATOR_ENV_FILE" "$API_IMAGE" \
        /usr/local/bin/sanvi-cli migrate status > "$stage/applied.txt"
    python3 "$root/scripts/check-deployment-migrations.py" --migrations-dir "$stage/migrations" \
        --policy "$stage/policy.json" --applied-file "$stage/applied.txt" --mode "$mode"
    # This ingress discards client forwarding claims. An outer host TLS proxy
    # must overwrite incoming XFF and have its exact source allowlisted here.
    if [[ -n "${SANVI_TRUSTED_EDGE_CIDR:-}" ]]; then
        python3 "$root/scripts/write-trusted-edge.py" "$SANVI_TRUSTED_EDGE_CIDR" > "$runtime/trusted-realip.conf.tmp"
    else
        : > "$runtime/trusted-realip.conf.tmp"
    fi
    mv "$runtime/trusted-realip.conf.tmp" "$runtime/trusted-realip.conf"
    if [[ "$mode" == maintenance ]]; then
        # Keep a durable marker: failure must never silently restart old
        # binaries against an incompatible, partly changed schema.
        : > "$root/.deploy/maintenance.pending"
        "${compose[@]}" stop api_proxy api api_next worker
    elif [[ -f "$root/.deploy/maintenance.pending" ]]; then
        echo 'interrupted maintenance deployment requires maintenance recovery' >&2
        exit 1
    fi
    docker run --rm --env-file "$SANVI_MIGRATOR_ENV_FILE" "$API_IMAGE" /usr/local/bin/sanvi-cli migrate up
    cleanup_migration_stage
    trap - EXIT
}

write_config() {
    local slot="$1" target="$2"
    cat > "$target" <<EOF
# active: $slot
upstream sanvi_api { server $slot:8080; }
server {
    listen 8080;
    location / {
        proxy_pass http://sanvi_api;
        proxy_http_version 1.1;
        proxy_set_header Host \$http_host;
        proxy_set_header Connection "";
        # This is the trusted ingress. Client forwarding claims are discarded.
        # An outer proxy must have its own authenticated real-IP allowlist;
        # never trust arbitrary incoming X-Forwarded-For at this boundary.
        proxy_set_header X-Forwarded-For \$remote_addr;
        proxy_set_header Forwarded "";
        proxy_set_header X-Forwarded-Host "";
        proxy_set_header X-Forwarded-Proto "";
        proxy_set_header X-Forwarded-Port "";
        proxy_set_header X-Real-IP "";
        proxy_connect_timeout 5s;
        proxy_read_timeout 35s;
        proxy_send_timeout 35s;
        # Never replay a mutation to mask a failed traffic switch.
        proxy_next_upstream off;
    }
}
EOF
}

if [[ -f "$root/.deploy/maintenance.pending" && "$mode" != maintenance ]]; then
    echo 'maintenance recovery must be explicit' >&2; exit 1
fi
if [[ ! -f "$active" || "$mode" == maintenance ]]; then
    # First installation (and migration from the old single listener)
    # requires a maintenance window. Subsequent deployments overlap slots.
    "${compose[@]}" pull api worker api_proxy
    "${compose[@]}" up -d --no-deps --wait api worker
    write_config api "$active.tmp"
    mv "$active.tmp" "$active"
    "${compose[@]}" up -d --no-deps --wait api_proxy
    rm -f "$root/.deploy/maintenance.pending" "$pending"
    exit 0
fi

current="$(sed -n 's/^# active: //p' "$active")"
if [[ -f "$pending" ]]; then
    # A killed SSH session may have written active.conf without applying
    # it. Reconcile the durable config before deriving an inactive slot;
    # otherwise that slot may still be the one nginx is serving.
    case "$current" in api|api_next) ;; *) echo 'invalid pending slot' >&2; exit 1 ;; esac
    "${compose[@]}" exec -T "$current" /usr/local/bin/sanvi-cli healthcheck
    "${compose[@]}" exec -T api_proxy nginx -t
    before="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o pid,args' | awk '/nginx: worker process/ {print $1}')"
    [[ -n "$before" ]] || { echo 'proxy has no live workers during recovery' >&2; exit 1; }
    "${compose[@]}" exec -T api_proxy nginx -s reload
    for ((attempt=0; attempt<drain_timeout; attempt++)); do
        live="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o pid,args' | awk '/nginx: worker process/ {print $1}')"
        waiting=0
        for pid in $before; do
            if grep -qx "$pid" <<<"$live"; then waiting=1; fi
        done
        if [[ "$waiting" -eq 0 && -n "$live" ]]; then break; fi
        if [[ "$attempt" -eq $((drain_timeout - 1)) ]]; then echo 'pending switch has not drained; no slot reused' >&2; exit 1; fi
        sleep 1
    done
    rm -f "$pending"
fi
case "$current" in
    api) candidate=api_next ;;
    api_next) candidate=api ;;
    *) echo 'refusing: invalid active slot' >&2; exit 1 ;;
esac
# Graceful nginx reload leaves old workers serving in-flight requests.
# A slow request blocks the next deploy rather than getting killed.
for ((attempt=0; attempt<drain_timeout; attempt++)); do
    workers="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o args')"
    if ! grep -q 'nginx: worker process is shutting down' <<<"$workers"; then
        break
    fi
    if [[ "$attempt" -eq $((drain_timeout - 1)) ]]; then
        echo 'refusing: previous proxy workers have not drained' >&2
        exit 1
    fi
    sleep 1
done
"${compose[@]}" pull "$candidate" worker
"${compose[@]}" up -d --no-deps --wait "$candidate" worker
write_config "$candidate" "$active.tmp"
cp "$active" "$active.previous"
# Journal before replacing configuration; keep it until reload and drain
# are confirmed. Every error/interruption leaves a recoverable switch.
: > "$pending"
mv "$active.tmp" "$active"
if ! "${compose[@]}" exec -T api_proxy nginx -t; then
    mv "$active.previous" "$active"
    exit 1
fi
old_workers="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o pid,args' | awk '/nginx: worker process/ {print $1}')"
[[ -n "$old_workers" ]] || { mv "$active.previous" "$active"; echo 'proxy has no live workers' >&2; exit 1; }
if ! "${compose[@]}" exec -T api_proxy nginx -s reload; then
    mv "$active.previous" "$active"
    exit 1
fi
# nginx -s reload acknowledges signal delivery, not the traffic switch.
# Wait for the old worker PIDs to disappear: this proves the new config
# was accepted and that every request using the previous slot drained.
for ((attempt=0; attempt<drain_timeout; attempt++)); do
    live="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o pid,args' | awk '/nginx: worker process/ {print $1}')"
    waiting=0
    for pid in $old_workers; do
        if grep -qx "$pid" <<<"$live"; then waiting=1; fi
    done
    if [[ "$waiting" -eq 0 && -n "$live" ]]; then break; fi
    if [[ "$attempt" -eq $((drain_timeout - 1)) ]]; then
        echo 'proxy reload has not drained; leave both API slots alive and inspect before another deploy' >&2
        exit 1
    fi
    sleep 1
done
rm -f "$pending"
# Leave the previous API alive. Its slot is reused only after the retired
# nginx workers have drained on the next invocation.
echo "staging traffic switched: $current -> $candidate ($API_IMAGE)"
