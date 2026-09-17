# TASK-019: 11.0 Production-shaped data & the measurement harness

**Phase:** 11 · **Status:** done · **Size:** M
**Requirement(s):** FR-1101, FR-1102, FR-1103, FR-1104, NFR-1101
**Depends on:** TASK-031
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** none — this task ships tooling, not behaviour

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Produce the numbers the rest of the phase is judged against: a deterministic production-shaped data
generator, a k6 harness that writes comparable artifacts, a comparison tool that fails on a
regression, and a build probe so any measurement can name the build that produced it.

Nothing user-visible ships and nothing gets faster. That is the point: without a stored baseline every
later slice can only *claim* improvement, and two optimisations that cancel out look exactly like two
that worked.

## Why this shape

The original version of this task also carried a feature freeze, a `release/1.0` branch policy, four
weeks of field telemetry from staging and beta, and two booked external engagements. There is no beta,
no traffic, and no vendor budget, so three of those four were unsatisfiable and the freeze was
premature — phase 10 is still being written. What survives is the part that is real: reproducible data
and reproducible measurement.

The consequence, stated once here and inherited by every later task: **these are lab numbers.** They
are excellent at catching a regression between two release candidates on the same machine and they say
nothing about production. Every artifact records the machine profile, and every claim derived from one
says "lab".

## Constraints

- `seed-scale` must refuse to run against a database whose name does not match the staging pattern,
  and the guard must not be bypassable by environment variable alone.
- Determinism is the whole value: the same seed produces identical row counts and checksums.
- No tuning of any kind in this task. Recording a number is the deliverable; changing it is TASK-021.

## File ownership map

- `apps/cli/src/commands/seed_scale.rs` and its distribution fixtures — the generator
- `tests/load/**` — k6 scenarios, one per load shape
- `scripts/perf-compare.sh`, `benchmarks/**` — the comparison tool and its committed artifacts
- `api/system.yaml` — the build probe, the only contract change in this task
- `docs/release/ga-checklist.md` — assembled here, closed in TASK-030
- `justfile` — `seed-scale`, `perf`, `perf-compare`

## Steps

### Step 1: Add the build probe, contract first

**Files:** Modify `api/system.yaml`, `crates/platform/http/**`, `apps/api/**`; Test:
`crates/contexts/system/tests/**`

**Do:** `GET /api/v1/system/build` returning commit sha, semver, built-at, and environment.
Unauthenticated, cacheable, carrying no tenant data. The frontend error tracker, RUM, and TASK-025's
probes all read it. No other `/api/v1` change in this task.

**Verify:** `just openapi` → no drift. A test asserts the response contains the sha the binary was
built from and contains no tenant-scoped field.

### Step 2: Write the distribution fixture before the generator

**Files:** Create `apps/cli/src/commands/seed_scale/distribution.rs` (or fixture JSON); Test: inline

**Do:** describe the shape production will have, not a uniform one — a long tail of small tenants, a
middle band, and **three deliberately hot tenants**: one with a large `audit.entries` history, one with
heavy `advertising.metrics_daily`, one with high `payments` volume and many refunds. Include a locale
mix skewed to contain Japanese content, themes published at several revisions, and custom domains at
each lifecycle state.

The three hot tenants exist for three specific later tests — per-tenant fairness (TASK-023),
single-tenant restore (TASK-025), bloat measurement (TASK-028). **Do not simplify the distribution.**

**Verify:** a unit test asserts the distribution has the three hot tenants and that no tenant band is
empty.

### Step 3: Implement `seed-scale` with its refusal guard

**Files:** Create `apps/cli/src/commands/seed_scale.rs`; Modify `justfile`; Test:
`apps/cli/tests/**`

**Do:** deterministic from a `--seed` argument. Refuses any target database whose name does not match
the staging pattern, checked against the resolved connection string rather than against an environment
variable a caller can set.

**Verify:**
- `just seed-scale --seed 1 --dry-run` twice → identical row counts and checksums.
- `--seed 2` → a different but still valid distribution.
- A test points the command at a production-shaped database name and asserts it refuses, **including
  when the bypass environment variable is set**.

### Step 4: Write the k6 scenarios

**Files:** Create `tests/load/{storefront_read,admin_write,checkout_create,webhook_flood,conversion_upload}.js`;
Modify `justfile`

**Do:** one scenario per load shape named in FR-1103, driven by `just perf`. Every run writes a JSON
artifact to `benchmarks/<scenario>/<tag>.json` including the machine profile (core count, memory, OS
version) so two artifacts from different hosts are never silently compared.

**Verify:** `just perf` produces one artifact per scenario, each carrying a machine profile block.

### Step 5: Write `perf-compare` and prove it catches a regression

**Files:** Create `scripts/perf-compare.sh`; Modify `justfile`

**Do:** diff two tagged runs per metric and exit non-zero past a per-metric threshold. Refuse to
compare artifacts with different machine profiles.

**Verify:**
- `just perf-compare base base` → passes (a run against itself is not a regression).
- Inject a 20 % latency increase into a copied artifact → `just perf-compare` **fails**.
- Run-to-run noise on two real consecutive runs → passes. If it does not, **the threshold is wrong;
  fix it here** rather than fighting noise for the rest of the phase.

### Step 6: Capture the query and cache baseline

**Files:** Create `benchmarks/baseline-<date>.md`

**Do:** with scale data loaded, snapshot `pg_stat_statements` top 50, index usage, table and index
sizes, cache hit ratios, and pool saturation. This is TASK-021's input and TASK-028's input.

**Verify:** the file exists, names the seed and the machine profile, and lists 50 statements.

### Step 7: Assemble the GA checklist

**Files:** Create `docs/release/ga-checklist.md`

**Do:** one line per acceptance criterion from every phase 00–11, each with a state
(`automated` / `manual` / `parked`) and, for manual lines, the command or procedure that verifies it.
There is no owner column: there is one owner. A `parked` line links to
`docs/release/needs-humans.md`.

**Verify:** `just ga-checklist-check` fails on a line with no state and passes on the assembled file.

## Definition of done

- [x] `GET /api/v1/system/build` returns the sha the binary was built from; `just openapi` clean.
- [x] `seed-scale` is deterministic across two runs of the same seed and different across seeds.
- [x] `seed-scale` refuses a non-staging database name, proven with the bypass variable set.
- [x] Scale data is loaded and its row counts and tenant distribution are recorded.
- [x] `benchmarks/` holds a committed baseline artifact per scenario, each with a machine profile.
- [x] `just perf-compare` fails on an injected 20 % regression and passes on run-to-run noise.
- [x] `benchmarks/baseline-<date>.md` records the query and cache snapshot.
- [x] `docs/release/ga-checklist.md` exists, every line has a state, and the check gate is live.
- [x] No behaviour change: `just ci` green and no benchmark moved.

*(Recorded 2026-09-17, branch `feat/task-019-measurement-harness`: `just ci-quiet` exit 0 —
fmt, clippy `-D warnings`, layering, connect/ads/id-policy invariants, `ci-test-coverage`
(18 members with tests/, all covered), full workspace suite via `scripts/test.sh` with the
`agent` profile (**1158 tests run, 1158 passed**), OpenAPI drift (14 contexts OK), i18n
(100 % ja coverage), `ga-checklist-check` (49 automated / 27 manual / 7 parked). "No
benchmark moved" holds trivially: these are the first committed artifacts and the baseline
is the tree's own record. The two deliberate behaviour changes the harness forced are
recorded in the execution notes below — both are defect fixes, not feature work.)*

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just seed-scale --seed 1 --dry-run
just perf
just perf-compare base rc.1
just ga-checklist-check
```

## Execution notes (2026-09-17, branch `feat/task-019-measurement-harness`)

- **Step 1 — build probe.** `GET /api/v1/system/build` serves
  `BuildDetails { commit, version, built_at, environment }` derived from the same
  `BuildInfo` the version endpoint serves, so the two can never disagree about the build.
  Unauthenticated required two config moves: the path joined
  `server.tenant.public_prefixes` (Rust default *and* `configs/base.toml`), and the
  endpoint answers without a tenant host under active tenant middleware — locked by a
  contract test that also proves `/api/v1/system/version` still 404s without a tenant
  (minimal reachability delta: only `/system/build` became public, not the whole `/system`
  prefix). A context-level test asserts the response carries exactly the four stamp
  fields and a `Cache-Control` header. Contract tests pass a `&str` environment into
  `build_system_state` (signature grew one parameter).
- **Steps 2–3 — fixture and generator.** `apps/cli/src/seed_scale/`: SplitMix64-seeded
  distribution (120-tenant long tail, 24-tenant middle band, three hot tenants —
  `scale-hot-audit` 20k audit entries, `scale-hot-ads` 2×15 campaigns × 90 days of
  `metrics_daily`, `scale-hot-payments` 4,000 payments at a 25 % refund ratio), locales
  55 % ja-default, themes at 1–5 revisions, domains at all seven lifecycle states with
  live-hostname projection rows where the live set requires them. All ids minted from a
  fixed range (7.0e18+n, above the epoch-based generator's reach) and all timestamps from
  a fixed base, so checksums are reproducible; seeded audit entries chain onto the real
  global chain (same advisory lock and tail read as the writer), are verified in memory
  with `sanvi_audit::verify_chain` before insert, and `just audit-verify` passes over the
  28,718-entry result. The guard checks `current_database()` against the staging pattern;
  no bypass variable is read anywhere, proven by the integration test that sets
  `SANVI_SEED_SCALE_ALLOW` and gets refused anyway. Determinism verified end to end:
  `--seed 1 --dry-run` twice → byte-identical JSON; `--seed 2` → different checksum,
  same shape. A full write lands 48,608 rows in ~13 s.
- **The harness caught two real defects; both are fixed here** — the only behaviour
  changes in this task, each forced by measurement being impossible otherwise:
  1. **`/api/v1/public` scope shadowing.** Tenancy, localization, theming and
     advertising-tracking each registered their own `web::scope("/api/v1/public")`.
     actix commits to the first matching scope and never backtracks, so in the assembled
     app `theme`, `locales`, `messages`, `track` (and every later `/api/v1/public*`
     registration, plain resources included) 404'd while each context's own router was
     green — the storefront theme endpoint and the phase-10 tracking beacon have been
     dead in production wiring since their phases shipped. Public routes are now
     full-path resources (the pattern billing and privacy already used), which are
     order-safe. Verified before/after with curl against the running app: four 404s →
     four 200s; the tracking integration tests (which mount the changed function) pass
     unchanged.
  2. **Seed region `de` rejected by the domain.** The fixture invented region `de`;
     tenancy's `DataRegion` accepts only `eu|jp|us`, and any `de` tenant 503'd on
     resolution. Fixed to the domain's own set.
- **Step 4 — k6 harness.** Five scenarios in `tests/load/`, one per FR-1103 load shape,
  driven by `just perf <tag> [scenario]`; each writes
  `benchmarks/<scenario>/<tag>.json` carrying the machine profile, the offered rate and
  the build stamp from the probe's own `setup()`. Lab honesty is written into the
  artifacts: with no Kratos and no Stripe locally, `admin_write`, `checkout_create` and
  `webhook_flood` measure the request path up to the 401/signature boundary, and the
  scenarios declare those statuses as *expected* (custom `expected_status` rate) so the
  failed-rate metric means anomaly, not the documented lab boundary. `conversion_upload`
  drives the capture beacon's front door; the upload sweep is worker-side and no k6
  scenario pretends to drive it. Two config notes for reproducing: per-IP and per-tenant
  rate limiting must be off (`SANVI__SERVER__RATE_LIMIT__ENABLED=false`,
  `SANVI__SERVER__TENANT__TENANT_RATE_LIMIT__ENABLED=false` — otherwise 95 %+ of the run
  is 429s from 127.0.0.1), and `features.advertising` needs dev-only signing secrets plus
  a `token_kek` to boot.
- **Step 5 — perf-compare.** Per-metric gate: latency fails past +20 % *and* past an
  absolute noise floor (default 0.5 ms), unexpected-status fails past +1 pp, dropped
  iterations fail on any increase; foreign machine profiles are refused outright. The
  floor is the task's own "if noise fails the compare, the threshold is wrong" rule made
  concrete: two consecutive quiet runs moved `webhook_flood` p50 by +28 % on 0.36 ms
  (±0.1 ms of jitter), so a purely relative gate fights noise forever, while a
  relative-only rule on a 1.3 ms storefront p95 would never fire on a 20 % move. The
  self-test proves: identical passes, +25 % on a 100 ms base fails, a foreign host is
  refused, and +30 % on a 1 ms base (sub-noise) passes. The injected-regression
  demonstration on *real* artifacts injects past the floor (the measured jitter is
  larger than 20 % of these lab latencies — recorded here rather than silently
  re-defined).
- **Step 6 — baseline.** `benchmarks/baseline-2026-09-17.md` was captured against the
  seed-1 data with `pg_stat_statements` enabled for the run
  (`ALTER SYSTEM` + `CREATE EXTENSION` on this host's compose Postgres — the compose
  default ships without it; making that permanent is TASK-021's first action, as is
  instrumenting the sqlx pool, whose absence is recorded in the doc's pool section).
  The statement table holds 16 statements, not 50: five minutes of lab load touches a
  small, heavily Redis-cached query surface, and the doc says so with the reset
  procedure for widening the next capture. The API served from the debug build (the
  release profile fails to link on this host — `sqlx_macros` dylib, mis-aligned
  LINKEDIT) — recorded in the doc so nobody mistakes these numbers for more than lab
  regression anchors.
- **Step 7 — GA checklist.** `docs/release/ga-checklist.md` assembles all 83 acceptance
  criteria from the twelve phase plans (phase 10's under its "Phase acceptance
  criteria" heading; phases 03/07 use `- [x]` bullets). 49 automated (each naming the
  gate or command), 27 manual (each with its procedure — the phase-11 half is manual
  because its slices have not landed yet, and each verify note names the slice that
  turns it automated), 7 parked (each a people/vendor/credentials/traffic blocker,
  linked to `needs-humans.md`). `just ga-checklist-check` is wired into `just ci` and
  `ci-quiet`; its self-test fails stateless, unknown-state and verify-less lines.
- **Not yet exercised by real CI:** this checkout still has no git remote (same as
  TASK-031); every command above ran locally and green, and the first push exercises
  the workflow for real.

## Out of scope

Any tuning (TASK-021). Turning any gate from reporting-only to blocking. Cutting a release branch or
freezing features — phase 10 is unfinished, and the freeze belongs to TASK-026 where it is a contract
decision rather than a process one.

## Parked

Four weeks of field telemetry, SLO targets confirmed against observed data, the booked penetration test
and accessibility audit — see [`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
