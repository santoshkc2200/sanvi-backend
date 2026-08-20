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
and the cross-track sub-phase breakdown in [`../../docs/phase-09-tenant-payments/`](../../docs/phase-09-tenant-payments/README.md).

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

To be numbered (`FR-10xx` / `NFR-10xx`) when phase 10 is decomposed. Until then, see
[`phase-10-advertising/implementation-plan.md`](phase-10-advertising/implementation-plan.md).

## Phase 11 — Hardening, Observability & GA (1.0.0)

To be numbered (`FR-11xx` / `NFR-11xx`) when phase 11 is decomposed. Until then, see
[`phase-11-hardening-ga/implementation-plan.md`](phase-11-hardening-ga/implementation-plan.md).
