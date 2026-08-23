# Runbook — failed payout

**Alert:** `PayoutFailed` — `increase(payments_payout_failures_total[5m]) > 0`
(see [payments-alerts](./payments-alerts.md)). Any increment pages: a failed
payout is the **tenant's money stuck at the provider**.

## What happened

Stripe attempted to pay out the connected account's balance to the tenant's
bank and failed (bank error, account closed, compliance hold). Sanvi records
the event (`payments.payouts`, status `failed`), notifies the tenant's admins,
and keeps every payment's `payout_status` truthful via balance-transaction
reconciliation. Sanvi never retries or re-schedules payouts — that is the
provider's/tenant's job.

## 1. Identify

```sql
SELECT p.tenant_id, p.external_payout_id, p.amount_minor, p.currency,
       p.updated_at, c.external_account_id
FROM payments.payouts p
JOIN payments.provider_connections c ON c.id = p.connection_id
WHERE p.status = 'failed'
ORDER BY p.updated_at DESC;
```

The worker log line `payout failed — the tenant's money is stuck at the
provider` carries tenant, payout id and amount.

## 2. Act

1. Confirm in Stripe's dashboard (or API) why the payout failed.
2. The tenant fixes the root cause with Stripe/bank — usually re-adding their
   bank account in Stripe's dashboard; payouts resume automatically on the
   next schedule.
3. Tell the tenant their money is safe at Stripe and will re-pay out once the
   bank details are fixed. Do not promise a date — Stripe's schedule owns it.

## 3. Disconnect interplay

A pending or failed-but-unsettled payout blocks disconnect
(`pending_payouts` blocker). If a tenant insists on disconnecting with a
*failed* payout outstanding, direct them to Stripe support first: the money
belongs to their account, and once Stripe resolves it (re-payout or refund to
balance) the guard clears. Never clear guards by hand.

## 4. Reconciliation backstop

If `payout.paid` never arrives after a fix, force a sync pass
(`cargo run -p sanvi-cli -- payments sync`) which re-fetches account state;
the payment list's `payout_status` repairs from balance-transaction links.
