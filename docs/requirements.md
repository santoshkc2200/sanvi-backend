# sanvi-backend — Numbered Requirements

Numbered requirements are the layer that task files trace to. They are introduced **from phase 09
onward**; phases 00–08 shipped before this practice and remain documented by their
`phase-NN-*/implementation-plan.md` files, which stay authoritative for what was built.

IDs are phase-prefixed (`FR-9xx` = phase 09, `FR-10xx` = phase 10, …) so they never collide and so a
requirement's phase is readable from the ID alone. IDs are permanent: when a requirement changes,
edit its text and note the change; when it is dropped, mark it `withdrawn` rather than reusing the
number.

`python3 ~/.claude/skills/sdlc-planner/scripts/sdlc.py check` verifies that every requirement here is
covered by at least one task in `tasks/`, and that every task names a requirement.

---

## Phase 09 — Tenant Payment Providers (0.10.0)

Derived from [`phase-09-tenant-payments/implementation-plan.md`](phase-09-tenant-payments/implementation-plan.md)
and sliced into tasks per [`tasks/phase-09/README.md`](tasks/phase-09/README.md).

### Functional

| ID | Requirement |
|---|---|
| **FR-901** | A tenant admin can connect their own Stripe account through Accounts v2 (`POST /v2/core/accounts`) plus an embedded onboarding Account Session, and the platform persists a connection row holding the resulting `acct_…`. Exactly one active connection per tenant per provider. |
| **FR-902** | `GET /api/v1/tenant/payments/providers` returns a provider catalog derived from the registered adapters (never a hardcoded handler list), gated on the `payments.stripe_connect` entitlement and the `payments.read` permission. |
| **FR-903** | `GET /api/v1/tenant/payments/connections/{id}` exposes status, capabilities, outstanding requirements with deadlines, blockers, and a server-computed `can_accept_payments` derived **only** from `configuration.merchant.capabilities.card_payments.status == "active"`. |
| **FR-904** | Connection lifecycle is driven by connected-account webhooks through an explicit state machine (`Pending → Onboarding → {Active, Restricted, Rejected} → Disconnected`), with a reconciliation job as the safety net for missed deliveries. |
| **FR-905** | `POST /api/v1/tenant/checkout` creates a Checkout Session **on the connected account** (direct charge) with dynamic payment methods, the tenant's locale, and success/cancel URLs validated against a verified tenant domain; `GET /api/v1/tenant/checkout/{id}` is the only source of order truth. |
| **FR-906** | Payment, refund, dispute and payout state is projected into tenant-scoped read models from webhooks (never fetched from the provider per request), queryable with the documented filter set, a per-payment timeline, and an audited server-side CSV export. |
| **FR-907** | A principal holding `payments.refund` can issue a full or partial refund on the connected account, idempotently, with a required reason, rejecting over-refunds with the remaining refundable amount. Refunds issued in the tenant's own Stripe Dashboard reconcile into the read model. |
| **FR-908** | When a tenant enables automatic tax, liability is routed to the connected account (`automatic_tax.liability = { type: 'account', account: acct_id }`), gated on a preflight that the account's tax settings are `active` and registrations exist, re-verified on a schedule. |
| **FR-909** | A per-tenant/per-plan application fee is configurable in the `platform-admin` context (never the tenant API), **disabled by default in 0.10.0**, applied as `payment_intent_data.application_fee_amount` on direct charges when enabled, and disclosed to the tenant it applies to. |
| **FR-910** | `DELETE /api/v1/tenant/payments/connections/{id}` disconnects a provider, refusing with `409` and structured blockers (`in_flight_payments`, `open_disputes`, `pending_payouts`) while money is unresolved. Historical records are retained; reconnecting creates a new connection row. |

### Non-functional

| ID | Requirement |
|---|---|
| **NFR-901** | **Sanvi is never the merchant of record.** Direct charges only; `defaults.responsibilities.fees_collector` and `losses_collector` both `"stripe"`; funds never touch the platform balance. Enforced by CI gates, asserted in the sandbox, and alarmed in production (any platform-balance movement from a tenant transaction pages). |
| **NFR-902** | No deprecated Connect v1 pattern exists anywhere in the repository — `type: 'standard'\|'express'\|'custom'`, `charges_enabled`, `payouts_enabled`, `payment_method_types`. `scripts/check-connect-invariants.sh` fails the build on any occurrence. |
| **NFR-903** | Every tenant-scoped `payments.*` table carries `tenant_id` and is protected by RLS; `payments.webhook_events` is platform-scoped and not tenant-readable. The schema stores **no** card data, bank details, or onboarding PII — only identifiers, status, and amounts. |
| **NFR-904** | Every mutating payments endpoint accepts and requires `Idempotency-Key`, enforced locally **and** passed through to the provider, so a retried request never produces a second side effect at Stripe. |
| **NFR-905** | The Connect webhook transport verifies the signature on the raw body before parsing, stores then processes, dedupes by event id, drops stale out-of-order events, dead-letters poison events with replay from `apps/cli`, and rejects events for unknown accounts without creating rows. The Connect signing secret and the phase-04 platform secret must not validate each other's events. |
| **NFR-906** | No `stripe::` type appears in a port signature or anywhere in the domain or application layers — enforced by `scripts/check-layering.sh`. Adding a second provider requires only a new adapter, proven by the fake provider adapter used in tests. |
| **NFR-907** | Money is represented as minor units plus an ISO-4217 code, never a float or `numeric`; arithmetic is currency-checked. JPY (zero-decimal) is correct at every layer and is a required test case. |
| **NFR-908** | Metrics, traces, alerts and runbooks exist for every payments failure mode before the phase is tagged; the checkout path is traced end to end including the outbound Stripe call. PCI scope stays SAQ-A. |
| **NFR-909** | Account Session client secrets are never persisted and never logged. The Connect restricted API key and Connect webhook signing secret are distinct per environment, distinct from the phase-04 platform credentials, and carry only Connect permissions. |

---

## Phase 10 — Advertising & Campaigns (0.11.0)

Derived from [`phase-10-advertising/implementation-plan.md`](phase-10-advertising/implementation-plan.md)
and sliced into tasks per [`tasks/phase-10/README.md`](tasks/phase-10/README.md).

### Functional

| ID | Requirement |
|---|---|
| **FR-1001** | `GET /api/v1/tenant/ads/platforms` returns a platform catalog assembled from the registered adapters (never a hardcoded list), carrying per platform its display metadata, entitlement key, connection state, and a versioned **capability matrix** — objectives, budget types, minimum budgets by currency, schedule granularity, targeting dimensions, creative placements and asset specs, and text limits **per locale**. The matrix is the single source both the backend validator and the frontend form engine consume. |
| **FR-1002** | A tenant admin connects a Google Ads and a Meta ad account through a backend-driven OAuth flow whose `state` is a signed, single-use, tenant-bound nonce, selects the account when several are eligible (a Google manager account resolves to many customer ids), and the platform persists one connection per `(tenant, platform, external_account_id)` holding the account's currency and timezone. |
| **FR-1003** | Connection health is maintained without tenant action: refresh tokens are renewed before expiry with jitter; a revoked or permanently failing credential sets `status = Disconnected`, emits `AdAccountDisconnected`, notifies the tenant once, and **pauses** dependent jobs rather than retrying; a granted-scope subset surfaces as a re-consent state naming the missing scope. `GET /connections` exposes `can_sync`, `can_upload_conversions`, `token_expires_at`, `scopes_missing`, `last_error`, `last_synced_at` as server-computed fields. |
| **FR-1004** | A tenant drafts a campaign tree (campaign → ad group → ad → creative) in Sanvi, validates it against the platform's capability matrix through `POST /campaigns/{id}/validate` returning **field-addressed** violations, and publishes it to the platform. `PATCH` on a published campaign requires `Idempotency-Key` and `If-Match` on the campaign revision; a stale revision returns `409` with the current state. |
| **FR-1005** | A nightly and on-demand reconciliation job pulls the remote campaign tree per connection and compares it field-wise against our last-known snapshot: a non-contradicting remote change updates the snapshot silently; a change that contradicts our intent sets `drift` with a diff, emits `CampaignDrifted`, and notifies. Reconciliation **never writes back**. `POST /campaigns/{id}/drift` carries an explicit `keep_theirs` or `reapply_ours` resolution, idempotent per drift revision. |
| **FR-1006** | The Google Ads adapter implements `AdPlatformPort` (auth, list accounts, campaign-tree CRUD, fetch metrics, upload conversions) against a pinned API version and passes the same port contract suite as the fake adapter, with per-ad review status and platform rejection reasons surfaced verbatim. |
| **FR-1007** | The Meta adapter implements the same `AdPlatformPort` against the Marketing API with its own capability matrix, and creatives are persisted with assets, per-locale copy, and placement assignments, validated against the target placements' asset specs **at upload time**, not at publish time. |
| **FR-1008** | `POST /api/v1/public/track`, mounted on the tenant's own verified domain, captures a conversion event with a stable server-issued `event_id`, click ids (`gclid`, `gbraid`, `wbraid`, `fbc`, `fbp`) distinguishing absent from empty, and a value taken from the phase-09 payment record where one exists (a client-reported value is recorded as such). Storage is idempotent on `(tenant_id, event_id)`. |
| **FR-1009** | Every captured event resolves the phase-05 `DirectiveResolver` for `ads_measurement` **and**, where the receiving vendor is registered as a third party rather than a service provider, `sale_or_share` / `targeted_advertising`, and persists the full `ConsentSnapshot` (purposes, answers, signal source, jurisdiction, resolver version). A denied purpose stores the event analytics-only with `suppressed_by_directive` naming the purpose and the source, and never queues it. |
| **FR-1010** | Permitted events upload to Google Ads (offline/enhanced conversions) and Meta (Conversions API) exactly once, batched, retried with backoff, deduplicated on the shared `event_id`, with **per-platform** upload state, permanent-versus-transient failure classification, a DLQ for parked events, an audited manual retry for retryable rows only, and a per-event diagnostics projection carrying capture, gate, and attempt history. |
| **FR-1011** | Audience/customer-list sync requires `sale_or_share` **and** `targeted_advertising` per subject; opted-out subjects are excluded from new lists and **removed from existing ones on the next scheduled refresh**. A withdrawal or opt-out after upload triggers a deletion/suppression request to each platform that supports one, recording an explicit unpropagated outcome where none exists. |
| **FR-1012** | Metrics ingestion runs per connection — hourly for the current day plus a rolling backfill of the trailing 30 days — as an upsert keyed by `(tenant, platform, campaign, date)`, maintains `metrics_daily` and its rollups, joins phase-09 revenue by `order_ref` (and by date bucket where attribution is impossible, reported at platform level rather than distributed by a guess), and serves `/metrics`, `/metrics/summary`, `/metrics/export`, `/metrics/freshness` from rollups over bounded date ranges, each row carrying its source currency, ad account timezone, and a `restating` marker. |
| **FR-1013** | Per-campaign and per-tenant daily and monthly budget caps are enforced by us in addition to the platform's own controls: a threshold evaluator runs after each ingestion cycle and emits `SpendThresholdReached` at 80 % and 100 % once per period per scope; auto-pause is off by default, pauses through the ordinary attributed mutation path, never auto-resumes, and a dry-run reports what would have been paused with no side effects. |
| **FR-1014** | The advertising context implements the phase-05 `PersonalDataProvider` — exporting and erasing hashed identifiers, click ids, and audience membership, retaining the consent snapshot under the registered statutory override — and Google and Meta are registered in the sub-processor registry with their **role** (service provider/contractor vs third party), contract terms, and APPI/EU transfer disclosures, which is the field FR-1009's gate reads. |

### Non-functional

| ID | Requirement |
|---|---|
| **NFR-1001** | **No identifier leaves Sanvi without a live directive that permits it.** Every `ConversionSinkPort` implementation takes a `ConsentSnapshot` as a required argument, so an upload path without one does not compile; the upload worker reads the decision recorded at capture and re-checks only for *withdrawal*, never upgrading a suppressed event after a later consent. An upload attempted without a stored snapshot pages. |
| **NFR-1002** | No raw PII exists in this context: identifiers are normalised then SHA-256 hashed (lowercase, trim, strip dots and plus-tags for email; E.164 for phone) and only hashes are persisted, logged, traced, or sent. A grep gate fails on any raw `email`/`phone` field name inside the context's adapters, and a full-capture assertion over the events table and tracing output finds neither. |
| **NFR-1003** | OAuth refresh tokens are encrypted with the phase-05 envelope scheme under a per-tenant DEK, access tokens are cached in memory only, and `SecretRef`'s `Debug`/`Display`/`Serialize` impls redact. Nothing token-shaped reaches a log line, trace attribute, error message, or response body; access to the decrypt path is audited from the first call. Connect and disconnect require fresh MFA and are audited with the actor, account, and scopes granted. |
| **NFR-1004** | Every `advertising.*` table carries `tenant_id` and is protected by RLS, **verified to inherit to child partitions by name**. `conversion_events` and `metrics_daily` are monthly range partitions with a partition-creation job scheduled from the first migration and retention windows registered in the phase-05 data map before either table holds a row. |
| **NFR-1005** | Money is minor units plus an ISO-4217 code, always paired with the currency of the ad account it came from; JPY is zero-decimal and correct at every layer. **No converted value is ever persisted** — FX is applied in the query layer for display only, from dated rate snapshots, and a rendered conversion carries its rate date. Cross-currency sums do not exist. |
| **NFR-1006** | Platform-reported conversion value and Sanvi-observed revenue are reported side by side, each labelled with its provenance, in the API and in every export. There is no blended attribution field anywhere — grep-asserted — and zero spend yields null, never infinity. |
| **NFR-1007** | Every mutating advertising endpoint accepts and requires `Idempotency-Key`; every mutation is recorded in an append-only change log with the actor, timestamp, field-level before/after, source (`sanvi` \| `platform`), and mutation id, so "who paused this campaign" is answerable and a publish interrupted by a timeout cannot create two campaigns. |
| **NFR-1008** | No platform-specific type or branch exists in the domain or application layers; platform difference is expressed only as capability-matrix data and adapter code. Adding a third network requires an adapter plus a matrix, proven by the fake adapter's asymmetric third matrix, and every adapter passes the same `AdPlatformPort` contract suite. |
| **NFR-1009** | Metrics ingestion is idempotent under restatement: re-ingesting a day **replaces** that day at the `metrics_daily` grain and at every rollup level. An append here silently doubles history, so it is asserted at every aggregation level rather than at the entry point. |
| **NFR-1010** | `/api/v1/public/track` is unauthenticated by design and hardened accordingly: per-tenant-domain signed rotatable site key, strict `Origin` checks, payload size caps, per-IP and per-domain rate limits, bot filtering, and no reflection of input or existence in any response. Platform API quota is budgeted per connection so one tenant's bulk edit cannot exhaust the shared developer-token quota. |
| **NFR-1011** | Metrics (`ads_sync_duration`, `ads_sync_failures_total`, `conversion_upload_total`, `conversion_suppressed_total{reason,source}`, `ads_token_refresh_failures`, `metrics_ingestion_lag_hours`, `budget_threshold_alerts_total`), their alerts (ingestion lag > 6 h, upload failure ratio > 5 %, token refresh failures, spend anomaly > 3σ, DLQ depth), end-to-end traces on the tracking path, and a runbook per failure mode all exist before the phase is tagged. |

## Phase 11 — Hardening, Observability & GA (1.0.0)

To be numbered (`FR-11xx` / `NFR-11xx`) when phase 11 is decomposed. Until then, see
[`phase-11-hardening-ga/implementation-plan.md`](phase-11-hardening-ga/implementation-plan.md).
