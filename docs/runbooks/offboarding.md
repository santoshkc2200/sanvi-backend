# Runbook — tenant offboarding: export-then-purge (TASK-028, FR-1131)

**Owner:** platform
**Source task:** [TASK-028 · 11.9](../tasks/phase-11/TASK-028-11-9-data-lifecycle-at-scale-archival-tenant-offbo.md) (FR-1131)
**Commands:** `just offboard`, `POST /api/v1/platform/tenants/{id}/offboarding`
**Drill command:** `cargo nextest run -p sanvi-cli --test offboard`

A departing tenant is exported completely and purged completely, with
evidence. **The purge is deliberately not reversible.** The two-person
confirmation this deployment cannot have is replaced by three things:
a dry-run, a verified export as a hard precondition, and a typed
confirmation naming the tenant. The purge step is unreachable — in the
record's own state machine — before the export verifies.

## The flow

```
POST /offboarding (idempotent by key)      tenancy.offboarding_requests: pending
just offboard --stage export               → exporting → exported (+ path, checksum)
just offboard --stage purge --confirm ...  → purging  → purged (+ purge report)
```

The API records the request (operators need the permission gate and the
audit trail); the CLI executes. Both create the record through the same
idempotent use case — a repeated key returns the existing request.

## What the command guarantees

- `--dry-run` (any stage) prints the plan and writes nothing.
- The export contains every context's data — asserted per context, the
  empty ones included as explicit empty bundles — packaged by the
  phase-05 export machinery (`data.json`/`data.csv`/`data.html` + a
  per-file-checksum `manifest.json`, ZipPackager) and sealed with the
  envelope scheme's passphrase encryption. The one-time passphrase is
  printed once, to stderr, for out-of-band conveyance; it is never
  stored.
- The export is *verified* before the record moves on: the sealed file
  is read back, decrypted, and every manifest checksum re-checked. At
  purge time the whole-file checksum is re-verified again.
- The purge refuses to start unless `--confirm` is exactly
  `offboard <slug>` — the typed confirmation naming the tenant.
- The purge is catalog-driven across every tenant-keyed table (children
  before parents), **keeps the lawfully retained classes** with their
  legal basis and retention end date in the report, sweeps the tenant's
  Redis cache keys and its cold archives, and finishes with:
  - the **residual scan** — every table and partition child *by name*
    plus the archive registry; anything unexpected fails loudly (the
    record goes to `failed`, the exit code is non-zero), never a quiet
    completion;
  - the **neighbour proof** — `--neighbour <slug>` checksums another
    tenant's entire content before and after; they must be
    byte-identical.
- The tenant row becomes a **tombstone** (`display_name = 'purged
  tenant'`, `purge_requested_at` stamped), not a DELETE: every
  tenant-keyed table cascades from it, so deleting it would destroy the
  rows the report just promised to keep — and the tombstone pins the
  slug and id against reuse.
- Re-running the purge answers the stored report (`idempotent_replay`)
  and deletes nothing.
- The whole operation is audited (`tenant.offboard.*` in
  `audit.entries`).

## What is lawfully retained (and why the list must not be empty)

| Class | Reason | Retention end |
|---|---|---|
| `invoice`, `transaction_record` | statutory windows from `privacy.retention_rules` (3650d); rows past the window are deleted | per-row `created_at + period` |
| `audit_log` | the hash-chained audit trail is append-only by construction; the tenant's entries are exported first | archival is the removal path |
| the tombstone + the offboarding record | FK anchor for retained rows; the evidence that the purge happened | with the last retained row |

**A tenant with completed payments cannot produce an empty retained
list** — that is a bug, not a clean purge, and the test asserts it.

## Run it

```bash
# 0. The lifecycle gate: offboarding starts from suspended or archived.
#    (suspend → archive through the normal lifecycle first.)

# 1. Record the request (or let the export stage do it — same use case).
curl -X POST .../api/v1/platform/tenants/{id}/offboarding \
  -d '{"idempotency_key": "2026-10-acme-exit"}'

# 2. Plan. Writes nothing.
just offboard -- --tenant acme --key 2026-10-acme-exit --dry-run

# 3. Export (prints the one-time passphrase to stderr — convey it now).
just offboard -- --tenant acme --key 2026-10-acme-exit --stage export

# 4. Purge. The confirmation names the tenant; the neighbour is proven
#    byte-identical; the residual scan must come back clean.
just offboard -- --tenant acme --key 2026-10-acme-exit --stage purge \
  --confirm "offboard acme" --neighbour other-tenant

# 5. Re-verify any time (the slug is gone; the report carries the id).
just offboard -- --tenant <decimal-id> --stage scan
```

The export artifact lives under `[privacy] export_dir`/`offboarding/`,
sealed; the recorded checksum is on the offboarding record.

## After a failed purge

A purge that fails mid-flight (residual scan, neighbour check) has
already deleted rows: the record says `failed` with the reason. Recover
by fixing the cause (usually a smuggled row) and re-running — the
idempotent replay re-scans and completes the record. There is no undo;
that is why the export came first.

## The human steps that remain (see docs/release/needs-humans.md)

Two-person confirmation on the purge is parked: a second operator. The
dry-run, the verified-export gate and the typed confirmation stand in
until there is one.
