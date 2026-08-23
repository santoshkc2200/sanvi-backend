# Runbook — webhook backlog & dead-letter queue (DLQ) replay

**Alerts:** `WebhookBacklog`, `WebhookDeadLetterDepth`, `WebhookLagHigh`
(see [payments-alerts](./payments-alerts.md)).

The Connect transport is store-first: `/internal/webhooks/stripe/connect`
verifies and stores each event (dedupe on the provider event id), answers 2xx,
and the worker's dispatcher drains unprocessed rows one transaction each. An
event that exhausts 5 attempts is **dead-lettered** — it stops being fetched
so a poison event never blocks the queue.

## 1. Assess

```bash
cargo run -p sanvi-cli -- payments webhooks
```

prints `unprocessed=<n> dead_letter=<n>` plus the DLQ head (id, account,
type, attempts, last error). A growing *unprocessed* count with no failures
means throughput trouble; dead letters mean specific events are failing.

```sql
SELECT stripe_event_id, type, received_at, processed_at, attempts, error
FROM payments.webhook_events
WHERE processed_at IS NULL AND attempts < 5
ORDER BY received_at;
```

- **Lag** (`payments_webhook_lag_seconds` p95 > 300s) without depth growth:
  usually a burst; watch, don't act.
- **Depth growth**: check worker logs for `connect webhook event processing
  failed`; if errors are all provider-unavailable follow
  [Stripe outage](./payments-stripe-outage.md).

## 2. Fix the root cause first

Replay re-runs the same handler on the stored payload; it is only useful once
whatever made it fail is fixed (bug deployed, provider recovered, dependency
restored). The stored payload is verbatim — replay after a Stripe **API
version bump** is exactly the drill in
[stripe-api-version-upgrade-drill](./stripe-api-version-upgrade-drill.md).

## 3. Replay

```bash
# One event:
cargo run -p sanvi-cli -- payments replay evt_1AbC...
```

`replay` clears the attempt budget so the dispatcher picks the row up again.
`Ok(false)` means it was not dead-lettered (nothing to do). Handlers are
idempotent (upserts keyed by provider ids, forward-only state machines), so
replaying an already-applied event is safe.

## 4. Verify

- `payments webhooks` shows the event gone from the DLQ head.
- No new `payments.webhook_dead_letter_gauge` increase.
- For money events, confirm the projection: the payment/refund/dispute/payout
  row exists and `platform.outbox` carries its event exactly once
  (transition-guarded).

## Never

- Never delete rows from `payments.webhook_events` to "clear" a backlog —
  a money event deleted unprocessed is money with no local record.
- Never bypass signature verification or hand-insert events.
