# Runbook — archival & restore (TASK-028, FR-1129)

**Owner:** platform
**Source task:** [TASK-028 · 11.9](../tasks/phase-11/TASK-028-11-9-data-lifecycle-at-scale-archival-tenant-offbo.md) (FR-1129)
**Commands:** `just archive`, `just archive-restore`
**Drill command:** `just archival-roundtrip`

Archival moves aged ranges of the unbounded tables (audit.entries, the
advertising histories, and — as their retention classes age out — the
money histories) to encrypted cold storage, so the hot database stays
hot. **Archival without a rehearsed restore is deletion with extra
steps**: every archive carries the sha256 of its canonical row
serialization, and the restore proves the rows came back identical —
count, ordering, checksum — or rolls back.

## What the command guarantees

- A read over an archived range answers **409 `archived-range`** (audit
  console), never a silent partial. After a restore, the same read
  answers normally.
- Archives are per `(table, range, tenant)`: a neighbour tenant's rows
  in the same range never move, and a departing tenant's offboarding
  purges exactly its own archives.
- Cold objects are sealed with the phase-05 envelope scheme's passphrase
  layer (PBKDF2-HMAC-SHA256 + AES-256-GCM), under a passphrase derived
  from the configured KEK. No KEK configured → the command refuses
  (fail closed).
- The move is one transaction: serialize, seal, write the cold object,
  record `platform.archives`, delete the rows. A crash leaves either the
  table intact or a complete archive.
- `audit.entries`'s append-only trigger is lifted for the delete and
  re-armed inside the same transaction — archival *is* that table's
  documented removal path, and only the migrator (table owner) can lift
  it.
- Re-archiving an overlapping range is refused loudly
  (`ArchivalError::RangeArchived`), pointing at the existing archive id.
- `--dry-run` prints the row counts and writes nothing.

## Run it

```bash
# 1. Rehearse: the round-trip gate (archive → archived-range answer →
#    restore → identical rows/ordering/checksums).
just archival-roundtrip

# 2. Plan the move (row counts only, nothing written).
just archive -- --table audit.entries --tenant acme \
  --from 2025-01-01T00:00:00Z --until 2025-04-01T00:00:00Z --dry-run

# 3. Move it. Connects as the migrator (owner) — pass the credentials the
#    same way `just migrate` does.
just archive -- --table audit.entries --tenant acme \
  --from 2025-01-01T00:00:00Z --until 2025-04-01T00:00:00Z \
  --user sanvi_migrator --password "$MIGRATOR_PASSWORD"

# 4. Read the evidence row (status, checksum, bytes, path).
psql -c "SELECT id, schema_name, table_name, tenant_id, range_start, range_end,
        row_count, checksum, bytes, status FROM platform.archives ORDER BY created_at DESC LIMIT 5;"

# 5. When the range is needed again — rehearsed, verified restore.
just archive-restore -- --id <archive-id> \
  --user sanvi_migrator --password "$MIGRATOR_PASSWORD"
```

`--tenant` accepts a slug, a decimal id, or `platform` (the rows with no
tenant — platform-actor audit entries). Archives are monthly-friendly
but any bounded range is legal.

## Scheduled archival (worker)

With `[features] archival = true`, the worker's `platform.archival_sweep`
job archives rows of the retention classes registered `action =
'archive'` (invoices, transaction records) once they age past their
registered window — monthly steps per tenant, boundary tracked by the
archives themselves, every pass recorded in `privacy.retention_runs`.
The retention report (`just retention-report`) shows each class running
inside its window.

## What is deliberately not here

- **Cold object storage** is parked: the mover writes to a local
  encrypted volume behind the `ColdStorage` port, so the storage adapter
  is a swap when there is somewhere to swap to.
- **Database restore from backup** is a different operation with a
  different runbook ([dr-restore](./dr-restore.md)).
- **Tenant offboarding** (export-then-purge, including purging this
  tenant's archives) has its own runbook
  ([offboarding](./offboarding.md)).

## Rehearsal log

| Date | Live/snapshot | Table | Range | Rows | Duration | Checksum verified |
|---|---|---|---|---|---|---|
| _(record the first run on scale data here — duration and IO impact are the task's deliverable)_ | | | | | | |
