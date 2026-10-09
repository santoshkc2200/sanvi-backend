#!/usr/bin/env bash
# Two API slots behind one stable listener. The active config is the state:
# never replace its backend, and never reuse a slot while old proxy workers
# can still send it requests. Use the same script for deploy and revert.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
compose=(docker compose -f "$root/docker-compose.staging.yml")
runtime="$root/.deploy/proxy"
active="$runtime/active.conf"
pending="$root/.deploy/switch.pending"

for image in "${API_IMAGE:-}" "${WORKER_IMAGE:-}"; do
    if [[ -z "$image" || "$image" == *:latest || "$image" != *:* ]]; then
        echo 'refusing: API_IMAGE and WORKER_IMAGE must be pinned tags or digests' >&2
        exit 2
    fi
done
mkdir -p "$runtime"
# Serialize all deploys/drills on this host; a concurrent switch would
# otherwise recreate the backend another deployment just made active.
exec 9>"$root/.deploy/deploy.lock"
flock -n 9 || { echo 'another staging deploy is running' >&2; exit 1; }

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
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_connect_timeout 5s;
        proxy_read_timeout 35s;
        proxy_send_timeout 35s;
        # Never replay a mutation to mask a failed traffic switch.
        proxy_next_upstream off;
    }
}
EOF
}

if [[ ! -f "$active" ]]; then
    # First installation (and migration from the old single listener)
    # requires a maintenance window. Subsequent deployments overlap slots.
    "${compose[@]}" pull api worker api_proxy
    "${compose[@]}" up -d --no-deps --wait api worker
    write_config api "$active.tmp"
    mv "$active.tmp" "$active"
    "${compose[@]}" up -d --no-deps --wait api_proxy
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
    for ((attempt=0; attempt<120; attempt++)); do
        live="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o pid,args' | awk '/nginx: worker process/ {print $1}')"
        waiting=0
        for pid in $before; do
            if grep -qx "$pid" <<<"$live"; then waiting=1; fi
        done
        if [[ "$waiting" -eq 0 && -n "$live" ]]; then break; fi
        if [[ "$attempt" -eq 119 ]]; then echo 'pending switch has not drained; no slot reused' >&2; exit 1; fi
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
for ((attempt=0; attempt<120; attempt++)); do
    workers="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o args')"
    if ! grep -q 'nginx: worker process is shutting down' <<<"$workers"; then
        break
    fi
    if [[ "$attempt" -eq 119 ]]; then
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
for ((attempt=0; attempt<120; attempt++)); do
    live="$("${compose[@]}" exec -T api_proxy sh -c 'ps -o pid,args' | awk '/nginx: worker process/ {print $1}')"
    waiting=0
    for pid in $old_workers; do
        if grep -qx "$pid" <<<"$live"; then waiting=1; fi
    done
    if [[ "$waiting" -eq 0 && -n "$live" ]]; then break; fi
    if [[ "$attempt" -eq 119 ]]; then
        echo 'proxy reload has not drained; leave both API slots alive and inspect before another deploy' >&2
        exit 1
    fi
    sleep 1
done
rm -f "$pending"
# Leave the previous API alive. Its slot is reused only after the retired
# nginx workers have drained on the next invocation.
echo "staging traffic switched: $current -> $candidate ($API_IMAGE)"
