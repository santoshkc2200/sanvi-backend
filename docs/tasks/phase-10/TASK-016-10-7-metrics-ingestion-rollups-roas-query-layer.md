# TASK-016: 10.7 Metrics ingestion, rollups & ROAS query layer

**Phase:** 10
**Status:** todo
**Requirement(s):** FR-1012, NFR-1005, NFR-1006, NFR-1009
**Depends on:** TASK-012, TASK-015
**Created:** 2026-08-20

**Slice:** 10.7 — Metrics ingestion, rollups & ROAS dashboard (backend half)
**Prerelease:** `v0.11.0-alpha.8` · **Flag:** `advertising.dashboard`

## Context

The numbers the phase exists for: spend, impressions, clicks, conversions, revenue, and ROAS for a
date range, matching the platform's own reporting within its restatement window — and honest about
where every number came from.

**The three rules:**

1. **A single fetch is never enough.** Ad platforms restate conversions for days after the fact.
   Ingestion is hourly for today plus a rolling backfill of the trailing 30 days, and re-ingesting a
   day **replaces** that day rather than adding to it.
2. **Two ROAS numbers, both labelled, never merged.** `sanvi_revenue / spend` and
   `platform_conversion_value / spend` are returned side by side. There is no blended field in the API
   and no blended column in an export. A merged attribution number is a confident lie, and once one
   ships it is impossible to withdraw.
3. **Timezone and currency are stated, never assumed.** Every ad account has its own timezone and its
   own currency. "Yesterday" differs per account; sums across accounts in different currencies do not
   exist. FX conversion is display-only, explicit, and carries its rate date.

TASK-015 supplies the conversions column; TASK-012 supplies the campaign grain.

## What to do

- [ ] **Contract** — `GET /ads/metrics?from&to&group_by&platform&campaign_id`,
      `GET /ads/metrics/summary?from&to&compare_to`, `GET /ads/metrics/export?format=csv`,
      `GET /ads/metrics/freshness`, all `advertising.metrics.read`. Every metrics row carries its
      source currency, the ad account timezone, and a `restating: bool` marker for days still inside
      the platform's restatement window. `freshness` returns per-connection last-ingested-at and lag —
      what the UI's "still updating" and "sync failed" states render from, not a guess based on the
      clock. Date ranges are bounded by a hard maximum span and served from rollups, never from the raw
      partitions.
- [ ] **Ingestion scheduler** — per connection: hourly for the current day, daily backfill across the
      trailing 30 days, the window configurable per platform because their restatement behaviour
      differs. Staggered per tenant so every connection does not fire on the hour.
- [ ] **Google Ads & Meta metrics adapters** — reporting queries at the campaign/ad-group/ad grain,
      normalised into `MetricPoint`. Spend arrives in the ad account currency and is stored in minor
      units with that currency; nothing is converted on the way in.
- [ ] **Idempotent restatement** — ingestion is an upsert keyed by `(tenant, platform, campaign, date)`.
      Re-ingesting a day overwrites it. This is the single most important correctness property in the
      task: an append here silently doubles history and no downstream test will catch it.
- [ ] **Rollups** — `metrics_daily` → campaign/platform/tenant aggregates maintained incrementally on
      ingest. Partition maintenance from TASK-009 continues; rollup rebuilds are idempotent and
      re-runnable.
- [ ] **Revenue join** — phase-09 payment records joined by `order_ref` where the conversion carries
      one, and by date bucket otherwise, into `sanvi_revenue_minor`. Attribution to a campaign runs
      through the conversion's click ids; where attribution is impossible the revenue is reported at
      platform or tenant level rather than distributed by a guess.
- [ ] **FX** — daily rate snapshots via `FxRatePort`, stored with their date. Conversion happens in the
      query layer for display only; **no converted value is ever persisted**.
- [ ] **ROAS query layer** — both ratios computed at query time, both returned, each with its
      numerator's provenance. Zero spend yields null, not infinity, and the contract says so.
- [ ] **Export** — CSV generation streamed and permission-gated, carrying the same labelled columns as
      the API. No blended column, no PII beyond what the tenant already holds.
- [ ] **Observability** — `ads_sync_duration{platform}`, `ads_sync_failures_total{platform,code}`,
      `metrics_ingestion_lag_hours`, with an alert on lag > 6 h.

## Acceptance criteria

- [ ] Metrics for a date range match the platform's own reporting within the documented restatement
      window, verified against a test account.
- [ ] Re-ingesting a day is provably idempotent at the `metrics_daily` grain **and at every rollup
      level**.
- [ ] Both attribution sources are returned and labelled; a grep gate proves no blended ROAS field
      exists in the API or the export.
- [ ] ROAS arithmetic is correct across currencies including JPY, with zero spend (null, not `∞`), zero
      revenue, and both zero.
- [ ] A conversion with an `order_ref` attributes to its campaign; one without is reported at platform
      level and never distributed across campaigns.
- [ ] No converted value is persisted anywhere (asserted over the schema and the ingest path); a
      displayed conversion carries its rate date.
- [ ] A purchase near midnight lands in the correct day for the **ad account's** timezone, not the
      server's or the tenant's.
- [ ] A stalled connection produces a lag value through `freshness` rather than stale numbers presented
      as current.
- [ ] An over-long date range is rejected with a clear message rather than timing out.
- [ ] Load: 500 campaigns × 90 days ingested within the scheduling window.

## Verification

```bash
just fmt
just clippy
just layering
just ads-invariants
cargo test -p sanvi-advertising
just test
just openapi
```

## Out of scope

Budget caps, threshold evaluation, and auto-pause (TASK-017); the dashboard UI, chart primitives, and
the attribution explainer (frontend TASK-016).

## Files likely touched

- `api/advertising.yaml` (metrics, summary, export, freshness)
- `crates/contexts/advertising/src/application/metrics/**`
- `crates/contexts/advertising/src/adapters/{google_ads,meta}/reporting.rs`
- `crates/contexts/advertising/src/adapters/{repos,metrics}.rs` (rollups, FX port)
- `apps/worker/**` (ingestion scheduler, backfill, rollup maintenance)
- `migrations/` (rollup tables/indexes if not already created in TASK-009)

## Notes / gotchas

- Rollback: `advertising.dashboard` off → metrics endpoints return
  `503 advertising/platform-unavailable`. **Keep the ingestion scheduler running** — stopping it opens
  a gap a later backfill can only partly close, because platform reporting windows expire.
- Ingesting into an append path "just for now" is the failure mode this task exists to prevent. Write
  the idempotency test first.
- Restatement is not an edge case; days inside the window are the normal case for the most-viewed part
  of the dashboard.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-016 done --note "<PR or commit>"`
