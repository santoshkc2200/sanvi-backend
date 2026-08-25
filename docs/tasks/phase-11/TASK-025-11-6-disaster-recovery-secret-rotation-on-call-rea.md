# TASK-025: 11.6 Disaster recovery, secret rotation & on-call readiness

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1118, FR-1119, FR-1120, FR-1121, NFR-1107
**Depends on:** TASK-023, TASK-024
**Created:** 2026-08-21

**Slice:** 11.6 — Disaster recovery, secret rotation & on-call readiness
**Prerelease:** `v1.0.0-rc.7` · **Flag:** `platform.status_probes`

## Context

Prove recovery rather than assert it. A timed full restore, a point-in-time restore, a single-tenant
restore, and a full credential rotation — **each run by someone who did not build the system**, each
with a measured duration written down next to the target it was supposed to meet.

RPO ≤ 5 min and RTO ≤ 1 h are the targets. Until a drill produces those numbers with a date and a
runner's name, they are marketing copy. A backup that has never been restored is a hypothesis.

This task depends on TASK-023 because a degraded mode is what you fall back *to* during a restore, and
on TASK-024 because the secrets inventory is what scopes the rotation drill.

## What to do

- [ ] **Contract** — two additions to `api/system.yaml`, spec-first because the status page and external
      monitors consume them:

      | Method | Path | Auth | Notes |
      |---|---|---|---|
      | GET | `/api/v1/system/health` | none | Liveness only; never touches the database |
      | GET | `/api/v1/system/ready` | none | Dependency readiness summary, no internal detail leaked |

      Readiness reports **states**, not hostnames, versions, or error strings — a readiness endpoint is
      a reconnaissance endpoint if it is honest to the wrong audience.
- [ ] **Backups** — continuous WAL archiving plus daily base backups, retention aligned to the phase-05
      rules, encryption at rest, and access to the backup store audited **separately from the database
      itself** (a backup is a copy of everything RLS protects).
- [ ] **Restore rehearsal, timed and monthly** — a documented procedure in `docs/runbooks/dr-restore.md`,
      executed on a schedule with a recorded start time, end time, data-loss window, and runner. The
      first execution lands in this task; the schedule outlives it.
- [ ] **Point-in-time recovery drill** — restore to a timestamp mid-transaction-stream and verify
      consistency across contexts: outbox, audit chain, and payment records, the three places a torn
      restore shows up first.
- [ ] **Single-tenant restore** — the far more common real request: restore one tenant's data into the
      live system without touching anyone else's rows. A documented, audited procedure with a dry-run
      mode, rehearsed against a hot tenant from TASK-019's scale data.
- [ ] **Secret compromise drill** — rotate every credential end to end and measure how long it takes:
      database, Redis, Kratos/Keto, Stripe platform and restricted keys, Connect webhook secrets, ad
      OAuth client secrets, the tracking site-key signing secret, the phase-05 KEK/DEK hierarchy, and
      the session signing keys. Output is a duration **and a runbook** per credential.
- [ ] **Degraded-mode operating procedure** — written: what the platform does with the database in
      read-only recovery, what operators tell tenants, which flags to set (TASK-023's `degraded_mode`),
      and who decides. Rehearsed in the same window as the restore drill.
- [ ] **Status probes** — real probes from outside the perimeter exercising the six SLO journeys behind
      `platform.status_probes`, not a manually updated page. Incident templates prepared in **both
      locales**, because a status page that only speaks English fails half the users during the one
      event they need it.
- [ ] **On-call readiness** — rotation defined, escalation policy written with response-time
      expectations per tier, alert routing tested end to end with a synthetic page, and a shadow week
      whose primary is someone who did not write the alerts.

## Acceptance criteria

- [ ] Backups run with WAL archiving; backup-store access is audited independently of the database.
- [ ] A full restore met **RPO ≤ 5 min and RTO ≤ 1 h in a timed drill**, with the report committed
      naming the date, build, and runner.
- [ ] PITR restores to a chosen timestamp; the audit chain verifies and the outbox has no
      duplicate-delivery window the idempotency keys do not cover.
- [ ] Single-tenant restore returns the target tenant's rows to the expected state, leaves a neighbour
      tenant's rows byte-identical before and after, and appears in the audit log.
- [ ] Every credential is rotated end to end with a per-credential duration and runbook, and **every old
      credential is asserted to be rejected afterwards** — the half that usually gets skipped.
- [ ] `GET /api/v1/system/health` returns successfully with the database stopped, asserted by a test.
- [ ] `GET /api/v1/system/ready` leaks nothing: a snapshot test on the response shape passes.
- [ ] The degraded-mode operating procedure is written and rehearsed.
- [ ] Status page probes are live and bilingual, with incident templates ready.
- [ ] A synthetic page reaches the on-call device inside the escalation policy's first tier, and the
      shadow week is complete.
- [ ] The monthly restore rehearsal is scheduled with an owner beyond GA.

## Verification

```bash
just fmt
just clippy
just test
just openapi
just dr-restore --rehearse
just dr-pitr --rehearse
just tenant-restore --dry-run
just rotate-credentials --rehearse
just test-health-without-db
```

## Out of scope

The frontend status page, banners, and restore-in-progress experience (frontend TASK-025). Archival and
its restore path, which is a different operation with a different runbook (TASK-028). The staged
rollout and its halt conditions (TASK-030).

## Files likely touched

- `api/system.yaml` (`/health`, `/ready`)
- `crates/platform/http/**` (probe handlers with their own timeouts)
- `apps/cli/**` (`tenant-restore`, `rotate-credentials` helpers)
- `docs/runbooks/dr-restore.md`, `docs/runbooks/rotation/*.md`, `docs/runbooks/degraded-mode.md`
- `docs/oncall/{rotation,escalation}.md`
- infrastructure definitions for WAL archiving, base backups, and external probes

## Notes / gotchas

- Rollback: `platform.status_probes` off silences probe traffic and the status page falls back to manual
  updates. Backups, restores, and rotations are operational procedures rather than deployed behaviour —
  there is nothing to roll back, only a drill to repeat. The one deployed change carrying risk is the
  readiness endpoint's dependency checks: they ship with a timeout and **cannot make the process
  unhealthy on their own**.
- Liveness that touches the database is the classic outage amplifier: the database gets slow, every
  instance fails its health check, the orchestrator kills them all. The test that stops the database and
  asserts `/health` still returns is guarding against exactly that.
- Rotation is measured end to end including the human steps. A rotation that takes six minutes of
  commands and four hours of waiting for an approval is a four-hour rotation.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
