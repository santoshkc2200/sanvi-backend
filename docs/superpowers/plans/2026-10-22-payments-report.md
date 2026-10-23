# Payments architecture remediation

Task 2 owns `crates/contexts/payments` and additive migration
`20261022000200_payments_idempotency_audit{.down}.sql`.

## Changes

- Checkout requests persist a SHA-256 digest of a canonical JSON request:
  normalized order reference, currency and locale; amount; recursively ordered
  customer context; success/cancel URLs. Both initial key lookup and concurrent
  winner adoption validate it. Changed payloads return 409 before provider work.
  Pending retries no longer replace the original customer context.
- Preexisting checkouts have no recoverable redirect snapshot, so replaying
  those keys fails closed with 409 and directs callers to retrieve the original
  checkout. GET/read models and existing hosted sessions continue to work.
- Refunds persist an immutable audit authorization snapshot in the same scoped
  transaction as the Pending refund, before a provider call. It includes the
  original actor, reason, note, IP, payment, amount, currency, provider/account
  and idempotency key.
- Pre-migration Pending/Failed refunds stage recovered attribution before a
  provider retry, using the refund's original stored actor/reason. Missing
  historical note/IP remain explicitly null; an original actor missing from the
  record blocks re-driving and requires reconciliation.
- Finalization and webhook-winner adoption mark the retained audit ready in the
  state/outbox transaction. Winner adoption moves original attribution before
  deleting the superseded row.
- API finalization and webhook workers serialize on a transaction-scoped
  provider-refund ID lock before looking up or correlating a row, including
  previously absent IDs. Webhook persistence reports its actual SQL transition
  to Succeeded and preserves terminal success; earlier reads cannot cause a
  duplicate `RefundIssued` event or metric.
- Stripe refund requests carry local refund ID/key metadata. Signed webhook
  processing binds the hint to tenant, payment, amount, currency, key and any
  known external ID, allowing provider success followed by an API crash to
  recover the original local row and its audit without another HTTP request.
- `DrainRefundAudits::new(TxManager, Arc<dyn RefundRepository>, Arc<dyn AuditWriter>)`
  exposes `execute(limit: u32) -> Result<usize, PaymentsError>`. It locks ready
  refund rows, appends the audit chain and marks delivery in one PlatformScope.
  A crash/failure rolls back both append and marker. API delivery is opportunistic;
  the worker must schedule this use case (root owns composition).
- Existing refund RLS, financial retention and offboarding cover the added
  snapshot fields; no additional table, provider, service or credential is added.
- Checkout OpenAPI annotations now describe payload conflicts. Root owns
  regenerating the committed OpenAPI output.

## Verification

- Checkout regression was run against saved HEAD payments sources (restored in
  `finally`): changed reference returned 200 instead of expected 409.
- Full shared-harness payments integration suite: **71 passed**, including
  provider-success/crash recovery, signed webhook metadata, uncorrelated webhook
  winner adoption, audit append rollback, independent retry, original actor/note/IP
  preservation, pre-migration Pending retry attribution and additive migration
  roundtrip.
- Independent final review identified a duplicate-event race between API
  finalization and a correlated webhook. A deterministic regression holds the
  API transition/outbox transaction open, observes the webhook's actual database
  lock wait, then commits it. Before the fix it emitted **2** events instead of
  **1** (observed RED); it passes after serialized/conditional persistence.
  A second regression runs two webhook workers concurrently and verifies both
  single publication and preservation of success through late Pending and
  repeated Succeeded notifications.
- The legacy pending-refund regression was observed RED before implementing its
  fallback: provider retry left the audit snapshot absent. It passes in the final
  full integration run.
- Payments library unit suite: **121 passed**.
- `cargo clippy -p sanvi-payments --all-targets -- -D warnings`: **passed**.
- Changed payment Rust files pass `rustfmt --check --edition 2024` and
  `git diff --check`; no workspace-wide formatting.

Final verification used the established shared Postgres/Redis harness through
`eval "$(./scripts/test-db.sh up)"`, followed by
`cargo test -p sanvi-payments --test payments_integration`,
`cargo test -p sanvi-payments --lib`, and the focused clippy command above.
No payments checks remain blocked.

## Limits and rollout

- Apply the additive migration before deploying new binaries. Roll back binaries
  first if needed; down migration removes queued snapshots, so drain/retain them
  before deliberately rolling the database back.
- Historical completed operations cannot reconstruct original notes/IP that were
  never durably recorded. The fix guarantees snapshots for new requests, and
  signed webhook recovery requires metadata emitted by the new adapter.
- No live Stripe credentials, external payments, commits or deployments were used.
