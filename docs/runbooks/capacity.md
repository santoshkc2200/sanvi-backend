# Capacity model — single host, lab-measured

**What this document is not:** a production capacity claim. Every constant
below was measured on one pinned Windows developer host (20 cores / 16 GB,
`{"cores":20,"mem_gb":16,"os":"Windows 11 26200","arch":"x86_64"}`) against
seed-1 scale data in the compose stack, release build 0.10.0, with the host
**shared with unrelated Docker stacks and the k6 generators themselves**.
It tells you what one lab host can tell you and stops there: the shape of
the scaling curves, the per-unit costs, and which gauge to watch — not how
many users production fits. The transition to a real capacity model needs
production traffic shape and a dedicated host; both are recorded in
[`docs/release/needs-humans.md`](../release/needs-humans.md).

**Alerts:** none answer this document directly — the resource triggers it
cites (`DbPoolAcquireWaitHigh`, `DbPoolAcquireFailures`,
`ApiLatencyBudgetBreached`) live in their own runbooks. This document is
the arithmetic those alerts fire into, which is why it is exempted from the
runbook xref gate with a reason in `scripts/runbook-xref-exempt.txt`.

## The measured constants (every one cites its artifact)

| constant | value | source |
|---|---|---|
| API CPU per request (mean, release build, storefront-weighted mix) | **1.5 core-ms/request** (api process 6.2 % of 20 cores at 840 req/s) | `benchmarks/load3x-2026-10-07.md` |
| API CPU per request (peak sample) | 3.1 core-ms (13.1 % of machine at the same load) | `benchmarks/load3x-2026-10-07.md` |
| API RSS, steady under load | 85–110 MB (cold start → 3× load) | `benchmarks/load3x-2026-10-07.md` |
| API RSS, post-burst plateau | **~460 MB** — Windows heap retention after a 10× burst; treat as the working set, not 110 | `benchmarks/spike10x-2026-10-07.md` |
| Worker RSS / CPU | ≤ 22.5 MB / ≤ 0.2 % of machine across every shape | `benchmarks/pool-sizing-2026-10-06.md`, `benchmarks/load3x-2026-10-07.md` |
| API DB pool demand at 3× projected peak | busy ≤ 5 of 10 connections, no acquire wait above the 50 ms bucket | `benchmarks/pool-sizing-2026-10-06.md` |
| Pool behaviour at the degradation knee | acquire waits reach ~1 s mean, idle → 1, RSS balloons to 769 MB — the collapse shape TASK-023 sheds above | `benchmarks/spike10x-2026-10-07.md` |
| DB statement mix under load | 13 distinct statements; dominant = `domains.live_hostnames` lookup at 0.022 ms mean, in front of the Redis caches | `benchmarks/baseline-2026-10-02.md` |
| Postgres CPU | ≤ 1 active query at every 2–5 s sample through 3× | `benchmarks/load3x-2026-10-07.md` |
| Redis memory at 147 tenants | 1.83 MB used (34 keys, peak 1.98 MB), no maxmemory set | `benchmarks/load3x-2026-10-07.md` run |
| Healthy throughput ceiling | ≥ 3× the harness's projected peak (690 iterations/s, 840 req/s) with every latency budget at 2.4–4.7 % of its budget | `benchmarks/load3x-2026-10-07.md` |
| Degradation onset | between 3× and 10×; not localisable on a shared host (two 10× runs failed through different bottlenecks: CPU in run A, pool in run B) | `benchmarks/spike10x-2026-10-07.md` |
| Transport cost floor | bodies < 1024 B are not compressed; 219 B problem bodies are the floor's justification | `benchmarks/transport-2026-10-06.md` |

## The mapping: requests/tenant/day → resources

Take `R` = requests/day for the whole platform and `T` = tenants. The lab
peak mix is **690 iterations/s ≈ 59.6 M iterations/day ≈ 72.6 M req/day**
(storefront/webhook iterations issue two requests); with the seed's
147 tenants that is ~494 k req/tenant/day — roughly 6× the storefront
traffic of a busy small tenant at peak, sustained for one day. Scale
arithmetically:

- **CPU:** `R × 1.5 core-ms` = core-seconds/day; divide by 86,400 for the
  mean core count. At the measured peak: 72.6 M × 1.5 core-ms = ~1,260
  core-seconds/day ≈ 1.2 cores mean, 2.6 cores peak — on 20 cores, the API
  saturates CPU at roughly **10–15× the projected peak**, but the shared
  host never let a 10× run verify that number cleanly
  ([spike10x](../../benchmarks/spike10x-2026-10-07.md)); treat 3× as the
  measured ceiling and everything above it as extrapolation.
- **Memory:** budget **512 MB per API process** (the 460 MB post-burst
  plateau plus headroom), plus ~20 MB per actix worker thread's
  allocator arenas at burst — the 769 MB transient at the knee shows what
  unshed queueing costs; TASK-023's load shedding is the bound on this
  number, not the allocator.
- **DB connections:** the API pool is capped at 10 (`[database]`, the
  measured 3× demand ≤ 5); the worker at 8 (2× its measured peak of 4).
  Production's api ceiling of 50 remains a prior guess the lab cannot
  measure — recorded as such in
  [`benchmarks/pool-sizing-2026-10-06.md`](../../benchmarks/pool-sizing-2026-10-06.md).
- **DB IOPS:** not measurable as a constraint in the lab — the working set
  (34 MB database, seed 1) fits in memory and every table sits at ~100 %
  heap/index hit. Production IOPS capacity is a needs-humans item; the
  query-side guard is `pg_stat_statements` (the dominant statement is
  0.022 ms and Redis-fronted).
- **Redis:** 1.83 MB at 147 tenants ⇒ ~12.5 kB/tenant for the hot keys
  (resolution + resolved themes + entitlements). 10,000 tenants ≈ 125 MB.
  `maxmemory` is unset in the lab; a production Redis needs a policy and a
  ceiling — recorded in needs-humans with the rest.

## Scaling triggers (the gauges to watch, per resource)

| resource | trigger | where it fires |
|---|---|---|
| DB pool | p95 acquire wait > 100 ms for 5 min | `DbPoolAcquireWaitHigh` (runbook: `db-pool-saturation.md`) |
| DB pool | any acquire failure in 5 min | `DbPoolAcquireFailures` (runbook: `db-pool-saturation.md`) |
| Latency | journey budget breach against `docs/slo/budgets.md` | `ApiLatencyBudgetBreached` (runbook: `latency-budget-breach.md`) |
| Memory (api) | RSS trending above the ~460 MB plateau | not alert-covered — a gauge on `/metrics` plus the soak procedure is the lab evidence; an RSS alert is future work when a production shape exists |
| Host CPU | sustained > 70 % | not alert-covered in the lab (host is shared); production placement owns this trigger |

## What one host cannot tell you

- Production connection budget, IOPS ceiling, Redis policy: deployment
  facts (`needs-humans.md`).
- Multi-tenant fairness at scale beyond the three seeded hot tenants:
  TASK-023 proves per-tenant fairness under load with the hot tenants; the
  long-tail production distribution will differ.
- The knee's exact position: measured only as "between 3× and 10×", on a
  host whose co-tenancy moves it run to run.
