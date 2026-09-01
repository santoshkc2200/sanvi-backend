# TASK-028: 11.9 Data lifecycle at scale, archival & tenant offboarding

**Phase:** 11 · **Status:** todo · **Size:** M
**Requirement(s):** FR-1128, FR-1129, FR-1130, FR-1131
**Depends on:** TASK-021, **phase 10 merged**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** `platform.archival`

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Make the tables that grow forever behave — partitioned, archived on a schedule, restorable through a
rehearsed path, vacuumed before bloat becomes an outage — and prove the exit door works: a departing
tenant exported completely and purged completely, with evidence.

## Why this shape

`audit.entries`, `platform.outbox`, the webhook event tables, and `advertising.conversion_events` /
`metrics_daily` are cheap for eleven phases and then, one morning, are the reason the database is out
of disk.

TASK-029 depends on this task for a specific reason: the compliance evidence pack **claims** retention
runs. This is where that claim becomes a report generated from live data.

**Archival without a rehearsed restore is deletion with extra steps.** The restore is rehearsed in this
task, not later.

## Constraints

- Archived data is **not** exposed as a separate API shape. A read that needs archived rows either
  restores them through the documented path or says the range is archived. A silently-partial result is
  the failure mode that makes archival untrustworthy.
- **The offboarding purge is deliberately not reversible.** It ships with a dry-run mode, a mandatory
  verified export before the purge step can start, and a typed confirmation naming the tenant.
- The residual scan must cover child partitions and archives **by name**.
- Partitioning a table that already holds a hot tenant's history is the migration most likely to need a
  maintenance window. Measure it against scale data before scheduling it, using TASK-023's
  expand/contract pattern.

## File ownership map

- `migrations/**` — partitioning of `audit.entries`, `platform.outbox`, webhook event tables
- the partition-creation job and its registry, extending phase 10's
- a new archival module — mover, cold-storage adapter, archived-range read path
- `apps/cli/**` — `archive-restore`, `offboard`
- `docs/runbooks/{archival,offboarding}.md`
- the phase-05 retention machinery's reporting surface, its staleness alert and runbook
- `api/platform_admin.yaml` — offboarding endpoints

## Steps

### Step 1: Add the offboarding endpoints, contract first

**Files:** Modify `api/platform_admin.yaml`

| Method | Path | Permission | Notes |
|---|---|---|---|
| POST | `/api/v1/platform/tenants/{id}/offboarding` | `platform.tenant.offboard` | Starts export-then-purge; idempotent by key |
| GET | `/api/v1/platform/tenants/{id}/offboarding` | `platform.tenant.offboard` | Status, artifact link, purge report |

**Verify:** `just openapi` clean; `just route-matrix` maps both routes with a permission and a
rate-limit class.

### Step 2: Partition the remaining unbounded tables from one registry

**Files:** Modify `migrations/**`; Modify the partition-creation job

**Do:** monthly range partitioning for `audit.entries`, `platform.outbox`, and the webhook event
tables, matching what phase 10 established for the advertising tables. **One job, one registry, one
alert when next month's partition does not exist by the 25th.**

**Verify:**
- `just partition-check` passes across all partitioned parents.
- Disabling the job fires the alert, asserted by a test.
- The migration's duration against scale data is recorded.
- `just rls-matrix` (TASK-024) still passes over the new child partitions **by name**.

### Step 3: Build archival with its restore path, and rehearse both

**Files:** Create the archival module; Create `apps/cli/src/commands/archive_restore.rs`,
`docs/runbooks/archival.md`

**Do:** per table — what moves, at what age, to where, encrypted under the phase-05 envelope scheme, and
how it is read back. Behind `platform.archival`. A read over an archived range returns an
archived-range response, never a partial one.

**Verify:** `just archival-roundtrip` — archive a range, query it (get the archived-range response),
restore it, and assert **identical rows including ordering and checksums**. The first run on scale data
records its duration and IO impact.

### Step 4: Prove the phase-05 retention rules actually run

**Files:** Modify the phase-05 retention machinery; Create its report surface and staleness alert

**Do:** a per-class report generated from live data — rule, last run, rows affected, oldest surviving
row — with an alert when a class has not run inside its window. Phase 05 wrote the rules; this proves
the machinery executes them.

**Verify:** `just retention-report` on scale data shows every class running inside its window; a
deliberately stalled class fires the alert.

### Step 5: Tune vacuum and prune indexes

**Files:** Modify `migrations/**`, table storage parameters

**Do:** measure bloat per table at scale; tune autovacuum per table size — the defaults are wrong for
both the hot tenant's audit table and the tiny lookup tables. Drop unused indexes, merge duplicates.

**Verify:** before/after sizes committed, measured on the hot tenant's audit table.

### Step 6: Build offboarding as export-then-purge

**Files:** Create `apps/cli/src/commands/offboard.rs`, `docs/runbooks/offboarding.md`

**Do:** export everything the tenant owns in a documented machine-readable format, **reusing the
phase-05 export machinery rather than reinventing it**; verify the export; *then* purge across every
context including archives, backup policy notes, caches, search indexes, and object storage. Produce a
purge report enumerating what was removed per context and what was intentionally retained, with the
reason and the retention end date.

Because there is no second operator, the two-person confirmation is replaced by: dry-run, a verified
export as a hard precondition, and a typed confirmation naming the tenant.

**Verify:**
- `just offboard --dry-run` reports without mutating.
- The export contains every context's data for the tenant, asserted per context.
- The purge step is unreachable before the export verifies.

### Step 7: Add the residual scan and make it fail loudly

**Files:** Create the residual scan; Modify `apps/cli/**`

**Do:** after a purge, search every context for residual rows for that tenant, **including partitions
and archives by name**. A non-empty result fails the job loudly rather than completing quietly.

**Verify:**
- The scan finds nothing after a purge, and a neighbour tenant's data is byte-identical.
- Re-running offboarding produces the same report and does not fail.
- **The purge report lists lawfully retained classes with their legal bases** — an empty retained list
  on a tenant with completed payments fails the test, because that is a bug, not a clean purge.

## Definition of done

- [ ] Every unbounded table is partitioned from one registry; disabling the job fires the alert.
- [ ] Archival round-trips with identical rows, ordering, and checksums; the first run's duration and IO
      impact are recorded.
- [ ] A read over an archived range returns an archived-range response, never a silent partial.
- [ ] Each retention class runs inside its window on scale data, and a stalled class alerts.
- [ ] Autovacuum is tuned per table size; unused and duplicate indexes removed; sizes committed.
- [ ] Offboarding export contains every context's data, asserted per context.
- [ ] Purge removes it, the residual scan finds nothing, and a neighbour tenant is byte-identical.
- [ ] Offboarding is idempotent.
- [ ] The purge report separates removed from lawfully retained, and a tenant with completed payments
      cannot produce an empty retained list.

## Verification

```bash
just fmt
just clippy
just test
just openapi
just migrate
just partition-check
just archival-roundtrip
just retention-report
just offboard --dry-run
just rls-matrix
```

## Out of scope

The evidence packs that consume this task's retention report (TASK-029). Frontend offboarding,
retention, and archived-range surfaces (frontend TASK-028). Database restore from backup, which is a
different operation with a different runbook (TASK-025).

## Parked

Cold object storage as the archival target — the mover writes to a local encrypted volume with the same
interface, so the storage adapter is a swap when there is somewhere to swap to. Two-person confirmation
on purge — see [`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
