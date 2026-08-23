# Runbook — dispute handling

**Alert:** `DisputeOpenAging` — the open-dispute gauge with a deadline inside
48h (see [payments-alerts](./payments-alerts.md)).

The tenant is merchant of record, so disputes land on **their** Stripe
account and Stripe owns the workflow. Sanvi stores the state and the response
deadline; evidence never touches Sanvi.

## 1. Identify

```sql
SELECT d.external_dispute_id, d.status, d.amount_minor, d.currency,
       d.reason, d.due_by, p.external_payment_id,
       c.tenant_id, c.external_account_id
FROM payments.disputes d
JOIN payments.payments p ON p.id = d.payment_id
JOIN payments.checkouts ck ON ck.id = p.checkout_id
JOIN payments.provider_connections c ON c.id = ck.connection_id
WHERE d.status IN ('needs_response', 'under_review',
                   'warning_needs_response', 'warning_under_review')
ORDER BY d.due_by NULLS LAST;
```

The tenant dashboard surfaces the same rows (`GET /api/v1/tenant/payments/disputes`)
and the tenant admin was notified at open time (with the deadline when Stripe
supplies one).

## 2. Act

1. **Deadline first**: `due_by` is the evidence deadline. Inside 48h is page-
   worthy; after it passes Stripe decides automatically and the tenant loses
   by forfeit.
2. Point the tenant at their Stripe dashboard — submitting evidence is theirs;
   Sanvi deliberately has no evidence upload or proxy.
3. Watch the row close itself: `charge.dispute.closed` (and friends) move the
   projection to `won` / `lost`. The open gauge decrements on the transition.

## 3. Disconnect interplay

An open dispute blocks disconnect (`open_disputes` blocker): losing the live
connection would cost the tenant their response window. Once the dispute
closes in any outcome, the guard clears.

## 4. If a dispute arrives for an unknown payment

The webhook logs `dispute for unknown payment; skipped` — usually an account
adopted outside Sanvi or a payment predating phase 09. Follow
[orphaned account adoption](./payments-orphaned-account.md) to decide whether
the account should exist here at all.
