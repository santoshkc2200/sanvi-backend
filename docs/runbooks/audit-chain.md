# Audit Chain Verification & Archival

The business audit trail (`audit.entries`) is append-only and
hash-chained. The app role has no UPDATE/DELETE grants and a BEFORE
trigger raises on either; every row's hash covers the previous row's
hash, so tampering is detectable by recomputation.

## Nightly verification

The worker verifies the chain on `scheduler.audit_verify_interval_secs`
(default: daily) and logs `audit chain verified (N entries)`. Manual run:

```bash
cargo run -p sanvi-cli -- audit verify     # fails non-zero on any mismatch
cargo run -p sanvi-cli -- audit list --limit 50 [--tenant-id <uuid>] [--action access.entitlement_granted]
```

## Incident: verification failed

1. **Treat as a security incident.** The chain proving "who did what" is
   compromised. Engage the on-call security engineer.
2. Freeze changes: stop the API/worker processes that write audit rows
   until the blast radius is understood.
3. Pull the row identified in the failure log (`entry_id`, `position`)
   with `audit list`; determine whether the DB was modified through a
   privileged path (the trigger + grants make accidental modification
   nearly impossible — a mismatch means credentials or backup tampering).
4. Restore the affected segment from the last verified backup, or
   re-export from cold storage.
5. Rotate every database credential, especially `sanvi_migrator` and any
   session that had UPDATE access.
6. Write the post-incident report; the metric
   `audit_chain_verify_failures` and the error-level log line are the
   alerting triggers.

## Retention & archival (≥ 1 year)

Rows are never deletable by the application role, which guarantees the
1-year retention floor by construction (`audit.retention_months`).
Archival to cold storage is a maintenance procedure under the
`sanvi_migrator` role — run from a scheduled job on a replica:

```sql
-- Export rows older than 90 days, chunked by seq, to cold storage.
COPY (
  SELECT * FROM audit.entries
  WHERE occurred_at < now() - interval '90 days'
) TO '/exports/audit/audit_<date>.csv' WITH (FORMAT csv);
```

The chain links every row, so pruning mid-chain breaks verification.
Deleting archived rows requires dropping `entries_no_update` first (a
change-control window, migrator role only), and is deliberately **not**
scripted here: prefer growing disk + cold export until phase-05
compliance work adds partitioned cold storage. The 1-year floor is the
hard requirement either way.
