# Phase 10 — Advertising & Campaign Management (backend)

**Version:** 0.11.0 · **Status:** in progress — slices 10.0–10.3 shipped against the fake adapter
**Depends on:** 01, 02, 03 (entitlements), 05 (directives, envelope crypto), 06, 08 (tenant domains),
09 (revenue) · **Unlocks:** tenants running and measuring paid acquisition inside Sanvi

**This document is the phase spec — the *what* and *why*.** Slice order and per-slice exit criteria
live in [`../tasks/phase-10/README.md`](../tasks/phase-10/README.md); the executable step-by-step
breakdown lives in the ten `TASK-0NN` files beside it. Status lives in
[`../tasks/backlog.md`](../tasks/backlog.md).

---

## 1. Goal

An `advertising` bounded context where a tenant connects Google Ads and Meta, manages campaigns,
tracks conversions back to revenue, and reads spend/sales/conversions/ROAS in one dashboard.
The platform abstraction must make the third and fourth ad network **an adapter, not a rewrite**.

## 2. Progress

Verified 2026-08-25 against the working tree (`cargo test -p sanvi-advertising --lib`: 38 passed).

| Slice | Task | Status | Shipped | Open |
|---|---|---|---|---|
| 10.0 contract & foundations | TASK-009 | ✅ code complete | `api/advertising.yaml`; `sanvi-advertising` crate; migration `20260826000000_advertising_core` (+7 follow-ups) with 8 tables, RLS, monthly partitions on `conversion_events`/`metrics_daily`; retention rules inserted into `privacy.retention_rules`; 6 `advertising.*` permissions + 3 entitlements registered; per-tenant KEK token vault; `advertising = false` flag; `just ads-invariants` (+ self-test) wired into `just ci` | staging deploy evidence |
| 10.1 domain & capability matrix | TASK-010 | ✅ code complete | `domain/{model,matrix,validation,ports,events}.rs`; `AdPlatformPort` (auth, accounts, campaign CRUD, pause/resume/delete, `fetch_metrics`, `upload`); `FakeAdPlatform` + `fixtures/capability-matrices.json` exported for `sanvi-frontend`; outbox event round-trip test green; `GET /ads/platforms` catalog | — |
| 10.2 connections & OAuth | TASK-011 | 🟡 partial | signed-state OAuth start/callback (forgery, expiry, replay, cross-tenant redemption all rejected); per-tenant encrypted refresh tokens; refresh scheduler + expiry; pending/reconnect lifecycle; `FreshAal2Required` step-up on connect/disconnect; health derived from each adapter's `required_scopes` | **real Google & Meta OAuth adapters and live `list_accounts`** — blocked on Google developer-token approval and Meta app review |
| 10.3 campaigns, Google adapter & drift | TASK-012 | 🟡 partial | campaign intent snapshots; optimistic revisions (`409` on stale); mutation change log; draft/validate/publish/patch/pause/resume + ad-groups/ads; drift detection with `KeepTheirs`/`ReapplyOurs`; nightly cross-tenant `ReconcileDueCampaigns` sweep in `apps/worker` (drift-only, never writes back); `RateLimitedAdapter` per-connection + platform-wide quota queue in api and worker | **real Google Ads adapter**; review-status polling (needs the real adapter to mean anything) |
| 10.4 Meta adapter & creatives | TASK-013 | ⬜ todo | — | blocked on 10.3 |
| 10.5 conversion capture & directive gate | TASK-014 | ⬜ todo | — | unblocked by 10.2 — **next available slice** |
| 10.6 upload, dedupe & diagnostics | TASK-015 | ⬜ todo | — | blocked on 10.5 |
| 10.7 metrics, rollups & ROAS | TASK-016 | ⬜ todo | — | blocked on 10.3 |
| 10.8 budget guardrails & alerts | TASK-017 | ⬜ todo | — | blocked on 10.7 |
| 10.9 privacy, hardening & release | TASK-018 | ⬜ todo | — | blocked on 10.4, 10.6, 10.8 |

**Critical path note.** Both remaining 10.2/10.3 gaps are the same external blocker: no live platform
credentials. The fake adapter carries the abstraction until they land, exactly as the cross-track plan
intends. Do **not** idle on the queue — 10.5 depends only on 10.2's connection lifecycle and the
phase-05 resolver, so conversion capture proceeds in parallel.

## 3. Scope

**In** — OAuth connections + token lifecycle · unified campaign tree (campaign → ad group/set → ad →
creative) with per-platform capability declarations · campaign CRUD, pause/resume, budget, schedule,
two-way sync · consent-gated deduplicated conversion capture and server-side upload · scheduled
metrics ingestion joined with phase-09 revenue · budget guardrails and alerting.

**Out** — creative generation or asset editing (tenant supplies assets; reuse `course-media` upload) ·
bid automation and ML optimisation (the metric store is designed to support it later) · networks
beyond Google and Meta this release (adapters planned: TikTok, LINE, X, Yahoo! JAPAN).

## 4. Invariants

Three rules govern every slice. A change that breaks one is wrong even if it passes its own tests.

1. **No identifier leaves Sanvi without a live directive that permits it** (NFR-1001/1002). Every
   outbound upload — conversion, audience refresh, enhanced match — asks the phase-05
   `DirectiveResolver` first and records the answer. There is no per-batch bypass.
2. **Two numbers, never one** (NFR-1006). Platform-reported conversion value and Sanvi-observed
   revenue are always side by side and labelled. Any field, screen, or export blending them is wrong.
3. **Our code spends the tenant's money** (NFR-1007). Every mutation is idempotent, attributed in a
   change log, and bounded by a cap.

## 5. Key decisions

| Decision | Choice | Why |
|---|---|---|
| Model | Unified domain model + per-platform `CapabilityMatrix` (objectives, budget types, targeting dimensions) | A lowest-common-denominator model is useless; a capability-aware one is honest |
| Sync direction | Two-way. Platform owns **delivery and metrics**; Sanvi owns **intent** (drafts, schedules, our approvals) | Ads change outside our UI constantly; pretending otherwise causes destructive overwrites |
| Conflict policy | Last-writer-wins per field + change log, plus a `drifted` flag when a remote change contradicts our intent | Users must see, never silently lose, changes made in the native UIs |
| Conversion capture | First-party endpoint on the tenant's own phase-08 domain, then server-side upload | Survives ITP and ad-blockers; keeps PII server-side where we hash and gate it |
| Identity for matching | Normalised SHA-256 email/phone, **only** when phase 05 resolves `ads_measurement` to `Allowed`. Never raw PII | Legal and platform requirement |
| Directive gate | Every upload asks for `ads_measurement` **and**, when the vendor is registered as a third party rather than a service provider, `sale_or_share` / `targeted_advertising`. The resolver already accounts for EU consent, APPI, and US opt-outs including GPC | Sending identifiers to an ad platform is a "sale"/"share" under US law; a measurement-only gate honours the EU rules and quietly violates the US ones |
| Audience uploads | Require `sale_or_share` **and** `targeted_advertising` per subject; opted-out subjects are excluded from the list and from every later refresh | An audience list is where an opt-out leaks most easily and retracts hardest |
| Deduplication | Stable `event_id` shared by browser pixel and server upload | Both platforms dedupe on it; without it conversions double-count and ROAS lies |
| Token storage | Refresh tokens under the phase-05 envelope scheme, per-tenant DEK, never logged | Ad account access is money access |
| Metrics store | Postgres, monthly partitions + rollups (`daily`, `campaign_daily`, `platform_daily`) | Simple, joinable with revenue, sufficient for years at this scale; columnar is a documented escape hatch |
| Attribution | Report platform conversions and Sanvi revenue side by side, never one blended number | Attribution windows differ per platform; a merged number is a lie with a decimal point |
| Currency | Store minor units + the ad account's currency; convert only for display, with a dated FX rate | Never persist a converted value |

## 6. Domain model

```rust
pub struct AdPlatformConnection { id, tenant_id, platform: AdPlatform /*GoogleAds|Meta*/,
    external_account_id: String, account_name: String, currency: Currency, timezone: Tz,
    status: ConnectionStatus, scopes: Vec<String>, token_ref: SecretRef,
    last_synced_at, last_error: Option<SyncError> }

pub struct Campaign { id, tenant_id, platform, external_id: Option<String>,
    name, objective: Objective, status: CampaignStatus /*Draft|Active|Paused|Ended|Archived*/,
    budget: Budget /*Daily|Lifetime*/, schedule: Schedule, drift: DriftState }
pub struct AdGroup { id, campaign_id, external_id, name, targeting: Targeting, bid: BidSettings }
pub struct Ad { id, ad_group_id, external_id, creative: CreativeRef, landing_url: Url,
                tracking_template: Option<String>, review_status: ReviewStatus }

pub struct ConversionEvent { id, tenant_id, event_id: String, name: ConversionName,
    occurred_at, value: Option<Money>, order_ref: Option<MerchantOrderRef>,
    click_ids: ClickIds /*gclid, gbraid, wbraid, fbc, fbp*/, hashed_identifiers: HashedIds,
    consent: ConsentSnapshot, upload_state: UploadState }

pub struct MetricPoint { tenant_id, platform, campaign_id, date, impressions, clicks,
    spend_minor, currency, conversions, conversion_value_minor, sanvi_revenue_minor }
```

**Ports:** `AdPlatformPort` (auth, list accounts, campaign-tree CRUD, fetch metrics, upload
conversions) · `ConversionSinkPort` · `FxRatePort`.

**Events:** `AdAccountConnected/Disconnected` · `CampaignCreated/Updated/Paused/Resumed` ·
`CampaignDrifted` · `ConversionRecorded` · `ConversionUploaded/UploadFailed` ·
`SpendThresholdReached` · `MetricsIngested`.

## 7. Schema

Shipped in `migrations/20260826000000_advertising_core.sql`. RLS on every table; `conversion_events`
and `metrics_daily` partitioned monthly with retention registered in the phase-05 data map.

```sql
CREATE SCHEMA advertising;

CREATE TABLE advertising.connections (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL, platform text NOT NULL,
  external_account_id text NOT NULL, account_name text, currency text, timezone text,
  status text NOT NULL, scopes text[] NOT NULL, token_ref text NOT NULL,
  connected_by uuid, connected_at timestamptz, last_synced_at timestamptz, last_error jsonb,
  UNIQUE (tenant_id, platform, external_account_id));

CREATE TABLE advertising.campaigns (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL, platform text, connection_id uuid NOT NULL,
  external_id text, name text NOT NULL, objective text NOT NULL, status text NOT NULL,
  budget jsonb NOT NULL, schedule jsonb, drift jsonb,
  created_at timestamptz, updated_at timestamptz, UNIQUE (connection_id, external_id));
-- advertising.ad_groups, advertising.ads, advertising.creatives follow the same shape.

CREATE TABLE advertising.conversion_events (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL, event_id text NOT NULL, name text NOT NULL,
  occurred_at timestamptz NOT NULL, value_minor bigint, currency text, order_ref text,
  click_ids jsonb NOT NULL DEFAULT '{}', hashed_ids jsonb NOT NULL DEFAULT '{}',
  consent jsonb NOT NULL, upload_state jsonb NOT NULL DEFAULT '{}',
  UNIQUE (tenant_id, event_id)) PARTITION BY RANGE (occurred_at);

CREATE TABLE advertising.metrics_daily (
  tenant_id uuid, platform text, connection_id uuid, campaign_id uuid, date date,
  impressions bigint, clicks bigint, spend_minor bigint, currency text,
  conversions numeric, conversion_value_minor bigint, sanvi_revenue_minor bigint,
  ingested_at timestamptz, PRIMARY KEY (tenant_id, platform, campaign_id, date))
  PARTITION BY RANGE (date);
```

## 8. Subsystems

### 8.1 Connections & OAuth — TASK-011

- **Google Ads:** OAuth 2.0 offline access, developer token, `login-customer-id` for manager
  accounts, account picker after consent. **Meta:** Facebook Login for Business, system-user token
  where possible, Business/Ad account selection, permission checks.
- `state` is a signed, single-use, tenant-bound nonce. CSRF protection is mandatory on every OAuth
  surface.
- The refresh scheduler renews before expiry. Revocation or expiry sets `status = Disconnected`,
  notifies the tenant once, and **pauses** dependent jobs rather than failing them repeatedly.
- Scope changes detected on sync surface as a re-consent prompt naming the missing scope.

### 8.2 Campaign operations — TASK-012 (Google), TASK-013 (Meta)

- Draft in Sanvi → validate against the platform's `CapabilityMatrix` (objective, budget type,
  targeting availability, creative specs, per-locale character limits) → publish → poll review status
  → live.
- Every mutation is idempotent via a client-generated mutation id, and recorded in a change log with
  the actor — so "who paused this campaign" is answerable.
- Nightly reconciliation pulls the remote tree. Differences from our last-known state mark `drift`
  and notify; they never silently overwrite tenant edits made in the native UI.
- Budget guardrails: per-campaign and per-tenant daily/monthly caps enforced by us (pause on breach)
  **in addition to** the platform's own controls, warning at 80 %.

### 8.3 Conversion pipeline — TASK-014 (capture), TASK-015 (upload)

```
storefront event (purchase/lead/signup) on the tenant's own domain
  → POST /api/v1/public/track          first-party, tenant-domain, rate-limited, bot-filtered
  → resolve the phase-05 directive snapshot for the subject's jurisdiction.
      ads_measurement not Allowed, or sale_or_share / targeted_advertising denied for a
      third-party vendor (incl. a US opt-out or a GPC signal)
        → store analytics-only, DO NOT upload, mark suppressed_by_directive
          with the purpose and the signal source that blocked it
  → enrich: click ids from cookies/URL (gclid, gbraid, wbraid, fbc, fbp);
            order value from the phase-09 payment record — the server-side value is the trustworthy one
  → hash identifiers (normalise, then SHA-256); never persist raw PII beyond what the order needs
  → queue upload → Google Ads conversion upload / Meta Conversions API,
                   batched, backoff-retried, deduplicated by event_id
  → record per-platform upload state and errors; expose a per-conversion diagnostic view
```

**Value integrity:** the conversion value comes from our own payment record wherever one exists, so
an inflated client-side value cannot poison ROAS.

### 8.4 Metrics & ROAS — TASK-016

- Ingest per connection: hourly for today, daily backfill across the trailing 30 days. Ad platforms
  restate conversions days after the fact, so a single fetch is never enough.
- Rollups: `metrics_daily` → campaign/platform/tenant aggregates; revenue joined from phase 09 by
  `order_ref` and by date bucket.
- Dashboard queries read the rollups over a bounded date range. ROAS is computed as
  `sanvi_revenue / spend` **and** `platform_conversion_value / spend`, both labelled, never merged.
- FX rates are snapshotted daily and used for display only, with the rate date shown.

## 9. API surface (`api/advertising.yaml`)

| Method | Path | Permission | Slice |
|---|---|---|---|
| GET | `/api/v1/tenant/ads/platforms` | `advertising.read` + per-platform entitlement | 10.1 |
| POST | `/api/v1/tenant/ads/connections/{platform}/oauth/start` \| `/callback` | `advertising.connect` + step-up | 10.2 |
| GET/DELETE | `/api/v1/tenant/ads/connections` | `advertising.connect` | 10.2 |
| GET/POST/PATCH | `/api/v1/tenant/ads/campaigns` (+ ad-groups, ads, creatives) | `advertising.campaign.*` | 10.3 |
| POST | `/api/v1/tenant/ads/campaigns/{id}/publish` \| `/validate` \| `/pause` \| `/resume` | `advertising.campaign.write` | 10.3 |
| GET | `/api/v1/tenant/ads/campaigns/{id}/changes` \| `/drift` | `advertising.campaign.read` | 10.3 |
| POST | `/api/v1/public/track` | none — tenant-domain, signed site key, rate-limited | 10.5 |
| GET | `/api/v1/tenant/ads/conversions` (+ diagnostics) | `advertising.metrics.read` | 10.6 |
| GET | `/api/v1/tenant/ads/metrics?from&to&group_by` | `advertising.metrics.read` | 10.7 |
| GET/PUT | `/api/v1/tenant/ads/budget-caps`, `GET /ads/budget-alerts` | `advertising.budget.manage` / `.read` | 10.8 |

## 10. Work breakdown → tasks

The twelve numbered work-breakdown items and the task each lands in are maintained in
[`../tasks/phase-10/README.md`](../tasks/phase-10/README.md) § *Traceability to the implementation
plan*, so the mapping has one owner. Current status per slice is in § 2 above.

## 11. Testing

- **Adapter contract suite** — one shared suite every adapter passes, run against recorded fixtures
  per platform plus a sandbox/test-account suite.
- **Capability validation** — an objective/budget combination the platform cannot express is
  rejected with a field path, before submission.
- **Directive matrix** — four cases a consent-only gate gets wrong, all required:
  no `ads_measurement` → nothing uploaded, and withdrawal stops future uploads · a US subject who
  never consented and never opted out → upload proceeds · a US subject who opted out → nothing
  uploaded · a browser sending GPC → nothing uploaded. Contract-tested against the phase-05 port.
- **Audience** — an opted-out subject is absent from a newly built list and removed from an existing
  one on the next refresh.
- **Dedupe** — browser pixel plus server event sharing an `event_id` count once.
- **Restatement** — re-ingesting a day updates rollups without double counting.
- **Drift** — a campaign paused in Google's UI shows as drifted and is never resurrected by our sync.
- **ROAS arithmetic** — across currencies including JPY, and with zero spend (null, never `∞`).
- **Load** — 500 campaigns × 90 days ingested within the scheduling window.

## 12. Security & compliance

- Refresh tokens encrypted per tenant; the decrypt path is audited; tokens never logged and redacted
  in traces.
- `/api/v1/public/track` is unauthenticated by design: per-domain signed site key, strict origin
  checks, aggressive rate limiting, payload size caps, and no reflection of input.
- Hashed identifiers only. Raw PII never reaches an ad platform. The consent snapshot is stored with
  each event as proof.
- Ad account access is money-adjacent: connection changes require fresh MFA and are audited.
- Phase-05 sub-processor registry updated with Google and Meta, including their **role**
  (service provider/contractor vs. third party — the field that decides whether an upload is a "sale"
  or "share" under US law), the contract terms relied on, and transfer disclosures for APPI
  cross-border provision and EU/UK transfers.
- A retracted directive (withdrawal or US opt-out) triggers a deletion/suppression request to every
  platform that supports one, and removal from every audience list on the next refresh.

## 13. Observability

**Metrics:** `ads_sync_duration{platform}` · `ads_sync_failures_total{platform,code}` ·
`conversion_upload_total{platform,result}` · `conversion_suppressed_total{reason}` ·
`ads_token_refresh_failures` · `metrics_ingestion_lag_hours` · `budget_threshold_alerts_total`.

**Alerts:** ingestion lag > 6 h · upload failure ratio > 5 % · token refresh failures ·
spend anomaly > 3σ day-over-day.

## 14. Phase acceptance criteria

- [ ] A tenant connects Google Ads and Meta, creates a campaign in Sanvi, and it appears live on the
      platform with matching budget and schedule.
- [ ] A storefront purchase produces exactly one conversion on each connected platform, valued from
      the server-side payment record.
- [ ] With `ads_measurement` withdrawn, or a US opt-out of sale/share including one expressed only
      via GPC, no conversion is uploaded and the suppression is visible in diagnostics with the
      purpose and the signal source that caused it.
- [ ] The dashboard shows spend, clicks, conversions, revenue, and ROAS for a date range, matching
      the platform's own reporting within the documented restatement window.
- [ ] A campaign paused natively is reported as drifted rather than overwritten.
- [ ] An opted-out subject is absent from new audience lists and removed from existing ones on refresh.
- [ ] Adding a third network requires only an adapter plus a capability matrix — proven with the fake.

## 15. Risks

| Risk | Mitigation |
|---|---|
| **Live credentials block 10.2/10.3 (active)** | Fake adapter carries the abstraction; 10.5 proceeds in parallel; developer-token and app-review applications submitted at kickoff |
| Ad platform API changes and deprecations | Versioned adapters, contract tests against fixtures, a per-platform deprecation calendar |
| Conversion double counting destroys trust in ROAS | Shared `event_id`, dedupe tests, side-by-side platform vs. Sanvi revenue |
| Attribution mismatch complaints | In-product methodology, both numbers shown, windows stated |
| Metrics volume growth | Partitioning, rollups, retention policy; columnar store as a documented escape hatch |
| Tenant spends real money through our bugs | Budget caps, auto-pause, mutation change log, dry-run for bulk edits |
