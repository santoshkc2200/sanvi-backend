# Phase 10 — Advertising & Campaign Management (backend)

**Target version:** 0.11.0
**Depends on:** phases 01, 02, 03 (entitlements), 05 (consent gates conversion upload), 06, 08 (tracking on tenant domains), 09 (revenue for ROAS)
**Unlocks:** tenants running and measuring paid acquisition from inside Sanvi

## Goal

An `advertising` bounded context where a tenant connects Google Ads and Meta (Facebook/Instagram)
accounts, creates and manages campaigns, tracks conversions back to revenue, and sees spend, sales,
conversions, and ROAS in one dashboard. The platform abstraction must make the third and fourth ad
network an adapter, not a rewrite.

## Scope

**In**
- Ad platform connections (OAuth), account selection, token lifecycle.
- Unified campaign model: campaign → ad set/ad group → ad → creative, with per-platform capability
  declarations and validation.
- Campaign operations: create, update, pause/resume, budget changes, schedule — synced both ways.
- Conversion tracking: first-party event capture on tenant storefronts, server-side upload to Google
  Ads (offline/enhanced conversions) and Meta (Conversions API), consent-gated and deduplicated.
- Metrics pipeline: scheduled ingestion of spend/impressions/clicks/conversions, joined with phase 09
  revenue, into an aggregate store powering the dashboard.
- Budget/spend guardrails and alerting.

**Out**
- Creative generation/asset editing (the tenant supplies assets; `course-media`-style upload reuse).
- Bid strategy automation and ML optimisation (later; the metric store is designed to support it).
- Networks beyond Google and Meta in this release (adapters planned: TikTok, LINE, X, Yahoo! JAPAN).

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Model | Unified domain model + per-platform `CapabilityMatrix` declaring supported objectives, budget types, targeting dimensions | A true lowest-common-denominator model is useless; a capability-aware one is honest |
| Sync direction | Two-way, with **the ad platform as the source of truth for delivery/metrics** and Sanvi as the source of truth for *intent* (drafts, schedules, our-side approvals) | Ads change outside our UI constantly; pretending otherwise causes destructive overwrites |
| Conflict policy | Last-writer-wins per field with a change log, plus a "drifted" flag when a remote change contradicts our intent | Users must see, not silently lose, changes made in Google/Meta UIs |
| Conversion capture | First-party endpoint on the tenant's own domain (phase 08) → server-side upload | Survives ITP/ad-blockers; keeps PII server-side where we can hash and consent-gate it |
| Identity for matching | Hashed (SHA-256, normalised) email/phone; **only** when phase 05 resolves `ads_measurement` to `Allowed`; never raw PII | Legal requirement and platform requirement |
| Directive gate | Every upload asks the phase-05 `DirectiveResolver` for `ads_measurement` **and**, when the receiving vendor is registered as a third party rather than a service provider, `sale_or_share` / `targeted_advertising`; the answer already accounts for EU consent, APPI, and US opt-outs incl. GPC | Sending identifiers to an ad platform is a "sale"/"share" under US law; a measurement-only gate would honour the EU rules and quietly violate the US ones |
| Audience uploads | Customer-list/audience sync requires `sale_or_share` **and** `targeted_advertising` per subject; subjects who opted out are excluded from the list and from any later refresh | An audience list is the one place an opt-out is easiest to leak past and hardest to retract |
| Deduplication | Stable `event_id` shared between browser pixel and server-side upload | Both platforms dedupe on it; without it, conversions double-count and ROAS lies |
| Token storage | OAuth refresh tokens encrypted with the phase-05 envelope scheme, per-tenant DEK, never logged | Ad account access is money access |
| Metrics store | Postgres with monthly partitions + rollup tables (`daily`, `campaign_daily`, `platform_daily`); revisit a columnar store only if volume demands | Simple, joinable with revenue, good enough for years at this scale |
| Attribution | Report **platform-reported conversions** and **Sanvi-observed revenue** side by side, never a single blended number presented as truth | Attribution windows differ per platform; a merged number is a lie with a decimal point |
| Currency | Ad platforms report in the ad account currency; store minor units + currency, convert only for display with a dated FX rate | Never store converted values |

## Domain model

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

Ports: `AdPlatformPort` (auth, list accounts, CRUD campaign tree, fetch metrics, upload conversions),
`ConversionSinkPort`, `FxRatePort`.

Events: `AdAccountConnected/Disconnected`, `CampaignCreated/Updated/Paused/Resumed`,
`CampaignDrifted`, `ConversionRecorded`, `ConversionUploaded/UploadFailed`, `SpendThresholdReached`,
`MetricsIngested`.

## Deliverables

### Schema

```sql
CREATE SCHEMA advertising;
CREATE TABLE advertising.connections (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL, platform text NOT NULL,
  external_account_id text NOT NULL, account_name text, currency text, timezone text,
  status text NOT NULL, scopes text[] NOT NULL, token_ref text NOT NULL,
  connected_by uuid, connected_at timestamptz, last_synced_at timestamptz, last_error jsonb,
  UNIQUE (tenant_id, platform, external_account_id));

CREATE TABLE advertising.campaigns (id uuid PRIMARY KEY, tenant_id uuid NOT NULL, platform text,
  connection_id uuid NOT NULL, external_id text, name text NOT NULL, objective text NOT NULL,
  status text NOT NULL, budget jsonb NOT NULL, schedule jsonb, drift jsonb,
  created_at timestamptz, updated_at timestamptz, UNIQUE (connection_id, external_id));
CREATE TABLE advertising.ad_groups (...);
CREATE TABLE advertising.ads (...);
CREATE TABLE advertising.creatives (...);

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

RLS on everything; conversion and metric tables partitioned monthly with a retention rule registered
in phase 05.

### Connection & OAuth

- Google Ads: OAuth 2.0 (offline access), developer token, login-customer-id for manager accounts,
  account picker after consent. Meta: Facebook Login for Business, system-user token where possible,
  Business/Ad account selection, permission checks.
- `state` parameter is a signed, single-use, tenant-bound nonce — CSRF protection is mandatory on
  every OAuth surface.
- Refresh scheduler renews tokens before expiry; revocation/expiry sets `status = Disconnected`,
  notifies the tenant, and pauses dependent jobs rather than failing them repeatedly.
- Scope changes on the platform side are detected on sync and surfaced as a re-consent prompt.

### Campaign operations

- Draft in Sanvi → validate against the platform's `CapabilityMatrix` (objective, budget type,
  targeting availability, creative specs, character limits per locale) → publish → poll/receive review
  status → live.
- Every mutation is idempotent (client-generated mutation id) and recorded in a change log with the
  actor, so "who paused this campaign" is answerable.
- Nightly reconciliation pulls the remote tree; differences from our last-known state mark `drift`
  and notify, they never silently overwrite tenant edits made in the native UI.
- Budget guardrails: per-campaign and per-tenant daily/monthly caps enforced by us (pause on breach)
  in addition to the platform's own controls, with a warning threshold at 80 %.

### Conversion pipeline

```
storefront event (purchase/lead/signup) on the tenant's own domain
  → POST /api/v1/public/track  (first-party, tenant-domain, rate-limited, bot-filtered)
  → resolve the directive snapshot (phase 05), which already applies the subject's jurisdiction:
    `ads_measurement` not `Allowed`, or `sale_or_share`/`targeted_advertising` denied for a
    third-party vendor (incl. a US opt-out or a GPC signal) → store as analytics-only,
    DO NOT upload, and mark `suppressed_by_directive` with the purpose and source that blocked it
  → enrich: click ids from cookies/URL (gclid/gbraid/wbraid/fbc/fbp), order value from phase 09
    payment record (server-side value is the trustworthy one)
  → hash identifiers (normalise → SHA-256), never persist raw PII beyond what the order needs
  → queue upload → Google Ads conversion upload / Meta Conversions API, batched, retried with
    backoff, deduplicated by `event_id`
  → record per-platform upload state and errors; expose a per-conversion diagnostic view
```

Value integrity: the conversion value comes from our own payment record where one exists, so
inflated client-side values cannot poison ROAS.

### Metrics & ROAS

- Scheduled ingestion per connection (hourly for today, daily backfill for the trailing 30 days —
  ad platforms restate conversions for days after the fact, so a single fetch is never enough).
- Rollups: `metrics_daily` → campaign/platform/tenant aggregates; revenue joined from phase 09 by
  `order_ref` and by date bucket.
- Dashboard queries served from rollups with a bounded date range; ROAS computed as
  `sanvi_revenue / spend` **and** `platform_conversion_value / spend`, both labelled, never merged.
- FX: rates snapshotted daily; display conversion only, with the rate date shown.

### API surface (`api/advertising.yaml`)

| Method | Path | Permission |
|---|---|---|
| GET | `/api/v1/tenant/ads/platforms` | `advertising.read` (+ entitlement per platform) |
| POST | `/api/v1/tenant/ads/connections/{platform}/oauth/start` \| `/callback` | `advertising.connect` |
| GET/DELETE | `/api/v1/tenant/ads/connections` | `advertising.connect` |
| GET/POST/PATCH | `/api/v1/tenant/ads/campaigns` (+ ad groups, ads, creatives) | `advertising.campaign.*` |
| POST | `/api/v1/tenant/ads/campaigns/{id}/pause` \| `/resume` | `advertising.campaign.write` |
| GET | `/api/v1/tenant/ads/metrics?from&to&group_by` | `advertising.metrics.read` |
| GET | `/api/v1/tenant/ads/conversions` (+ diagnostics) | `advertising.metrics.read` |
| POST | `/api/v1/public/track` | none (tenant-domain, signed site key, rate-limited) |
| GET | `/api/v1/tenant/ads/budget-alerts` | `advertising.read` |

## Work breakdown

1. Domain model + `CapabilityMatrix` + validation engine.
2. OAuth flows (both platforms), signed state, token encryption, refresh scheduler, revocation.
3. `AdPlatformPort` + Google Ads adapter (campaign tree CRUD, metrics, conversion upload).
4. Meta adapter (Marketing API campaign tree, Insights, Conversions API).
5. Tracking endpoint: bot filtering, rate limits, consent resolution, click-id capture, event storage.
6. Upload workers with batching, retry, dedupe, per-event diagnostics, and consent suppression.
7. Metrics ingestion scheduler, restatement-aware backfill, partitioning and rollups.
8. Revenue join with phase 09 and the ROAS query layer.
9. Budget guardrails, threshold alerts, auto-pause, and its audit trail.
10. Drift detection and reconciliation reporting.
11. Privacy integration: `PersonalDataProvider` for this context (export/erase conversion identifiers).
12. Runbooks: token revoked, platform API outage, conversion upload failures, spend anomaly.

## Testing

- Adapter contract tests against recorded fixtures per platform, plus a sandbox/test-account suite.
- Capability validation tests: rejecting an objective/budget combination the platform cannot express.
- Directive tests: no `ads_measurement` → nothing is uploaded, and withdrawal stops future uploads
  (contract test against phase 05's port). Run the same case for a US subject who never consented but
  never opted out (upload proceeds), one who opted out (nothing uploaded), and one whose browser sent
  GPC (nothing uploaded) — the three cases that a consent-only gate gets wrong.
- Audience test: an opted-out subject is absent from a newly built list and removed from an existing
  one on the next refresh.
- Dedupe test: browser pixel + server event with the same `event_id` counts once.
- Restatement test: re-ingesting a day updates rollups without double counting.
- Drift test: a campaign paused in Google's UI shows as drifted, is not resurrected by our sync.
- ROAS arithmetic tests across currencies, including JPY, and with zero spend (no division by zero).
- Load: 500 campaigns × 90 days ingestion within the scheduling window.

## Security & compliance

- Refresh tokens encrypted per tenant; access to the decrypt path is audited; tokens never logged and
  redacted in traces.
- `/api/v1/public/track` is unauthenticated by design: signed site key per tenant domain, strict
  origin checks, aggressive rate limiting, payload size caps, and no reflection of input.
- Hashed identifiers only; raw PII is never sent to ad platforms; consent snapshot stored with each
  event as proof.
- Ad account access is high-value: connection changes require fresh MFA and are audited.
- Sub-processor registry (phase 05) updated with Google and Meta, including their **role**
  (service provider/contractor vs. third party — the field that decides whether an upload is a "sale"
  or "share" under US law), the contract terms relied on, and transfer disclosures for APPI
  cross-border provision and EU/UK transfers to US platforms.
- A retracted directive (consent withdrawal or a US opt-out) triggers a deletion/suppression request
  to the platforms that support one, and removal from every audience list on the next refresh.

## Observability

Metrics: `ads_sync_duration{platform}`, `ads_sync_failures_total{platform,code}`,
`conversion_upload_total{platform,result}`, `conversion_suppressed_total{reason}`,
`ads_token_refresh_failures`, `metrics_ingestion_lag_hours`, `budget_threshold_alerts_total`.
Alerts: ingestion lag > 6 h, upload failure ratio > 5 %, token refresh failures, spend anomaly
(> 3σ day-over-day).

## Acceptance criteria

- [ ] A tenant connects Google Ads and Meta, creates a campaign in Sanvi, and it appears live on the
      platform with matching budget and schedule.
- [ ] A storefront purchase produces exactly one conversion on each connected platform, with the
      value taken from the server-side payment record.
- [ ] With `ads_measurement` withdrawn, or with a US opt-out of sale/share (including one expressed
      only via GPC), no conversion is uploaded and the suppression is visible in diagnostics with the
      purpose and source that caused it.
- [ ] The dashboard shows spend, clicks, conversions, revenue, and ROAS for a date range, matching
      the platform's own reporting within the documented restatement window.
- [ ] A campaign paused natively is reported as drifted rather than overwritten.
- [ ] Adding a third network requires only an adapter + capability matrix (proven with a fake).

## Risks

| Risk | Mitigation |
|---|---|
| Ad platform API changes / deprecations | Versioned adapters, contract tests against fixtures, deprecation calendar tracked per platform |
| Google Ads developer token approval delays | Start the application at phase kickoff; test-account access unblocks development in the meantime |
| Conversion double counting destroys trust in ROAS | Shared `event_id`, dedupe tests, and side-by-side platform vs Sanvi revenue reporting |
| Attribution mismatch complaints | Documented methodology in-product, both numbers shown, windows stated |
| Metrics volume growth | Partitioning, rollups, retention policy; columnar store is a documented escape hatch |
| Tenant spends real money through our bugs | Budget caps, auto-pause, mutation change log, and dry-run mode for bulk edits |
