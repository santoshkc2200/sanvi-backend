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
# 1. Start the core stack (Postgres + Redis)
./scripts/dev.sh up

# 2. Apply migrations (runs under the migrator role; the API role is
#    NOBYPASSRLS and cannot change the schema)
./scripts/migrate.sh

# 3. Run the API
cargo run -p sanvi-api

# In another shell:
curl http://localhost:8080/healthz
curl http://localhost:8080/readyz
curl http://localhost:8080/api/v1/system/version
curl http://localhost:8080/openapi.json
curl http://localhost:8080/metrics

# Scalar API docs: http://localhost:8080/scalar (dev dependency, see below)
```

The full stack (Ory Kratos + Keto, MailHog, MinIO, Jaeger) starts with
`./scripts/dev.sh up --full`. Ory containers run from day one but nothing
consumes them until phase 02.

## Repository layout

```
crates/
  kernel/                  # shared kernel: ids, time, pagination, event envelope
  platform/
    http/                  # actix server, middleware, problem+json, OpenAPI
    persistence/           # pool, TxManager, migrations, health probe
    messaging/             # outbox dispatcher, Redis Streams client
    telemetry/             # tracing, OTel, Prometheus, redaction
  contexts/
    system/                # reference context (health, version, build info)
apps/
  api/                     # composition root (binary + lib for tests/CLI)
  worker/                  # outbox dispatcher loop
  cli/                     # migrate, seed, config check, openapi
api/                       # committed per-context OpenAPI specs (contract)
migrations/                # timestamped SQL migrations (reversible)
configs/                   # base.toml + per-env overlays (no secrets)
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
