# Lab latency budgets — the six journeys (TASK-020 step 6)

**Lab, not SLOs.** These are regression guards measured by the TASK-019
harness (`just perf`) against seed-1 scale data on the pinned machine
profile below. There is no traffic, so there is no error budget to burn;
these numbers become SLOs with burn-rate alerting only when there are
users — that transition, and everything a real SLO needs beyond a
command, is recorded in
[`docs/release/needs-humans.md`](../release/needs-humans.md).

A budget breach is a `just perf-compare` failure against the stored
baseline, or the `ApiLatencyBudgetBreached` alert, not a promise to a
customer.

- **Measured:** 2026-09-18, seed 1 (`checksum 22e2bac8…`, 48,608 rows —
  the post-erratum re-seed), API debug build 0.10.0, ten workers
- **Machine profile:** `{"cores":10,"mem_gb":16,"os":"Darwin 27.0.0","arch":"arm64"}`
- **Harness:** five k6 shapes, 60 s each at the offered rates
  (storefront_read 50 rps, admin_write 20, checkout_create 10,
  webhook_flood 100, conversion_upload 50); rate limiting off;
  `features.advertising` on with dev-only signing secrets
- **Artifacts:** `benchmarks/<scenario>/baseline-2026-09-18.json`
  (`stats_version: 2`) — each journey's measured value below is read from
  the committed artifact of the same name
- **Exact query:** the p95 of `http_req_duration` as rendered by k6's
  `handleSummary` into the artifact's `http_req_duration_p95_ms`
  (thresholds in `tests/load/lib.js`); the alert-side query lives in the
  `ApiLatencyBudgetBreached` rule in `observability/alerts/rules.json`

## The six journeys

| # | Journey | Metric | Budget | Measured (this artifact) | Verdict |
| --- | --- | --- | --- | --- | --- |
| 1 | Storefront read (SSR API) | p95 latency | ≤ 200 ms | **5.16 ms** (`storefront_read`) | 2.6 % of budget |
| 2 | Admin API writes | p95 latency | ≤ 400 ms | **7.89 ms** (`admin_write`) | 2.0 % of budget |
| 3 | Checkout create (tenant) | success ratio under load, excl. provider declines | ≥ 99.5 % | **100 %** (`checkout_create`, 602/602; lab stops at the 401 session boundary — see below) | passes, at the lab boundary |
| 4 | Webhook processing | time to entitlement effect, p99 | < 30 s | **boundary only**: receiver p99 380.8 ms (`webhook_flood`); effect time needs Stripe locally | front door measured, effect not measurable in lab |
| 5 | Conversion upload | success ratio per sweep | ≥ 99 % | **boundary only**: capture beacon p95 0.694 ms, 0 failed (`conversion_upload`); the sweep is worker-side (`conversion_upload_total`) | front door measured, sweep ratio from worker metrics |
| 6 | Domain issuance | verified → live, p95 after DNS propagation | < 10 min | **not measured** — the harness ships five shapes and none drives issuance end to end (sandbox registrar) | gap, see below |

## What the lab cannot measure (and what replaces it)

Three of the six journeys cannot complete end to end without live
providers, and one has no harness shape at all. The budgets doc records
the measured *proxy* rather than pretending:

- **Webhook entitlement effect (4):** the k6 shape drives unsigned events
  to the signature boundary. The effect half of the journey (outbox →
  entitlement grant) is covered by phase-09 integration tests, not by a
  wall-clock measurement; the first live Stripe rehearsal must record the
  end-to-end p99 here (TASK-025's drill is the natural host).
- **Conversion sweep success ratio (5):** the harness measures capture;
  the sweep's ratio is `conversion_upload_total{result="succeeded"} /
  conversion_upload_total` on `/metrics`, alert-covered by
  `ConversionUploadFailureSpike`. A worker-side sweep under load is
  TASK-021's soak shape.
- **Domain issuance (6):** needs the sandbox registrar driven end to end
  under load — a harness shape that does not exist yet. Recorded as the
  one journey with no measured value; adding the sixth shape is a
  TASK-021 follow-up, not a silent omission.

## Lab-vs-production rule

Every number above is labelled lab and pinned to one machine profile;
`just perf-compare` refuses artifacts whose profiles differ. None of
these numbers is a capacity claim — the plan's risk register says exactly
that, and the pool-saturation section of the baseline (not yet
instrumented, TASK-021's first action) is the reminder that the lab API
runs with a ten-connection pool on a laptop.
