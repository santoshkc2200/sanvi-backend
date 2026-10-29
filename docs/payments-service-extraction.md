# Payments extraction

Payments runs from the separate sibling `payments-service` repository, with its own
database, image, migrations, Stripe Connect credentials, Connect webhook receiver and
background jobs (webhook dispatch, connection sync, metrics, tax checks, refund audits,
webhook-event partition maintenance). Its README and `docs/cutover.md` document
configuration, verified data transfer and rollback.

The platform binary uses a gateway for the existing `/api/v1/tenant/payments/*` and
`/api/v1/tenant/checkout/*` URLs. `features.payments` still gates these routes. Configure
`SANVI__PAYMENTS_SERVICE__BASE_URL`, `SANVI__PAYMENTS_SERVICE__SIGNING_SECRET` (signs
requests to the service) and `SANVI__PAYMENTS_SERVICE__INBOUND_SECRET` (verifies its
deliveries), each at least 32 bytes and never equal. Each delegation carries a unique ID
that the service accepts once.

The platform keeps session and tenant resolution, authoritative permission checks and
entitlement enforcement. Every route except `GET /payments/providers` requires the
`payments.stripe_connect` entitlement; the provider catalog needs only `payments.read`, so an
unentitled tenant can see what upgrading buys. Requests that fail any platform check are
refused before anything is signed or sent to the service. A request-bound thirty-second
delegation carries the verified tenant, the subject without role IDs, and the route's
permission key (`payments.read`, `payments.manage`, `payments.checkout` or
`payments.refund`). The service checks that key against the method and path and rejects any
other. The delegation also signs the caller's `Idempotency-Key`, which is forwarded and must
match, and the client address the platform resolved, which the service uses for audit
entries and its per-IP checkout throttle. Browser credentials are not forwarded. There is no local fallback: an unconfigured or
unreachable service returns 503.

Payments owns provider connections, checkouts, payments, refunds, disputes, payouts, tax
settings, the Connect webhook inbox and all writes to its own database. The platform worker
no longer runs `payments.webhook_dispatch`, `payments.connection_sync`, `payments.metrics`,
`payments.tax_check` or `payments.refund_audits`. The API no longer mounts
`/internal/webhooks/stripe/connect`; set `PAYMENTS_UPSTREAM` (for example
`payments_service:8084`) when running `scripts/deploy-staging.sh`, and the ingress routes
that exact path to the service. The public webhook URL registered with Stripe does not change.

Payments forwards audit commands to `/internal/payments/audit` and domain events to
`/internal/payments/events`. Both endpoints require a request-bound delegation (audience
`sanvi-platform-payments-audit`, purposes `audit` and `event`). Receipts in
`audit.payments_receipts` and `platform.payments_event_receipts`, keyed by audit command ID or
event ID, commit atomically with the audit append or outbox insert. A retry after a lost
acknowledgement produces one durable platform effect, and an ID reused with different
content is rejected with 409 Conflict. Only `payments.*` audit actions with a tenant, and
only `payments.*` events, are accepted.

Privacy orchestration uses `RemotePaymentsPrivacyProvider`, which calls the service's signed
`/internal/privacy/collect`, `/internal/privacy/erase` and `/internal/privacy/rectify`
(purpose `privacy`). These run the embedded `PaymentsPrivacyProvider` and return its real
bundle, erase report or rectification result. A 409, an unreadable answer or an unreachable
service leaves the request pending; nothing is reported done unless the service did it.

Historical Payments migrations, the in-process handlers (`*_embedded`) and repositories remain
for pre-cutover rollback and existing regression fixtures. The production binary does not
construct Payments state. After service traffic starts, keep the service database
authoritative even when rolling application images back. Dropping the old data and
reclaiming its schema are separate contraction changes, after explicit cutover verification.

No production deployment, data transfer or destructive contraction is performed by this
change. Apply the platform receipt migrations before starting service delivery, and freeze
old writers before copying data.

## Advertising and operator commands

Advertising capture joins a conversion's order reference to its payment record through
`RemotePaymentLookup`, a signed `GET /internal/payments/conversion-value` (purpose
`conversion`). The lookup runs before capture opens its transaction, like the consent gate,
so no pooled connection is held across the call. A 404 means no payment is recorded and the
client's value is used as a marked hint; any other failure fails the capture rather than
trusting that hint.

The Connect webhook status, dead-letter replay and manual sync commands moved from
`sanvi-cli payments` to the service binary: `sanvi-payments-service webhooks`,
`replay <event-id>` and `sync`.
