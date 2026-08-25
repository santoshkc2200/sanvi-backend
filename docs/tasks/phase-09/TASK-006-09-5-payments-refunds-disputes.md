# TASK-006: 09.5 Payments, refunds & disputes

**Phase:** 09
**Status:** done
**Requirement(s):** FR-906, FR-907, NFR-904, NFR-907
**Depends on:** TASK-005
**Created:** 2026-08-20

**Slice:** 09.5 — Payments, refunds & disputes
**Prerelease:** `v0.10.0-alpha.6` · **Flag:** `payments.checkout` · **Parallel with:** TASK-007

## Context

The tenant can see every payment, refund one, and know a dispute exists. Refunds are the most
dangerous operation in the phase — irreversible, arithmetic-sensitive and currency-sensitive — so they
get permission gating, idempotency, an exact-amount response and an audit record with a reason.

Disputes are **read-only with a Dashboard deep link** in this release. Proxying evidence submission
would mean holding evidence documents, contradicting "no PII at Sanvi".

## What to do

- [ ] **Contract** — add to `api/payments.yaml`, merged first:
      | Method | Path | Permission |
      |---|---|---|
      | GET | `/api/v1/tenant/payments` | `payments.read` — filters: date range, status, customer, currency; cursor paginated |
      | GET | `/api/v1/tenant/payments/{id}` | `payments.read` — timeline, refunds, dispute state |
      | POST | `/api/v1/tenant/payments/{id}/refund` | `payments.refund` — `Idempotency-Key` required |
      | GET | `/api/v1/tenant/payments/disputes` | `payments.read` |

      Refund request: `{ amount: { minor_units, currency } | null /* null = full */, reason, note }`.
      The response is the refund record, never a bare 204 — a partial refund of an unexpected amount
      is a support incident, so the UI must be able to show what actually happened.
- [ ] **Payment read model** — projected from the TASK-005 webhook handlers, never fetched from Stripe
      per request. List and detail queries are tenant-scoped by RLS and indexed for the filter set.
- [ ] **Timeline** — a first-class projection derived from stored events (created, succeeded/failed,
      refunded, disputed, paid out), each with a timestamp and an actor where one exists. This is the
      artifact support uses; do not derive it in the UI.
- [ ] **Refund use case**
      - `payments.refund` **and** an audit reason are both required.
      - Arithmetic in minor units against the already-refunded total; an over-refund is rejected with
        the remaining refundable amount in the error, not a generic 422.
      - `Idempotency-Key` passed through to Stripe **and** enforced locally, so a retried request
        never issues a second refund even if our first response was lost.
      - Executed on the connected account — the refund is the tenant's money moving, not ours.
      - Emits `RefundIssued`; audited with actor, amount, currency and reason.
- [ ] **Refund webhooks** — `charge.refunded` reconciles the local record with Stripe's truth,
      including refunds initiated directly in the tenant's Stripe Dashboard. Tenants have
      `dashboard: "full"`; they *will* refund outside our UI, and the read model must not go stale.
- [ ] **Dispute projection** — `charge.dispute.created` / `charge.dispute.closed` → dispute state on
      the payment, plus a `DisputeOpened` event and a tenant notification. Store status, amount,
      reason and the response deadline; store **no evidence**.
- [ ] **CSV export** — server-side, streamed, tenant-scoped, audited. The column set is fixed and
      documented: it will be pasted into accounting software, and changing columns later breaks
      tenants' workflows.

## Acceptance criteria

- [ ] A retried refund request with the same key issues exactly one refund at Stripe.
- [ ] Partial-refund arithmetic is correct in JPY (zero-decimal) and USD, including a sequence of
      partials summing to the total and one that would exceed it.
- [ ] An over-refund rejection reports the remaining refundable amount.
- [ ] A role without `payments.refund` receives a 403 from the API.
- [ ] A refund issued in Stripe's Dashboard appears in the read model via webhook, with no manual
      intervention.
- [ ] A dispute lifecycle from sandbox fixtures shows the payment disputed and then closed, with no
      evidence stored by us.
- [ ] CSV export is tenant-scoped (a cross-tenant probe returns nothing) and audited.
- [ ] The list endpoint serves every documented filter and cursor-paginates.

## Verification

```bash
just fmt
just clippy
just layering
just connect-invariants
cargo test -p sanvi-payments
just test
just openapi
just audit-verify
```

## Out of scope

Payouts, tax liability and the application fee (TASK-007); disconnect guards (TASK-008); any
dispute-evidence handling at all.

## Files likely touched

- `crates/contexts/payments/src/application/{refund,list_payments,export}.rs`
- `crates/contexts/payments/src/adapters/repos.rs` (read models, indexes)
- `crates/contexts/payments/src/adapters/http/{handlers,routes,webhooks}.rs`
- `migrations/` (read-model indexes, if the TASK-001 schema needs them)
- `api/payments.yaml`

## Notes / gotchas

- Rollback: remove the `payments.refund` permission rather than redeploying if the arithmetic is
  suspect — tenants can still refund in their own Stripe Dashboard, which is the honest fallback.
- The read model is derived and can be rebuilt from `payments.webhook_events`; keep that property.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
