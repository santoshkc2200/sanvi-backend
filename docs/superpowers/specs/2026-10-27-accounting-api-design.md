# Accounting tenant API and freee composition

Delivery step "Authenticated API and admin screens" from
`docs/superpowers/plans/2026-10-26-accounting-connections.md`, backend half.
The Svelte tenant-admin screens are a separate frontend task and consume the
contract defined here.

Written from the delivery plan without a requirements interview. Choices not
dictated by the plan or by existing code are marked **(design call)**.

## Intent

The accounting context is a tested library with no way in. Tenant admins
cannot create a book, register a freee company, grant consent, or choose a
cutover date, and `apps/api` composes only the privacy provider. This step
exposes the existing use cases through authenticated tenant routes, records
who did what, and composes the freee adapter, all behind a rollout switch
that stays off. Nothing posts money: ingestion and export are the next step.

## Goals

1. Tenant routes for books, connections, freee consent, activation and
   switching, mounted under the existing tenant scope and permission gate.
2. `tenant.accounting.read` and `tenant.accounting.manage` in the access
   catalog and default roles.
3. A durable audit record for every human mutation, committed in the same
   transaction as the change and delivered to the audit chain by the worker.
4. Generated `api/accounting.yaml`, diffed by CI like every other context.
5. `features.accounting` (default `false`) and an `accounting.freee`
   entitlement (default off) gating the routes. With the flag on and freee
   secrets present, `CredentialVerifier` with `FreeeClient` replaces
   `UnavailableVerifier`.

## Non-goals

- Ingestion, export, provider posting, the refresh sweep job.
- Posting routes, including `ResolvePosting`. No postings can exist until
  ingestion ships, so the routes would have nothing to act on **(design call)**.
- Money Forward and Xero connectors. Their enum values stay registrable as
  metadata; consent and verification for them fail closed as today.
- A legal-entity registry (see Open decisions).
- Registering the freee developer app and deploying its secrets. That is an
  operator task outside the repository; this step only reads the config.
- Frontend screens.

## Anti-goals (failure even if it compiles)

- Tenant, user, actor or connection identity taken from a request body or
  query instead of `CurrentTenant` / `CurrentSubject`.
- Any route that lets a tenant user perform an `Export` operation.
- A verifier, adapter or config path that reports success without calling
  freee; an "unverified but allow" fallback when secrets are missing.
- Tokens, refresh tokens, authorization codes or signed state appearing in
  responses (other than the authorization URL), logs, audit `before`/`after`
  payloads, outbox events or `Debug` output.
- A human mutation that commits without its audit command, or an audit
  command for a change that rolled back.
- Mounting routes, or making any freee call, while `features.accounting` is
  `false`.

## Constraints from the existing code

- Use cases already authorize through `AccountingAccess`; HTTP handlers build
  `Actor::User { tenant_id, user_id }` from authenticated context only.
- Book mutations take an expected `revision` and return
  `AccountingError::StaleRevision` on mismatch.
- `CompleteAuthorization` burns the state before exchanging the code and keeps
  the grant only when freee's `company_id` equals the registered organization.
  `RevokeAuthorization` refuses connections that are active, scheduled or
  draining. The handlers add no logic of their own on top.
- `FreeeSettings.redirect_uri` is a single registered URI. freee redirects the
  browser to the tenant-admin frontend, never to the API.
- `AuditWriter::append` needs a `PlatformScope`, not the accounting tenant
  transaction, so audit uses the payments pattern: a command row in the
  context's own schema, drained into the chain by the worker
  (`DrainRefundAudits`, migration `20261022000200_payments_idempotency_audit`).
- Rollout follows advertising: a `FeaturesConfig` switch decides whether state
  is built and routes are mounted (`apps/api/src/lib.rs` tenant scope), and an
  entitlement decides per tenant (`RequiredAnyFeature::declared`).
- Errors map once, in a handler-level `api_error`, to `sanvi_http::ApiError`.
- Every new migration is reversible, touches only the `accounting` schema, and
  gets a classification in `deployment/migration-policy.json`.

## Design

### Permissions and entitlement

Add to `crates/contexts/access/src/domain/permissions.rs`, tenant scope:

| Key | Grants |
| --- | --- |
| `tenant.accounting.read` | Read books, connections, periods and credential status |
| `tenant.accounting.manage` | Create books, register connections, grant/revoke consent, activate, schedule/cancel switches |

Names follow the plan and the existing `tenant.domains.*` / `tenant.theming.*`
keys. Default roles: `tenant_owner` and `tenant_admin` get both; `tenant_member`
gets neither **(design call, matching payments.manage and advertising.connect)**.

The exporter capability is not a catalog permission. `Permission::Export` is
only reachable through `Actor::System`, which no HTTP handler can construct.
The `AccountingAccess` adapter maps `Read`/`Manage` to the two keys above for
`Actor::User`, denies `Export` for every user, and allows `Export` only for
system actors. This adapter is the access check composed into `Services`.

Add a `FeatureSeed` `accounting.freee` (boolean, default off, public). Every
accounting route requires it in addition to its permission. Accounting has
only one working provider, so a single entitlement is enough for now
**(design call)**; a second provider adds its own key.

### Rollout switch and composition

- `FeaturesConfig.accounting: bool`, `#[serde(default)]` (off), in both
  `apps/api` and `apps/worker` config, documented like `advertising`.
- `AccountingConfig` in the accounting crate, loaded the same way as
  `AdvertisingConfig`: `freee_client_id`, `freee_client_secret: Redacted`,
  `freee_redirect_uri`, `oauth_state_signing_secret: Redacted`,
  `token_kek: Redacted` (base64, 32 bytes), `token_kek_id`. Secrets come only
  from deployed environment overrides (`SANVI__ACCOUNTING__...`).
- Flag off: no accounting state built, no routes mounted, no worker job.
  The privacy provider stays registered exactly as today.
- Flag on, any freee secret empty: routes mount. Reads, `CreateBook` and
  `RegisterConnection` work; consent start answers `503`; verification stays
  `UnavailableVerifier`, so activation and switching fail closed. Boot logs one
  warning naming the empty keys, never their values.
- Flag on, secrets present: compose `FreeeClient`, `PostgresCredentialStore`,
  `Authorization` and `CredentialVerifier`. A malformed KEK or empty redirect
  URI is a boot error (exit 1), like advertising.
- Boot rejects `oauth_state_signing_secret` equal to any other context's
  state or webhook secret, following the existing shared-secret check in
  `apps/api/src/config.rs`.

### Application additions

The routes need a few things the library lacks:

1. **`ListBooks`** (`Read`): the tenant's books, ordered by ID. Add `list` to
   `AccountingStore`. Tenants have a handful of books, so there is no
   pagination; cap at 100 and document it **(design call)**.
2. **Server-generated connection IDs.** `RegisterConnection` currently takes a
   caller-built `Connection`. Move ID generation into the use case, as
   `CreateBook` does: the command carries provider, organization ID and
   mappings, and the use case draws a Snowflake from `Services.ids`.
3. **Credential status for reads.** `GetBook` and `ListBooks` responses include
   each connection's `CredentialStatus` (`none`, `active`,
   `reauthorization_required`, `revoked`). Add a read-only
   `CredentialStore::statuses(tenant_id, book_id)` that never selects token
   columns.
4. **Audit commands** (below) on every `Manage` use case.

### Audit

New table `accounting.audit_commands` (tenant-scoped, RLS like the rest of the
schema): `id` Snowflake, `tenant_id`, `book_id`, `entry jsonb`
(`AuditEntryData`), `created_at`, `delivered_at`. The runtime role may insert
and set `delivered_at` once; it gets no delete and no update of `entry`.

- The store's mutation transaction (`create`, `change`, the credential
  authorize/revoke transaction) inserts the command with the outbox event.
  Use cases accept a request context (`request_id`, `ip`, `user_agent`) next
  to the `Actor`. The handler fills it from the request, never from the body.
- Actions: `accounting.book.created`, `accounting.connection.registered`,
  `accounting.connection.authorized`, `accounting.connection.revoked`,
  `accounting.destination.activated`, `accounting.switch.scheduled`,
  `accounting.switch.canceled`. `resource_type` is `accounting_book`;
  `resource_id` is the book ID. `after` holds IDs, provider, organization ID,
  dates and mapping codes, and never credential material.
- Failed consent attempts (state rejected, organization mismatch) are not
  audited: no state changed, and a failure write would need its own
  transaction. They are logged at `warn` with tenant and book IDs only
  **(design call)**.
- Worker job `accounting.deliver_audits`, mounted only when
  `features.accounting` is on: in one unscoped transaction it locks pending
  rows (`FOR UPDATE SKIP LOCKED`, batch limit), appends each through
  `AuditWriter`, and sets `delivered_at`, like `DrainRefundAudits`.
- Privacy: `entry` contains the acting user's ID. Extend
  `AccountingPrivacyProvider` so a tenant-user access request exports that
  user's undelivered and delivered command rows, and erasure is handled by the
  same review requirement the provider already raises. It must not claim
  automatic erasure. The audit chain's own lifecycle is unchanged.

### Routes

All routes sit under the tenant scope (`/api/v1/tenant`), require
`accounting.freee`, and use `CurrentTenant` + `CurrentSubject`. Book IDs and
connection IDs are decimal strings. A mutation of an existing book requires
`If-Match: "<revision>"`; responses return the book with `ETag` set to its
revision, like advertising budget caps.

| Method and path | Permission | Fresh MFA | Use case |
| --- | --- | --- | --- |
| `GET /accounting/books` | read | | `ListBooks` |
| `POST /accounting/books` | manage | | `CreateBook` |
| `GET /accounting/books/{book_id}` | read | | `GetBook` |
| `POST /accounting/books/{book_id}/connections` | manage | | `RegisterConnection` |
| `POST /accounting/books/{book_id}/connections/{connection_id}/authorization` | manage | yes | `StartAuthorization` |
| `DELETE /accounting/books/{book_id}/connections/{connection_id}/authorization` | manage | yes | `RevokeAuthorization` |
| `POST /accounting/authorization/callback` | manage | | `CompleteAuthorization` |
| `POST /accounting/books/{book_id}/activation` | manage | yes | `ActivateBook` |
| `PUT /accounting/books/{book_id}/switch` | manage | yes | `ScheduleSwitch` |
| `DELETE /accounting/books/{book_id}/switch` | manage | | `CancelSwitch` |

Fresh MFA (`FreshAal2Required`) protects the steps that change where money
will be posted or that grant or drop provider access. Cancelling a pending
switch only restores the current destination **(design call)**.

Consent flow:

1. The admin UI calls `POST .../authorization`; the response is
   `{ "authorization_url": "..." }`. The signed state stays inside the URL;
   the frontend never constructs it.
2. freee redirects the browser to the registered frontend URI with `code` and
   `state`.
3. The frontend calls `POST /accounting/authorization/callback` with
   `{ "state", "code" }` in the JSON body. `CompleteAuthorization` checks that
   the state was issued to this tenant and user. The response is the updated
   book.

The callback is a `POST` with a JSON body, unlike advertising's `GET` with
query parameters, so the code never lands in access logs or browser history
and a GET never has side effects **(design call)**. It has no book ID in the
path because the state carries it.

Bodies:

- `POST /books`: `{ "legal_entity_key": string }`. See Open decisions.
- `POST /connections`: `{ "provider": "freee" | "money_forward" | "xero",
  "organization_id": string, "mappings": { revenue, refund, clearing,
  tax_codes } }`, in the domain's existing field shapes, `deny_unknown_fields`.
- `POST /activation`: `{ "connection_id", "starts_at" }`.
- `PUT /switch`: `{ "connection_id", "cutover_at" }`.

Book view: `id`, `legal_entity_key`, `revision`, `connections[]` (`id`,
`provider`, `organization_id`, `mappings`, `credential_status`), `periods[]`
(connection, start, end), and the scheduled switch if one exists. Period state
(`active`, `scheduled`, `draining`, `closed`) is derived from the clock at
read time so the UI can show registration, authorization and active posting
as separate states. Sync health has nothing to report until export exists, so
it is left out instead of shipping a placeholder.

### Error mapping

| `AccountingError` | Status |
| --- | --- |
| `Invalid` | 400 |
| `NotFound` | 404 (also for another tenant's IDs; RLS already hides them) |
| `Forbidden` | 403 |
| `StaleRevision` | 409, like advertising; missing or malformed `If-Match` is 400, like advertising campaign routes |
| `Conflict`, `IdempotencyConflict` | 409 |
| `ReauthorizationRequired` | 409 with a stable problem `type` so the UI can offer reconnect |
| `AuthorizationRejected` | 400 with a stable problem `type` |
| `VerificationFailed` | 422 |
| `HistoricalImport` | 422 |
| `VerificationUnavailable`, `ProviderUnavailable` | 503 |
| `Storage` | 500, detail not exposed |

Problem details for `AuthorizationRejected` and `VerificationFailed` may carry
the static reason string; none of them carries a provider response body.

### OpenAPI

An accounting `ApiDoc` with `#[utoipa::path]` on every handler, registered with
`sanvi-cli openapi --write`, producing `api/accounting.yaml`. Response
descriptions state the fail-closed behaviour (503 when freee is not
configured, 422 when verification fails).

## Tests

- Access catalog: the keys exist; owner and admin roles have them; member does
  not.
- `AccountingAccess` adapter: user plus `Export` is denied even when the user
  holds both permissions; a system actor is denied `Read`/`Manage`.
- Each route: 401 without a session, 403 without the permission, 403 without
  the entitlement, 404 when routes are unmounted with the flag off; fresh-MFA
  routes reject a stale session.
- IDOR: another tenant's book and connection IDs answer 404 on every route.
  Add the routes to `apps/api/tests/integration/idor_sweep.rs`.
- `If-Match`: a stale revision gives 409, a missing one 400, and no audit
  command is written.
- Audit: every successful mutation writes exactly one command in its
  transaction; a mutation forced to fail after the change rolls back both;
  the drain job delivers once across two concurrent workers; no command
  contains token, code or state material.
- Consent: the callback with another user's state is rejected; replaying the
  state is rejected; a freee double for the HTTP adapter (the existing
  `freee.rs` test style, not a runtime adapter) covers the organization
  mismatch and success paths end to end through the handler.
- Composition: flag on with empty secrets keeps `UnavailableVerifier`
  (activation answers 503) and start answers 503; a malformed KEK fails boot.
- Privacy provider: an access request for the acting user includes their audit
  commands.
- Contract: `api/accounting.yaml` is regenerated and the OpenAPI gate passes.
- Gates: `just ci-quiet` (or its `scripts/*.sh` equivalents), with the new
  migration classified.

## Rollout and rollback

The migration only adds a table, so it is classified `rolling`. With
`features.accounting` off the release changes nothing at runtime. Enable it
first on staging with freee sandbox credentials and the entitlement granted to
one test tenant. Turning the flag off unmounts the routes and stops the drain
job; pending audit commands stay in the table and deliver when it comes back
on. Rolling back the migration requires delivering or exporting pending audit
commands first.

## Left to the builder

- Module layout under `src/adapters/http/`, handler names, view struct names.
- Exact `AuditEntryData.after` field set within the rules above.
- Drain batch size and schedule, using the shared scheduler defaults.
- Whether the request context is a new struct or extends `Actor`; it must not
  become part of authorization.

## Open decisions for the owner

1. **Where `legal_entity_key` comes from.** The README says a trusted registry
   supplies it, and no registry exists. This draft accepts an admin-supplied
   key (1–64 chars, `[a-z0-9_-]`), unique per tenant, as a stable label and
   not a verified legal entity. The alternative is one implicit book per
   tenant with a fixed key until a registry exists.
2. **Default roles.** This draft grants manage to `tenant_admin` as well as
   the owner. If switching a company's accounting destination should be
   owner-only, like refunds, drop it from `tenant_admin`.
