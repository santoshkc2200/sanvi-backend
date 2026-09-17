# Sanvi task runner. Install: brew install just  (optional; scripts/*.sh
# cover the same ground without it).

default:
    @just --list

# --- local development -----------------------------------------------------

up:
    ./scripts/dev.sh up

up-full:
    ./scripts/dev.sh up --full

down:
    ./scripts/dev.sh down

reset:
    ./scripts/dev.sh reset

logs:
    ./scripts/dev.sh logs -f

migrate:
    ./scripts/migrate.sh

migrate-down:
    ./scripts/migrate.sh down

seed:
    ./scripts/seed.sh

# TASK-019 (FR-1101): deterministic production-shaped scale data. Targets a
# staging-named database (SANVI__DATABASE__NAME=sanvi_scale, say) — the
# command refuses anything that does not match the staging pattern.
seed-scale *args:
    cargo run -p sanvi-cli -- seed-scale {{args}}

seed-roles:
    cargo run -p sanvi-cli -- seed roles

seed-access:
    cargo run -p sanvi-cli -- seed access

seed-features:
    cargo run -p sanvi-cli -- seed features

feature-list:
    cargo run -p sanvi-cli -- feature list

entitlement-grant tenant feature:
    cargo run -p sanvi-cli -- entitlement grant --tenant {{tenant}} --feature {{feature}}

entitlement-revoke tenant feature:
    cargo run -p sanvi-cli -- entitlement revoke --tenant {{tenant}} --feature {{feature}}

entitlement-list tenant:
    cargo run -p sanvi-cli -- entitlement list --tenant {{tenant}}

impersonation-list:
    cargo run -p sanvi-cli -- impersonation list

audit-verify:
    cargo run -p sanvi-cli -- audit verify

audit-list:
    cargo run -p sanvi-cli -- audit list

approval-list:
    cargo run -p sanvi-cli -- approval list

membership-grant email role tenant:
    cargo run -p sanvi-cli -- membership grant --email {{email}} --role {{role}} --tenant {{tenant}}

platform-grant email role:
    cargo run -p sanvi-cli -- membership grant --email {{email}} --role {{role}} --platform

api:
    cargo run -p sanvi-api

worker:
    cargo run -p sanvi-worker

config-check:
    cargo run -p sanvi-cli -- config

billing-plans:
    cargo run -p sanvi-cli -- billing plans

billing-tax-check:
    cargo run -p sanvi-cli -- billing tax-check

billing-reconcile:
    cargo run -p sanvi-cli -- billing reconcile

billing-webhooks:
    cargo run -p sanvi-cli -- billing webhooks

tenant-create slug display_name region:
    cargo run -p sanvi-cli -- tenant create --slug {{slug}} --display-name "{{display_name}}" --region {{region}} --activate

tenant-list:
    cargo run -p sanvi-cli -- tenant list

# --- quality gates ----------------------------------------------------------

fmt:
    cargo fmt --check

# No crate defines [features] (check with: rg -n '^\[features\]' -g '**/Cargo.toml' before re-adding it); --all-features omitted to save resolver overhead.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

layering:
    ./scripts/check-layering.sh

connect-invariants:
    ./scripts/check-connect-invariants.sh

connect-invariants-self-test:
    ./scripts/check-connect-invariants.sh --self-test

ads-invariants:
    ./scripts/check-ads-invariants.sh

ads-invariants-self-test:
    ./scripts/check-ads-invariants.sh --self-test

id-policy:
    ./scripts/check-id-policy.sh

id-policy-self-test:
    ./scripts/check-id-policy.sh --self-test

# Every member that ships tests/ is run by a job in pr.yml, proven by a
# self-test that drops one and expects failure. TASK-031.
ci-test-coverage:
    ./scripts/check-ci-test-coverage.sh

ci-test-coverage-self-test:
    ./scripts/check-ci-test-coverage.sh --self-test

# `just test` and `just test-quiet` need nothing from this recipe -- they run
# the same script themselves and eval its output in their own shell.
#
# Running *this* recipe before a bare `cargo` is the trap: `just` runs it in a
# subprocess, so the `export` lines it prints are shown to you but never reach
# your shell, and the tests then take the fallback path and leak a container
# per test process anyway. To scope a run by hand, run the script directly, so
# the eval happens in the shell the tests will inherit:
#
#     eval "$(./scripts/test-db.sh up)"
#     cargo nextest run -p sanvi-billing
#
# One Postgres + Redis for the whole run; each test still gets its own
# database, cloned from a pre-migrated template. Without `test-db` every test
# process starts (and leaves behind) a container of its own.
test-db:
    ./scripts/test-db.sh up

test-db-down:
    ./scripts/test-db.sh down

# `target/debug/incremental` is a cache cargo writes to but never prunes: it
# reached 101 GB on a machine that had been building this workspace for a
# while, at which point reading it costs more than the rebuilds it saves.
# Deleting it only costs one full rebuild.
clean-incremental:
    rm -rf target/debug/incremental

# `deps` and `.fingerprint` accumulate one distinct compiled copy of every
# crate per toolchain/profile/tool combination (plain rustc vs clippy-driver
# are cached separately by design) and cargo never deletes old ones as code
# changes over days of work. `cargo sweep` walks the whole `target/` tree —
# release and doc artifacts included, not just debug — and removes every copy
# untouched for 14+ days.
clean-stale:
    if command -v cargo-sweep >/dev/null 2>&1; then \
        cargo sweep --time 14; \
    else \
        echo 'cargo-sweep not installed; run: cargo install cargo-sweep'; \
    fi

# Unit + application tests only: no Docker, no database, no container start.
# The inner loop. `just test` remains the full gate.
test-fast:
    cargo nextest run --workspace -E 'kind(lib) or kind(bin)'

test:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(./scripts/test-db.sh up)"
    if command -v cargo-nextest >/dev/null; then
        cargo nextest run --workspace
    else
        cargo test --workspace
    fi

# Same suite, failures only. A passing run of 822 tests prints 822 lines under
# the default profile; the `agent` nextest profile prints none of them and the
# full captured output of anything that fails. For CI logs, and for coding
# agents, where every passing line is paid for and read by nobody.
test-quiet:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(./scripts/test-db.sh up)"
    cargo nextest run --workspace --profile agent

# Type-check only, one line per diagnostic. The cheapest gate that catches most
# mistakes — run this before `test`, which has to link every test binary.
check:
    cargo c

# `clippy` with one line per diagnostic instead of a rendered snippet.
clippy-quiet:
    cargo k

openapi:
    ./scripts/check-openapi.sh

i18n:
    cargo run -p sanvi-cli -- i18n check

i18n-report:
    cargo run -p sanvi-cli -- i18n report

i18n-extract:
    cargo run -p sanvi-cli -- i18n extract

ci: fmt clippy layering connect-invariants ads-invariants id-policy ci-test-coverage test openapi i18n

# TASK-019 (FR-1103): run the k6 load shapes against a running API and
# write benchmarks/<scenario>/<tag>.json. Point PERF_BASE_URL at the API
# (default http://localhost:8081, scale data loaded) and set PERF_TAG per
# run: `just perf rc.1`. Requires k6.
perf tag="local" scenario="all":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v k6 >/dev/null 2>&1; then
        echo "k6 is required (brew install k6)" >&2
        exit 1
    fi
    mem_gb=0
    if command -v sysctl >/dev/null 2>&1; then
        mem_gb=$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 ))
    elif [[ -r /proc/meminfo ]]; then
        mem_gb=$(( $(awk '/MemTotal/ {print int($2/1048576)}' /proc/meminfo) ))
    fi
    export PERF_TAG="{{tag}}"
    export PERF_BASE_URL="${PERF_BASE_URL:-http://localhost:8081}"
    export PERF_SEED="${PERF_SEED:-1}"
    export PERF_MACHINE="{\"cores\":$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 0),\"mem_gb\":${mem_gb},\"os\":\"$(uname -s) $(uname -r)\",\"arch\":\"$(uname -m)\"}"
    scenarios=(storefront_read admin_write checkout_create webhook_flood conversion_upload)
    if [[ "{{scenario}}" != "all" ]]; then
        scenarios=("{{scenario}}")
    fi
    for s in "${scenarios[@]}"; do
        echo "perf: $s (tag {{tag}})"
        mkdir -p "benchmarks/$s"
        case "$s" in
            storefront_read) rate=50 ;;
            admin_write) rate=20 ;;
            checkout_create) rate=10 ;;
            webhook_flood) rate=100 ;;
            conversion_upload) rate=50 ;;
            *) rate=0 ;;
        esac
        k6 run -e PERF_SCENARIO="$s" -e PERF_RATE="${PERF_RATE:-$rate}" "tests/load/$s.js"
    done

# TASK-019 (FR-1103): diff two tagged runs; a regression past the
# per-metric tolerance blocks the merge. Refuses artifacts whose machine
# profiles differ. `--self-test` proves the check fails on an injected
# latency increase and refuses a foreign machine profile.
perf-compare base_tag="base" cand_tag="local" *scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    ./scripts/perf-compare.sh "{{base_tag}}" "{{cand_tag}}" {{scenario}}

perf-compare-self-test:
    ./scripts/perf-compare.sh --self-test

# `ci` with the low-noise variants of the two chattiest gates.
ci-quiet: fmt clippy-quiet layering connect-invariants ads-invariants id-policy ci-test-coverage test-quiet openapi i18n
