# Runbook — Stripe API version upgrade drill

Every future Stripe API version bump runs this drill **before** production.
It turns "we hope the new pinned version parses" into a verified statement:
stored Connect webhook payloads — the real shapes Stripe sent us — are
replayed against the candidate build's handlers.

Stripe pins our requests via `payments.stripe_api_version` (kept in lockstep
with `billing.stripe_api_version`); what changes underneath us is the
*event/payload* shapes Stripe sends back. The transport stores those payloads
verbatim (`payments.webhook_events.payload`, with the reported
`api_version`), which is exactly what makes this drill possible.

## The drill

### 0. Preconditions

- A staging environment with its own Stripe sandbox and webhook endpoint
  secret (`payments.connect_webhook_secret`, distinct from production's).
- The current build has been receiving events long enough to hold a corpus
  covering every handled type: `account.updated`, `capability.updated`,
  `v2.core.account…`, `checkout.session.*`, `payment_intent.*`,
  `charge.refunded`, `refund.*`, `charge.dispute.*`, `payout.*`,
  `tax.settings.updated`. If a type is missing from storage, synthesize one
  from Stripe's docs **and say so in the drill report**.

### 1. Snapshot the corpus (current version)

```bash
psql "$SANVI_DB_URL" -c "
  COPY (
    SELECT stripe_event_id, type, api_version, payload
    FROM payments.webhook_events
    WHERE type IN ('account.updated','capability.updated',
                   'checkout.session.completed','checkout.session.expired',
                   'payment_intent.succeeded','payment_intent.payment_failed',
                   'payment_intent.canceled','charge.refunded','refund.created',
                   'refund.updated','charge.dispute.created',
                   'charge.dispute.closed','payout.paid','payout.failed')
      AND error IS NULL
  ) TO STDOUT WITH (FORMAT csv)"
  > webhook-corpus-<old-version>.csv
```

Record counts per type — the replay must account for every row.

### 2. Build the candidate

Bump both pins together (drift here means request shapes and webhook shapes
disagree):

```toml
# payments + billing config: stripe_api_version = "<new version>"
```

Deploy the candidate to staging with the OLD data untouched.

### 3. Replay the corpus into staging

Load each payload as a **fresh** event (suffixed id keeps the primary-key
dedupe out of the way) so the dispatcher treats them as new work:

```sql
INSERT INTO payments.webhook_events
       (stripe_event_id, account_id, type, api_version, payload,
        received_at, attempts)
SELECT stripe_event_id || '-drill', account_id, type, api_version, payload,
       now(), 0
FROM payments.webhook_events
WHERE error IS NULL AND processed_at IS NOT NULL;
-- scope with the same WHERE list as step 1
```

Then drain:

```bash
cargo run -p sanvi-cli -- payments webhooks   # watch depth go to zero
cargo run -p sanvi-cli -- payments sync       # provider-side reconciliation too
```

### 4. Judge the result

A pass means **every** replayed event either applied or skipped for a
*documented* reason:

```sql
-- Failures: any attempt burned on the candidate is a finding.
SELECT stripe_event_id, type, error FROM payments.webhook_events
WHERE stripe_event_id LIKE '%-drill' AND error IS NOT NULL;

-- Money truth: projections moved exactly once per transitioned event
-- (handlers are idempotent; totals should match the pre-drill baseline
-- plus one application per replayed money event).
```

- Zero failed attempts, projections sane, metrics incrementing
  (`payments_payment_succeeded_total`, refund/payout counters) → **pass**;
  record versions, corpus size, date and operator in the release notes.
- Failed attempts → the bump found a real shape change: fix the handler,
  redeploy, wipe `%-drill` rows, replay again. Never ship a bump with red
  drill output.

### 5. Clean up & roll forward

```sql
DELETE FROM payments.webhook_events WHERE stripe_event_id LIKE '%-drill';
```

(Replay rows only; never purge live history.) Then roll the version bump
through the normal release train. If production must roll **back** after a
bump, revert both pins together and run this drill on the reverted pair —
a downgrade is an upgrade in reverse.

## Why stored payloads and not live traffic

Live post-bump events arrive once, uncontrolled, and their handling mistakes
are silent until reconciliation catches them. The corpus is known, counted,
and repeatable — the same property the DLQ replay leans on
([webhook backlog](./payments-webhook-backlog.md)).
