# TASK-009: 10.0 Contract, schema & foundations

**Phase:** 10
**Status:** todo
**Requirement(s):** FR-1001, NFR-1002, NFR-1003, NFR-1004, NFR-1005, NFR-1007
**Depends on:** phase 03, 05, 08, 09
**Created:** 2026-08-20

**Slice:** 10.0 — Contract, schema & foundations
**Prerelease:** `v0.11.0-alpha.1` · **Flag:** `advertising.enabled` (off)

## Context

The skeleton and the guardrails, before any ad-platform call exists. At the end of this task the
`advertising` context compiles, owns a partitioned schema with RLS, is reachable through an
entitlement-gated route that returns an empty platform list, and the CI gates protecting the phase
invariant — *no identifier leaves Sanvi without a live directive that permits it* — already fail on
violations.

Nothing user-visible ships, and that is the point: the expensive mistakes in an advertising context
are raw PII on the wire and an unbounded metrics table, and both are made in the first week.

## What to do

The contract is merged **first** — `api/advertising.yaml`, registered in `apps/api/src/openapi.rs`
alongside the existing twelve contexts — then the code.

- [ ] **Contract** — new `api/advertising.yaml` with one endpoint,
      `GET /api/v1/tenant/ads/platforms` (`advertising.read` + entitlement), returning
      `{ "platforms": [] }` until TASK-010. Also fixed here because every later slice depends on the
      shapes being stable:
      - the `advertising/*` problem-details namespace: `platform-unavailable`,
        `connection-not-active`, `capability-unsupported`, `token-expired`, `directive-suppressed`,
        `budget-cap-reached`;
      - money as minor units + ISO-4217 code, **always paired with the currency of the ad account it
        came from**; converted values are never stored, only rendered, and a rendered conversion
        carries its FX rate date. JPY examples in the spec now;
      - `MetricPoint` and the rollup grain (`tenant/platform/campaign/date`), so TASK-016's chart
        primitives can be built against fixtures from day one;
      - `Idempotency-Key` documented on every mutating advertising endpoint — every one of them can
        spend money.
- [ ] **Crate scaffolding** — `crates/contexts/advertising/` following the `domains` and `payments`
      layout: `src/{lib,config}.rs`, `src/domain/`, `src/application/`,
      `src/adapters/{http,repos,events,metrics,stubs}.rs`. Wired into the workspace `Cargo.toml`,
      `apps/api` and `apps/worker`.
- [ ] **Migration** — `migrations/20260824000000_advertising_core.sql` (+ `.down.sql`), next in
      sequence after `20260823000000_tenant_payments`. Creates the `advertising` schema and the tables
      from the implementation plan (`connections`, `campaigns`, `ad_groups`, `ads`, `creatives`,
      `conversion_events`, `metrics_daily`).
      - `conversion_events` is `PARTITION BY RANGE (occurred_at)`, `metrics_daily` by `(date)`, both
        monthly, **with the partition-creation job scheduled in the same change**. A partitioned table
        with no partition-maker is a production incident on the first of the month.
      - Money columns are `bigint` minor units plus a `text` currency column, never `numeric`.
- [ ] **RLS** — tenant isolation on every table per the phase-01 pattern, including both partitioned
      parents. Policy inheritance to child partitions must be verified explicitly; this is the one
      place the phase-01 pattern does not apply unchanged.
- [ ] **Retention** — register `conversion_events` and `metrics_daily` with the phase-05 retention
      framework now, with their windows, before either table has a row in it.
- [ ] **Permissions** — register `advertising.read`, `advertising.connect`,
      `advertising.campaign.read`, `advertising.campaign.write`, `advertising.metrics.read`,
      `advertising.budget.manage` in the `access` seed and add them to tenant-owner and tenant-admin
      roles. `advertising.connect` and `advertising.budget.manage` are deliberately **not** in the
      default staff role — both are money.
- [ ] **Entitlements** — `advertising.google_ads` and `advertising.meta` in the `platform-admin`
      catalog, granted per plan. The route returns `403 entitlement-required`, not 404, so the
      frontend can show an upgrade prompt rather than a dead end.
- [ ] **Token envelope** — `token_ref` resolution through the phase-05 per-tenant DEK scheme, with a
      `SecretRef` newtype whose `Debug`/`Display`/`Serialize` impls redact. Access to the decrypt path
      is audited from the first call, not retrofitted.
- [ ] **Config & secrets** — `configs/` entries for the Google Ads developer token, both OAuth client
      id/secret pairs, the tracking site-key signing secret, and pinned API versions per platform.
      Distinct per environment.
- [ ] **Feature flag** — `advertising.enabled`, default off, checked at the HTTP boundary.
- [ ] **CI gates** (the important part of this task) — `scripts/check-ads-invariants.sh` with a
      `--self-test` mode proving each gate catches a deliberate violation, wired into `just ci`:
      - a type-level gate: the `AdPlatformPort` upload signature accepts only `HashedIds`, and a grep
        gate fails on any raw `email`/`phone` field name inside this context's `src/adapters/`;
      - a test asserting `SecretRef` cannot be logged — `format!("{:?}")` yields the redaction marker
        and a tracing-subscriber capture over the OAuth path contains no token substring;
      - a gate asserting every `ConversionSinkPort` implementation is constructed behind the directive
        check, so an upload path with no `ConsentSnapshot` argument does not compile;
      - a build-output scan asserting no OAuth client secret or developer token appears in any
        frontend bundle.

## Acceptance criteria

- [ ] `api/advertising.yaml` is served and `just openapi` passes; handler/spec divergence fails CI.
- [ ] `just migrate` then `just migrate-down` round-trips cleanly on a seeded database, including
      partition creation and drop, and the partition job has created next month's partitions on
      staging.
- [ ] Tenant A cannot select tenant B's `connections`, `campaigns`, or `conversion_events` rows —
      asserted through the repo layer, directly in SQL, **and against a child partition by name**.
- [ ] `GET /api/v1/tenant/ads/platforms` returns 403 without the entitlement, 403 without
      `advertising.read`, and 200 with `{ "platforms": [] }` with both.
- [ ] All six `advertising.*` permissions and both entitlements are visible in the platform admin
      console.
- [ ] `just ads-invariants-self-test` demonstrates every gate fails on a deliberate violation.
- [ ] Retention rules for both partitioned tables are registered in the phase-05 data map.
- [ ] No Google Ads or Meta SDK call exists anywhere in the codebase yet.

## Verification

```bash
just fmt
just clippy
just layering
just ads-invariants
just ads-invariants-self-test
cargo test -p sanvi-advertising
just migrate && just migrate-down
just test
just openapi
```

## Out of scope

The domain model beyond what the empty catalog needs, the capability matrix, ports and the fake
adapter (all TASK-010), every OAuth call (TASK-011), and anything that uploads (TASK-014+).

## Files likely touched

- `api/advertising.yaml`, `apps/api/src/openapi.rs`
- `crates/contexts/advertising/**`, `Cargo.toml`, `apps/api/**`, `apps/worker/**`
- `migrations/20260824000000_advertising_core.sql` + `.down.sql`
- `configs/base.toml` + per-env overlays
- `scripts/check-ads-invariants.sh`, `justfile`
- `access` permission seed, `platform_admin` entitlement catalog, phase-05 retention registry

## Notes / gotchas

- The migration timestamp must stay after `20260823000000_tenant_payments`.
- RLS on a partitioned parent is not automatically what you think it is — assert against a named child
  partition, not just the parent.
- Ship the partition-creation job in this change, not "later in the phase".
- Rollback for this slice is flag-off; the migration stays, since empty tables are harmless.
- Start the Google Ads developer token application and the Meta app review **now**, not at TASK-011.
  Both are external approval queues measured in weeks; test-account access unblocks development
  meanwhile.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-009 done --note "<PR or commit>"`
