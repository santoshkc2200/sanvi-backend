# Runbook — accidental fee enablement rollback

**Trigger:** the invariant alarm `PaymentsPlatformBalanceMovement`
(see [payments-alerts](./payments-alerts.md)) — an application fee or
transfer shape observed on a tenant transaction — or a platform-admin
misconfiguration of `platform_admin.application_fees`.

## Why this is sev-1

Sanvi is never the merchant of record and takes **no fee** in 0.10.0 (the
platform monetises via subscription). A fee on a tenant's direct charge moves
money from the tenant to the platform balance: it breaks the phase-09
invariant, creates unearned revenue, and is exactly what the alarm exists
to catch. The dispatcher increments
`payments_platform_balance_movements_total` and logs
`PLATFORM BALANCE MOVEMENT on a tenant transaction` with the account id.

## 1. Stop the bleeding (minutes)

1. Platform admin console (or SQL read-only check first):
   ```sql
   SELECT tenant_id, enabled, updated_at, updated_by
   FROM platform_admin.application_fees WHERE enabled;
   ```
2. Disable the offending row (`enabled = false`) through the platform-admin
   API — never by hand-editing; the table keeps history and the audit chain.
   Disabling per tenant needs no deploy (configuration is data).
3. If the master switch was flipped, flip it back. The checkout path reads
   the configuration per request, so the next session carries no fee.

## 2. Assess the damage

```sql
-- Tenant payments that reported a platform fee:
SELECT tenant_id, external_payment_id, fee_amount_minor, fee_currency,
       created_at
FROM payments.payments
WHERE fee_amount_minor IS NOT NULL AND fee_amount_minor > 0
ORDER BY created_at DESC;
```

Cross-check each against Stripe (balance transactions on the connected
account). Every affected payment needs a decision recorded with finance:

- **Refund the fee** to the tenant via a Stripe transfer/credit from the
  platform account (this is the one case where platform money legitimately
  moves: repairing an invariant violation), or
- offset against the tenant's subscription with their written agreement.

## 3. Root cause

The fee rides `payment_intent_data.application_fee_amount`, set only from
`ApplicationFeePolicy`. In 0.10.0 the shipped policy is `NoApplicationFee`
and cannot produce a fee; so a violation means either:

- the per-tenant configuration store returned a config when none should
  exist (data problem — who updated the row? `updated_by` + audit), or
- code regressed (policy bypassed). Run
  `./scripts/check-connect-invariants.sh` and the TASK-005 release-default
  tests; both must pass.

## 4. Prove it's over

- Alarm stops firing: `increase(payments_platform_balance_movements_total[15m]) == 0`.
- New sessions carry no fee (create one sandbox checkout, inspect at Stripe).
- Write up the incident; append the reconciliation to the audit chain.

The rollback order note from [the outage runbook](./payments-stripe-outage.md)
applies if you must go further: checkout flag off first, Connect flag second,
webhooks never.
