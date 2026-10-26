# Accounting integration foundation

This bounded context stores tenant accounting destinations, controlled
provider cutovers, and freee OAuth consent. It does not implement accounting
books, stock management, provider posting, a public HTTP API, or tenant-admin
screens yet.

## Implemented

- One integration book per `(tenant_id, legal_entity_key)`. The key is a stable
  tenant-local reference supplied by a trusted legal-entity registry/composition;
  it is not a legal name and does not verify that a real-world company exists.
- Candidate destination bindings for freee, Money Forward, and Xero. Registration
  is metadata only and never represents completed OAuth or verified access.
- Required revenue/refund/clearing account mappings and a map of source tax
  classifications to provider codes. No tax rates or accounting rules are inferred.
- Initial activation bounded to a maximum backfill of 365 days (`MAX_BACKFILL`),
  scheduled switching, and cancellation before cutover. Switches take effect
  automatically at `cutover_at` without a manual completion gate.
- Provider verification before activation and scheduling. The only shipped verifier
  fails closed. Test verifiers are test-only.
- Mandatory privacy-provider registration. Tenant-user requests with existing
  organization metadata require explicit subject/entity review; customer requests
  never export unrelated company settings or silently erase history.
- PostgreSQL tenant RLS, per-book transaction locks, and outbox events committed
  with every change. Candidate connections and historical periods are retained.
- Date-based routing across periods: events route by `accounting_at` to the period
  that contains them, with a 60-day grace window (`LATE_EVENT_GRACE`) allowing late
  events into closed periods. Events older than grace or preceding the first period
  are rejected as `HistoricalImport`.
- Source-event assignments deduplicated across provider changes, with canonical
  SHA-256 fingerprints and immutable connection ownership.
- Explicit posting status lifecycle with terminal immutability enforced by a
  database trigger (`accounting.postings_terminal_immutable`):

| From | To | Use case | Actor |
| --- | --- | --- | --- |
| (new) | `pending` | `AssignPosting` | system (`Export`) |
| `pending`, `failed` | `submitted` | `MarkSubmitted` | system (`Export`) |
| `submitted` | `delivered` | `RecordDelivery(external_record_id)` | system (`Export`) |
| `submitted` | `failed` | `RecordFailure(reason)` | system (`Export`) |
| `pending`, `submitted`, `failed` | `resolved_manually` | `ResolvePosting(note, external_record_id?)` | user (`Manage`) |

- freee OAuth (`adapters::freee`, `application::authorization`):
  `StartAuthorization` issues signed, single-use, 10-minute state bound to the
  tenant, user, book and connection; `CompleteAuthorization` burns it before
  exchanging the code and keeps the grant only if freee's `company_id` equals
  the registered organization. Tokens are sealed per tenant
  (`accounting.provider_credentials`, DEKs in `accounting.tenant_secret_keys`,
  KEK from deployed config). `AccessTokens` refreshes under the credential row
  lock because freee refresh tokens are single-use; a refused refresh moves the
  credential to `reauthorization_required`. `RevokeAuthorization` is refused
  while the connection is active, scheduled or draining.
- `CredentialVerifier` replaces `UnavailableVerifier` once composed: it needs
  an active credential, a freee role of `admin` or `simple_accounting`, and
  every mapped account and tax code available in that company.
- Separation of `Actor::User` (for tenant users with `Read` or `Manage` permissions)
  from `Actor::System` (for background workers with `Export` permission). Users cannot
  execute export operations; system actors cannot access management endpoints.

Retry the original source event after a provider switch and it returns its original
destination. Changed source content conflicts. Out-of-window historical work is
rejected rather than silently backfilled.

Returning to an old provider creates a fresh binding; old mappings and posting
references are retained. No destination binding is reused across active periods.
There is no delete/rewrite-history API or automatic migration of external books.

## Composition boundary

`application::Services` requires an `AccountingAccess`, `ProviderVerifier`, store,
clock, and ID generator. Every use case checks authorization via `Services::authorize`
before reading or mutating state. HTTP actors must come from authenticated request
context, and system actors represent trusted background services. Connection IDs must
come from the platform Snowflake generator when an enrollment adapter is added.

`PostgresAccountingStore` implements atomic persistence. Authorization and
provider verification have no permissive production defaults. Compose
`CredentialVerifier` with `FreeeClient` and `PostgresCredentialStore` once the
freee app credentials, state key and token KEK are deployed; until then keep
`UnavailableVerifier`. Do not compose a test verifier into an application.
There is no in-process success fallback.

`AssignPosting` accepts an event ID, accounting timestamp and a fingerprint of
canonical source facts. It stores routing metadata only; no money is sent and no
accounting document is created. A future ingestion adapter must authenticate the
source and validate its tenant, legal entity and transaction facts. A future
exporter must retain those facts durably, treat network timeouts as `submitted`
(querying the provider before retrying), and call `RecordDelivery` only after
confirming the external result. Do not treat a network timeout as permission to
re-send without checking, assign a new source event, or declare success.

## Validation

```sh
cargo check -p sanvi-accounting --all-targets
cargo clippy -p sanvi-accounting --all-targets --all-features -- -D warnings
scripts/test.sh --quiet -p sanvi-accounting
```

Use the repository test wrapper so all integration tests share one PostgreSQL
server. SQLx metadata lives in the workspace `.sqlx` directory. The reversible
migration creates only the `accounting` schema and grants no delete permission or
posting-destination update permission to the runtime role.

See [delivery plan](../../../docs/superpowers/plans/2026-10-26-accounting-connections.md)
for the remaining OAuth, API/UI, exporter, and recovery work.
See [verification results](../../../docs/superpowers/plans/2026-10-26-accounting-verification.md)
for tested behavior and the pre-existing deployment-policy gate failure.
