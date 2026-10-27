# Accounting Tenant API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the accounting use cases through authenticated, audited tenant routes and compose the freee adapter, all behind `features.accounting` (off).

**Architecture:** Human mutations gain a `RequestContext` and write an `accounting.audit_commands` row in the same tenant transaction as the change; the worker drains rows into the audit chain. A new `adapters::http` module in `sanvi-accounting` holds handlers and routes; `apps/api/src/accounting.rs` composes them with an `Authorizer`-backed `AccountingAccess` and, when secrets exist, the freee credential stack.

**Tech Stack:** Rust, actix-web, SQLx/Postgres, utoipa, existing `sanvi-http`, `sanvi-audit`, `sanvi-access`.

**Spec:** `docs/superpowers/specs/2026-10-27-accounting-api-design.md`

## Global Constraints

- Every accounting route requires the `accounting.freee` entitlement and a permission (`tenant.accounting.read` / `tenant.accounting.manage`).
- `features.accounting` defaults to `false` in `apps/api` and `apps/worker`; off means no accounting state, no routes, no drain job. The privacy provider stays registered regardless.
- Identity (tenant, user) comes only from `CurrentTenant` / `CurrentSubject`. Request bodies use `deny_unknown_fields`.
- Mutations of an existing book require `If-Match: "<revision>"`; missing or malformed is 400, stale is 409. Responses carry `ETag: "<revision>"`.
- No token, refresh token, authorization code or signed state in any response (except `authorization_url`), log, audit entry, outbox event or `Debug` output.
- Never compose a verifier or adapter that succeeds without calling freee. Missing secrets keep `UnavailableVerifier`.
- Migrations: reversible, `accounting` schema only, classified in `deployment/migration-policy.json`.
- Run DB tests only through `scripts/test.sh` (never bare `cargo test` on crates with `tests/`). Run `cargo sqlx prepare --workspace` after query changes.
- `tenant_owner` and `tenant_admin` get both permissions; `tenant_member` gets neither.
- `legal_entity_key` is admin-supplied and validated only by `Book::new` (1–128 bytes, no surrounding whitespace) and per-tenant uniqueness.

## Review Focus

- **Concurrent callbacks with one state** (double-clicked redirect, two tabs): exactly one succeeds, the other is 400 `accounting/authorization-rejected`, and exactly one `accounting.connection.authorized` audit row exists. Test added to Task 2.
- **Revoking an already-revoked connection:** answers success without a second audit row (status unchanged). Test added to Task 2.
- **Callback reached by a user who lost `tenant.accounting.manage` after starting:** 403, state not burned. Test added to Task 6.
- **Feature flag on, freee secrets empty:** `GET` and `POST /books` work; `POST .../authorization`, the callback and `DELETE .../authorization` answer 503; activation answers 503. Test added to Task 5.
- **Datetime bodies without an offset** (`"2026-11-01T00:00:00"`): rejected as 400, never interpreted as local time. Test added to Task 6.

---

### Task 1: Permissions, roles and entitlement

**Files:**
- Modify: `crates/contexts/access/src/domain/permissions.rs` (after the `tenant.domains.*` entries)
- Modify: `crates/contexts/access/src/domain/roles.rs` (`tenant_owner`, `tenant_admin` seeds)
- Modify: `crates/contexts/access/src/domain/features.rs` (new `// --- accounting` block after advertising)
- Test: unit tests in the same files' `mod tests`

**Interfaces:**
- Produces: permission keys `"tenant.accounting.read"`, `"tenant.accounting.manage"`; feature key `"accounting.freee"`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn accounting_permissions_are_tenant_scoped() {
    for key in ["tenant.accounting.read", "tenant.accounting.manage"] {
        let def = PERMISSIONS.iter().find(|p| p.key == key).expect(key);
        assert_eq!(def.scope, PermissionScope::Tenant);
    }
}

#[test]
fn owner_and_admin_manage_accounting_member_does_not() {
    for (role, expected) in [("tenant_owner", true), ("tenant_admin", true), ("tenant_member", false)] {
        let seed = ROLE_SEEDS.iter().find(|r| r.key == role).unwrap();
        for key in ["tenant.accounting.read", "tenant.accounting.manage"] {
            assert_eq!(seed.permissions.contains(&key), expected, "{role} {key}");
        }
    }
}

#[test]
fn accounting_freee_is_off_by_default() {
    let seed = FEATURE_SEEDS.iter().find(|f| f.key == "accounting.freee").unwrap();
    assert_eq!(seed.kind, FeatureKind::Boolean);
    assert!(!seed.default_enabled);
    assert_eq!(seed.visibility, Visibility::Public);
}
```

Use the actual constant names in those files if they differ from `PERMISSIONS` / `ROLE_SEEDS` / `FEATURE_SEEDS`.

- [ ] **Step 2: Run** `just test-fast` (or `scripts/test.sh -p sanvi-access`). Expected: the three tests FAIL.
- [ ] **Step 3: Add the entries.** Descriptions: read = "Read accounting books, connections, periods and credential status"; manage = "Create accounting books, register connections, grant or revoke provider consent, and schedule destination changes (audited)". Feature name "freee accounting".
- [ ] **Step 4: Run** `scripts/test.sh -p sanvi-access -p sanvi-api`. Expected: PASS, including `apps/api` `permission_catalog` and `feature_catalog` suites (update any snapshot they hold).
- [ ] **Step 5: Commit** `feat(access): add accounting permissions and freee entitlement`

---

### Task 2: Audit commands in the mutation transaction

**Files:**
- Create: `migrations/<UTC timestamp>_accounting_audit_commands.sql` and `.down.sql`
- Modify: `deployment/migration-policy.json` (classify `rolling`)
- Create: `crates/contexts/accounting/src/adapters/audit.rs`
- Modify: `crates/contexts/accounting/Cargo.toml` (add `sanvi-audit`)
- Modify: `crates/contexts/accounting/src/application/ports.rs`, `application/mod.rs`, `application/authorization.rs`
- Modify: `crates/contexts/accounting/src/adapters/postgres.rs`, `adapters/credentials.rs`, `adapters/mod.rs`
- Test: `crates/contexts/accounting/tests/accounting_integration.rs`, `tests/authorization_integration.rs`

**Interfaces:**
- Produces (in `application::ports`, re-exported from `application`):

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestContext {
    pub request_id: Option<String>,
    pub ip: Option<std::net::IpAddr>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditRecord {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub book_id: BookId,
    pub action: &'static str,
    pub occurred_at: DateTime<Utc>,
    pub after: serde_json::Value,
    pub request: RequestContext,
}
```

- Changed port signatures:
  - `AccountingStore::create(&self, book: Book, event: AccountingEvent, audit: AuditRecord)`
  - `AccountingStore::change(&self, tenant_id, book_id, revision, change: BookChange, audit: Option<AuditRecord>)`: the row is written only when `change` returns `Some(event)`.
  - `AccountingStore::transition(&self, tenant_id, posting_id, change: PostingChange, audit: Option<AuditRecord>)`: the row is written only when the posting changes. `ResolvePosting` passes `Some`; the system-actor transitions pass `None`.
  - `CredentialStore::authorize(&self, tenant_id, credential, audit: AuditRecord)`
  - `CredentialStore::update(&self, tenant_id, connection_id, change: CredentialChange, audit: Option<AuditRecord>)`: the row is written only when the status changes. `AccessTokens` refresh passes `None`.
- Changed use cases: every `Manage` use case's `execute` takes `request: RequestContext` as its second parameter, after `actor`: `CreateBook`, `RegisterConnection`, `ActivateBook`, `ScheduleSwitch`, `CancelSwitch`, `ResolvePosting`, `StartAuthorization`, `CompleteAuthorization`, `RevokeAuthorization`. `StartAuthorization` writes no audit row because nothing changes.
- Actions (exact strings): `accounting.book.created`, `accounting.connection.registered`, `accounting.connection.authorized`, `accounting.connection.revoked`, `accounting.destination.activated`, `accounting.switch.scheduled`, `accounting.switch.canceled`, `accounting.posting.resolved`.
- `adapters::audit::record(scope: &mut ScopedTx<'_>, ids: &dyn RawIdGenerator, record: &AuditRecord) -> Result<(), AccountingError>` inserts one row whose `entry` is `serde_json::to_value(AuditEntryData { actor_type: ActorType::User, actor_id: Some(user_id), on_behalf_of: None, tenant_id: Some(tenant_id), resource_type: Some("accounting_book"), resource_id: Some(book_id.to_string()), before: None, .. })`.

- [ ] **Step 1: Write the migration.**

```sql
CREATE TABLE accounting.audit_commands (
    id bigint PRIMARY KEY CHECK (id > 0),
    tenant_id bigint NOT NULL CHECK (tenant_id > 0),
    book_id bigint NOT NULL,
    entry jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,
    FOREIGN KEY (tenant_id, book_id) REFERENCES accounting.books (tenant_id, id)
);
CREATE INDEX audit_commands_pending ON accounting.audit_commands (created_at, id) WHERE delivered_at IS NULL;
SELECT platform.enable_tenant_rls('accounting', 'audit_commands');
GRANT SELECT, INSERT ON accounting.audit_commands TO sanvi_app;
GRANT UPDATE (delivered_at) ON accounting.audit_commands TO sanvi_app;
```

Add a trigger rejecting an update that changes `delivered_at` from non-null, or changes any other column, modelled on `accounting.postings_terminal_immutable`. The down migration drops the table and states that pending rows must be delivered first. Classify it `rolling`.

- [ ] **Step 2: Write the failing integration tests** (in `accounting_integration.rs` unless noted):
  - `every_management_change_writes_one_audit_command`: run CreateBook, RegisterConnection, ActivateBook, ScheduleSwitch and CancelSwitch with a `RequestContext { request_id: Some("req-1"), ip: Some(127.0.0.1), user_agent: Some("test") }`. Assert the actions in order, one row each, `entry.actor_id` = the user, `entry.request_id == "req-1"`, `entry.resource_id` = the book ID.
  - `stale_revision_writes_no_audit_command`: RegisterConnection with `revision - 1` returns `StaleRevision`; row count unchanged.
  - `failed_verification_rolls_back_audit_command`: ActivateBook with a rejecting verifier returns its error; row count unchanged.
  - `audit_command_entry_is_immutable`: as the runtime role, `UPDATE ... SET entry = '{}'` fails; setting `delivered_at` twice fails; `DELETE` fails.
  - `authorization_integration.rs::callback_and_revoke_are_audited_without_secrets`: complete consent with the existing test OAuth double, then revoke. Expect `accounting.connection.authorized` then `accounting.connection.revoked`. No row's `entry` text contains the test access token, refresh token, code or state.
  - `authorization_integration.rs::concurrent_callbacks_with_one_state_authorize_once` (Review Focus): two concurrent `CompleteAuthorization` calls with the same state. Exactly one is `Ok`, the other is `AuthorizationRejected`, and there is one `authorized` row.
  - `authorization_integration.rs::revoking_twice_audits_once` (Review Focus): the second `RevokeAuthorization` is `Ok` and adds no row.

- [ ] **Step 3: Run** `scripts/test.sh -p sanvi-accounting`. Expected: compile failure on the new signatures.
- [ ] **Step 4: Implement.** Add the types, change the port signatures, and implement `adapters::audit::record`. Call it from `PostgresAccountingStore::create`/`change` and `PostgresCredentialStore::authorize`/`update` next to their outbox append. Build each `AuditRecord` in the use case from command inputs plus `services.clock.now()`. `after` holds IDs, provider, organization ID, dates and mapping codes. Add a private helper `fn audit_record(actor: Actor, request: RequestContext, book_id: BookId, action: &'static str, after: serde_json::Value, now: DateTime<Utc>) -> Result<AuditRecord, AccountingError>` in `application/mod.rs`; it returns `Forbidden` for `Actor::System`. Update existing tests to pass `RequestContext::default()`.
- [ ] **Step 5: Run** `cargo sqlx prepare --workspace`, then `scripts/test.sh -p sanvi-accounting`, `python3 scripts/test-deployment-migrations.py`, `./scripts/check-migration-expand-contract.sh`. Expected: all PASS, 0 violations.
- [ ] **Step 6: Commit** `feat(accounting): record audit commands with every management change`

---

### Task 3: Read model and server-generated connection IDs

**Files:**
- Modify: `crates/contexts/accounting/src/application/mod.rs`, `application/ports.rs`, `adapters/postgres.rs`
- Test: `crates/contexts/accounting/tests/accounting_integration.rs`

**Interfaces:**
- Produces:

```rust
pub struct BookSummary {
    pub book: Book,
    /// Connections with no credential row are absent.
    pub credentials: Vec<(ConnectionId, CredentialStatus)>,
}

pub struct RegisterConnectionCommand {
    pub provider: Provider,
    pub organization_id: String,
    pub mappings: AccountMappings,
}

pub const LIST_BOOKS_LIMIT: i64 = 100;
```

  - `AccountingStore::list(&self, tenant_id) -> Result<Vec<Book>, AccountingError>`: ordered by `id`, `LIMIT 100`.
  - `AccountingStore::credential_statuses(&self, tenant_id, book_id) -> Result<Vec<(ConnectionId, CredentialStatus)>, AccountingError>`: selects only `connection_id, status`.
  - `ListBooks(pub Arc<Services>)::execute(&self, actor) -> Result<Vec<BookSummary>, AccountingError>` (`Read`).
  - `GetBook::execute(&self, actor, book_id) -> Result<BookSummary, AccountingError>` (return type changes).
  - `RegisterConnection::execute(&self, actor, request, book_id, revision, command: RegisterConnectionCommand) -> Result<Book, AccountingError>`: draws the ID from `Services.ids` and builds the `Connection` through its existing constructor.

- [ ] **Step 1: Write the failing tests:**
  - `list_books_returns_only_the_callers_tenant_in_id_order`: two books in tenant A and one in tenant B; A sees 2 in ascending `id` order.
  - `get_book_reports_credential_status_per_connection`: two connections, one with an authorized credential. That one is reported `Active` and the other is absent.
  - `register_connection_assigns_a_snowflake_id`: the returned book's new connection has an ID greater than 0 and different from the book ID; registering twice gives distinct IDs.
  - `read_requires_read_permission`: `TestAccess(false)` makes `ListBooks` return `Forbidden`.
- [ ] **Step 2: Run** `scripts/test.sh -p sanvi-accounting`. Expected: FAIL (missing items).
- [ ] **Step 3: Implement** the two store methods (`query_as!`, tenant transaction), `ListBooks`, the `GetBook` return type, and the new `RegisterConnection` signature. Update existing callers.
- [ ] **Step 4: Run** `cargo sqlx prepare --workspace`, then `scripts/test.sh -p sanvi-accounting`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(accounting): list books with credential status and assign connection ids`

---

### Task 4: Audit delivery job and privacy coverage

**Files:**
- Modify: `crates/contexts/accounting/src/adapters/audit.rs`
- Modify: `crates/contexts/accounting/src/adapters/privacy_provider.rs` (module comment and test)
- Modify: `apps/worker/src/main.rs` (`FeaturesConfig.accounting`, job), `apps/worker/Cargo.toml` if `sanvi-accounting` is missing
- Test: `crates/contexts/accounting/tests/accounting_integration.rs`

**Interfaces:**
- Consumes: `accounting.audit_commands` (Task 2), `sanvi_audit::domain::AuditWriter`.
- Produces: `pub struct AuditDelivery { transactions: TxManager, writer: Arc<dyn AuditWriter> }` with `pub fn new(transactions: TxManager, writer: Arc<dyn AuditWriter>) -> Self` and `pub async fn deliver(&self, limit: i64) -> Result<usize, AccountingError>`. Worker job name `"accounting.audit_commands"`, interval 5 s, limit 100.

- [ ] **Step 1: Write the failing tests:**
  - `delivery_appends_each_command_once`: three commands; `deliver(100)` returns 3 and a second call returns 0. The audit chain holds three entries with the accounting actions, and `VerifyAuditChain` passes.
  - `concurrent_deliveries_do_not_duplicate`: five commands and two `deliver(100)` calls joined concurrently. The results sum to 5 and the chain holds five entries.
  - `privacy_review_covers_audited_users`: after one audited `CreateBook` by user U, `collect` and `erase` for `SubjectRef` tenant-user U both return `PrivacyError::ProcessingPending`.
- [ ] **Step 2: Run** `scripts/test.sh -p sanvi-accounting`. Expected: FAIL.
- [ ] **Step 3: Implement `deliver`.** In one `begin_unscoped("deliver_accounting_audits", "accounting-worker")` scope, select pending rows `ORDER BY created_at, id LIMIT $1 FOR UPDATE SKIP LOCKED`, deserialize each `entry` into `AuditEntryData`, `append` it, set `delivered_at = now()`, then commit. A deserialization failure is an error that leaves the row pending. Rewrite the privacy module comment to say that `oauth_states` and `audit_commands` hold acting user IDs and are covered by the review requirement.
- [ ] **Step 4: Add the worker job** to the `jobs` vector next to `payments.refund_audits`, built only when `config.features.accounting`. Add `accounting: bool` (`#[serde(default)]`) to the worker `FeaturesConfig`, with a doc comment mirroring the api's.
- [ ] **Step 5: Run** `scripts/test.sh -p sanvi-accounting -p sanvi-worker` and `cargo k`. Expected: PASS, no warnings.
- [ ] **Step 6: Commit** `feat(accounting): deliver audit commands to the audit chain`

---

### Task 5: Configuration and composition

**Files:**
- Create: `crates/contexts/accounting/src/config.rs` (`AccountingConfig`)
- Create: `crates/contexts/accounting/src/adapters/http/mod.rs`, `adapters/http/state.rs`
- Create: `apps/api/src/accounting.rs`
- Modify: `apps/api/src/config.rs` (`FeaturesConfig.accounting`, `AppConfig.accounting`, shared-secret check), `apps/api/src/main.rs`, `apps/api/src/lib.rs` (`pub mod accounting;`, `build_app` parameter), `apps/api/Cargo.toml`, `configs/base.toml` (`[accounting]` with empty values and a comment like `[advertising]`)
- Modify: every `build_app(` caller (four call sites) to pass the new argument
- Test: `apps/api/src/accounting.rs` `mod tests`; `apps/api/src/config.rs` tests

**Interfaces:**
- Produces:

```rust
// sanvi_accounting::config
pub struct AccountingConfig {
    pub freee_client_id: String,
    pub freee_client_secret: Redacted<String>,
    pub freee_redirect_uri: String,
    pub oauth_state_signing_secret: Redacted<String>,
    #[serde(default)] pub token_kek: Redacted<String>,
    #[serde(default = "default_token_kek_id")] pub token_kek_id: String,
}

// sanvi_accounting::adapters::http::AccountingState
pub struct AccountingState {
    pub list_books: Arc<ListBooks>,
    pub get_book: Arc<GetBook>,
    pub create_book: Arc<CreateBook>,
    pub register_connection: Arc<RegisterConnection>,
    pub activate_book: Arc<ActivateBook>,
    pub schedule_switch: Arc<ScheduleSwitch>,
    pub cancel_switch: Arc<CancelSwitch>,
    /// `None` until freee secrets are deployed; consent routes answer 503.
    pub consent: Option<Consent>,
}
pub struct Consent {
    pub start: Arc<StartAuthorization>,
    pub complete: Arc<CompleteAuthorization>,
    pub revoke: Arc<RevokeAuthorization>,
}

// apps/api/src/accounting.rs
pub fn build_accounting_state(
    config: &AccountingConfig,
    transactions: TxManager,
    ids: Arc<dyn RawIdGenerator>,
    authorizer: Arc<dyn Authorizer>,
    clock: Arc<dyn Clock>,
) -> anyhow::Result<AccountingState>;
```

- `AuthorizerAccess(Arc<dyn Authorizer>)` in `apps/api/src/accounting.rs` implements `AccountingAccess`. It maps `Read` to `tenant.accounting.read` and `Manage` to `tenant.accounting.manage`, and calls `check_fresh` for `Manage` and `check` for `Read`, with `ResourceRef::Tenant(tenant_id)`. `Denied` maps to `Forbidden`; an `AccessError` maps to `ProviderUnavailable`, never `Ok`. It returns `Forbidden` for `Export` and for any system actor.
- `build_app` gains `accounting: Option<sanvi_accounting::adapters::http::AccountingState>` after `advertising`.

- [ ] **Step 1: Write the failing unit tests** in `apps/api/src/accounting.rs`:
  - `empty_secrets_compose_without_consent_and_verification_fails_closed`: with `AccountingConfig` all empty, `state.consent.is_none()`. `activate_book` against a stub store returns `VerificationUnavailable`, which proves `UnavailableVerifier` is in use.
  - `malformed_kek_is_a_boot_error`: with every secret set but `token_kek = "not-base64"`, `build_accounting_state` returns `Err`, and the message names `token_kek` without containing its value.
  - `empty_redirect_uri_with_secrets_is_a_boot_error`.
  - `access_maps_permissions_and_denies_export`: a stub `Authorizer` records keys. `Read` checks `tenant.accounting.read`, `Manage` checks `tenant.accounting.manage` (fresh), `Export` is `Forbidden` without calling the authorizer, and `AccessError` becomes `ProviderUnavailable`.
  - In `config.rs`: `accounting_is_off_by_default` and `accounting_state_secret_must_differ_from_advertising`.
- [ ] **Step 2: Run** `cargo c`, then `scripts/test.sh -p sanvi-api`. Expected: FAIL.
- [ ] **Step 3: Implement.** Secrets count as present only when client ID, client secret, state secret and KEK are all non-empty. Then build `FreeeClient::new(FreeeSettings::production(...))`, `PostgresCredentialStore::new(..)`, `Authorization` and `CredentialVerifier`, and set `consent: Some(..)`. Otherwise use `UnavailableVerifier` and `consent: None`, and log one `warn` naming the empty keys. In `main.rs`, build the state only when `config.features.accounting` (exit 1 on `Err`, like advertising). Pass it to `build_app`, which does not mount routes yet. Callers in tests pass `None`.
- [ ] **Step 4: Run** `cargo c`, `cargo k`, `scripts/test.sh -p sanvi-api`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(api): compose the accounting context behind features.accounting`

---

### Task 6: HTTP handlers and routes

**Files:**
- Create: `crates/contexts/accounting/src/adapters/http/handlers.rs`, `adapters/http/routes.rs`
- Modify: `crates/contexts/accounting/Cargo.toml` (`actix-web`, `utoipa`, `sanvi-http`)
- Modify: `apps/api/src/lib.rs` (mount `accounting_tenant_scope_routes` in the tenant scope when `config.features.accounting` and the state is `Some`; register `web::Data<AccountingState>`)
- Create: `apps/api/tests/integration/accounting_routes.rs`, registered in `apps/api/tests/integration/main.rs`
- Test: handler unit tests for `api_error` in `handlers.rs`

**Interfaces:**
- Consumes: `AccountingState` (Task 5), use cases (Tasks 2–3).
- Produces: `pub fn accounting_tenant_scope_routes(cfg: &mut web::ServiceConfig)`, `pub const ACCOUNTING_ENTITLEMENTS: &[&str] = &["accounting.freee"];`, `pub fn api_error(error: AccountingError) -> ApiError`, and `AccountingProblemType` with types `accounting/authorization-rejected`, `accounting/reauthorization-required`, `accounting/verification-failed`, `accounting/provider-unavailable`.
- Routes, permissions and fresh-MFA as in the spec's route table. Handler operation IDs: `list_accounting_books`, `create_accounting_book`, `get_accounting_book`, `register_accounting_connection`, `start_accounting_authorization`, `revoke_accounting_authorization`, `complete_accounting_authorization`, `activate_accounting_book`, `schedule_accounting_switch`, `cancel_accounting_switch`. Tag `accounting`.
- Views: `BookView { id, legal_entity_key, revision, connections: Vec<ConnectionView>, periods: Vec<PeriodView>, scheduled_switch: Option<PeriodView> }`, `ConnectionView { id, provider, organization_id, mappings, credential_status }`, `PeriodView { connection_id, starts_at, ends_at: Option, state }`. `state` is one of `active|scheduled|draining|closed`, derived from `Book` methods at the request clock. `credential_status` is one of `none|active|reauthorization_required|revoked`. `StartAuthorizationResponse { authorization_url }`.
- Requests: `CreateBookRequest { legal_entity_key }`, `RegisterConnectionRequest { provider, organization_id, mappings }`, `CompleteAuthorizationRequest { state, code }`, `ActivateBookRequest { connection_id, starts_at }`, `ScheduleSwitchRequest { connection_id, cutover_at }`. All are `deny_unknown_fields`, and datetimes are RFC 3339 with an offset.
- `RequestContext` comes from `sanvi_http::RequestId`, `sanvi_http::client_ip(&req)` and the `User-Agent` header, truncated to 512 bytes.

- [ ] **Step 1: Write the failing `api_error` unit test** `error_mapping_matches_the_spec`. It asserts the status for every `AccountingError` variant per the spec table (`Invalid` 400, `NotFound` 404, `Forbidden` 403, `StaleRevision` 409, `Conflict` 409, `IdempotencyConflict` 409, `ReauthorizationRequired` 409 + type, `AuthorizationRejected` 400 + type, `VerificationFailed` 422 + type, `HistoricalImport` 422, `VerificationUnavailable` 503 + type, `ProviderUnavailable` 503 + type, `Storage` 500). It also asserts that `Storage("secret detail")` does not expose `"secret detail"`.
- [ ] **Step 2: Write the failing route tests** in `accounting_routes.rs`, reusing the session, Keto-stub and tenant fixtures from `idor_sweep.rs` (extract them into a shared module if they are private):
  - `routes_are_absent_when_the_flag_is_off`: `GET /api/v1/tenant/accounting/books` is 404.
  - `entitlement_is_required`: the flag is on, the entitlement is not granted, and the user is an owner: 403.
  - `member_cannot_read_owner_and_admin_can`.
  - `create_register_and_read_round_trip`: `POST /books` gives 201 with `ETag: "1"` (or the actual initial revision). `POST /connections` with `If-Match` gives 200 and a new `ETag`. `GET` shows the connection with `credential_status: "none"`.
  - `missing_if_match_is_400_and_stale_is_409`.
  - `fresh_mfa_is_required_for_consent_activation_and_switch`: a stale-AAL session gets 403 on those four routes and 200/503 with fresh MFA.
  - `consent_routes_answer_503_without_secrets` and `activation_answers_503_without_secrets` (Review Focus).
  - `callback_after_losing_manage_is_403_and_keeps_state` (Review Focus): start, revoke the role, call the callback: 403; restore the role and call the callback with the same state: it reaches the exchange (an `AuthorizationRejected` from the test double counts, since that proves the state was not burned).
  - `naive_datetime_is_rejected` (Review Focus): `starts_at: "2026-11-01T00:00:00"` gives 400.
  - `unknown_body_field_is_rejected`: `POST /books` with `tenant_id` in the body gives 400.
  - `responses_never_contain_tokens`: after consent with the test double, no response body or header contains its token strings.
- [ ] **Step 3: Run** `scripts/test.sh -p sanvi-accounting -p sanvi-api`. Expected: FAIL.
- [ ] **Step 4: Implement** the handlers with `#[utoipa::path]` (document 503 "freee is not configured" and 422 "provider verification failed"), the routes using `require_permission!("tenant.accounting.read", tenant)` / `("tenant.accounting.manage", tenant)` plus `RequiredAnyFeature::declared(ACCOUNTING_ENTITLEMENTS)` on every resource, `FreshAal2Required` on the four routes, and the mount in `lib.rs`. Return `ETag` on every book response. `POST /books` returns 201.
- [ ] **Step 5: Run** `cargo k`, then `scripts/test.sh -p sanvi-accounting -p sanvi-api`. Expected: PASS.
- [ ] **Step 6: Commit** `feat(accounting): serve the tenant accounting API`

---

### Task 7: Contract, route matrix, docs and gates

**Files:**
- Modify: `apps/api/src/openapi.rs` (paths, schemas, `accounting` tag, `doc_contains_the_accounting_contract` test)
- Create: `api/accounting.yaml` (generated)
- Modify: `docs/security/route-metadata.json` and generated matrix (`just route-matrix-write` / `scripts/generate-route-matrix.sh --write`)
- Modify: `apps/api/tests/integration/idor_sweep.rs` (build `accounting` state, seed tenant B book and connection with the marker string, include routes)
- Modify: `crates/contexts/accounting/README.md`, `docs/superpowers/plans/2026-10-26-accounting-connections.md` (mark the backend half of the API step done, frontend open), `docs/security/threat-model.md` (consent callback, audit command integrity)

- [ ] **Step 1: Write the failing test** `doc_contains_the_accounting_contract`. It asserts the JSON contains `/api/v1/tenant/accounting/books`, `/api/v1/tenant/accounting/authorization/callback` and `"accounting"`.
- [ ] **Step 2: Register** the handlers and schemas in `ApiDoc`. Tag description: "Tenant accounting destinations and freee consent. Problem types: accounting/authorization-rejected, accounting/reauthorization-required, accounting/verification-failed, accounting/provider-unavailable. Book mutations require If-Match with the book revision. Nothing is posted to a provider yet."
- [ ] **Step 3: Generate** with `cargo run -p sanvi-cli -- openapi --write`, then `just route-matrix-write` (or `./scripts/generate-route-matrix.sh --write`). Fill route metadata with the declared permission, `CurrentTenant` and handler evidence.
- [ ] **Step 4: Extend the IDOR sweep** so the coverage test lists every accounting `CurrentTenant` route, and every ID-bearing route goes through sweep 2. Then run `scripts/test.sh -p sanvi-api`. Expected: PASS, with tenant B's marker absent from all responses.
- [ ] **Step 5: Update the docs.** The README lists the routes and composition and changes the legal-entity sentence to "supplied by a tenant admin as a stable label". The delivery plan's "Authenticated API and admin screens" section notes the backend is done and the Svelte screens remain. Add the threat-model entries.
- [ ] **Step 6: Run all gates:** `just ci-quiet`, or without `just` the `scripts/*.sh` equivalents in `CLAUDE.md`, plus `python3 scripts/test-deployment-migrations.py`. Expected: every gate passes. Report any that fail with their output; do not claim green.
- [ ] **Step 7: Commit** `docs(accounting): publish the accounting contract and route matrix`
