# Vacuum tuning & index pruning — TASK-028 step 5 (FR-1130)

**Lab, not production.** Measured on the dev test Postgres 18 container
(tasks run on it), 2026-10-10, synthetic volumes (60k-row tables). The
per-table settings are derived from table *shape* (append-mostly /
update-mostly / tiny lookup), and the production re-measure on the scale
seed is the follow-up below.

## What changed (`migrations/20261010000700_vacuum_tuning_index_pruning.sql`)

| Table class | Tables | Setting | Rationale |
|---|---|---|---|
| Hot append-mostly | `audit.entries`, `platform.outbox`, `billing.webhook_events`, `payments.webhook_events`, `advertising.conversion_events` | `vacuum_scale_factor 0.02`, `analyze_scale_factor 0.01`, `vacuum_insert_scale_factor 0.02` on every child | these only ever grow; insert-scale tuning is what keeps the index map filled (the explain review's "map filled" window is exactly autovacuum's lag after a bulk load) |
| Update-mostly money | `billing.invoices`, `payments.payments`, `payments.checkouts` | `vacuum_scale_factor 0.05`, `vacuum_threshold 1000` | status transitions rewrite rows; a low *fixed* threshold keeps pace as the tables grow instead of a percentage of a growing number |
| Tiny per-tenant lookups | `subscriptions`, `tax_settings`, `provider_connections`, `advertising.connections`, `tracking_settings`, `tenant_secret_keys`, `guardrail_suspensions` | `vacuum_threshold 100000` | hundreds of tables that never churn; autovacuum was visiting them for nothing |

Partitioned parents cannot carry storage parameters (Postgres rejects
`ALTER TABLE … SET` on relkind `p`), so the per-parent values live in
`platform.partitioned_tables.reloptions` — every child the registry
creates is born tuned — and the migration applies them to the existing
children by name.

## Index pruning (measured against the plan inventory)

| Index | Why it goes |
|---|---|
| `payments.payments_tenant_idx (tenant_id)` | strict prefix of `payments_tenant_created_id_idx (tenant_id, created_at DESC, id DESC)` (and of the tenant_currency/payout_status composites); no pinned plan reads it |
| `payments.disputes_created_idx (tenant_id, created_at DESC, id DESC)` | byte-for-byte duplicate of `disputes_tenant_idx` after the 20261006 realignment; doubles every dispute write's index maintenance |

`apps/api/tests/perf/explain_assertions.rs` pins `disputes_tenant_idx`,
which is the survivor.

## Measured (lab, 60k rows, payments.payments)

| Metric | Value |
|---|---|
| 60k rows, total relation size (before churn) | 16 MB |
| 10% status-transition churn (6k rewrites) | +1 MB dead-tuple footprint |
| tuned vacuum trigger (0.05·60k + 1000) | fires at **4,000** dead tuples |
| default trigger (0.20·60k + 50) | would wait for **12,050** |
| after `VACUUM (ANALYZE)` | 0 dead tuples, size 17 MB (pages reclaimed for reuse; table not rewritten) |
| `audit.entries` churn probe | deletes refused by the append-only trigger, as designed — its tuning is insert-driven (`vacuum_insert_scale_factor`), not churn-driven |

The before/after table *sizes* barely move in a lab: vacuum's win is the
dead-tuple ceiling between passes (3× lower here), which is what keeps
the hot tenant's index-only scans index-only at production volume
(`benchmarks/explain-2026-10-06.md`, "map filled").

## Production re-measure (the committed follow-up)

On the scale seed (`just seed-scale`, then the hot tenant's audit child):

```sql
SELECT relname, n_live_tup, n_dead_tup, last_autovacuum
FROM pg_stat_user_tables
WHERE relname LIKE 'entries\_%' OR relname LIKE 'conversion\_events\_%'
ORDER BY n_dead_tup DESC;
```

Record live/dead and `pg_total_relation_size` before the migration and
one autovacuum cycle after it in this file's rehearsal log.
