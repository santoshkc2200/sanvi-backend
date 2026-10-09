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

# TASK-020 (FR-1105): every use case carries a `<context>.<usecase>` span.
# Regenerates observability/trace-coverage.md and fails on any unspanned
# use case — or on a stale committed matrix.
trace-coverage:
    ./scripts/check-trace-coverage.sh

trace-coverage-write:
    ./scripts/check-trace-coverage.sh --write

trace-coverage-self-test:
    ./scripts/check-trace-coverage.sh --self-test

# TASK-020 (FR-1106): the log emitter never renders a corpus PII value.
# `--self-test` plants one unredacted emission and expects the scan to
# catch it.
log-pii-scan:
    ./scripts/check-log-pii.sh

log-pii-scan-self-test:
    ./scripts/check-log-pii.sh --self-test

# TASK-024 (FR-1116): every secret-shaped struct field is Redacted/SecretRef
# wrapped or carries a reasoned, rule-satisfying exemption. `--self-test`
# plants an unredacted field in a real src tree and expects the gate to
# fail naming it.
secret-scan:
    ./scripts/check-secret-scan.sh

secret-scan-self-test:
    ./scripts/check-secret-scan.sh --self-test

# TASK-024 (FR-1117): dependency advisory gate. Accepted findings live in
# deny.toml (the ledger) with an `expires:` date in each reason; the
# acceptance check fails the build on a missing or expired date.
audit:
    cargo audit
    cargo deny check
    ./scripts/check-audit-acceptances.sh

audit-acceptances-self-test:
    ./scripts/check-audit-acceptances.sh --self-test

# TASK-024 (FR-1117): CycloneDX SBOM for the whole workspace. The release
# workflow publishes the artifact with the tag (.github/workflows/release.yml);
# this recipe is the local form of the same command.
sbom:
    @command -v cargo-cyclonedx >/dev/null 2>&1 || { echo "cargo-cyclonedx not installed; run: cargo install cargo-cyclonedx --locked" >&2; exit 1; }
    cargo cyclonedx --all --format json

# TASK-020 (FR-1106): the alert registry evaluated against recorded
# series — a ten-minute outage fires, a single failed request does not,
# and every rule fires on its own synthetic breach.
# Bare `cargo nextest` is exempted here only because `alert_rules` is
# DB-less (no testsupport Postgres fixture); if it ever grows one, route
# through scripts/test.sh — see CLAUDE.md on stranded containers.
alert-rules-test:
    cargo nextest run -p sanvi-telemetry --test alert_rules

# TASK-020 (FR-1105): the runbook loop, both directions — every alert has
# a runbook that declares it, every declared alert is registered, and
# every alert-less runbook is exempted with a reason.
runbook-xref:
    ./scripts/check-runbook-xref.sh

runbook-xref-self-test:
    ./scripts/check-runbook-xref.sh --self-test

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

# TASK-024 slice 11.5a: complete reviewed route inventory and committed freshness.
route-matrix:
    ./scripts/generate-route-matrix.sh

route-matrix-write:
    ./scripts/generate-route-matrix.sh --write

route-matrix-self-test:
    ./scripts/generate-route-matrix.sh --self-test

# TASK-024 slice 11.5b: the cross-tenant IDOR sweep over the route
# matrix. Scoped to the one test binary; `just test` runs it as part of
# the workspace suite anyway.
test-idor:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(./scripts/test-db.sh up)"
    cargo nextest run -p sanvi-api --test integration idor_sweep

# TASK-024 slice 11.5c: the schema-wide RLS matrix — every tenant-scoped
# table catalog-enumerated, every child partition asserted by name. Same
# scoping convention as test-idor: the workspace suite runs it too.
rls-matrix:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(./scripts/test-db.sh up)"
    cargo nextest run -p sanvi-api --test integration rls_matrix

# TASK-021 (FR-1108/FR-1110): the perf test target — the bounded-collections
# contract gate (11.2b) and the EXPLAIN plan gate for the profiled read
# models (11.2d); statement-count guards (11.2e) join it as their slice
# lands. Same scoping convention as test-idor: the workspace suite runs it
# too.
test-perf:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(./scripts/test-db.sh up)"
    cargo nextest run -p sanvi-api --test perf

# TASK-022 (FR-1111): the SSR cache-key partition tests — two tenants
# cannot share an entry, and locale, theme revision and device class each
# partition the key. No database; runs in the workspace suite too.
test-cache-keys:
    cargo nextest run -p sanvi-cache --test ssr_cache_negative

# TASK-023 (FR-1112): one injection test per dependency-matrix row. The
# gate runs first (every row names a test that exists, every test file is
# named by a row), then the suite itself. In CI the suite runs inside the
# integration job's derived crate list; the gate runs in resilience-gates.
test-dependency-matrix:
    #!/usr/bin/env bash
    set -euo pipefail
    ./scripts/check-dependency-matrix.sh
    eval "$(./scripts/test-db.sh up)"
    cargo nextest run -p resilience-tests

dependency-matrix-self-test:
    ./scripts/check-dependency-matrix.sh --self-test

# TASK-023 (NFR-1105): no unbounded outbound call — every reqwest client
# sets a timeout.
check-timeouts:
    ./scripts/check-timeouts.sh

check-timeouts-self-test:
    ./scripts/check-timeouts.sh --self-test

# TASK-023 (FR-1112): expand/contract migrations — a live-column touch
# needs a same-file contract step or an explicit annotation.
check-migration-expand-contract:
    ./scripts/check-migration-expand-contract.sh

check-migration-expand-contract-self-test:
    ./scripts/check-migration-expand-contract.sh --self-test

# TASK-023 (FR-1112): rolling-restart readiness. The live drill recreates
# staging containers under k6 load and runs on the staging host only;
# this recipe validates the mechanisms it depends on.
rolling-restart-dry-run:
    ./scripts/rolling-restart.sh --dry-run

openapi:
    ./scripts/check-openapi.sh

i18n:
    cargo run -p sanvi-cli -- i18n check

i18n-report:
    cargo run -p sanvi-cli -- i18n report

i18n-extract:
    cargo run -p sanvi-cli -- i18n extract

ci: fmt clippy layering connect-invariants ads-invariants id-policy ci-test-coverage test test-perf openapi route-matrix i18n ga-checklist-check trace-coverage log-pii-scan secret-scan runbook-xref alert-rules-test test-dependency-matrix check-timeouts check-migration-expand-contract rolling-restart-dry-run

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
    # Gate on the OS, not on tool presence: Linux ships a `sysctl` too
    # (procps), whose hw.memsize probe fails and left every Linux artifact
    # recording mem_gb: 0 — two Linux hosts then profile as one machine.
    mem_gb=0
    case "$(uname -s)" in
        Darwin)
            mem_gb=$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 ))
            ;;
        Linux)
            if [[ -r /proc/meminfo ]]; then
                mem_gb=$(( $(awk '/MemTotal/ {print int($2/1048576)}' /proc/meminfo) ))
            fi
            ;;
    esac
    export PERF_TAG="{{tag}}"
    export PERF_BASE_URL="${PERF_BASE_URL:-http://localhost:8081}"
    export PERF_SEED="${PERF_SEED:-1}"
    export PERF_MACHINE="{\"cores\":$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 0),\"mem_gb\":${mem_gb},\"os\":\"$(uname -s) $(uname -r)\",\"arch\":\"$(uname -m)\"}"
    scenarios=(storefront_read admin_write checkout_create webhook_flood conversion_upload)
    if [[ "{{scenario}}" != "all" ]]; then
        scenarios=("{{scenario}}")
    fi
    # k6 exits 99 on a threshold breach: keep sweeping so one breaching
    # shape doesn't leave the tag with partial artifacts — the breach is
    # still reported and still fails the recipe once every shape ran.
    rc=0
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
        if ! k6 run -e PERF_SCENARIO="$s" -e PERF_RATE="${PERF_RATE:-$rate}" "tests/load/$s.js"; then
            echo "perf: $s FAILED (threshold breach or k6 error; artifact may still have been written)" >&2
            rc=1
        fi
    done
    exit "$rc"

# TASK-019 (FR-1103): diff two tagged runs; a regression past the
# per-metric tolerance blocks the merge. Refuses artifacts whose machine
# profiles differ, whose stats_version is missing or mismatched, or whose
# build stamp is missing or failed; a build-stamp difference is announced.
# `--self-test` proves the check fails on an injected latency increase and
# refuses a foreign machine profile.
perf-compare base_tag="base" cand_tag="local" *scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    ./scripts/perf-compare.sh "{{base_tag}}" "{{cand_tag}}" {{scenario}}

perf-compare-self-test:
    ./scripts/perf-compare.sh --self-test

# TASK-019 (FR-1104): every GA-criterion line carries a state; fails the
# build while any line has none.
ga-checklist-check:
    ./scripts/check-ga-checklist.sh

# `ci` with the low-noise variants of the two chattiest gates.
ci-quiet: fmt clippy-quiet layering connect-invariants ads-invariants id-policy ci-test-coverage test-quiet test-perf openapi route-matrix i18n ga-checklist-check trace-coverage log-pii-scan secret-scan runbook-xref alert-rules-test test-dependency-matrix check-timeouts check-migration-expand-contract rolling-restart-dry-run
