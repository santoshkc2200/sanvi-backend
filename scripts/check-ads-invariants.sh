#!/usr/bin/env bash
# Phase-10 invariant gate: no raw customer identifier can enter an adapter
# before directive-gated hashing exists (TASK-014). The narrow scope avoids
# banning unrelated identity/privacy code while protecting this context.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
adapter_root="$root/crates/contexts/advertising/src/adapters"

contains_raw_identifier() {
  # TASK-014's public track handler is the one legitimate place a raw
  # identifier reaches an adapter: it receives it as short-lived transport
  # input and hands it straight to domain/hashing.rs, which is the only
  # code that may ever turn it into a `HashedIdentifier` — the handler
  # itself never stores or forwards the raw value anywhere else. Every
  # other adapter file stays banned.
  rg -n --glob '*.rs' --glob '!tracking_handlers.rs' -i '\b(email|phone)\b' "$1" >/dev/null
}

contains_platform_literal() {
  rg -n --glob '*.rs' -i '\b(google(_ads)?|meta(_ads)?)\b' "$1" >/dev/null
}

ad_platform_port_body() {
  awk '
    /trait[[:space:]]+AdPlatformPort/ { inside = 1 }
    inside { print }
    inside && /^[[:space:]]*}/ { exit }
  ' "$1"
}

port_has_safe_upload_signature() {
  ad_platform_port_body "$1" | rg -U -Pq \
    'fn\s+upload\s*\((?=[^;{}]*\bHashedIds\b)(?=[^;{}]*\bConsentSnapshot\b)[^;{}]*\)'
}

scan_frontend_bundle() {
  local bundle="${ADVERTISING_FRONTEND_BUNDLE:-}"
  if [[ -z "$bundle" || ! -d "$bundle" ]]; then
    if [[ -d "$root/../sanvi-frontend" ]]; then
      bundle="$root/../sanvi-frontend"
    elif [[ -d "$root/../../sanvi-frontend" ]]; then
      bundle="$root/../../sanvi-frontend"
    else
      echo "advertising invariant failed: set ADVERTISING_FRONTEND_BUNDLE to built frontend assets" >&2
      return 1
    fi
  fi

  if rg -n -i 'google_ads_developer_token|google_ads_client_secret|meta_client_secret' "$bundle" >/dev/null; then
    echo "advertising invariant failed: server credential key in frontend bundle" >&2
    return 1
  fi

  # CI supplies deployed credential values through these ephemeral variables.
  # Fixed-string matching catches real leaks rather than only their config key.
  local name value
  for name in GOOGLE_ADS_DEVELOPER_TOKEN GOOGLE_ADS_CLIENT_SECRET META_CLIENT_SECRET; do
    value="${!name:-}"
    if [[ -n "$value" ]] && rg -F -n -- "$value" "$bundle" >/dev/null; then
      echo "advertising invariant failed: server credential value in frontend bundle" >&2
      return 1
    fi
  done
}

if [[ "${1:-}" == "--self-test" ]]; then
  temp="$(mktemp -d)"
  trap 'rm -rf "$temp"' EXIT
  printf '%s\n' 'struct Illegal { email: String }' > "$temp/illegal.rs"
  if ! contains_raw_identifier "$temp"; then
    echo "self-test failed: raw identifier fixture passed" >&2
    exit 1
  fi
  printf '%s\n' 'struct Safe { hashed_ids: String }' > "$temp/safe.rs"
  if contains_raw_identifier "$temp/safe.rs"; then
    echo "self-test failed: safe fixture rejected" >&2
    exit 1
  fi
  mkdir -p "$temp/http_dir"
  printf '%s\n' 'struct TrackConversionRequest { email: Option<String> }' > "$temp/http_dir/tracking_handlers.rs"
  if contains_raw_identifier "$temp/http_dir"; then
    echo "self-test failed: tracking_handlers.rs transport DTO rejected" >&2
    exit 1
  fi
  printf '%s\n' 'struct Illegal { email: String }' > "$temp/http_dir/other_handlers.rs"
  if ! contains_raw_identifier "$temp/http_dir"; then
    echo "self-test failed: raw identifier outside tracking_handlers.rs passed" >&2
    exit 1
  fi
  printf '%s\n' 'const PLATFORM: &str = "google_ads";' > "$temp/platform.rs"
  if ! contains_platform_literal "$temp/platform.rs"; then
    echo "self-test failed: platform literal fixture passed" >&2
    exit 1
  fi
  cat > "$temp/ports.rs" <<'EOF'
struct HashedIds;
struct ConsentSnapshot;
trait AdPlatformPort {
    fn upload(&self, ids: HashedIds, consent: ConsentSnapshot);
}
EOF
  if ! port_has_safe_upload_signature "$temp/ports.rs"; then
    echo "self-test failed: safe upload signature rejected" >&2
    exit 1
  fi
  cat > "$temp/unsafe_ports.rs" <<'EOF'
struct HashedIds;
struct ConsentSnapshot;
trait AdPlatformPort {
    fn upload(&self, email: String);
}
EOF
  if port_has_safe_upload_signature "$temp/unsafe_ports.rs"; then
    echo "self-test failed: unsafe upload signature passed" >&2
    exit 1
  fi
  mkdir "$temp/bundle"
  printf '%s\n' 'window.leaked = "ads-canary-secret";' > "$temp/bundle/app.js"
  if GOOGLE_ADS_DEVELOPER_TOKEN='ads-canary-secret' ADVERTISING_FRONTEND_BUNDLE="$temp/bundle" scan_frontend_bundle 2>/dev/null; then
    echo "self-test failed: credential bundle passed" >&2
    exit 1
  fi
  echo "ads invariant self-test: OK"
  exit 0
fi

if contains_raw_identifier "$adapter_root"; then
  echo "advertising invariant failed: raw identifier field in adapter" >&2
  exit 1
fi

# Platform differences live in adapter descriptors and their matrix data. A
# literal in domain/application code is a future adapter rewrite waiting to
# happen; this gate fails before that dependency can become entrenched.
for layer in "$root/crates/contexts/advertising/src/domain" "$root/crates/contexts/advertising/src/application"; do
  if contains_platform_literal "$layer"; then
    echo "advertising invariant failed: platform literal outside adapter" >&2
    exit 1
  fi
done

# Future upload ports must make their safety types explicit. TASK-010 creates
# the port; TASK-014 supplies ConsentSnapshot. This detects a premature
# unsafe signature without imposing a placeholder port in the domain today.
port="$root/crates/contexts/advertising/src/domain/ports.rs"
if [[ -f "$port" ]] && rg -q 'trait AdPlatformPort' "$port"; then
  port_has_safe_upload_signature "$port" || {
    echo "advertising invariant failed: AdPlatformPort upload must accept HashedIds and ConsentSnapshot" >&2
    exit 1
  }
fi

# Bundled browser assets must never carry configured server-side credentials.
# This is intentionally fail-closed: CI must build the frontend and pass its
# artifact path instead of claiming to scan an absent sibling checkout.
scan_frontend_bundle

echo "ads invariants: OK"
