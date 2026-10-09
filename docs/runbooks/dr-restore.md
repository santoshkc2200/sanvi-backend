# Runbook — disaster recovery: timed restore, PITR, single-tenant restore

**Owner:** platform engineering · **Source task:** TASK-025 (FR-1119, NFR-1107) ·
**Drills:** `just dr-restore --rehearse`, `just dr-pitr --rehearse`,
`just tenant-restore --dry-run` · **Scripts:** `scripts/db/backup.sh`,
`scripts/db/restore.sh`, `apps/cli` `tenant-restore`

**Every number below is a local number.** The drill runs `docker` on an
engineering host against a compose-scale dataset (48,608 rows, 147 tenants,
28,718 seeded audit entries). It proves the *restore procedure* — the same
`pg_basebackup` + WAL-replay commands, the same consistency questions — not a
production RPO/RTO. The managed-posture numbers (multi-AZ, audited backup
store, production-scale RPO ≤ 5 min / RTO ≤ 1 h) are parked in
[`../release/needs-humans.md`](../release/needs-humans.md).

A backup that has never been restored is a hypothesis. That is true at any
scale.

## Preconditions

- Docker and the `postgres:18` image present; ports 55432/55433 free
  (`DRILL_PG_PORT` / `DRILL_RESTORE_PORT` override).
- The workspace compiles (`cargo build -p sanvi-cli`) — the drills drive the
  real `sanvi-cli` (migrations, seeds, `audit verify`) against the drill
  database, so a broken build fails the drill before it proves anything.
- `--rehearse` on the restore writes the measured report to
  `benchmarks/dr/restore-<scenario>-<session>.json`, which is committed.

## The drill

### 1. Take the backup (`scripts/db/backup.sh`)

Brings up a dedicated drill Postgres (`archive_mode=on`, `archive_command`
copying into a per-session volume, `archive_timeout=15s`), applies the
production role split, migrates, loads the production-shaped scale data
(`seed-scale`, seed 1), and takes a `pg_basebackup` into a session volume.
Two `entitlement grant` transactions — one before the base backup, one
after — write real audit-chain and outbox rows, with the marker timestamps
recorded; the PITR target is one second after the last pre-backup marker.

The retained artifact is the session directory's `base.tar.enc`
(AES-256-CBC/PBKDF2, passphrase `DRILL_BACKUP_PASSPHRASE`). Session
directories are pruned at `--retention-days` (default 30) — the local
stand-in for the phase-05 backup policy, which needs managed storage to be
real.

The script fails loudly rather than drilling pointlessly: a WAL archive that
is still empty after the post-backup switch means `archive_command` has been
failing, and the drill would prove nothing past the base backup.

### 2. Restore and time it (`scripts/db/restore.sh`)

Restores the base backup into a **clean** container, replays the archived
WAL, and measures from container start to read-write service — that number
is the local RTO. Default: replay to the end of the archive. With
`--target-time`: point-in-time recovery to that instant.

Consistency checks run against the restored database before the drill passes:

1. **Audit chain** — `sanvi-cli audit verify` (the real `VerifyAuditChain`
   use case, every hash link) against the restored database.
2. **Markers** — a full restore carries all three markers; a PITR carries
   the pre-target markers and must *not* carry the post-backup one.
3. **The outbox** — row counts (total and unpublished) match the recorded
   state, and no row carries a delivery stamp from after the restore point.
   Unpublished rows are the expected posture: the worker re-delivers on
   recovery, and handlers are idempotent by the dependency matrix
   (`docs/runbooks/dependency-matrix.md`).
4. **Payment records** — counts match the recorded state; external payment
   ids stay unique per tenant after replay.

### 3. Single-tenant restore (`sanvi-cli tenant-restore`)

The far more common real request: one tenant's data returns to a snapshot
state inside the live system, nobody else's rows move. See
[`docs/runbooks/dr-single-tenant-restore.md`](dr-single-tenant-restore.md).

## Rehearsal log

| Date | Scenario | Dataset | Restore (RTO, local) | Data-loss window | Audit chain | Machine |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-10-09 | full | seed 1 · 48,608 rows · 147 tenants | **2 s** (`benchmarks/dr/restore-full-20261009T050241Z.json`) | 0 s vs. quiesced source (replayed to last archived record) | verified, 28,720 entries | 20 cores, Windows/MINGW64, Docker |
| 2026-10-09 | pitr | seed 1 · 48,608 rows · 147 tenants | **1 s** (`benchmarks/dr/restore-pitr-20261009T050724Z.json`) | 11 s (by design: pinned to `2026-10-09T05:07:37.499907+00:00`) | verified, 28,719 entries — post-backup transaction correctly absent | 20 cores, Windows/MINGW64, Docker |

Both reports carry `rto_label: "local — docker on this host, not a
production number"`. The CI-resident form of the same drill — a true
restore-to-timestamp with the outbox/audit/payment checks and a full-replay
control — is `tests/recovery/tests/pitr.rs`, run by the integration job on
every PR.

## Rehearse it

```bash
just dr-restore --rehearse   # backup + full restore + checks + report
just dr-pitr --rehearse      # backup + restore pinned to the recorded target
just test-health-without-db  # the probes a restore-deploy cycle depends on
```

Portable form (no `just`):

```bash
session="$(./scripts/db/backup.sh | sed -n 's/^backup: session \(.*\) complete$/\1/p')"
./scripts/db/restore.sh --session "$session" --rehearse
# PITR: add --target-time "$(jq -r '.pitr_target' < ".drill/$session/markers.json")"
```

## Why the checks sit where they do

The audit chain verifies with the shipped verifier, not a script-side hash
reimplementation — a check that only agrees with itself proves nothing. The
outbox check reads the recorded pre/post row counts, so "the restore landed
exactly on the recorded state" is falsifiable, not asserted. The markers are
what make the PINNED restore falsifiable: a recovery that overshot the
target carries marker B and fails the drill.
