# Accounting extraction

Accounting runs from the separate sibling `accounting-service` repository, with
its own database, image, migrations and durable delivery scheduler. Its README and
`docs/cutover.md` document configuration, verified data transfer and rollback.

The platform binary uses a gateway for existing `/api/v1/tenant/accounting/*` URLs.
`features.accounting` still gates these routes. Configure
`SANVI__ACCOUNTING_SERVICE__BASE_URL` and
`SANVI__ACCOUNTING_SERVICE__SIGNING_SECRET` (signs requests to the service) and
`SANVI__ACCOUNTING_SERVICE__INBOUND_SECRET` (verifies its deliveries), each at least
32 bytes and never equal. Each delegation carries a unique ID that the service
accepts once.
The platform retains session/tenant resolution, entitlement checks, authoritative
permissions and fresh MFA. Request-bound thirty-second delegations convey only
verified identity and granted Accounting permissions; browser credentials are stripped.
There is no automatic local fallback. Unconfigured/unreachable service returns 503.

Accounting owns provider credentials, books, cutovers, postings and all writes to
its own database. Remove freee/KEK/OAuth secrets from the platform environment after
cutover; configure them only in the service. Existing encrypted credentials are copied
without re-encryption using the original KEK and ID. The frontend callback URL stays
unchanged. The platform worker no longer schedules `accounting.audit_commands`.

Accounting forwards audit commands to `/internal/accounting/audit` and domain events
to `/internal/accounting/events`. These endpoints require request-bound delegation;
receipts, keyed by audit command ID or event ID, commit atomically with the audit
append/outbox insert. A retry after a lost
acknowledgement produces one durable platform effect. Privacy orchestration uses the
service's signed `/internal/privacy/review`, failing closed while unavailable.

## Contraction

The platform no longer holds Accounting data or embedded code. Migration
`20261030000400_accounting_contraction` drops the `accounting` schema (books,
connections, candidate connections, historical periods, and postings) and its
supporting functions/triggers. Its down migration recreates the schema exactly
as it was, empty. No data was copied out: the only deployment was a development
environment with no data worth keeping.

`sanvi-accounting` now holds only what fronts the service: the delegation and gateway,
the published HTTP contract types (`adapters/http/contract.rs`, used for
`api/accounting.yaml`), route permissions, and the remote privacy provider.

What moved with the data, and is now the service's concern:

- provider credentials (freee/MoneyForward/Xero) and token KEK;
- books, connections, cutovers, and date-based period routing;
- posting lifecycle, terminal immutability, and external provider deliveries;
- privacy export and purge orchestration executed via `/internal/privacy/review`.

