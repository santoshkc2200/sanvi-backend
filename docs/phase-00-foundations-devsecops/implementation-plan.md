# Phase 00 — Foundations & DevSecOps (backend)

**Target version:** 0.1.0
**Depends on:** nothing
**Unlocks:** every other phase

## Goal

A repository where a new engineer can run `./scripts/dev.sh up` and get a working API, where the
architecture is enforced by the compiler and CI rather than by convention, and where security is a
build gate rather than a review comment. No business features ship in this phase — the deliverable
is the machine that builds every later phase.

## Scope

**In**
- Cargo workspace with the crate layout from [`../architecture-overview.md`](../architecture-overview.md).
- One vertical slice (`health` + a trivial `system` context) proving the layering end to end.
- docker-compose local stack: Postgres, Redis, Ory Kratos, Ory Keto, Mailpit, MinIO, Jaeger.
- Configuration, error model, telemetry, OpenAPI generation, migration runner.
- CI/CD pipeline with security gates; container image build; staging deploy.

**Out**
- Tenancy (phase 01), auth enforcement (phase 02) — Ory containers run but nothing consumes them yet.
- Any product domain logic.

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Web framework | actix-web 4 | Required by brief; mature, fastest tier, good middleware model |
| DB access | SQLx (postgres, `macros`, `migrate`, offline) | Compile-time checked SQL without ORM lock-in |
| Config | `figment` (TOML + env, `SANVI__` prefix, `__` nesting) | Layered, typed, testable |
| Errors | `thiserror` in libs, `anyhow` only in binaries; RFC 9457 at edge | Typed domain errors, useful HTTP errors |
| Telemetry | `tracing` + `tracing-opentelemetry` + OTLP | One story for logs/traces/metrics |
| OpenAPI | `utoipa` annotations → generated doc, CI-diffed against `api/*.yaml` | Contract-first without hand-drift |
| Auth (later) | Ory Kratos (AuthN) + Keto (AuthZ) | Required by brief; offloads credential handling |
| Task runner | `just` (`justfile`) + thin `scripts/*.sh` wrappers | Discoverable, CI/local parity |
| MSRV | Pinned via `rust-toolchain.toml` | Reproducible builds |

## Deliverables

### 1. Workspace skeleton

```
Cargo.toml (workspace, resolver = "2", shared [workspace.dependencies])
rust-toolchain.toml            # channel, components: rustfmt clippy
crates/kernel/                 # Id newtypes, Clock, Uuid v7 gen, PageRequest, DomainError, EventEnvelope
crates/platform/http/          # server builder, middleware, problem+json, OpenAPI mount
crates/platform/persistence/   # PgPool builder, TxManager, migration runner, health probe
crates/platform/messaging/     # outbox types + dispatcher skeleton, Redis Streams client
crates/platform/telemetry/     # tracing init, OTel pipeline, metrics registry, redaction layer
crates/contexts/system/        # the reference slice: version, health, build info
apps/api/  apps/worker/  apps/cli/
```

`apps/api/src/main.rs` is the composition root: read config → init telemetry → build pools →
construct adapters → inject into use cases → mount routes → serve with graceful shutdown
(SIGTERM → stop accepting, drain in-flight with a deadline, close pools).

### 2. Configuration (`configs/`)

```
configs/base.toml          # defaults, no secrets
configs/local.toml         # docker-compose hostnames
configs/staging.toml  configs/production.toml
```

Typed into `AppConfig { server, database, redis, telemetry, ory, features }`. Secrets exclusively
from env (`SANVI__DATABASE__PASSWORD`) sourced from the platform secret manager. Startup fails fast
with a readable report listing *all* invalid/missing keys, not just the first.

### 3. Local stack (`docker-compose.yml`)

| Service | Purpose |
|---|---|
| `postgres:18` | Primary DB; init script creates `sanvi_app` (NOBYPASSRLS) and `sanvi_migrator` roles |
| `redis:8` | Cache, streams, locks |
| `kratos` + `kratos-migrate` | Identity (configured in phase 02, running now) |
| `keto` + `keto-migrate` | Permissions |
| `mailpit` | Local mail capture |
| `minio` | S3-compatible object storage |
| `jaeger` (all-in-one, OTLP) | Trace viewing |

Plus `Dockerfile` for the API: multi-stage, `cargo-chef` dependency caching, distroless/`debian:slim`
runtime, non-root user, no shell in the final image, `HEALTHCHECK`, OCI labels with git SHA.

### 4. Cross-cutting HTTP concerns

- `RequestId` (accept inbound `x-request-id`, else UUIDv7).
- Tracing span per request with `http.*` and `sanvi.*` attributes.
- Security headers: HSTS, `X-Content-Type-Options`, `Referrer-Policy`, `X-Frame-Options: DENY`,
  `Permissions-Policy`, and a CSP for API error pages.
- CORS: explicit origin allow-list from config, credentials enabled, no wildcards.
- Body size limits, request timeout, concurrency limit, and per-IP rate limit (Redis token bucket).
- Panic handler → 500 problem+json without leaking internals.

### 5. CI/CD (GitHub Actions or equivalent)

Pipeline `pr.yml`:
1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. Architecture lint — `cargo-deny` + a custom `scripts/check-layering.sh` asserting no
   `sqlx|actix|reqwest` import inside any `src/domain/**`
4. `cargo test --workspace` (SQLx offline; testcontainers for integration job)
5. `cargo sqlx prepare --check`
6. OpenAPI drift check
7. Security gates: `cargo audit`, `cargo deny check licenses bans advisories`,
   secret scan (`gitleaks`), SBOM (`cargo cyclonedx`), container scan (`trivy`), IaC scan
8. Coverage (`cargo llvm-cov`) with a floor that ratchets upward, never downward

Pipeline `main.yml`: build & sign image (cosign, keyless), push to registry, run migrations as a
pre-deploy job, deploy to staging, smoke test, publish SBOM + provenance attestation.

Branch protection: signed commits, linear history, required checks, CODEOWNERS review.

### 6. Developer experience

- `scripts/dev.sh` (up/down/reset/logs), `scripts/migrate.sh`, `scripts/seed.sh`,
  `scripts/new-context.sh <name>` scaffolding a context crate with the four-layer skeleton and a
  registered route module.
- Pre-commit hooks: fmt, clippy on staged crates, gitleaks, conventional-commit message lint.
- `CONTRIBUTING.md` with the layering rules and the "how to add a use case" recipe.
- ADR folder `docs/adr/` with `0001-modular-monolith.md`, `0002-rls-tenancy.md`,
  `0003-openapi-contract-first.md` seeded now.

## Work breakdown

1. Repo hygiene: `.gitignore`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `deny.toml`,
   `.editorconfig`, licence, CODEOWNERS, PR template, Conventional Commits config.
2. Cargo workspace + `kernel` crate (ids, clock, errors, pagination, event envelope).
3. `platform/telemetry` — tracing/OTel init, JSON logs, redaction layer, metrics registry.
4. `platform/persistence` — pool builder with sane limits, `TxManager`, migration runner, probes.
5. `platform/http` — server builder, middleware stack, problem+json mapper, OpenAPI mount.
6. `contexts/system` reference slice with domain/application/adapters and its full test set.
7. docker-compose + role bootstrap SQL + Kratos/Keto config placeholders.
8. `apps/api`, `apps/worker` (empty loop + graceful shutdown), `apps/cli` (`migrate`, `seed`, `config check`).
9. CI pipelines and security gates; make the build reproducible and cached.
10. Dockerfile, image signing, staging deploy, smoke test.
11. Docs: README quickstart, ADRs, CONTRIBUTING.

## Data model

Only infrastructure tables this phase:

```sql
-- migrations/00000000000001_platform_bootstrap.sql
CREATE SCHEMA IF NOT EXISTS platform;
CREATE TABLE platform.outbox (
  id            uuid PRIMARY KEY,
  aggregate     text        NOT NULL,
  aggregate_id  uuid        NOT NULL,
  event_type    text        NOT NULL,
  payload       jsonb       NOT NULL,
  metadata      jsonb       NOT NULL DEFAULT '{}',
  occurred_at   timestamptz NOT NULL DEFAULT now(),
  published_at  timestamptz,
  attempts      int         NOT NULL DEFAULT 0
);
CREATE INDEX ON platform.outbox (published_at NULLS FIRST, occurred_at);

CREATE TABLE platform.idempotency_keys (
  key          text PRIMARY KEY,
  scope        text        NOT NULL,
  response     jsonb,
  status       text        NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  expires_at   timestamptz NOT NULL
);
```

## Testing

- Unit tests in `kernel` (id ordering, clock, error mapping).
- Integration: migration up/down against a fresh container; pool health; Redis round trip.
- HTTP: `/healthz`, `/readyz` (degrades when Postgres is down), `/metrics`, error mapping shape.
- CI-level: layering lint has a fixture proving it fails on a domain file importing `sqlx`.

## Security & compliance

- Threat model v0 (STRIDE) recorded in `docs/security/threat-model.md`; refreshed each phase.
- Dependency policy in `deny.toml`: allowed licences, banned crates, advisory failure.
- Least-privilege DB roles from day one; app role cannot create tables or bypass RLS.
- Secret handling documented; `.env.example` contains no real values; gitleaks in pre-commit and CI.

## Observability

Baseline dashboards: request rate/latency/errors, pool saturation, process metrics. Alert rules for
readiness failures and error-rate burn. `docs/runbooks/` seeded with `api-down.md`.

## Acceptance criteria

- [x] `git clone && ./scripts/dev.sh up && curl :8080/healthz` works on a clean machine in < 10 min.
- [x] `cargo test --workspace` green; coverage reported; layering lint proven to fail on violation.
- [ ] CI runs all gates in < 12 min on cache hit; image signed and SBOM published.
- [ ] Staging deploy is automated and reversible; rollback documented and rehearsed once.
- [x] `/metrics` scraped, traces visible in Jaeger for a sample request.
- [x] ADRs 0001–0003 merged; README quickstart validated by someone who did not write it.

## Risks

| Risk | Mitigation |
|---|---|
| Over-engineering the skeleton before real domains exist | Only the `system` slice is built; `scripts/new-context.sh` makes later contexts cheap |
| SQLx offline data drift causing red CI | `cargo sqlx prepare --check` in CI + pre-commit hook |
| Slow Rust CI eroding feedback loops | `cargo-chef`, sccache, split jobs, `cargo nextest` |
| docker-compose sprawl slowing laptops | Profiles: `core` (pg+redis) default, `full` adds Ory/MinIO/Jaeger |
