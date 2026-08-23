# Runbook — Stripe outage / degraded mode

**Alert:** `CheckoutFailureSpike` (failure ratio spike) plus adapter error
logs (`payment provider unavailable`). See
[payments-alerts](./payments-alerts.md).

## Failure modes and what the system already does

| Where | Behaviour |
|---|---|
| Checkout creation | The use case returns `502 payments/provider-unavailable` after its own backoff; the storefront shows "temporarily unavailable". Nothing is persisted for the failed attempt (the checkout row commits only with a session or as a recoverable pending row — retries adopt it by Idempotency-Key). |
| Webhook processing | Events are stored first, then processed; a provider fetch failure fails the *attempt*, consuming one of 5 retry budgets. Processing resumes automatically when Stripe recovers. |
| Mid-onboarding | A failed status fetch during connection create logs a warning and the row stays `Pending`; the reconciliation job (`payments sync`) fills truth in later. |

The design goal: Stripe being down delays truth, never loses money. Money
taken at Stripe always has (or will have) a local record via the stored
webhook.

## 1. Confirm

- Check Stripe's status page / dashboard notices.
- `increase(payments_webhook_events_total{result="failed"}[10m]) > 0` with
  provider-unavailable errors confirms degraded processing.
- One manual probe: `cargo run -p sanvi-cli -- payments sync` — errors here
  are the adapter failing at Stripe.

## 2. During the incident

- **Do not** flip any feature flag. Checkout answering 502 is correct;
  webhook endpoints must stay deployed and receiving (store-first keeps
  working if only API reads fail).
- Watch `payments_webhook_unprocessed_gauge`: a growing backlog during the
  outage is expected; see [webhook backlog](./payments-webhook-backlog.md)
  if it does not drain after recovery.
- Communicate: tenants see per-request 502s already; an incident note goes to
  operators' status channel.

## 3. Recovery

No manual repair: retries drain the queue, reconciliation re-syncs accounts.
Verify:

```sql
SELECT count(*) FROM payments.webhook_events WHERE processed_at IS NULL AND attempts < 5;
```

trending to 0, checkout creations succeeding again, and
`payments_connection_status_gauge` stable. If dead letters accumulated,
follow [DLQ replay](./payments-webhook-backlog.md).

## Rollback order (phase-level)

If a payments defect forces flags off: `payments.checkout` first (stop new
money), then `payments.stripe_connect` (stop new connections). **Never** roll
back the webhook endpoint or handlers — in-flight payments must keep being
recorded or money moves with no local record.
