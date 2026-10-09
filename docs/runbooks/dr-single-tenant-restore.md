# Runbook — single-tenant restore

**Owner:** platform engineering · **Source task:** TASK-025 (FR-1119,
NFR-1107) · **Command:** `sanvi-cli tenant-restore` · **Drill:**
`just tenant-restore --dry-run`

The far more common real request is not "restore the platform" — it is "one
tenant lost data; put that tenant back". This restores one tenant's rows to
a snapshot state *inside the live system*, from a restored backup
addressable by DSN (typically the `scripts/db/restore.sh --keep` container).
Nobody else's rows move, and the operation audits itself.

## What the command guarantees

- **Scoped statements only.** Every statement is tenant-scoped or
  key-scoped; the plan is enumerated from the snapshot's own catalog (every
  base table with a `tenant_id` column in an app schema, plus the tenant's
  own row), so a table the plan does not name is a table the restore cannot
  touch.
- **Two passes, one transaction.** Children first, delete what the snapshot
  lacks; parents first, upsert the snapshot rows byte-exactly (`ON CONFLICT
  ... DO UPDATE` on the table's own primary key — composite keys included).
  The whole apply is one live transaction: it either lands entire or not at
  all.
- **Audited in the same transaction.** The `tenant.restore` audit entry
  carries the slug, snapshot database and per-table upserted/deleted counts.
  A restore that happened is always a restore that was audited.
- **`audit.entries` is never restored** — append-only by construction; a
  restore joins the history, it never rewrites one.
- **Refusals.** Same-server-and-database as live (`--from-dsn` must be a
  restored snapshot), tenant-id mismatch between live and snapshot for the
  slug, primary keys the plan cannot key on, foreign-key cycles the
  topological order cannot linearize.
- **`--dry-run` writes nothing** — the per-table snapshot/live counts, the
  plan, no audit entry.

The falsifiable isolation proof lives in
`apps/cli/tests/tenant_restore.rs`: a neighbour tenant's rows are asserted
byte-identical before and after by checksum, the target returns to exactly
the snapshot state, and the audit entry is asserted present. The drill below
is the same proof against production-shaped scale data.

## Run it

```bash
# 1. A restored snapshot, kept up (see docs/runbooks/dr-restore.md):
./scripts/db/restore.sh --session <sid> --keep          # e.g. port 55433

# 2. Plan against the live system — writes nothing:
just tenant-restore --tenant <slug> --from-dsn "postgres://...@127.0.0.1:55433/<db>" --dry-run

# 3. Apply (reads --tenant, applies the plan above in one transaction):
just tenant-restore --tenant <slug> --from-dsn "postgres://...@127.0.0.1:55433/<db>"
```

The live side is whatever `SANVI__DATABASE__*` points at — run under the
migrator (or superuser in the drill) credentials: the restore writes with
the platform scope, which the least-privilege application role cannot hold.

## Rehearsal log

| Date | Live / snapshot | Tenant | Result | Neighbour | Audit |
| --- | --- | --- | --- | --- | --- |
| 2026-10-09 | two restored drill servers (seed 1 scale data, 147 tenants); live deliberately drifted (display name, theme, an extra `processing` payment) | `scale-hot-ads` (the seed's hot tenant: 30 campaigns, 2,700 daily-metric rows across partitions, connections, themes) | dry-run plan printed, nothing written; apply restored the drifted rows to snapshot state in **1.1 s** across 60 tables (drift payment deleted, snapshot rows upserted byte-exactly) | checksum of neighbour `…005`'s payments/settings/tenant/entitlements/memberships rows **identical to snapshot** after the restore | `tenant.restore` entry present with per-table counts |

Local numbers, docker on this host. The monthly rehearsal schedule is
parked with the managed posture in
[`../release/needs-humans.md`](../release/needs-humans.md).

## After any real restore

1. Read the audit entry back (`just audit-list`) and attach it to the
   incident record.
2. If the tenant was restored because of bad writes, the outbox may still
   carry undelivered events from the bad window — triage with
   `just dlq-list` / the platform outbox before unblocking the tenant.
3. If the corruption reached caches (sessions, entitlements, themes), flush
   the affected Redis keys before telling the tenant it is over.
