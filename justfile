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

membership-grant:
    cargo run -p sanvi-cli -- membership grant --email {{email}} --role {{role}} --tenant {{tenant}}

platform-grant:
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

tenant-create:
    cargo run -p sanvi-cli -- tenant create --slug {{slug}} --display-name "{{display_name}}" --region {{region}} --activate

tenant-list:
    cargo run -p sanvi-cli -- tenant list

# --- quality gates ----------------------------------------------------------

fmt:
    cargo fmt --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

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

test:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(./scripts/test-db.sh up)"
    if command -v cargo-nextest >/dev/null; then
        cargo nextest run --workspace
    else
        cargo test --workspace
    fi

openapi:
    cargo run -p sanvi-cli -- openapi --check

i18n:
    cargo run -p sanvi-cli -- i18n check

i18n-report:
    cargo run -p sanvi-cli -- i18n report

i18n-extract:
    cargo run -p sanvi-cli -- i18n extract

ci: fmt clippy layering connect-invariants ads-invariants test openapi i18n
