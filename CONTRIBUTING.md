# Contributing to sanvi-backend

## The rules (enforced, not advisory)

1. **Layering.** `domain` depends on nothing but `sanvi-kernel` and
   std/serde. No `sqlx`, `actix`, `reqwest` inside any `src/domain/` module —
   CI runs `scripts/check-layering.sh` (with a fixture proving it catches
   violations).
2. **Errors.** Domain errors are `thiserror` enums in `domain/error.rs`.
   Adapters map infrastructure errors at the boundary; handlers convert
   domain errors into `sanvi_http::ApiError` (RFC 9457 problem+json, one
   place for status mapping).
3. **Contracts.** Handler changes require the OpenAPI annotations to be
   updated; CI diffs against `api/*.yaml`. Regenerate with
   `cargo run -p sanvi-cli -- openapi --write`.
4. **Transactions.** Every write path runs in a `Tx` from
   `sanvi_persistence::TxManager`; anything that emits domain events writes
   to `platform.outbox` in the same transaction. Use PostgreSQL-backed jobs for
   durable background work and the shared scheduler for recurring maintenance.
   Never rely on a detached task or Redis as the only durable record of work.
   Consumers must be idempotent; database-only effects and inbox acknowledgement
   should share a transaction. Keep broker SDKs behind platform delivery adapters
   so NATS or Kafka can be added without changing context use cases.
5. **Secrets.** Never in config files or `.env.example`. Local-only dev
   values go in your gitignored `.env`.
6. **SQL.** Migrations are timestamped, context-tagged, reversible, and only
   touch their own schema. Query macros (`query_as!`) + committed `.sqlx`
   cache; run `cargo sqlx prepare --workspace` when queries change.
7. **IDs.** First-party entity IDs use `sanvi_kernel::Id<Tag>` Snowflakes:
   raw SQL rows use `i64`, domain mapping uses `TryFrom`, and transport uses
   decimal strings. Do not add UUID entity IDs or `Id::from_uuid`/`as_uuid`.
   UUID is limited to named external/correlation fields listed in
   `scripts/id-policy-uuid-exceptions.txt`; run `just id-policy` before
   submitting ID-related work.

## Adding a use case (the recipe)

1. **Domain**: add value objects/entities + invariants in
   `crates/contexts/<ctx>/src/domain/`, and the port trait(s) the use case
   needs (e.g. `async fn by_id(...)` on a repository port).
2. **Application**: one struct per use case in
   `crates/contexts/<ctx>/src/application/`, with
   `execute(cmd) -> Result<Output, <Ctx>Error>`. Owns transactions,
   authorization checks, event publication, idempotency.
3. **Adapters**:
   - driven: implement the port (sqlx repo under `src/adapters/`).
   - driving: actix handler under `src/adapters/http/`, annotate with
     `#[utoipa::path(...)]`, register in `routes`, export in the context's
     `ApiDoc`.
4. **Composition**: wire the adapter into the use case in
   `apps/api/src/lib.rs` and mount routes.
5. **Tests**: unit tests for invariants, application tests with fake ports
   (in-memory), integration against a real Postgres via
   `sanvi_testsupport::test_db()` when the adapter hits real infrastructure.
6. **Contract**: `cargo run -p sanvi-cli -- openapi --write`, commit
   `api/<context>.yaml`.
7. **Docs**: migrations (if any), threat model note if security-relevant.

## Testing

| Level | Where | Command |
|---|---|---|
| Unit | `#[cfg(test)]` in crate | `cargo test -p <crate>` |
| Application | fakes in test modules | `cargo test -p <crate>` |
| Integration | `tests/` against a real Postgres (Docker required) | `just test` |
| Contract | `apps/api/tests/http_contract.rs` | `cargo test -p sanvi-api --test integration` |

Each crate's integration tests now compile into a single `integration` target, so `--test integration` is how you scope to them.

`sanvi_testsupport::test_db()` hands back a private, fully migrated database.
It is cloned from a template the harness migrates once, and every test in the
run shares one server, so a suite costs a `CREATE DATABASE` rather than a
container start plus 54 migrations.

`just test` starts that server (`scripts/test-db.sh up`) and exports
`SANVI_TEST_PG_PORT` / `SANVI_TEST_REDIS_PORT`. Running `cargo test` or
`cargo nextest run` without them still works — each test *process* falls back
to starting its own container, and leaves it running (`./scripts/test-db.sh
prune` reaps strays). Under `nextest`, which runs a process per test, that
fallback is expensive: export the variables first.

Redis has no per-test equivalent of `CREATE DATABASE` — a stock server has
sixteen logical databases and the workspace has hundreds of tests — so the
one shared server is divided by key name instead. **Every Redis key a test
touches must come from `sanvi_testsupport::test_stream()`**, which is unique
per test and per run. A hard-coded key name is shared with every other test
using it and with every previous run, which shows up as a count assertion
finding two of something it created one of.

### macOS prerequisite: enable developer mode before a cold run

On macOS, the first execution of each freshly linked binary is held up while
Gatekeeper scans it, and the scans serialise: for this workspace's ~50 test
binaries that is several minutes before the first test runs — measured at
~15 of a cold run's 16 minutes (`docs/phase-11-hardening-ga/
test-performance-baseline.md`). Later runs against the same binaries start
instantly.

Exempt the host before a cold run:

```
sudo spctl developer-mode enable-terminal
```

then System Settings → Privacy & Security → **Developer Tools**, enabling the
terminal, IDE, or agent host you run the suite from. The exemption is per
parent process, so every new host needs it again.

`scripts/test.sh` detects the missing exemption and prints the fix as a
warning before the run, so a slow cold run is diagnosable at the host that
runs it rather than documented only here.

## Commits & PRs

Conventional Commits (`feat`, `fix`, `chore`, `ci`, `docs`, `security` …)
with a scope from the commitlint config; enforced by pre-commit and a PR
template checklist. Branches: short-lived feature branches into `main`,
signed commits, linear history.

## Pre-commit

Install once: `pre-commit install`. Hooks: fmt, clippy (staged crates),
gitleaks, commitizen, trailing whitespace.

## Definition of done

Tests green, clippy/fmt/layering clean, OpenAPI committed, migrations
reversible, docs updated, feature flag defined where relevant, rollback plan
in the PR description.
