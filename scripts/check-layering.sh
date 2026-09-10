#!/usr/bin/env bash
# Architecture lint: domain modules must stay free of infrastructure crates,
# contexts must preserve boundary ownership, application services must not
# depend on adapters, and SQL queries must not cross context schemas.
#
#   ./scripts/check-layering.sh            # check the workspace
#   ./scripts/check-layering.sh --self-test # prove the lint catches violations
#
# Rules enforced (from docs/architecture-overview.md):
# 1. Domain purity: no infrastructure crates (sqlx, actix_web, reqwest,
#    hyper, tokio_postgres, diesel, redis, aws_sdk, stripe, sanvi_persistence)
#    inside src/domain/ modules.
# 2. Context ownership: a context must not import another context's domain
#    or repository types.
# 3. Hexagonal direction: application use cases must not depend on adapters
#    (crate::adapters / super::adapters / adapters::).
# 4. Persistence boundaries: one schema per context; cross-schema SQL joins
#    and cross-context schema queries are forbidden.

set -euo pipefail

cd "$(dirname "$0")/.."

FORBIDDEN=(sqlx actix_web actix-web reqwest hyper tokio_postgres diesel redis aws_sdk stripe sanvi_persistence sanvi-persistence)

CONTEXTS="access advertising audit billing domains identity localization media notifications payments platform_admin privacy system tenancy theming"

is_context() {
    local needle="$1"
    for c in $CONTEXTS; do
        [[ "$c" == "$needle" ]] && return 0
    done
    return 1
}

# ==============================================================================
# Narrow, individually justified architecture exceptions
# (Documented in docs/architecture-overview.md)
# ==============================================================================

# Exception 1: sanvi_persistence in domain port signatures
# Technical debt: ports take concrete ScopedTx/PlatformScope/PgPool handles
# pending transaction abstraction migration (ADR-002).
is_accepted_domain_infra_exception() {
    local file="$1"
    local crate="$2"
    if [[ "$crate" != "sanvi_persistence" && "$crate" != "sanvi-persistence" ]]; then
        return 1
    fi
    case "$file" in
        crates/contexts/access/src/domain/ports.rs|\
        crates/contexts/advertising/src/domain/campaign_store.rs|\
        crates/contexts/advertising/src/domain/events.rs|\
        crates/contexts/advertising/src/domain/metrics.rs|\
        crates/contexts/advertising/src/domain/ports.rs|\
        crates/contexts/advertising/src/domain/token_vault.rs|\
        crates/contexts/audit/src/domain/ports.rs|\
        crates/contexts/billing/src/domain/ports.rs|\
        crates/contexts/domains/src/domain/ports.rs|\
        crates/contexts/identity/src/domain/ports.rs|\
        crates/contexts/localization/src/domain/ports.rs|\
        crates/contexts/payments/src/domain/events.rs|\
        crates/contexts/payments/src/domain/ports.rs|\
        crates/contexts/platform_admin/src/domain/ports.rs|\
        crates/contexts/privacy/src/domain/ports.rs|\
        crates/contexts/tenancy/src/domain/ports.rs|\
        crates/contexts/theming/src/domain/ports.rs)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

# Exception 2: Cross-context domain/repository imports
# Narrowly permitted existing integrations pending cross-context port extraction.
is_accepted_context_ownership_exception() {
    local file="$1"
    local other_ctx="$2"
    case "$file" in
        # Identity membership reading for relation-tuple generation & role verification
        crates/contexts/access/src/adapters/membership_source.rs|\
        crates/contexts/access/src/adapters/role_catalog.rs|\
        crates/contexts/access/src/application/rebuild_tuples.rs)
            [[ "$other_ctx" == "identity" ]] && return 0
            ;;
        # Audit logging in security-critical access use cases
        crates/contexts/access/src/application/entitlements.rs|\
        crates/contexts/access/src/application/features.rs|\
        crates/contexts/access/src/application/roles.rs)
            [[ "$other_ctx" == "audit" ]] && return 0
            ;;
        # Ad conversion tracking & consent resolution via privacy context
        crates/contexts/advertising/src/adapters/http/tracking_handlers.rs|\
        crates/contexts/advertising/src/adapters/vendor_role.rs|\
        crates/contexts/advertising/src/application/tracking/audiences.rs|\
        crates/contexts/advertising/src/application/tracking/capture.rs|\
        crates/contexts/advertising/src/application/tracking/retraction.rs|\
        crates/contexts/advertising/src/application/tracking/retry.rs|\
        crates/contexts/advertising/src/application/tracking/upload.rs)
            [[ "$other_ctx" == "privacy" ]] && return 0
            ;;
        # Ad conversion verification against payments checkout repository
        crates/contexts/advertising/src/adapters/payment_lookup.rs)
            [[ "$other_ctx" == "payments" ]] && return 0
            ;;
        # Plan entitlement sync to access context
        crates/contexts/billing/src/adapters/sync.rs)
            [[ "$other_ctx" == "access" ]] && return 0
            ;;
        # Billing plan management syncing access features and emitting audit entries
        crates/contexts/billing/src/application/plans.rs)
            case "$other_ctx" in
                access|audit) return 0 ;;
            esac
            ;;
        # Audit logging in billing mutations
        crates/contexts/billing/src/application/checkout.rs|\
        crates/contexts/billing/src/application/override_sub.rs|\
        crates/contexts/billing/src/application/portal.rs)
            [[ "$other_ctx" == "audit" ]] && return 0
            ;;
        # Audit logging in payments mutations
        crates/contexts/payments/src/application/account_session.rs|\
        crates/contexts/payments/src/application/create_connection.rs|\
        crates/contexts/payments/src/application/disconnect.rs|\
        crates/contexts/payments/src/application/export.rs|\
        crates/contexts/payments/src/application/refund.rs)
            [[ "$other_ctx" == "audit" ]] && return 0
            ;;
        # Platform admin composite aggregate delegating across access, identity, tenancy, audit
        crates/contexts/platform_admin/src/adapters/gates.rs|\
        crates/contexts/platform_admin/src/domain/ports.rs)
            [[ "$other_ctx" == "access" ]] && return 0
            ;;
        crates/contexts/platform_admin/src/adapters/verifier.rs)
            [[ "$other_ctx" == "identity" ]] && return 0
            ;;
        crates/contexts/platform_admin/src/adapters/repo.rs)
            case "$other_ctx" in
                access|identity|tenancy|audit) return 0 ;;
            esac
            ;;
        crates/contexts/platform_admin/src/application/approvals.rs)
            [[ "$other_ctx" == "audit" ]] && return 0
            ;;
        crates/contexts/platform_admin/src/application/impersonation.rs)
            case "$other_ctx" in
                audit|identity) return 0 ;;
            esac
            ;;
        # Privacy SPI implementations: each context implements PersonalDataProvider in adapters
        crates/contexts/access/src/adapters/privacy_provider.rs|\
        crates/contexts/advertising/src/adapters/privacy_provider.rs|\
        crates/contexts/audit/src/adapters/privacy_provider.rs|\
        crates/contexts/billing/src/adapters/privacy_provider.rs|\
        crates/contexts/domains/src/adapters/privacy_provider.rs|\
        crates/contexts/identity/src/adapters/privacy_provider.rs|\
        crates/contexts/localization/src/adapters/privacy_provider.rs|\
        crates/contexts/media/src/adapters/privacy_provider.rs|\
        crates/contexts/notifications/src/adapters/privacy_provider.rs|\
        crates/contexts/payments/src/adapters/privacy_provider.rs|\
        crates/contexts/platform_admin/src/adapters/privacy_provider.rs|\
        crates/contexts/system/src/adapters/privacy_provider.rs|\
        crates/contexts/tenancy/src/adapters/privacy_provider.rs|\
        crates/contexts/theming/src/adapters/privacy_provider.rs)
            [[ "$other_ctx" == "privacy" ]] && return 0
            ;;
    esac
    return 1
}

# Exception 3: Application layer references to concrete adapters
# Concrete metrics, in-memory caches, adapter utilities, and unit test fakes.
is_accepted_app_adapter_exception() {
    local file="$1"
    case "$file" in
        crates/contexts/advertising/src/application/budget/evaluator.rs|\
        crates/contexts/advertising/src/application/catalog.rs|\
        crates/contexts/advertising/src/application/connections.rs|\
        crates/contexts/advertising/src/application/metrics/sweep.rs|\
        crates/contexts/advertising/src/application/registry.rs|\
        crates/contexts/advertising/src/application/token_refresh.rs|\
        crates/contexts/advertising/src/application/tracking/audiences.rs|\
        crates/contexts/advertising/src/application/tracking/retraction.rs|\
        crates/contexts/advertising/src/application/tracking/upload.rs|\
        crates/contexts/billing/src/application/webhooks.rs|\
        crates/contexts/domains/src/application/lookup.rs|\
        crates/contexts/localization/src/application/catalog_service.rs|\
        crates/contexts/payments/src/application/catalog.rs|\
        crates/contexts/payments/src/application/sync_connection.rs|\
        crates/contexts/payments/src/application/tax_settings.rs|\
        crates/contexts/payments/src/application/webhooks.rs|\
        crates/contexts/privacy/src/application/exports.rs|\
        crates/contexts/privacy/src/application/retention.rs|\
        crates/contexts/theming/src/application/publish.rs|\
        crates/contexts/theming/src/application/resolve.rs)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

# Exception 4: Cross-schema SQL joins and queries
is_accepted_cross_schema_exception() {
    local file="$1"
    local target_schema="$2"
    case "$file" in
        crates/contexts/billing/src/adapters/repos.rs)
            [[ "$target_schema" == "tenancy" ]] && return 0
            ;;
        crates/contexts/localization/src/adapters/repos.rs)
            [[ "$target_schema" == "identity" ]] && return 0
            ;;
    esac
    return 1
}

# ==============================================================================
# Enforcement checks
# ==============================================================================

VIOLATIONS=0

check_domain_layering() {
    for crate in "${FORBIDDEN[@]}"; do
        local pattern="use +${crate}|(^|[^A-Za-z0-9_])${crate}::|extern crate +${crate}"
        while IFS=: read -r file line match; do
            [[ -z "$file" ]] && continue
            local rel_file="${file#./}"
            if ! is_accepted_domain_infra_exception "$rel_file" "$crate"; then
                echo "DOMAIN PURITY VIOLATION: $rel_file:$line imports infrastructure crate '$crate'" >&2
                echo "  $match" >&2
                VIOLATIONS=$((VIOLATIONS + 1))
            fi
        done < <(rg -n -g '**/src/domain/**.rs' -e "use +${crate}" -e "(^|[^A-Za-z0-9_])${crate}::" -e "extern crate +${crate}" crates/ 2>/dev/null || true)
    done
}

check_context_ownership() {
    while IFS=: read -r file line match; do
        [[ -z "$file" ]] && continue
        local rel_file="${file#./}"
        if [[ "$rel_file" =~ ^crates/contexts/([a-zA-Z0-9_]+)/src/ ]]; then
            local ctx="${BASH_REMATCH[1]}"
        else
            continue
        fi
        if [[ "$match" =~ sanvi_([a-zA-Z0-9_]+):: ]]; then
            local other="${BASH_REMATCH[1]}"
            if is_context "$other" && [[ "$other" != "$ctx" ]]; then
                if ! is_accepted_context_ownership_exception "$rel_file" "$other"; then
                    echo "CONTEXT OWNERSHIP VIOLATION: $rel_file:$line in context '$ctx' imports from context '$other'" >&2
                    echo "  $match" >&2
                    VIOLATIONS=$((VIOLATIONS + 1))
                fi
            fi
        fi
    done < <(rg -n "sanvi_[a-zA-Z0-9_]+::(domain|adapters|[A-Za-z0-9_]*Repo)" crates/contexts/*/src/ 2>/dev/null || true)
}

check_app_adapter_direction() {
    while IFS=: read -r file line match; do
        [[ -z "$file" ]] && continue
        local rel_file="${file#./}"
        if ! is_accepted_app_adapter_exception "$rel_file"; then
            echo "HEXAGONAL DIRECTION VIOLATION: $rel_file:$line in application layer depends on adapters" >&2
            echo "  $match" >&2
            VIOLATIONS=$((VIOLATIONS + 1))
        fi
    done < <(rg -n "\b(?:crate|super)::adapters\b|\badapters::" crates/contexts/*/src/application/ 2>/dev/null || true)
}

check_cross_schema_sql() {
    while IFS=: read -r file line match; do
        [[ -z "$file" ]] && continue
        local rel_file="${file#./}"
        if [[ "$rel_file" =~ ^crates/contexts/([a-zA-Z0-9_]+)/src/ ]]; then
            local ctx="${BASH_REMATCH[1]}"
        else
            continue
        fi
        if [[ "$match" =~ [Ff][Rr][Oo][Mm]|[Jj][Oo][Ii][Nn]|[Ii][Nn][Ss][Ee][Rr][Tt][[:space:]]+[Ii][Nn][Tt][Oo]|[Uu][Pp][Dd][Aa][Tt][Ee]|[Tt][Aa][Bb][Ll][Ee] ]]; then
            if [[ "$match" =~ ([a-zA-Z0-9_]+)\.([a-zA-Z0-9_]+) ]]; then
                local raw_target="${BASH_REMATCH[1]}"
                local target
                target="$(printf "%s" "$raw_target" | tr "[:upper:]" "[:lower:]")"
                if is_context "$target" && [[ "$target" != "$ctx" ]]; then
                    if ! is_accepted_cross_schema_exception "$rel_file" "$target"; then
                        echo "CROSS-SCHEMA SQL VIOLATION: $rel_file:$line in context '$ctx' queries schema '$target'" >&2
                        echo "  $match" >&2
                        VIOLATIONS=$((VIOLATIONS + 1))
                    fi
                fi
            fi
        fi
    done < <(rg -i -n "\b(FROM|JOIN|INTO|UPDATE|TABLE)\s+([a-zA-Z0-9_]+)\.([a-zA-Z0-9_]+)" crates/contexts/*/src/ 2>/dev/null || true)
}

check() {
    VIOLATIONS=0
    check_domain_layering
    check_context_ownership
    check_app_adapter_direction
    check_cross_schema_sql

    if [[ "$VIOLATIONS" -eq 0 ]]; then
        return 0
    else
        return 1
    fi
}

# ==============================================================================
# Self-test suite
# ==============================================================================

if [[ "${1:-}" == "--self-test" ]]; then
    FIXTURES=(
        "crates/contexts/system/src/domain/__layering_fixture.rs"
        "crates/contexts/system/src/domain/__ownership_fixture.rs"
        "crates/contexts/system/src/application/__direction_fixture.rs"
        "crates/contexts/system/src/adapters/__sql_fixture.rs"
    )
    cleanup() {
        for f in "${FIXTURES[@]}"; do
            rm -f "$f"
        done
    }
    trap cleanup EXIT
    cleanup

    # 1. Domain layering: 'use' import of infrastructure crate
    cat > "${FIXTURES[0]}" <<'EOF'
use sqlx::PgPool;
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: 'use sqlx' import was not detected" >&2
        exit 1
    fi
    echo "self-test OK: 'use sqlx' import detected as expected"

    # 2. Domain layering: bare qualified path
    cat > "${FIXTURES[0]}" <<'EOF'
pub fn handler() -> sqlx::Error {
    unimplemented!()
}
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: bare qualified path 'sqlx::Error' was not detected" >&2
        exit 1
    fi
    echo "self-test OK: bare qualified path detected as expected"

    # 3. Domain layering: sanvi_persistence re-export in domain
    cat > "${FIXTURES[0]}" <<'EOF'
use sanvi_persistence::PgPool;
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: 'sanvi_persistence' import in domain was not detected" >&2
        exit 1
    fi
    echo "self-test OK: 'sanvi_persistence' import detected as expected"

    cat > "${FIXTURES[0]}" <<'EOF'
pub fn get_scope() -> sanvi_persistence::ScopedTx<'static> {
    unimplemented!()
}
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: bare qualified 'sanvi_persistence::ScopedTx' was not detected" >&2
        exit 1
    fi
    echo "self-test OK: bare qualified 'sanvi_persistence' path detected as expected"
    rm -f "${FIXTURES[0]}"

    # 4. Context ownership: context importing another context's domain
    cat > "${FIXTURES[1]}" <<'EOF'
use sanvi_billing::domain::Subscription;
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: cross-context domain import was not detected" >&2
        exit 1
    fi
    echo "self-test OK: cross-context domain import detected as expected"

    # 5. Context ownership: context importing another context's repository
    cat > "${FIXTURES[1]}" <<'EOF'
use sanvi_identity::domain::MembershipRepository;
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: cross-context repository import was not detected" >&2
        exit 1
    fi
    echo "self-test OK: cross-context repository import detected as expected"
    rm -f "${FIXTURES[1]}"

    # 6. Hexagonal direction: application importing adapter
    cat > "${FIXTURES[2]}" <<'EOF'
use crate::adapters::LiveCache;
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: application-to-adapter dependency was not detected" >&2
        exit 1
    fi
    echo "self-test OK: application-to-adapter dependency detected as expected"
    rm -f "${FIXTURES[2]}"

    # 7. Cross-schema SQL: SQL referencing foreign context schema
    cat > "${FIXTURES[3]}" <<'EOF'
pub const QUERY: &str = "SELECT id FROM tenancy.tenants WHERE status = 'active'";
EOF
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: cross-schema SQL reference was not detected" >&2
        exit 1
    fi
    echo "self-test OK: cross-schema SQL reference detected as expected"
    rm -f "${FIXTURES[3]}"

    # 8. Exit status wrap: prove that exactly 256 violations causes check() to return failure (code 1)
    # Generate 256 distinct violations in a fixture
    : > "${FIXTURES[0]}"
    for i in $(seq 1 256); do
        echo "use sqlx::PgPool as P$i;" >> "${FIXTURES[0]}"
    done
    if check 2>/dev/null; then
        echo "SELF-TEST FAILED: exit-status modulo 256 wrap bug! Exactly 256 violations reported success." >&2
        exit 1
    fi
    echo "self-test OK: exit-status wrap bug fixed (256 violations correctly fails)"
    rm -f "${FIXTURES[0]}"

    echo "all architecture gate self-tests PASSED"
    exit 0
fi

# Normal execution
if check; then
    echo "layering OK: no architecture violations found"
    exit 0
else
    echo "layering violations found: $VIOLATIONS violation(s) (see above)" >&2
    exit 1
fi
