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

test:
    cargo test --workspace

openapi:
    cargo run -p sanvi-cli -- openapi --check

ci: fmt clippy layering test openapi
