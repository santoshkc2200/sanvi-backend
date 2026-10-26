# Accounting foundation verification — 2026-10-26

## Passed on the final code

- Scoped formatting: `cargo fmt --check -p sanvi-accounting` clean.
- Accounting Clippy: `SQLX_OFFLINE=true cargo clippy -p sanvi-accounting --all-targets --all-features -- -D warnings` clean (no issues).
- Layering check: `scripts/check-layering.sh` reported no architecture violations.
- Migration expand/contract check: `./scripts/check-migration-expand-contract.sh` reported 0 violations in 67 scanned migrations.
- Scoped SQLx offline check: `SQLX_OFFLINE=true cargo check -p sanvi-accounting --all-targets` clean with 10 accounting query metadata files in `.sqlx`.
- `scripts/test.sh -p sanvi-accounting`:
  **33 tests passed, 0 skipped** across 2 binaries:
  - 11 unit tests in `domain::model::tests`: date routing, half-open cutover instant, 60-day late grace window, 365-day backfill boundary, single scheduled switch limit, cancellation before vs at cutover instant, fresh binding re-enrollment, validation invariants.
  - 9 unit tests in `domain::posting::tests`: submission, delivery, failure retry attempts counter, idempotent resubmission, manual resolution from any open state, terminal status immutability, text bounds (1–1024 bytes).
  - 13 integration tests in `sanvi-accounting::accounting_integration`: reversible migration without touching other contexts, user vs system actor authorization enforcement, date-based routing across cutover with concurrent assigns, posting lifecycle outbox events (`posting_assigned`, `posting_submitted`, `posting_failed`, `posting_delivered`), human `ResolvePosting` unwedge path, RLS and column privileges, and privacy provider review.
- Combined gates run: `scripts/test.sh -p sanvi-accounting -p sanvi-api`:
  **132 tests passed, 0 skipped** across 6 binaries (33 accounting tests + 99 api tests).

## Existing deployment-policy failure

`python3 scripts/test-deployment-migrations.py` passes six tests and fails the
checked-in migration classification test on the pre-existing
`20261015000100_privacy_rights_enforcement.sql`, which is not classified in
`deployment/migration-policy.json`. Its classification was left unchanged.
The new `20261026000200_accounting_destinations.sql` is classified `rolling` and
passes expand/contract validation. Do not report all repository gates as green.

## Not exercised or enabled

No live accounting provider was contacted. No OAuth, export worker, operational
accounting HTTP route, or admin screen is implemented or enabled. Verification
adapters in production fail closed. Organization metadata touching a tenant-user
privacy request requires explicit review/fulfillment; the current adapter does
not silently declare it erased or apply a blanket statutory-retention claim.

The schema was applied only to isolated/shared-harness test databases. Temporary
SQL-preparation databases were removed; the standard shared test servers remain.
No production or staging deployment was performed.
