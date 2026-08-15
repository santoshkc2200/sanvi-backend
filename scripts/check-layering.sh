#!/usr/bin/env bash
# Architecture lint: domain modules must stay free of infrastructure crates.
#
#   ./scripts/check-layering.sh            # check the workspace
#   ./scripts/check-layering.sh --self-test # prove the lint catches violations
#
# Non-negotiable #1 from docs/architecture-overview.md: no sqlx, actix,
# reqwest (or any HTTP/DB runtime) inside a src/domain/ module.

set -euo pipefail

cd "$(dirname "$0")/.."

FORBIDDEN=(sqlx actix_web actix-web reqwest hyper tokio_postgres diesel redis aws_sdk)

check() {
    local violations=0
    while IFS= read -r file; do
        for crate in "${FORBIDDEN[@]}"; do
            if grep -nE "use +${crate}|::${crate}::|extern crate +${crate}" "$file" >/dev/null 2>&1; then
                echo "LAYERING VIOLATION: $file imports $crate" >&2
                grep -nE "use +${crate}|::${crate}::|extern crate +${crate}" "$file" >&2
                violations=$((violations + 1))
            fi
        done
    done < <(find crates -path '*/src/domain/*' -name '*.rs' -type f 2>/dev/null || true)
    return "$violations"
}

if [[ "${1:-}" == "--self-test" ]]; then
    FIXTURE="crates/contexts/system/src/domain/__layering_fixture.rs"
    trap 'rm -f "$FIXTURE"' EXIT
    cat > "$FIXTURE" <<'EOF'
// CI fixture: proves the layering lint fails on a violating domain file.
use sqlx::PgPool;
EOF
    if check; then
        echo "SELF-TEST FAILED: violation was not detected" >&2
        exit 1
    fi
    echo "self-test OK: violation detected as expected"
    exit 0
fi

if check; then
    echo "layering OK: no infrastructure imports under src/domain"
else
    echo "layering violations found (see above)" >&2
    exit 1
fi
