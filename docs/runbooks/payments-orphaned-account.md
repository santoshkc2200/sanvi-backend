# Runbook — orphaned account adoption

**Alert:** `UnknownAccountEvents` — `increase(payments_webhook_unknown_account_total[1h]) > 0`
(see [payments-alerts](./payments-alerts.md)).

A Connect webhook named an account Sanvi does not know. The dispatcher
records it, alerts, and **never provisions**. An unknown account is one of:

1. **A leaked/misconfigured webhook endpoint** (another Stripe app pointing
   at us),
2. **An orphan from a crashed create** (account created at Stripe, row never
   persisted), or
3. **A test/sandbox account** from a misconfigured environment.

## 1. Identify the account

```sql
SELECT stripe_event_id, type, account_id, received_at
FROM payments.webhook_events
WHERE account_id NOT IN (
    SELECT external_account_id FROM payments.provider_connections
)
ORDER BY received_at DESC LIMIT 50;
```

## 2. Decide

- **Orphan adoption (case 2):** if a tenant reports "we connected but
  onboarding vanished" around the event time, adopt, don't recreate: the
  retry of their connect request already adopts the same account by design
  (`connection_id_for` is deterministic per `(tenant, Idempotency-Key)` and
  is the provider-side idempotency key). Have them click reconnect with the
  same session; verify a new `pending` row appears for that account id in
  the next sync.
- **Leaked endpoint (case 1):** events keep arriving for accounts that no
  tenant claims. Rotate nothing client-side; instead confirm the endpoint's
  secret allow-list (`payments.connect_webhook_secret` differs from every
  other Stripe endpoint secret) and IP allow-list
  (`payments.webhook_allowed_clients`). If events are *verifying* against our
  secret, someone has our secret — rotate it and follow the audit chain.

## 3. Clean up

Unknown-account events are marked processed on arrival; they need no replay.
If volume is large after an incident, they may be archived out with counsel's
sign-off — but money-shaped events (`payment_intent.*`, `payout.*`,
`charge.*`) must first be proven not to belong to any tenant we know.

## Never

- Never auto-create connection rows from webhooks — provisioning is always a
  tenant-initiated, audited act.
