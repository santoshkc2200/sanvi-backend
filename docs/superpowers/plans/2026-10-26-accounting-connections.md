# Tenant accounting connections and controlled provider switching

## Outcome and scope

Tenants keep their preferred accounting product and can change it later without
changing ecommerce, restaurant, POS or inventory. Sanvi owns operational source
facts and accounting-integration state; each external provider owns its books.
The platform's own subscription revenue is separate from merchant revenue.

The first backend milestone introduces `sanvi-accounting`: domain invariants,
authorized application ports/use cases, PostgreSQL persistence, a reversible
migration, durable outbox events, privacy-provider participation, and focused tests. This is not yet a customer-
accessible integration. No routes are mounted, no OAuth credentials are stored,
and no accounting API is called. No OpenAPI contract changes are needed until
HTTP handlers are introduced.

## Invariants implemented in the first milestone

1. The tenant and stable legal-entity key identify one integration book, which
   has one active posting destination. This is not enforcement of real-world
   company identity; enrollment must resolve the key through a trusted registry.
2. A candidate provider binding carries organization/account/tax mapping metadata.
   Registration alone never grants posting access. Activation and switching
   require trusted verification of tenant-bound credentials and provider access.
3. Connection bindings and their mappings are immutable. An admin can return to
   a previous provider by registering a fresh binding, preserving every old one.
4. Switching is scheduled for an explicit UTC instant. The UI must eventually
   translate the accountant's intended local date/time to that instant; do not
   silently use browser/server local time or infer an accounting period.
5. Before cutover, cancellation keeps the current destination. Switches take
   effect automatically at the cutover instant without a manual completion step;
   canceling after cutover is forbidden. Routing assigns by accounting date across
   contiguous periods with a 60-day late grace window (`LATE_EVENT_GRACE`) allowing
   delayed postings into closed periods.
6. Initial activation backfill is bounded to 365 days (`MAX_BACKFILL`). Old and new
   periods meet at the exact half-open cutover boundary; the old provider continues
   to drain in-window work until grace expires. At most one scheduled switch may
   exist at any time.
7. Assignment uniqueness is `(tenant_id, book_id, source_event_id)`, independent
   of provider. Replay returns the original assignment, even after cutover.
   Changed fingerprints/timestamps conflict. Provider IDs and connection ownership
   remain associated with the original assignment.
8. Unassigned historical transactions are rejected by default. Moving old books,
   opening balances, unpaid invoices, refunds across a cutover, and reconciliation
   adjustments require a separate explicit migration/correction workflow.
9. Every mutation and its outbox event commit or roll back together. RLS covers
   both tables; runtime permissions prevent deleting history or rewriting posting
   ownership. Optimistic revisions prevent competing admin changes from winning.

## Remaining delivery sequence

### Provider authorization and credentials

Implement freee as the first concrete adapter while retaining provider-neutral
ports. Register the developer app and configure OAuth credentials through the
existing secret-management boundary. Add tenant/user/provider-bound single-use
OAuth state, callback validation, encrypted credentials, refresh concurrency,
revocation handling, and organization/mapping lookup. Respect each provider's
actual authorization scopes, application review, rate limits and account plans.
Money Forward and Xero enum values identify candidates, not working connectors.

The default verifier rejects activation and switching until a concrete adapter
is composed. Never add a manual "verified" flag or an always-successful runtime
adapter. Verify organization/account/tax IDs against provider responses, not
administrator-submitted names. This context currently stores no credentials.

### Authenticated API and admin screens

Add generated accounting OpenAPI and tenant-scoped routes under the existing
identity/access stack. Proposed permissions are `tenant.accounting.read`,
`tenant.accounting.manage`, and a distinct internal exporter capability.
Add transactional human-actor audit records through the existing audit boundary
and its subject-data lifecycle before exposing admin mutation routes. Domain
outbox events currently contain book/revision/posting references, not user IDs.
Register the feature/permission catalog and keep routes behind an accounting
feature flag until authorization, OAuth and data lifecycle hooks are complete.
Never accept Actor/tenant/connection IDs as identity from an unauthenticated body.

Tenant admins choose a legal entity, provider organization, mappings, and an
explicit cutover date; display pending work and the last verified status. Show
registration, authorized access, active posting, and synchronization health as
different states. Store each tenant's credentials and provider organization
independently. Add the screens to the existing Svelte tenant-admin application.

### Ingestion and delivery

Consume authenticated sale/refund/payment/settlement facts from ecommerce and
restaurant, including POS through ecommerce. Preserve complete source snapshots,
currency/minor-unit amounts, tax classifications and explicit accounting dates.
Keep stock quantities authoritative in ecommerce; never replay accounting
inventory data into stock balances automatically.

Use the existing durable PostgreSQL jobs/outbox infrastructure for exports.
Introduce a durable provider-operation record before sending externally, stable
provider-scoped idempotency keys, retry/backoff/rate-limit handling and confirmed
result reconciliation. The current assignment ledger is not an export queue or
a provider response journal. A timeout leaves the posting in `submitted`;
exporters must query the provider before retrying.
Keep checkout independent of accounting availability.

After cutover, a refund for an old sale is not merely a historical reimport or a
new copy of the original invoice. Explicitly define its accounting destination,
original-record linkage, tax treatment and reconciliation behavior with the
merchant's accounting process before implementing cross-cutover corrections.
Do not automatically replay historical source events into a new provider.

### Draining, recovery and privacy

Switches take effect automatically at `cutover_at`. Postings follow an explicit
status machine (`pending`, `submitted`, `delivered`, `failed`, `resolved_manually`)
with a human resolution path (`ResolvePosting`) so failed postings or provider
glitches never wedge routing. While closed periods are inside the 60-day grace
window (`draining_connections`), credentials for the previous provider must remain
valid and authorized so late events can drain. Credential revocation and re-dating
of rejected events remain future work.

The accounting privacy adapter is registered in the shared registry and declares
the existing billing data class. Existing organization metadata triggers a
processing-pending review requirement for tenant-user access/erasure/correction;
it never claims statutory retention or automatic erasure. It exports no company
settings for end-user/customer requests. Full subject-to-entity mapping and
explicit fulfillment are required before operational accounting is enabled.

Define retention/export/offboarding behavior for integration metadata and source
facts, complete the reviewed fulfillment/retention adapters, and implement
credential revocation without deleting reconciliation history. No route or
provider integration is production-enabled until these hooks are complete.

## Verification and rollout

Focused tests cover legal-entity uniqueness, verification/access failures,
historical import rejection, exact cutover boundaries, duplicate/conflicting
source replay, concurrent management and routing, forced RLS/column permissions,
outbox rollback, and reversing/reapplying only this migration. Live provider
authorization and money posting are not exercised by these tests.

The migration is additive. Existing app processes compose only the privacy adapter, not operational
accounting use cases, so adding the schema exposes no new endpoint and changes
no existing checkout.
For rollback, stop future accounting writers and preserve/export the accounting
history before running the matching down migration; external provider records
are never reversed by a schema rollback. Existing deployment compatibility rules
still apply: do not downgrade schema automatically or force old binaries through
the hardened deployment controller.
