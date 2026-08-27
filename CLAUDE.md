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
| One crate | `cargo test -p <crate>` | no Docker needed unless it has `tests/` |
| Gates | `just ci-quiet` | fmt, clippy, layering, invariants, test, openapi, i18n |
| Dev stack | `scripts/dev.sh up` | Postgres + Redis in Docker |

Prefer `cargo c` before `scripts/test.sh`: it catches most mistakes without
linking 822 tests' worth of binaries.

`scripts/test.sh` starts one shared Postgres and clones a pre-migrated template
per test. Running `cargo test` directly instead makes each test process start
its own container.

## Cost notes

- A cold `cargo test --workspace` is a multi-minute link-heavy build. Scope to
  `-p <crate>` while iterating and run the full suite once at the end.
- `target/` reaches tens of GB. `just clean-incremental` drops the unbounded
  `target/debug/incremental` cache without forcing a full dependency rebuild.
- `just clean-stale` sweeps the whole `target/` tree of artifacts untouched
  for 14 days, release artifacts included.
- Query changes need `cargo sqlx prepare --workspace`, else offline builds fail.
