# TASK-019: 11.0 Production-shaped data & the measurement harness

**Phase:** 11 · **Status:** todo · **Size:** M
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

- [ ] `GET /api/v1/system/build` returns the sha the binary was built from; `just openapi` clean.
- [ ] `seed-scale` is deterministic across two runs of the same seed and different across seeds.
- [ ] `seed-scale` refuses a non-staging database name, proven with the bypass variable set.
- [ ] Scale data is loaded and its row counts and tenant distribution are recorded.
- [ ] `benchmarks/` holds a committed baseline artifact per scenario, each with a machine profile.
- [ ] `just perf-compare` fails on an injected 20 % regression and passes on run-to-run noise.
- [ ] `benchmarks/baseline-<date>.md` records the query and cache snapshot.
- [ ] `docs/release/ga-checklist.md` exists, every line has a state, and the check gate is live.
- [ ] No behaviour change: `just ci` green and no benchmark moved.

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
