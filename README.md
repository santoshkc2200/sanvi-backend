# sanvi-backend

Rust modular monolith powering Sanvi, a multi-tenant, highly configurable SaaS
platform. actix-web 4, SQLx (compile-time-checked SQL) over PostgreSQL,
Redis, Ory Kratos/Keto for identity and access, Stripe for billing.

- Architecture invariants: [`docs/architecture-overview.md`](docs/architecture-overview.md)
- Phase plans: [`docs/`](docs/) (one `phase-NN-*` folder per delivery phase)
- ADRs: [`docs/adr/`](docs/adr/)

## Quickstart

Prerequisites: Rust 1.93+ (`rust-toolchain.toml` pins the exact toolchain),
Docker with Compose, `just` (optional).

```bash
# 1. Start the stack (Postgres + Redis; --full adds Ory Kratos/Keto,
#    Mailpit, MinIO, OpenObserve)
./scripts/dev.sh up --full

# 2. Apply migrations (runs under the migrator role; the API role is
#    NOBYPASSRLS and cannot change the schema)
./scripts/migrate.sh

# 3. Seed the system role catalog + Keto relation tuples + feature catalog
cargo run -p sanvi-cli -- seed roles
cargo run -p sanvi-cli -- seed features

# 4. Run the API (and the worker, which keeps Keto in sync with events)
cargo run -p sanvi-api
cargo run -p sanvi-worker

# In another shell:
curl http://localhost:8080/healthz
curl http://localhost:8080/readyz
curl http://localhost:8080/api/v1/system/version
curl http://localhost:8080/openapi.json
curl http://localhost:8080/metrics

# Scalar API docs: http://localhost:8080/scalar (dev dependency, see below)
```

### Creating a tenant (phase 01)

Until self-service lands, operators provision tenants with the CLI:

```bash
cargo run -p sanvi-cli -- tenant create --slug acme --display-name "Acme Corp" --region us --activate
cargo run -p sanvi-cli -- tenant list
```

Once provisioned, `acme` serves traffic on its subdomain
(`acme.localhost` locally; `acme.platform.sanvi.app` in production):

```bash
curl http://localhost:8080/api/v1/public/tenant-context -H 'Host: acme.localhost'
curl http://localhost:8080/api/v1/tenant/context -H 'Host: acme.localhost'
```

Tenant isolation is enforced twice over: repositories can only be
constructed from tenant-scoped transactions (`TxManager::begin_for`), and
Postgres RLS (`FORCE ROW LEVEL SECURITY` on every `tenant_id`-carrying table)
filters rows by `SET LOCAL sanvi.tenant_id` — covered by the RLS matrix test.

### Identity & access (phase 02)

Sign in happens in Ory Kratos (Google, Microsoft, or email OTP); the API
verifies Kratos sessions (whoami + a ≤ 60 s Redis cache) and authorizes
every non-public route through a permission declared in the central
registry (Keto relation tuples + a ≤ 30 s decision cache). The
"no route without a permission" catalog test fails CI on any undeclared
route.

```bash
# 1. A user registers/signs in through Kratos (http://localhost:4433);
#    the webhooks create the local projection in identity.users.

# 2. Grant the first tenant owner (after they registered):
cargo run -p sanvi-cli -- membership grant --email owner@example.com --role tenant_owner --tenant acme

# 3. Platform operators (control plane) get a platform role — Keto-only
#    until phase 03 gives platform memberships a database home:
cargo run -p sanvi-cli -- membership grant --email operator@example.com --role platform_owner --platform

# 4. Rebuild every Keto tuple from the catalog + memberships (safe to
#    re-run; wipes Keto-only platform assignments — re-run step 3 after):
cargo run -p sanvi-cli -- seed access

# 5. Invite members through the API (requires a session token):
curl -X POST http://localhost:8080/api/v1/tenant/invitations \
  -H "Authorization: Bearer <session-token>" -H 'Host: acme.localhost' \
  -H 'Content-Type: application/json' \
  -d '{"email": "bob@example.com", "role_ids": ["<tenant_member role id>"]}'
```

Webhook receivers live under `/internal/kratos/*` and are protected by a
shared secret (`SANVI__IDENTITY__WEBHOOK_SECRET`, must match the Kratos
config) plus a client-IP allow-list. Adding an OIDC provider is
configuration-only: [`docs/runbooks/add-oidc-provider.md`](docs/runbooks/add-oidc-provider.md).

The full stack (Ory Kratos + Keto, Mailpit, MinIO, OpenObserve) starts with
`./scripts/dev.sh up --full`. Ory containers run from day one but nothing
consumes them until phase 02.

### Traces and metrics

OpenObserve is the OTLP backend in every environment: traces and metrics are
pushed to it over OTLP/gRPC, while `/metrics` keeps serving Prometheus text for
local scraping. Export is off by default locally — uncomment the
`SANVI__TELEMETRY__OTLP__*` block in `.env.example` (its `AUTH_TOKEN` is the
base64 of the compose root credentials) and restart the API. The UI is at
<http://localhost:5080>, `root@sanvi.local` / `OpenObserve-local-dev1`; spans
land in the `default` organization, stream `default`.

Ingestion is authenticated, so the API refuses to start when
`telemetry.otlp.enabled` is true without an `auth_token` — a 401 discovered
hours later in an export batch is worse than a failure at boot.

## Repository layout

```
crates/
  kernel/                  # shared kernel: ids, time, pagination, event envelope
  platform/
    http/                  # actix server, middleware, tenant resolution, authn/authz, problem+json, OpenAPI
    persistence/           # pool, TxManager (scoped transactions), migrations, idempotency
    messaging/             # outbox writer/dispatcher, stream consumers, scheduler, Redis
    cache/                 # tenant-namespaced caching (prefix enforced by construction)
    telemetry/             # tracing, OTel, Prometheus, redaction
  contexts/
    system/                # reference context (health, version, build info)
    tenancy/               # tenants, lifecycle, settings, resolution (phase 01)
    identity/              # users, memberships, invitations, account linking (phase 02)
    access/                # permission registry, roles, authorization, features/entitlements (phase 02/03)
    audit/                 # append-only hash-chained audit trail (phase 03)
    platform_admin/        # operator control plane, impersonation, four-eyes (phase 03)
apps/
  api/                     # composition root (binary + lib for tests/CLI)
  worker/                  # outbox dispatcher + consumers (tenancy/identity/access) + scheduler (phase 01–03 jobs)
  cli/                     # migrate, tenant, seed roles/access/features, feature/entitlement/impersonation/audit/approval, config, openapi
api/                       # committed per-context OpenAPI specs (contract)
migrations/                # timestamped SQL migrations (reversible)
configs/                   # base.toml + per-env overlays + Ory configs (no secrets)
scripts/                   # dev, migrate, seed, lint, smoke helpers
```

## Architecture in one paragraph

A modular monolith of bounded contexts, each with hexagonal layering:
`domain` (pure, ports as traits) ← `application` (use cases) ← `adapters`
(driving: HTTP handlers; driven: sqlx/redis/stripe/ory implementations).
Contexts talk synchronously through self-defined ports or asynchronously via
the transactional outbox → Redis Streams. Postgres uses one schema per
context and row-level security for tenant isolation. The rules are enforced
by `scripts/check-layering.sh` in CI, not by convention.

## Quality gates

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
./scripts/check-layering.sh            # + --self-test
cargo test --workspace                 # unit/application; integration needs Docker
cargo sqlx prepare --workspace --check # committed .sqlx must match the schema
./scripts/check-openapi.sh             # api/*.yaml must match code annotations
```

Everything above runs in CI on every PR; see `.github/workflows/pr.yml`.
`main` builds and signs a container image (cosign, keyless), runs migrations
as a pre-deploy job, deploys to staging, and smoke-tests it.

## Configuration

`configs/base.toml` holds defaults; `configs/{local,staging,production}.toml`
overlay per environment (selected by `SANVI_ENV`); environment variables with
the `SANVI__` prefix and `__` nesting override everything
(`SANVI__DATABASE__PASSWORD`). Secrets exist only in the environment — never
in committed files. `cargo run -p sanvi-cli -- config` prints the resolved
configuration with secrets redacted.

## Security notes

- Least-privilege Postgres roles from day one (`sanvi_app` is `NOBYPASSRLS`).
- Every error response is RFC 9457 `application/problem+json` with a
  `trace_id`; no internals leak (see `crates/platform/http/src/error.rs`).
- `Redacted<T>` is the only way sensitive values reach logs/traces.
- Threat model: [`docs/security/threat-model.md`](docs/security/threat-model.md).
- Runbooks: [`docs/runbooks/`](docs/runbooks/).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the layering rules, the
"how to add a use case" recipe, and commit conventions. Conventional Commits
enforced by pre-commit; CODEOWNERS cover the architecture invariants.
