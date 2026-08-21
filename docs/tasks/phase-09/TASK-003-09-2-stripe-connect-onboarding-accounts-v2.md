# TASK-003: 09.2 Stripe Connect onboarding (Accounts v2)

**Phase:** 09
**Status:** done
**Requirement(s):** FR-901, NFR-901, NFR-904, NFR-909
**Depends on:** TASK-002
**Created:** 2026-08-20

**Slice:** 09.2 — Stripe Connect onboarding
**Prerelease:** `v0.10.0-alpha.3` · **Flag:** `payments.stripe_connect`

## Context

A tenant admin clicks *Connect Stripe*, completes Stripe's embedded onboarding, and we hold a real
`acct_…` with a persisted connection row. Status is still naive here — it becomes truthful in
TASK-004 when webhooks arrive. This task is about creating the account **correctly**, because the
account creation call is the single least reversible decision in the whole phase.

## The account creation call is the spec

Every dimension is asserted by a test. Getting one wrong means migrating live merchants later.

| Dimension | Value |
|---|---|
| API | `POST /v2/core/accounts` (Accounts v2) |
| Configuration | `configuration.merchant` requesting `card_payments` |
| Dashboard | `dashboard: "full"` |
| Fees collector | `defaults.responsibilities.fees_collector: "stripe"` |
| Losses collector | `defaults.responsibilities.losses_collector: "stripe"` |
| Identity | country + business profile prefilled from tenant data |
| Idempotency | keyed by our connection id |

Never `type: 'standard' | 'express' | 'custom'` — v1 patterns, and the TASK-001 grep gate fails the
build if they appear. Never `charges_enabled` as a readiness signal.

## What to do

- [ ] **Contract** — add to `api/payments.yaml`, merged before the code:
      | Method | Path | Permission | Notes |
      |---|---|---|---|
      | POST | `/api/v1/tenant/payments/connections` | `payments.manage` + entitlement | Idempotent; returns the connection, 409 if an active one exists |
      | POST | `/api/v1/tenant/payments/connections/{id}/session` | `payments.manage` | Short-lived Account Session client secret + the components enabled on it |
      | GET | `/api/v1/tenant/payments/connections/{id}` | `payments.read` | Status, capabilities, requirements (thin until TASK-004) |

      The session response carries `expires_at` and is documented as non-cacheable, never-logged and
      single-render.
- [ ] **Stripe Connect adapter** (`src/adapters/stripe_connect/`) implementing `PaymentProviderPort`:
      account creation exactly per the table above, Account Session creation with the component set
      (`account_onboarding` now; `notification_banner`, `account_management`, `payments`, `payouts`
      added in TASK-004/TASK-007), and status fetch.
- [ ] **Pinned API version** — set explicitly on the client, from config. An unpinned client is a
      silent breaking change waiting for a Stripe release.
- [ ] **Error taxonomy** — map Stripe failures to domain errors, never a generic 500: invalid country,
      country unsupported for Connect, rate limit (retry with backoff), auth failure (alert, do not
      retry), idempotency conflict. Stripe request ids are logged for support; error bodies are not
      echoed to the client.
- [ ] **Create-connection use case** — entitlement check → single-active-connection guard (the
      TASK-001 partial unique index is the backstop) → create at Stripe with our connection id as the
      idempotency key → persist → emit `ProviderConnectionStarted` → audit with actor.
      Order matters: persist the `acct_…` before returning, so a crashed response never orphans an
      account at Stripe. If persistence fails after creation, TASK-004's reconciliation job adopts
      the orphan by idempotency key rather than creating a second account.
- [ ] **Account Session use case** — fetched per render, short TTL, never persisted, never logged.
      Refuse to issue a session for a connection in `Disconnected`.
- [ ] **Audit** — connection creation and every session issuance recorded with actor, tenant and IP.

## Acceptance criteria

- [ ] A sandbox test creates an account and asserts **every** dimension in the table above on the
      returned object, including a negative assertion that no v1 `type` field was sent.
- [ ] Two **concurrent** create calls yield exactly one Stripe account and one row (run concurrently,
      not sequentially).
- [ ] Orphan drill: killing the process between Stripe creation and persistence, then retrying,
      adopts the account rather than duplicating it.
- [ ] An expired session secret is rejected, and a session is refused for a disconnected connection.
- [ ] No account session secret appears in logs — asserted against a captured log sink.
- [ ] Every Stripe error class maps to its own `payments/*` problem type, with the Stripe request id
      logged and no Stripe error body in the response.

## Verification

```bash
just fmt
just clippy
just layering
just connect-invariants
cargo test -p sanvi-payments
just test
just openapi
```

## Out of scope

Webhooks and truthful status (TASK-004), checkout (TASK-005), and the `notification_banner` /
`account_management` / `payouts` components on the Account Session (TASK-004, TASK-007).

## Files likely touched

- `crates/contexts/payments/src/adapters/stripe_connect/**`
- `crates/contexts/payments/src/application/{create_connection,account_session}.rs`
- `crates/contexts/payments/src/domain/error.rs`
- `api/payments.yaml`, `configs/**`

## Notes / gotchas

- Rollback does **not** delete accounts at Stripe. Flag off hides the entry point; mark connections
  `Pending` and let TASK-004's sync reconcile them. An account with no charges is harmless.
- The idempotency key is our connection id, generated before the Stripe call — not a random per-request
  value, or the orphan-adoption path cannot work.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-003 done --note "<PR or commit>"`
