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
   to `platform.outbox` in the same transaction.
5. **Secrets.** Never in config files or `.env.example`. Local-only dev
   values go in your gitignored `.env`.
6. **SQL.** Migrations are timestamped, context-tagged, reversible, and only
   touch their own schema. Query macros (`query_as!`) + committed `.sqlx`
   cache; run `cargo sqlx prepare --workspace` when queries change.

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
   (in-memory), integration via testcontainers when the adapter hits real
   infrastructure.
6. **Contract**: `cargo run -p sanvi-cli -- openapi --write`, commit
   `api/<context>.yaml`.
7. **Docs**: migrations (if any), threat model note if security-relevant.

## Testing

| Level | Where | Command |
|---|---|---|
| Unit | `#[cfg(test)]` in crate | `cargo test -p <crate>` |
| Application | fakes in test modules | `cargo test -p <crate>` |
| Integration | `tests/` with testcontainers (Docker required) | `cargo test -p sanvi-persistence -p sanvi-messaging` |
| Contract | `apps/api/tests/http_contract.rs` | `cargo test -p sanvi-api` |

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
