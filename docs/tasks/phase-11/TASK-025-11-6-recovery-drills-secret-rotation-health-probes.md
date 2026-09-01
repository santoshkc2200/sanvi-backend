# TASK-025: 11.6 Recovery drills, secret rotation & health probes

**Phase:** 11 · **Status:** todo · **Size:** M
**Requirement(s):** FR-1118, FR-1119, FR-1120, NFR-1107
**Depends on:** TASK-023, TASK-024, **phase 10 merged**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** none

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Prove recovery rather than assert it. A timed full restore, a point-in-time restore, a single-tenant
restore, and a full credential rotation — each with a measured duration written down next to the target
it was supposed to meet. Plus the two probes a restore and a deploy both depend on.

## Why this shape

The original task assumed managed infrastructure: continuous WAL archiving to an encrypted store with
independently audited access, multi-AZ posture, external probes from outside the perimeter, an on-call
rotation with a shadow week, and a status page. None of that exists — there is no cloud account and no
IaC directory in this repository.

What survives is the part that is actually the risky part: **the restore procedure itself**. A
`pg_basebackup` plus WAL-replay drill against the compose stack exercises the same commands, the same
consistency questions, and the same failure modes as a managed restore would; only the scale and the
storage target differ. RPO and RTO numbers from it are local numbers and are labelled as such.

A backup that has never been restored is a hypothesis. That is true at any scale.

## Constraints

- **Liveness must never touch the database.** The database gets slow, every instance fails its health
  check, the orchestrator kills them all. The test that stops the database and asserts `/health` still
  returns is guarding against exactly that.
- Readiness reports **states**, never hostnames, versions, or error strings. A readiness endpoint is a
  reconnaissance endpoint if it is honest to the wrong audience.
- The readiness dependency checks ship with their own timeout and **cannot make the process unhealthy
  on their own**.
- Rotation is measured end to end. A rotation that takes six minutes of commands and four hours of
  waiting is a four-hour rotation.

## File ownership map

- `api/system.yaml` — `/health` and `/ready`
- `crates/platform/http/**` — probe handlers with their own timeouts
- `apps/cli/**` — `tenant-restore`, `rotate-credentials`
- `docs/runbooks/dr-restore.md`, `docs/runbooks/rotation/*.md`, `docs/runbooks/degraded-mode.md`
- `scripts/db/backup.sh`, `scripts/db/restore.sh` — the drill scripts
- `docker-compose.staging.yml` — the drill target

## Steps

### Step 1: Add the two probes, contract first

**Files:** Modify `api/system.yaml`, `crates/platform/http/**`; Test: `tests/system/probes.rs`

| Method | Path | Auth | Notes |
|---|---|---|---|
| GET | `/api/v1/system/health` | none | Liveness only; never touches the database |
| GET | `/api/v1/system/ready` | none | Dependency readiness as states; no internal detail |

**Verify:**
- `just test-health-without-db` — stop Postgres, assert `/health` still returns 200.
- A snapshot test on `/ready` asserts the response contains no hostname, version, or error string.

### Step 2: Add graceful shutdown

**Files:** Modify `apps/api/**`, `apps/worker/**`

**Do:** on SIGTERM, stop accepting new work, drain in-flight requests and jobs inside a bounded window,
then exit. This is what makes TASK-023's rolling restart drop zero requests.

**Verify:** a test sends SIGTERM under load and asserts every in-flight request completes.

### Step 3: Script the backup and restore, then time the drill

**Files:** Create `scripts/db/backup.sh`, `scripts/db/restore.sh`, `docs/runbooks/dr-restore.md`

**Do:** `pg_basebackup` plus WAL archiving to a local volume, retention aligned to the phase-05 rules,
encryption at rest. Then restore into a clean container and time it, with scale data loaded.

**Verify:** `just dr-restore --rehearse` records start time, end time, data-loss window, and the machine
profile into a committed report. Record the achieved RPO and RTO and **label them local**.

### Step 4: Rehearse point-in-time recovery

**Files:** Modify `docs/runbooks/dr-restore.md`; Create `tests/recovery/pitr.rs`

**Do:** restore to a timestamp mid-transaction-stream and verify consistency across the three places a
torn restore shows up first: the outbox, the audit chain, and payment records.

**Verify:** `just dr-pitr --rehearse` → the audit chain verifies, and the outbox has no
duplicate-delivery window the idempotency keys do not cover.

### Step 5: Build and rehearse single-tenant restore

**Files:** Create `apps/cli/src/commands/tenant_restore.rs`; Modify `docs/runbooks/dr-restore.md`

**Do:** the far more common real request — restore one tenant's data into the live system without
touching anyone else's rows. Dry-run mode, audited, rehearsed against a hot tenant from `seed-scale`.

**Verify:** the target tenant's rows return to the expected state; **a neighbour tenant's rows are
byte-identical before and after**, asserted by checksum; the operation appears in the audit log.

### Step 6: Rotate every credential and prove the old ones are rejected

**Files:** Create `apps/cli/src/commands/rotate_credentials.rs`, `docs/runbooks/rotation/*.md`

**Do:** database, Redis, Kratos/Keto, Stripe platform and restricted keys, Connect webhook secrets, ad
OAuth client secrets, the tracking site-key signing secret, the phase-05 KEK/DEK hierarchy, and session
signing keys. Scope comes from TASK-024's secrets inventory. Output is a duration and a runbook per
credential.

**Verify:** `just rotate-credentials --rehearse` records a per-credential duration, **and asserts every
old credential is rejected afterwards** — the half that usually gets skipped.

### Step 7: Write and rehearse the degraded-mode procedure

**Files:** Create `docs/runbooks/degraded-mode.md`

**Do:** what the platform does with the database in read-only recovery, what operators tell tenants,
which flags to set (TASK-023's `degraded_mode`), and who decides. Rehearse it in the same window as the
restore drill.

**Verify:** the runbook's commands were executed during the restore drill and the date is recorded.

## Definition of done

- [ ] `/health` returns with the database stopped, asserted by a test.
- [ ] `/ready` leaks nothing; the snapshot test passes.
- [ ] SIGTERM drains in-flight work; no request is dropped.
- [ ] A full restore drill records a duration, data-loss window, machine profile, and date, labelled
      local.
- [ ] PITR restores to a chosen timestamp; the audit chain verifies and the outbox is duplicate-safe.
- [ ] Single-tenant restore returns the target's rows and leaves a neighbour byte-identical, with an
      audit entry.
- [ ] Every credential rotates end to end with a duration and a runbook, and every old credential is
      proven rejected.
- [ ] The degraded-mode procedure is written and its commands were executed.

## Verification

```bash
just fmt
just clippy
just test
just openapi
just test-health-without-db
just dr-restore --rehearse
just dr-pitr --rehearse
just tenant-restore --dry-run
just rotate-credentials --rehearse
```

## Out of scope

Archival and its restore path, which is a different operation with a different runbook (TASK-028). The
frontend status page, banners, and restore-in-progress experience (frontend TASK-025). The release
rollout (TASK-030).

## Parked

Continuous WAL archiving to an encrypted store with independently audited access, multi-AZ posture,
production RPO/RTO validation, external probes, the public status page, on-call rotation, escalation
policy, and the shadow week — see [`../../release/needs-humans.md`](../../release/needs-humans.md).
The monthly restore rehearsal schedule is parked with them; the drill script it would run is built here.

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
