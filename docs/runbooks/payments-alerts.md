# Payments alert rules (phase 09)

The metric set every alert reads lives in `crates/contexts/payments/src/adapters/metrics.rs`
(`payments_*`, exposed on `/metrics`). This file is the canonical list of
phase-09 alerts: name, condition, severity, and the runbook that tells you
what to do when one fires. Wire them into the monitoring stack verbatim;
a rule here without a runbook section is a bug.

## The invariant alarm

| Alert | Condition | Severity |
|---|---|---|
| `PaymentsPlatformBalanceMovement` | `increase(payments_platform_balance_movements_total[5m]) > 0` | **page immediately** |

Sanvi is never the merchant of record: direct charges land on the tenant's
connected account and funds never touch the platform balance. Any movement
detected from a tenant transaction — an application fee or transfer shape
appearing on a charge event — is by construction impossible, which is why
it pages instead of logging. Treat every firing as a sev-1: freeze fee/Connect
configuration changes, identify the tenant/account from the log line
(`PLATFORM BALANCE MOVEMENT on a tenant transaction`), reconcile the amounts,
and follow [the accidental-fee rollback runbook](./payments-accidental-fee-rollback.md).

## Connection lifecycle

| Alert | Condition | Severity | Runbook |
|---|---|---|---|
| `ConnectionsStuckOnboarding` | `max_over_time(payments_connection_status_gauge{status="onboarding"}[7d]) > 0` for accounts whose `last_synced_at` is > 7 days old (`SELECT count(*) FROM payments.provider_connections WHERE status = 'onboarding' AND disconnected_at IS NULL AND last_synced_at < now() - interval '7 days'`) | ticket | [restricted account](./payments-restricted-account.md) |
| `ConnectionRestrictedRegression` | `increase(payments_webhook_events_total{result="processed"}[5m]) > 0 and increase(payments_connection_status_gauge{status="restricted"}[10m]) > 0` (any new Restricted row) | ticket per tenant | [restricted account](./payments-restricted-account.md) |
| `TenantDisconnected` | `increase(payments_connection_disconnects_total{result="ok"}[1h]) > 0` | info (funnel signal) | — |

## Webhook transport

| Alert | Condition | Severity | Runbook |
|---|---|---|---|
| `WebhookBacklog` | `payments_webhook_unprocessed_gauge > 100` for > 15m | page | [webhook backlog & DLQ replay](./payments-webhook-backlog.md) |
| `WebhookDeadLetterDepth` | `payments_webhook_dead_letter_gauge > 0` | page | [webhook backlog & DLQ replay](./payments-webhook-backlog.md) |
| `WebhookLagHigh` | p95 of `payments_webhook_lag_seconds` > 300s for > 15m | ticket | [webhook backlog](./payments-webhook-backlog.md) |
| `UnknownAccountEvents` | `increase(payments_webhook_unknown_account_total[1h]) > 0` | ticket | [orphaned account adoption](./payments-orphaned-account.md) |

## Money

| Alert | Condition | Severity | Runbook |
|---|---|---|---|
| `PayoutFailed` | `increase(payments_payout_failures_total[5m]) > 0` | page (tenant money stuck) | [failed payout](./payments-failed-payout.md) |
| `CheckoutFailureSpike` | failure ratio `sum(rate(payments_payment_failure_total[10m])) / sum(rate(payments_payment_succeeded_total[10m]) + sum(rate(payments_payment_failure_total[10m]))) > 0.3` while baseline < 0.1 | page if sustained 30m | [Stripe outage / degraded mode](./payments-stripe-outage.md) |
| `DisputeOpenAging` | `payments_dispute_open_gauge > 0` with the oldest open dispute's `due_by` within 48h | ticket | [dispute handling](./payments-dispute.md) |

## Notes

- Gauges (`connection_status`, `dispute_open`, webhook depths) are reset and
  recomputed each pass; counters are transition-guarded so redeliveries do not
  inflate them.
- The stuck-Onboarding alert deliberately uses both the gauge and the database
  predicate: the gauge shows fleet shape, the query names the rows to chase.
