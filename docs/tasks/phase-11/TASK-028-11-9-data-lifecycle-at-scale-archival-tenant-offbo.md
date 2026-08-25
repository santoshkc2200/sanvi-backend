# TASK-028: 11.9 Data lifecycle at scale, archival & tenant offboarding

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1128, FR-1129, FR-1130, FR-1131
**Depends on:** TASK-021
**Created:** 2026-08-21

**Slice:** 11.9 — Data lifecycle at scale, archival & tenant offboarding
**Prerelease:** `v1.0.0-rc.10` · **Flag:** `platform.archival`

## Context

Make the tables that grow forever behave: partitioned, archived on a schedule, restorable through a
documented path, and vacuumed before bloat becomes an outage. Then prove the exit door works — a
departing tenant can be exported completely and purged completely, with evidence.

The tables in question — `audit.entries`, `platform.outbox`, the webhook event tables, and
`advertising.conversion_events` / `metrics_daily` — are the ones that are cheap for eleven phases and
then, one morning, are the reason the database is out of disk.

TASK-029 depends on this task for a specific reason: the compliance evidence pack **claims** retention
runs. This is where that claim becomes a report generated from live data.

## What to do

- [ ] **Contract** — two additions to `api/platform_admin.yaml`, spec-first because the platform console
      drives them:

      | Method | Path | Permission | Notes |
      |---|---|---|---|
      | POST | `/api/v1/platform/tenants/{id}/offboarding` | `platform.tenant.offboard` | Starts export-then-purge; idempotent by key |
      | GET | `/api/v1/platform/tenants/{id}/offboarding` | `platform.tenant.offboard` | Status, artifact link, purge report |

      Archived data is **not** exposed as a separate API shape: a read that needs archived rows either
      restores them through the documented path or says the range is archived. A silently-partial result
      is the failure mode that makes archival untrustworthy.
- [ ] **Partitioning applied where it is missing** — `audit.entries`, `platform.outbox`, and the webhook
      event tables get the same monthly range partitioning phase 10 established for the advertising
      tables, with the partition-creation job covering **all** partitioned parents. One job, one
      registry, one alert when next month's partition does not exist by the 25th.
- [ ] **Archival policies** — per table: what moves, at what age, to where (cold object storage,
      encrypted with the same envelope scheme), and how it is read back. Behind `platform.archival`,
      with the first run on staging scale data measured for duration and IO impact.
- [ ] **Documented restore path** — a runbook plus a CLI command that restores an archived range into a
      queryable table, rehearsed once **in this task**. Archival without a rehearsed restore is deletion
      with extra steps.
- [ ] **Retention verification** — the phase-05 retention rules are *asserted to be running*: a
      per-class report (rule, last run, rows affected, oldest surviving row) generated from live data,
      with an alert when a class has not run inside its window. Phase 05 wrote the rules; this task
      proves the machinery executes them.
- [ ] **Index and vacuum strategy** — bloat measured per table at scale, autovacuum tuned per table size
      (the defaults are wrong for both the hot tenant's audit table and the tiny lookup tables), unused
      indexes dropped and duplicates merged, with before/after sizes recorded.
- [ ] **Tenant offboarding job** — export everything the tenant owns in a documented, machine-readable
      format (reusing the phase-05 export machinery rather than reinventing it), **verify the export**,
      *then* purge across every context including archives, backup policy notes, caches, search indexes,
      and object storage. Produces a purge report enumerating what was removed per context and what was
      intentionally retained (financial records with a legal basis, for example) with the reason and the
      retention end date.
- [ ] **Purge verification** — an automated check that runs after a purge and searches every context for
      residual rows for that tenant, including partitions and archives; a non-empty result **fails the
      job loudly** rather than completing quietly.

## Acceptance criteria

- [ ] Every unbounded table is partitioned, with one job covering all partitioned parents from a single
      registry; disabling the job fires the alert (tested).
- [ ] Archival round-trip: archive a range, query it (get the archived-range response, never a silent
      partial), restore it, and get identical rows including ordering and checksums.
- [ ] The first archival run on scale data has its duration and IO impact recorded.
- [ ] Each phase-05 retention class runs inside its window on scale data, and a stalled class alerts.
- [ ] Autovacuum is tuned per table size; unused and duplicate indexes are removed; before/after sizes
      are committed, measured on the hot tenant's audit table.
- [ ] Offboarding export contains every context's data for the tenant, asserted per context.
- [ ] Purge removes it, the residual scan finds nothing, and a neighbour tenant's data is untouched.
- [ ] Offboarding is idempotent: re-running produces the same report and does not fail.
- [ ] The purge report lists lawfully retained classes with their legal bases — **an empty retained list
      on a tenant with completed payments is a bug, not a clean purge.**

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
```

## Out of scope

The evidence packs and jurisdiction drills that consume this task's retention report (TASK-029). The
frontend offboarding, retention, and archived-range surfaces (frontend TASK-028). Database restore from
backup, which is a different operation with a different runbook (TASK-025).

## Files likely touched

- `migrations/**` (partitioning of `audit.entries`, `platform.outbox`, webhook event tables)
- the partition-creation job and its registry (extending phase 10's)
- a new archival module (mover, cold-storage adapter, archived-range read path)
- `apps/cli/**` (`archive-restore`, `offboard`), `docs/runbooks/{archival,offboarding}.md`
- the phase-05 retention machinery's reporting surface, plus its staleness alert and runbook
- `api/platform_admin.yaml` (offboarding endpoints)

## Notes / gotchas

- Rollback: `platform.archival` off stops the mover; already-archived data stays reachable through the
  restore path — which is exactly why the restore path is rehearsed in this task rather than later.
  Partitioning and index changes are migrations with reversals. **Offboarding purge is deliberately not
  reversible**: it ships with a dry-run mode, a two-person confirmation, and a mandatory verified export
  before the purge step can start.
- Partitioning a table that already holds a hot tenant's history is the migration most likely to need a
  maintenance window. Measure it against scale data before scheduling it, and use the expand/contract
  pattern TASK-023 enforces.
- The residual scan must cover child partitions and archives by name. A purge that misses an archived
  partition is the compliance finding TASK-029 will surface anyway, more expensively.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
