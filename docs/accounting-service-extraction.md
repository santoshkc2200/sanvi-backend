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

Historical Accounting migrations and in-process builders remain for pre-cutover
rollback and existing regression fixtures. They are not selected by the production
binary. After service traffic starts, keep the service database authoritative even
when rolling application images back. Dropping old data/source and reclaiming its
schema are separate contraction changes, after explicit cutover verification.

No production deployment, data transfer or destructive contraction is performed by
this change. Apply platform receipt migrations before starting service delivery;
freeze old writers before copying data. See the service cutover procedure for the
operator sequence and post-write rollback restrictions.
