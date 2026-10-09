# TASK-025: 11.6 Recovery drills, secret rotation & health probes

**Phase:** 11 · **Status:** done · **Size:** M
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

**Scope decision (2026-10-09, user-approved):** complete a local rehearsal and
park live provider operations. The command rotates disposable PostgreSQL/Redis
credentials and exercises replacement/retirement with production signing,
webhook and envelope crypto. Provider issuance/revocation, deployment-wide key
migrations, rollout/overlap windows and old-session invalidation are explicitly
parked, with procedures in `docs/runbooks/rotation/providers.md` and
`docs/release/needs-humans.md`. Local signature rejection is not provider revocation.

### Step 7: Write and rehearse the degraded-mode procedure

**Files:** Create `docs/runbooks/degraded-mode.md`

**Do:** what the platform does with the database in read-only recovery, what operators tell tenants,
which flags to set (TASK-023's `degraded_mode`), and who decides. Rehearse it in the same window as the
restore drill.

**Verify:** the runbook's commands were executed during the restore drill and the date is recorded.

## Definition of done

- [x] `/health` returns with the database stopped, asserted by a test.
- [x] `/ready` leaks nothing; the snapshot test passes.
- [x] SIGTERM drains in-flight work; no request is dropped.
- [x] A full restore drill records a duration, data-loss window, machine profile, and date, labelled
      local.
- [x] PITR restores to a chosen timestamp; the audit chain verifies and the outbox is duplicate-safe.
- [x] Single-tenant restore returns the target's rows and leaves a neighbour byte-identical, with an
      audit entry.
- [x] Every locally rehearsed credential has a duration, runbook, replacement-acceptance proof and
      old-credential rejection proof; live provider/deployment operations are parked per the scope decision above.
- [x] The degraded-mode procedure is written and its commands were executed.

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

## Completion evidence — 2026-10-09

Closing commit: `feat(ops): complete local recovery and credential rotation drills (TASK-025)`.
Earlier implementation: `c497542` (probes/shutdown), `24a5b3e` (restore/PITR),
`33e6114` (single-tenant restore). This slice finishes rotation, degraded-mode,
encrypted-artifact replay and verification; the user-owned `AGENTS.md` change is excluded.

| Proof | Executed result |
| --- | --- |
| Workspace suite (`bash scripts/test.sh --quiet`) | 1,415 / 1,415 pass across 68 binaries, zero skipped; includes DB-outage probes, states-only readiness snapshots, graceful request drain, worker shutdown, physical PITR and tenant-restore dry-run/apply + neighbour checksums + audit |
| Format / lint | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings` pass |
| Contract / route matrix | `target/debug/sanvi-cli openapi --check` passes all 14 contexts; `python -X utf8 scripts/generate-route-matrix.py` complete/current |
| Full encrypted restore | `bash scripts/db/rehearse-recovery.sh`: seed 1, 48,608 rows, 147 tenants; 15 s local RTO including decrypt/materialize, 0 s loss against quiesced source; audit chain verifies 28,720 entries |
| Encrypted PITR | Same backup to `2026-10-09T06:37:31.572719+00:00`; 15 s local RTO; 3 s intentional loss vs recorded source horizon (not time spent running the rehearsal); 28,719 audit entries, post-target marker absent, outbox/payments consistent |
| Local targets | RTO 15 s ≤ 3,600 s; full RPO 0 s / intentional PITR loss 3 s ≤ 300 s. These are local fixture results, not production validation |
| Rotation | `target/debug/sanvi-cli rotate-credentials --rehearse`: 4,499 ms total; ten measured credential classes, each replacement accepted / retired rejected. PostgreSQL/Redis reject authentication specifically, not arbitrary connection failures; signing uses still-valid tokens, OAuth requires signature rejection; persisted DEKs rewrapped with production crypto |
| Degraded procedure | Both restore reports record the executed `sanvi_app` read-only SELECT, SQLSTATE `25006` write rejection, and fresh read-write recovery command; edge maintenance/messages remain parked |
| Extraction negative control | `bash scripts/db/test-encrypted-restore.sh` passes. Temporarily restoring the swallowed extraction-error behavior makes this test fail (`extraction failure reached the PostgreSQL recovery wait`), fixed behavior passes; no success report or leftover materialization volumes |
| Fresh review | Three P2 findings fixed: stale replay volumes, swallowed extraction errors, retained anonymous PGDATA volumes. The subsequent full suite and final physical drills pass |

Reports: `benchmarks/dr/restore-full-20261009T063716Z.json`,
`benchmarks/dr/restore-pitr-20261009T063716Z.json`, `benchmarks/dr/rotation-local.json`.
Machine: Windows x86_64, 20 logical cores, Docker, Postgres 18 / Redis 8.
Sub-millisecond signing checks legitimately record 0 ms at millisecond resolution.

Rulings: resume the existing implementation in the current checkout, saving
completion on `codex/task025-completion`; provider/deployment operations are
parked under the explicit user-approved local scope (cost if misapplied: no
production revocation evidence, documented rather than inferred). Readiness's
fixed dependency list is exempt from pagination; the report's public credential
kind is a reasoned `not-a-secret` exemption. Both gate omissions were found by
the first full-suite run and verified by the subsequent green run. Backup
cleanup now removes plaintext source volumes and restore consumes encrypted
base+WAL, with decrypt time in RTO. A remote is configured, but this completion
has not been pushed and no CI or staging run is claimed.

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
