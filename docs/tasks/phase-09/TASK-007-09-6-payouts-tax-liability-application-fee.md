# TASK-007: 09.6 Payouts, tax liability & application fee

**Phase:** 09
**Status:** done
**Requirement(s):** FR-908, FR-909, NFR-901
**Depends on:** TASK-004, TASK-005
**Created:** 2026-08-20

**Slice:** 09.6 — Payouts, tax liability & application fee
**Prerelease:** `v0.10.0-alpha.7` · **Flag:** `payments.stripe_connect` · **Parallel with:** TASK-006

## Context

Close the three remaining money questions: *where does the money go* (payouts), *who owes the tax*
(the connected account), and *does Sanvi take a cut* (it can, and in 0.10.0 it does not).

All three are places where the wrong default is a legal problem rather than a bug, so each ships with
its position stated in the API and the UI, not only in code.

## What to do

- [ ] **Contract** — add to `api/payments.yaml`:
      | Method | Path | Permission |
      |---|---|---|
      | GET | `/api/v1/tenant/payments/payouts` | `payments.read` |
      | GET | `/api/v1/tenant/payments/tax-settings` | `payments.read` |
      | PUT | `/api/v1/tenant/payments/tax-settings` | `payments.manage` — enable/disable automatic tax |

      Per-tenant application-fee configuration lives in the **platform-admin** context, not the tenant
      API — a tenant cannot set the platform's own fee. That contract change goes in
      `api/platform_admin.yaml`.

### Payouts (read-only)

- [ ] `PayoutReadPort` implementation over the connected account: schedule, next payout, recent
      history. We do not reimplement Stripe's payout UI — the embedded component is the primary
      surface; this endpoint exists for the list view and for support.
- [ ] `payout.paid` / `payout.failed` webhook handlers → `PayoutPaid` event, payout status on the
      payment read model (the "payout status" column in TASK-006's list), and a tenant notification on
      failure. A failed payout means the tenant's money is stuck: it is an alert, not a row.

### Tax liability

- [ ] **Liability routing** — with direct charges the merchant of record is the connected account, so
      when a tenant enables automatic tax we set
      `automatic_tax.liability = { type: 'account', account: acct_id }` on the session.
- [ ] **Preflight** — before enabling, confirm the connected account's tax settings `status` is
      `active` and that registrations exist. Without them **no tax is collected and Stripe returns no
      error** — a silent failure that surfaces months later as an unpaid liability. If the preflight
      fails, the toggle stays off and the API says exactly which step is missing.
- [ ] **Ongoing check** — re-verify on a schedule and when tax-related account events arrive; a tenant
      whose registrations lapse gets warned rather than silently under-collecting.
- [ ] **We never advise** — the API responses and copy state that registration and remittance are the
      tenant's responsibility, and point at their tax advisor. This is a legal determination, not a
      feature.

### Application fee

- [ ] **Per-tenant / per-plan fee configuration** in platform-admin: basis points and/or a fixed
      amount, with a currency-aware minimum. Default **disabled**; 0.10.0 ships with it off because we
      monetise via subscription.
- [ ] **Application at checkout** — when enabled, compute `application_fee_amount` in minor units and
      set it on `payment_intent_data` for the direct charge. Rounding is defined once and documented
      (round half up on the fee, never on the total); zero-decimal currencies use the same code path.
- [ ] **Blocked combinations** — an assertion and a test that `application_fee_amount` is never used
      with separate charges and transfers, and that `losses_collector: "stripe"` is never paired with
      destination or separate charges.
- [ ] **Disclosure** — when a fee is enabled for a tenant it is exposed to that tenant in their
      payment settings and on the payment detail. A platform fee the merchant cannot see is not
      acceptable, whatever the contract says.

## Acceptance criteria

- [ ] A sandbox payout paid and a payout failed are both reflected; the failure raises an alert.
- [ ] An account with no registrations cannot enable automatic tax, and the reason returned is
      specific rather than generic.
- [ ] A session created with automatic tax carries `liability.type == 'account'` with the connected
      account id — asserted on the created object.
- [ ] Lapsed registrations trigger the warning path on the scheduled re-check.
- [ ] **Release default:** no `application_fee_amount` on any created PaymentIntent — the same
      assertion TASK-005 makes, kept here as a regression guard.
- [ ] Fee on: minor-unit computation and rounding are correct in USD and JPY, the fee appears on the
      charge in the sandbox, and the remainder is the tenant's.
- [ ] The blocked-combination assertions fail the build if either pattern is introduced.

## Verification

```bash
just fmt
just clippy
just layering
just connect-invariants
cargo test -p sanvi-payments
just test
just openapi
just billing-tax-check
```

## Out of scope

Refunds and disputes (TASK-006), disconnect (TASK-008), and any tax advice — including "helpful"
suggestions about which registrations a tenant needs.

## Files likely touched

- `crates/contexts/payments/src/application/{payouts,tax_settings,fees}.rs`
- `crates/contexts/payments/src/adapters/stripe_connect/{payouts,tax}.rs`
- `crates/contexts/payments/src/adapters/http/webhooks.rs` (payout handlers)
- `crates/contexts/platform_admin/**` (fee configuration), `api/platform_admin.yaml`
- `api/payments.yaml`, `configs/**`

## Notes / gotchas

- Rollback: fee configuration is data — disable per tenant with no deploy. Tax can be toggled off per
  tenant, but disabling automatic tax mid-period changes what customers are charged; treat it as a
  tenant-facing change requiring notice, not a silent flag flip.
- The preflight is the whole point of the tax work. An enabled toggle that collects nothing is worse
  than a disabled one.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
