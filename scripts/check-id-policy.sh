#!/usr/bin/env bash
# Guard the Snowflake migration boundary. First-party entity IDs are signed
# INT8 storage and decimal strings at boundaries; UUIDs remain only for the
# named external/correlation fields in scripts/id-policy-uuid-exceptions.txt.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exceptions="$root/scripts/id-policy-uuid-exceptions.txt"

fail=0
report() {
  printf 'id policy violation: %s\n' "$*" >&2
  fail=1
}

allowed_uuid_field() {
  local field="$1"
  rg -q "^${field}:" "$exceptions"
}

check_sql_file() {
  local file="$1"
  while IFS=: read -r line text; do
    local field
    [[ "$text" =~ ([a-z_]+)[[:space:]]+uuid ]] || continue
    field="${BASH_REMATCH[1]}"
    if ! allowed_uuid_field "$field"; then
      report "$file:$line first-party UUID column: $text"
    fi
  done < <(rg -n -i '\b[a-z_]+[[:space:]]+uuid\b' "$file" || true)
}

check_tree() {
  local path
  while IFS= read -r path; do
    check_sql_file "$path"
  done < <(rg --files "$root/migrations" -g '*.sql')

  check_legacy_rust "$root/crates" "$root/apps"
}

check_legacy_rust() {
  if rg -n 'Id::(from_uuid|as_uuid)|Uuid::new_v5' "$@"; then
    report 'legacy UUID entity-ID conversion API found'
  fi
}

self_test() {
  local fixture
  fixture="$(mktemp -d)"
  trap 'rm -rf "${fixture:-}"' EXIT

  printf 'CREATE TABLE bad (id uuid PRIMARY KEY);\n' > "$fixture/bad.sql"
  if check_sql_file "$fixture/bad.sql"; then :; fi
  if [[ "$fail" != 1 ]]; then
    printf 'self-test failed: UUID entity column was accepted\n' >&2
    return 1
  fi

  fail=0
  printf 'CREATE TABLE good (kratos_identity_id uuid);\n' > "$fixture/good.sql"
  check_sql_file "$fixture/good.sql"
  [[ "$fail" == 0 ]] || {
    printf 'self-test failed: documented UUID exception was rejected\n' >&2
    return 1
  }

  fail=0
  printf 'Id::from_uuid(value);\n' > "$fixture/bad.rs"
  check_legacy_rust "$fixture/bad.rs"
  if [[ "$fail" != 1 ]]; then
    printf 'self-test failed: legacy UUID conversion was accepted\n' >&2
    return 1
  fi
  fail=0
}

case "${1:-}" in
  '') check_tree ;;
  --self-test) self_test; check_tree ;;
  *) printf 'usage: %s [--self-test]\n' "$0" >&2; exit 2 ;;
esac

[[ "$fail" == 0 ]]
