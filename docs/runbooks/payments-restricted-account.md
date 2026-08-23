# Runbook — restricted / regressed connected account

**Alerts:** `ConnectionsStuckOnboarding`, `ConnectionRestrictedRegression`
(see [payments-alerts](./payments-alerts.md)).

A connection is `Restricted` when requirements are past due or the
`card_payments` capability left `active`; it is stuck `Onboarding` when the
tenant started but never finished. Both mean the tenant cannot take payments.
The tenant is merchant of record: only they can supply what Stripe asks for.

## 1. Identify

```sql
SELECT id, tenant_id, external_account_id, status, last_synced_at,
       requirements->'past_due' AS past_due,
       capabilities->'card_payments' AS card_payments
FROM payments.provider_connections
WHERE disconnected_at IS NULL
  AND (status IN ('onboarding', 'restricted')
       OR last_synced_at < now() - interval '7 days');
```

## 2. Triage

- **Past-due requirements:** the row's `requirements.past_due` lists Stripe
  requirement codes. The tenant has already been notified (deadline reminders
  fire within 7 days of a due date). If the deadline passed, escalate to the
  tenant admin — Sanvi cannot and must not submit provider PII on their behalf.
- **Capability regression** (`card_payments != active`): check the Connect
  webhook stream for the triggering event (`capability.updated`,
  `v2.core.account…`). Common causes: failed KYC refresh, changed business
  data, Stripe-initiated review. The account status endpoint
  (`GET /api/v1/tenant/payments/connections/{id}`) shows the same truth to
  the tenant.
- **Stuck > 7 days with no recent sync**: force one pass — `just payments-sync`
  (`cargo run -p sanvi-cli -- payments sync`). If sync errors, follow
  [Stripe outage](./payments-stripe-outage.md).

## 3. Recovery

Recovery is webhook-driven: once the tenant fixes their Stripe account, the
next event (or reconciliation pass) moves the row back through the legal
transitions (`Restricted → Active`). There is no manual SQL to write — never
hand-edit statuses; the transition table will refuse illegal moves and the
audit chain will show yours.

## 4. If the tenant wants out instead

Point them at disconnect (settings page or `DELETE
/api/v1/tenant/payments/connections/{id}`). The API refuses while money is in
motion and names the blockers; see [dispute handling](./payments-dispute.md)
and [failed payout](./payments-failed-payout.md) for clearing those.
