# Runbook — API latency budget breach

**Alerts:** `ApiLatencyBudgetBreached` — `histogram_quantile(0.95, sum by (le) (rate(http_server_duration_seconds_bucket[5m]))) > 0.4` for 10m
(see the [alerts registry](../../observability/alerts/rules.json) and
[docs/slo/budgets.md](../slo/budgets.md)).

The p95 of served requests is above the tightest journey budget for ten
minutes. Nothing is down — the API answers — but the storefront feels slow,
and the budgets in `docs/slo/budgets.md` are the line where "slow" becomes
an incident instead of a mood.

## Detection

The alert fires from the Prometheus-side expression above. Confirm it on
the live system:

```bash
# RED view per route: who is slow, not just the aggregate
curl -s http://localhost:8081/metrics | grep http_server_duration
```

Compare against the stored baseline for the same shape:
`benchmarks/storefront_read/baseline-2026-09-18.json` (p95 5.16 ms lab) —
a breach at 400 ms means a regression of two orders of magnitude, or load
far past the lab profile. `just perf-compare base candidate` decides
which: re-run `just perf <tag>` against the current build and diff.

## Triage

1. **Which route?** `http_server_duration_seconds` carries `route` and
   `context` labels — the top offender names the owning context.
2. **Which query?** Open the trace for a slow request: every use case
   carries a `<context>.<usecase>` span (TASK-020), errors and slow spans
   are tail-sampled, and the child spans name the repository call. Find
   the span whose duration dominates. The trace id is in the error body's
   `trace_id` field or the `traceparent` response header of any affected
   request.
3. **Cache or database?** A collapsed Redis cache shows as a storm of
   `tenant_resolution_*` and theme-resolution reads; a query problem shows
   as one statement with `calls` flat and `mean` spiking in
   `pg_stat_statements`.

```bash
docker exec sanvi-postgres-1 psql -U postgres -c \
  "SELECT calls, round(mean_exec_time::numeric, 2) AS mean_ms, left(query, 80) \
   FROM pg_stat_statements ORDER BY mean_exec_time DESC LIMIT 10"
```

## Mitigation

- **Regression from a deploy:** roll the deploy back — the latency budget
  gate exists so that this is the cheap answer, and TASK-021 owns the fix
  after.
- **Cache collapse:** the resolution caches repopulate on demand; shedding
  load (concurrency limit) while they warm is acceptable. Restarting the
  API re-warms nothing faster than traffic does.
- **Runaway query:** cancel it (`SELECT pg_cancel_backend(pid)`) only with
  the owning tenant identified — the statement may be mid-transaction.

## Escalation

- p95 above budget for 30 minutes after mitigation, or any p99 above
  5 s: page the on-call engineer.
- Two breaches from two different causes in one week: open a TASK-021
  capacity work item with both traces attached; do not keep tuning
  one-off.
