#!/usr/bin/env bash
# Contract-first gate: the committed api/*.yaml specs must match the
# OpenAPI generated from code annotations.
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
CLI_BIN="$TARGET_DIR/debug/sanvi-cli"
CACHE_FILE="$TARGET_DIR/.check-openapi.cache"

if command -v sha256sum >/dev/null 2>&1; then
    hash_files() { xargs -0 sha256sum; }
    hash_stream() { sha256sum | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    hash_files() { xargs -0 shasum -a 256; }
    hash_stream() { shasum -a 256 | awk '{print $1}'; }
elif command -v md5sum >/dev/null 2>&1; then
    hash_files() { xargs -0 md5sum; }
    hash_stream() { md5sum | awk '{print $1}'; }
elif command -v md5 >/dev/null 2>&1; then
    hash_files() { xargs -0 md5 -r; }
    hash_stream() { md5 | awk '{print $1}'; }
else
    hash_files() { xargs -0 cksum; }
    hash_stream() { cksum | awk '{print $1}'; }
fi

# `git ls-files --cached` lists index entries, so a file deleted in the working
# tree but not yet staged is still named here and the hash step then fails on a
# path that no longer exists. Drop those: an unstaged deletion is an ordinary
# working-tree state, not a reason for the gate to error out.
keep_existing() {
    while IFS= read -r -d '' path; do
        [[ -e "$path" ]] && printf '%s\0' "$path"
    done
    return 0
}

calc_code_hash() {
    local roots=(apps crates Cargo.toml Cargo.lock migrations .sqlx)
    if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git ls-files -z --cached --others --exclude-standard -- "${roots[@]}" | keep_existing | LC_ALL=C sort -z | hash_files | hash_stream
    else
        local existing_roots=()
        for root in "${roots[@]}"; do
            if [[ -e "$root" ]]; then
                existing_roots+=("$root")
            fi
        done
        if [[ "${#existing_roots[@]}" -gt 0 ]]; then
            find "${existing_roots[@]}" \( -name target -o -name .git \) -prune -o -type f -print0 | LC_ALL=C sort -z | hash_files | hash_stream
        else
            printf "" | hash_stream
        fi
    fi
}

code_hash=$(calc_code_hash)

if [[ -f "$CACHE_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$CACHE_FILE" 2>/dev/null || true
fi

bin_hash=""
if [[ -f "$CLI_BIN" ]]; then
    bin_hash=$(hash_stream < "$CLI_BIN")
fi

# Rebuild sanvi-cli if the binary is missing, code inputs changed, or binary was rebuilt elsewhere.
if [[ ! -x "$CLI_BIN" || "$code_hash" != "${CACHED_CODE_HASH:-}" || "$bin_hash" != "${CACHED_BIN_HASH:-}" ]]; then
    cargo build -p sanvi-cli
fi

# If CLI_BIN is not executable after build (e.g. custom CARGO_BUILD_TARGET), fall back to cargo run.
if [[ ! -x "$CLI_BIN" ]]; then
    echo "sanvi-cli binary not found at $CLI_BIN; falling back to cargo run" >&2
    cargo run -p sanvi-cli -- openapi --check
    exit $?
fi

# Run the OpenAPI check using the compiled binary directly.
"$CLI_BIN" openapi --check

# Record successful check in cache.
bin_hash=$(hash_stream < "$CLI_BIN")
{
    mkdir -p "$TARGET_DIR"
    cat > "$CACHE_FILE" <<EOF
CACHED_CODE_HASH=$code_hash
CACHED_BIN_HASH=$bin_hash
EOF
} 2>/dev/null || true
