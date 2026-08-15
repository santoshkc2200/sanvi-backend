# Phase 11 — Hardening, Observability & GA (backend)

**Target version:** 1.0.0
**Depends on:** phases 00–10
**Unlocks:** general availability and a public API contract we are willing to be held to

## Goal

Turn a feature-complete system into an operable product: measurable reliability targets, proven
recovery, a security posture validated by someone other than the authors, and performance that holds
under the load we expect plus headroom.

## Scope

**In**
- SLOs, error budgets, alerting that maps to user pain rather than to machine noise.
- Full tracing/metrics/log coverage review; runbook completeness; on-call readiness.
- Performance: profiling, query tuning, caching review, load and soak testing, capacity model.
- Resilience: dependency failure behaviour, backpressure, graceful degradation, chaos drills.
- Disaster recovery: backup/restore rehearsal, RPO/RTO validation, multi-AZ posture.
- Security: threat-model refresh, third-party penetration test, remediation, supply-chain review.
- API stabilisation: `v1` freeze, deprecation policy, rate limits and quotas per plan.
- Data lifecycle at scale: partitioning, archival, index review, vacuum/bloat management.

**Out**
- New product features. This phase adds no user-facing capability except quotas/limits UI surfaces.

## SLOs (initial, reviewed quarterly)

| Service journey | SLI | Target |
|---|---|---|
| Storefront read (SSR API) | availability; p95 latency | 99.9 % / ≤ 200 ms |
| Admin API writes | availability; p95 latency | 99.9 % / ≤ 400 ms |
| Checkout create (tenant) | success ratio | ≥ 99.5 % excluding provider-side declines |
| Webhook processing | time to entitlement effect | 99 % < 30 s |
| Conversion upload | success within 1 h | ≥ 99 % |
| Domain issuance | verified → live | 95 % < 10 min after DNS propagation |

Error budget policy: burning > 50 % of a monthly budget freezes feature merges for that context until
reliability work lands. Written down, agreed before GA, not negotiated during an incident.

## Work breakdown

### 1. Observability completion
- Trace coverage audit: every use case has a span; every external call is a child span with peer
  attributes; sampling is tail-based on errors and latency outliers.
- Structured log review: no PII (automated scanner in CI against a sample corpus), consistent fields,
  log levels sane at production volume.
- Dashboards per bounded context (RED + domain metrics) and per journey (funnel views).
- Alert rules: symptom-based (SLO burn rate, multi-window multi-burn) with paging vs ticket tiers.
- `docs/runbooks/` complete: one per alert, each with detection, triage, mitigation, and escalation;
  a runbook without an alert and an alert without a runbook both fail the audit.

### 2. Performance
- Profile the top 20 endpoints (`cargo flamegraph`, `tokio-console` for task stalls).
- Query review: `pg_stat_statements` top offenders, missing/duplicate indexes, sequential scans on
  tenant-scoped tables, RLS policy cost, `EXPLAIN (ANALYZE, BUFFERS)` assertions in perf tests.
- Connection pool sizing derived from measured concurrency, not guessed; statement timeouts set.
- Cache review: hit ratios, stampede protection (single-flight), negative caching, TTL sanity.
- N+1 elimination across read models; introduce dedicated query-side projections where needed.
- Response compression, HTTP keep-alive tuning, payload trimming, cursor pagination everywhere.
- Load test with `k6`: steady state at 3× projected peak, spike test, 24 h soak for leak detection.
- Capacity model: requests/tenant/day → CPU, memory, DB IOPS, with a documented scaling trigger.

### 3. Resilience
- Dependency matrix: for each of Postgres, Redis, Kratos, Keto, Stripe, DNS/ACME, ad platforms,
  object storage — define behaviour when slow, when erroring, and when down, then **test it**.
- Graceful degradation rules: Redis down → serve from Postgres with reduced throughput; Keto down →
  deny by default for writes, allow cached reads for ≤ 30 s; Stripe down → queue and retry, never
  double-charge; ad platforms down → queue conversions, never drop.
- Backpressure: bounded queues, load shedding with `503 + Retry-After` above a concurrency ceiling,
  per-tenant fairness so one tenant cannot starve others.
- Chaos drills: kill the primary DB, sever Redis, expire all sessions, replay a webhook flood — each
  run as a scheduled game day with findings tracked.
- Zero-downtime deploys verified: rolling restart under load with no dropped requests; migrations are
  expand/contract with a documented two-phase pattern.

### 4. Disaster recovery
- Backups: continuous WAL archiving + daily base backups; restore rehearsed **monthly** with a timed,
  documented result (a backup that has never been restored is a hypothesis).
- Targets: RPO ≤ 5 min, RTO ≤ 1 h; validated, not asserted.
- Point-in-time recovery drill including "restore one tenant's data" (the far more common request).
- Secret compromise drill: rotate every credential (DB, Redis, Stripe RAK, OAuth clients, signing
  keys) end to end and measure how long it takes.
- Documented degraded-mode operation and a public status page fed by real probes.

### 5. Security
- Threat-model refresh across all contexts; STRIDE per trust boundary, with the new attack surfaces
  from phases 08–10 (tenant domains, public tracking endpoint, ad tokens) explicitly covered.
- Third-party penetration test against staging with production-like data volumes; all findings
  triaged, high/critical fixed before GA, remainder scheduled with owners.
- Automated scanning in CI: SAST, dependency audit, container scan, IaC scan, secret scan — all
  failing the build, none advisory.
- Authorization review: enumerate every route and assert its permission, tenant scoping, and rate
  limit; a generated matrix is reviewed by two people.
- Multi-tenant isolation review: RLS matrix test re-run, plus manual attempts at cross-tenant access
  through every ID-bearing parameter (IDOR sweep).
- Abuse controls: per-tenant and per-IP quotas, signup abuse detection, tracking-endpoint abuse,
  domain-purchase fraud checks.
- Supply chain: SBOM published per release, dependency pinning, provenance attestation, review policy
  for new dependencies, `cargo vet`-style auditing for critical crates.

### 6. API stabilisation
- Freeze `/api/v1`: OpenAPI reviewed end to end for consistency (naming, pagination, errors, filters).
- Deprecation policy published: 6 months notice, `Deprecation`/`Sunset` headers, changelog entries.
- Per-plan rate limits and quotas enforced with clear `429` semantics (`Retry-After`,
  `RateLimit-*` headers) and documented burst behaviour.
- Public API docs generated from the spec with worked examples per journey and per locale.
- Backwards-compatibility test suite: recorded client requests from staging replayed against each
  release candidate.

### 7. Data lifecycle
- Partition and archival policies applied to `audit.entries`, `advertising.*`, `platform.outbox`,
  webhook event tables; archived data reachable through a documented restore path.
- Retention rules from phase 05 verified as actually running, with per-class reporting.
- Index bloat and vacuum strategy tuned; autovacuum settings reviewed per table size.
- A tenant offboarding job that exports and then purges a departing tenant completely, verified.

### 8. Compliance readiness
- Privacy evidence pack per jurisdiction (EU/UK, Japan, US states in scope): Art. 30 records, risk
  assessments, sub-processor list with roles and transfer mechanisms, retention rules as published,
  and the DSR/appeal metrics for the period — all generated from live configuration.
- A live-fire DSR drill per jurisdiction (access, erasure, US opt-out incl. GPC, and an appeal),
  timed against the jurisdiction's own clock, not the internal SLA.
- Breach tabletop that exercises the per-jurisdiction obligation calculator end to end, including a
  US multi-state incident and the encryption safe-harbour assessment.
- Notice and notice-at-collection reviewed by counsel against what the system actually does, with the
  diff produced by the generators rather than by reading.

## Testing

- Full regression suite green, including all phase acceptance criteria re-run as a release gate.
- Load, spike, and soak reports attached to the release; regressions vs the previous run block GA.
- DR drill report with measured RPO/RTO.
- Pen-test report with remediation evidence.
- Game-day findings closed or explicitly accepted with an owner and a date.

## Acceptance criteria

- [ ] Every SLO has an SLI, a dashboard, a burn-rate alert, and an owner.
- [ ] Every alert has a runbook; every runbook has been walked through by someone who did not write it.
- [ ] Load test at 3× projected peak holds all latency SLOs with < 70 % resource utilisation.
- [ ] A full restore from backup meets RPO/RTO in a timed drill, and a single-tenant restore works.
- [ ] No high or critical pen-test findings remain open.
- [ ] Cross-tenant isolation verified independently (RLS matrix + manual IDOR sweep + pen test).
- [ ] Zero-downtime deploy and rollback demonstrated under load.
- [ ] `/api/v1` frozen, documented, rate-limited, and covered by the compatibility replay suite.
- [ ] On-call rotation, escalation policy, and status page live before the GA announcement.
- [ ] A DSR drill in every jurisdiction in scope completes inside that jurisdiction's legal deadline,
      with the evidence pack generated rather than assembled by hand.

## Risks

| Risk | Mitigation |
|---|---|
| Hardening becomes an open-ended sink | Time-boxed with the acceptance list above as the exit criteria; anything else becomes post-GA backlog |
| Pen-test findings arrive late | Book the test at phase kickoff against a feature-frozen staging environment |
| Load testing against unrepresentative data | Generate production-shaped data: tenant size distribution, hot tenants, realistic metric volumes |
| SLOs set by aspiration rather than measurement | Set initial targets from four weeks of staging/beta data, then commit |
| Team fatigue before launch | Sequence: performance → resilience → security → DR, with each producing a shippable improvement |
