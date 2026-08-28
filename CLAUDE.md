# sanvi-backend

Rust modular monolith: DDD bounded contexts, hexagonal architecture, actix-web
+ SQLx/Postgres + Redis.

**`CONTRIBUTING.md` is the authority** on the enforced rules (layering, error
mapping, OpenAPI contracts, transactions, SQL) and on the recipe for adding a
use case. Read it before writing code; it is not duplicated here.

## Layout

```
crates/kernel/            shared primitives, no infrastructure
crates/platform/          cache http messaging persistence telemetry testsupport
crates/contexts/<ctx>/    one bounded context; src/{domain,application,adapters}
apps/{api,cli,worker}/    composition roots and binaries
api/*.yaml                generated OpenAPI, one file per context
migrations/               timestamped, context-tagged, reversible
docs/phase-NN-*/          plans and specs per phase
docs/adr/                 architecture decisions
```

## Commands

`just` is not installed on every machine. The `scripts/*.sh` equivalents are
the portable form; `just --list` shows the full recipe set where it is.

| Goal | Command | Notes |
| --- | --- | --- |
| Type-check | `cargo c` | alias: `check --all-targets --message-format short` |
| Lint | `cargo k` | alias: clippy, `-D warnings`, short diagnostics |
| Format | `cargo fmt --check` | |
| Fast test | `just test-fast` | unit + application only, no Docker |
| Test | `scripts/test.sh --quiet` | failures only; drop `--quiet` to see every test |
| One crate | `scripts/test.sh -p <crate>` | scopes to that crate, on the shared server |
| Gates | `just ci-quiet` | fmt, clippy, layering, invariants, test, openapi, i18n |
| Dev stack | `scripts/dev.sh up` | Postgres + Redis in Docker |

Prefer `cargo c` before `scripts/test.sh`: it catches most mistakes without
linking 822 tests' worth of binaries.

`scripts/test.sh` starts one shared Postgres and clones a pre-migrated template
per test. **Do not run `cargo test` or `cargo nextest run` directly on anything
with a `tests/` directory.** Without `SANVI_TEST_PG_PORT` the harness starts a
container per test *process* and `mem::forget`s it, so it is never removed
(`crates/platform/testsupport/src/postgres.rs:109`). `nextest` forks per test,
so a workspace run strands ~100 Postgres containers, ~119 MB each; they OOM the
Docker VM, and the kills surface as bogus `Pool<Postgres> expect failed`
panics in unrelated crates.

If you need a bare `cargo` invocation, put the `eval` in the **same command** —
each tool call gets a fresh shell, so exporting it in a previous call does
nothing:

```bash
eval "$(./scripts/test-db.sh up)" && cargo nextest run -p <crate>
```

`scripts/test.sh` and `just test` already do this internally. `just test-fast`
touches no database and needs none of it.

## Cost notes

- A cold `cargo test --workspace` is a multi-minute link-heavy build. Scope with
  `scripts/test.sh -p <crate>` while iterating and run the full suite once at
  the end.
- `target/` reaches tens of GB. `just clean-incremental` drops the unbounded
  `target/debug/incremental` cache without forcing a full dependency rebuild.
- `just clean-stale` sweeps the whole `target/` tree of artifacts untouched
  for 14 days, release artifacts included.
- Query changes need `cargo sqlx prepare --workspace`, else offline builds fail.
