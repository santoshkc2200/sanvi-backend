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

- `target/debug/deps` held **327,615 files** and **5,430 executables** at the
  2026-08-27 measurement. `just clean-stale` was run on 2026-09-15 (branch
  `feat/task-031-ci-coverage-repair`, `cargo sweep --time 14`): it removed
  nothing, because every artifact in the tree is under 14 days old, and
  `target/debug/deps` now holds **65,532 files** and **122 executables** —
  the 327,615 figure no longer describes this tree. The recipe stays;
  the next stale window will have something to collect.
- The PR workflow's two test jobs do cover the workspace — but that is an
  accident of duplication, not a mechanism. The unit job runs
  `--workspace` minus `sanvi-persistence`, `sanvi-messaging` and
  `sanvi-tenancy`, so it runs every member's tests *including* the
  integration tests of everything not in that exclude list: `cargo nextest
  list` with the unit job's exact arguments selects `sanvi-access::
  integration`, `sanvi-advertising::integration`,
  `sanvi-audit::audit_integration`, `sanvi-identity::integration`,
  `sanvi-platform-admin::impersonation` and `sanvi-theming::
  theming_integration`. An earlier revision of this section claimed those six
  were tested by no CI job; that was wrong, and so was the revision before
  it, which claimed the same of the three Docker-dependent crates. What was
  genuinely missing was anything that would have *detected* either mistake:
  the integration job's eight-crate `-p` list was maintained by hand,
  overlapped the unit job in five crates, and named only three of the fifteen
  members whose tests need Docker. Fixed in
  [TASK-031](../tasks/phase-11/TASK-031-11-a-ci-coverage-repair-and-gate-truth.md):
  the integration job derives its crate list from the workspace
  (`scripts/check-ci-test-coverage.sh --integration-packages`), and the
  `ci-test-coverage` gate fails any PR where a member that ships `tests/` is
  not run by a job — with a self-test proving it fails when one is dropped.
