# Test suite performance: measured baseline

Measured 2026-08-27 on a 10-core / 16 GB macOS machine (Darwin 25.5.0),
branch `perf/test-suite`.

## Headline

The suite itself is fast. Everything expensive around it is not.

| Phase of a full `scripts/test.sh --quiet` | Cold (freshly linked) | Warm (same binaries) |
| --- | --- | --- |
| Build + link | 1m 21s | 0.44s |
| macOS Gatekeeper first-exec scan | **~14m 54s** | 0s |
| Test execution (864 tests, 40 binaries) | 22.3s | 23.5s |
| **Total** | **16m 37s** | **25.2s** |

Both runs: `864 tests run: 864 passed, 0 skipped`.

The Gatekeeper row is not measured directly — it is the remainder after build
and execution, and it is corroborated by direct observation: during a cold run
`syspolicyd` sits at 35-48% CPU and `XprotectService` at ~13% while
`cargo-nextest` itself sits at 0.0% CPU, parked in `dyld`. macOS scans each
freshly linked binary on its first execution and the scans serialise.

## What this means

Roughly **97% of a cold run is not testing**. Optimising test code, database
setup, or parallelism divides into a 22-second window. The one change that
matters on macOS is exempting the process that spawns the suite:

```
sudo spctl developer-mode enable-terminal
```

then System Settings → Privacy & Security → **Developer Tools**, enabling every
terminal, IDE, or agent host that runs the suite. The exemption is per parent
process, so each host needs it separately — which is why it silently regresses.

This was already documented in `CONTRIBUTING.md`, but as a subsection rather
than a prerequisite, and nothing detected or reported it. A cold run simply
looked slow.

## No true pre-change baseline exists

The "before" column could not be captured. `cargo nextest list --workspace` on
the unmodified tree was killed after **45 minutes** without ever reaching a
test — it was still in Gatekeeper. That failure is itself the finding, but it
means the numbers above are an *after* measurement, not a delta.

The invariant that was checked across the change: test attributes counted
statically before and after the `tests/integration/` restructure — **868 across
203 files, unchanged**.

## Changes behind the "after" numbers

- 20 integration test files merged into 8 per-crate `tests/integration/`
  targets. Test binaries ~50 → 40, so 10 fewer links and 10 fewer Gatekeeper
  scans per cold run.
- Test Postgres 11 → 18, matching dev and production (`docker-compose.yml`).
  Unlocks `CREATE DATABASE ... STRATEGY = wal_log`, which drops the two forced
  checkpoints PostgreSQL performs per file-copy clone — 164 clones per run —
  and `DROP DATABASE ... WITH (FORCE)`, which removes a `pg_terminate_backend`
  round trip per test.
- `admin_pool()` → `admin_conn()`: a bare `PgConnection` where a full `PgPool`
  was built to issue two statements and hang up, twice per test.
- nextest `database` test group caps concurrent template cloning at 4.
- `just test-fast` runs `kind(lib) or kind(bin)` — unit and application tests
  only, no Docker.

## Still open

- `target/debug/deps` holds **327,615 files** and **5,430 executables**,
  including many stale copies of the same binary (7 `sanvi_worker`, 4
  `sanvi_api`, 4 `http_contract`) at 28-104 MB each. Cargo stats this tree on
  every invocation. `just clean-stale` exists and is not being run.
- `.github/workflows/pr.yml` runs two test jobs, and between them six crates
  are tested by neither. The unit job excludes `sanvi-persistence`,
  `sanvi-messaging` and `sanvi-tenancy` because they need Docker — those three
  *are* covered, by the integration job at `pr.yml:179`. What that job does not
  cover is everything absent from its hand-written crate list:
  `sanvi-access`, `sanvi-advertising`, `sanvi-audit`, `sanvi-identity`,
  `sanvi-platform-admin` and `sanvi-theming` all ship a `tests/` directory that
  no CI job runs. A job that runs no tests still passes, so nothing reported it.
  Fixed in
  [TASK-031](../tasks/phase-11/TASK-031-11-a-ci-coverage-repair-and-gate-truth.md),
  which derives the list from the workspace rather than maintaining it by hand.

  *(An earlier revision of this document claimed the three Docker-dependent
  crates were untested. That was wrong; the integration job covers them.)*
