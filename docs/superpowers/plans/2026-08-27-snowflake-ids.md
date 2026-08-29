# Snowflake Identifier Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace every Sanvi-owned backend UUID or identity-generated entity ID with a typed, application-generated Snowflake stored as PostgreSQL `BIGINT` and serialized as a decimal string.

**Architecture:** `sanvi-kernel` owns validated raw and tagged Snowflake values plus a fallible, object-safe generator backed by `id-forge`. Bounded contexts keep local typed-ID ports, persistence adapters alone convert to `i64`, and API adapters describe IDs as strings. This is a database-reset flag-day migration; approved UUID exceptions remain unchanged.

**Tech Stack:** Rust, `id-forge 1.0.0`, Serde, Actix Web, Utoipa, SQLx 0.9, PostgreSQL 18, Redis, Keto, Nextest

**Spec:** `docs/superpowers/specs/2026-08-27-snowflake-ids-design.md`

## Global Constraints

- Work only in `/Users/hitohospital/personal/dev/sanvi/.worktrees/sanvi-backend-snowflake-ids` on `feat/snowflake-ids`.
- Preserve UUIDs for request/trace IDs, OAuth nonces, Ory IDs, provider IDs, and caller-owned idempotency keys.
- Use epoch `2026-01-01T00:00:00Z`, 41 timestamp bits, 10 node bits, and 12 sequence bits.
- Runtime node IDs are `1..=1023`; node `0` is reserved for stable seed IDs.
- Store first-party IDs as PostgreSQL `BIGINT`; serialize them as quoted canonical decimal strings.
- Do not add `utoipa`, SQLx, Actix, or configuration-framework dependencies to `sanvi-kernel`.
- Keep context domain layers dependent only on `sanvi-kernel`, std, and Serde.
- Do not preserve or backfill existing UUID data; all migration verification starts from a clean database.
- Do not edit `sanvi-frontend` or `hitox-media-service`.
- Use `rtk` before every shell command.
- Use `apply_patch` for file edits.
- Run the exact repository gates from `CLAUDE.md`; do not substitute commands.

## File ownership map

- `crates/kernel/src/ids.rs`: raw Snowflake value, tagged IDs, generator port/adapter, parsing and Serde.
- `crates/kernel/src/events.rs`: typed event ID and raw Snowflake aggregate reference.
- `crates/platform/persistence/src/tx.rs`: generator-aware transaction manager and platform audit IDs.
- `crates/platform/messaging/**`: `BIGINT` outbox and processed-event readers/writers.
- `crates/contexts/<context>/src/domain/**`: typed first-party IDs and fallible ID ports.
- `crates/contexts/<context>/src/adapters/**`: `i64` SQLx rows, typed conversions, string boundary values.
- `migrations/*.sql`: reset-only `BIGINT` schema, RLS casts, seeds, and approved UUID exceptions.
- `apps/{api,worker,cli}/**`: node configuration, shared generator construction, and injection.
- `crates/platform/http/src/openapi.rs`: reusable Snowflake OpenAPI schema helper.
- `scripts/check-id-policy.sh`: permanent policy gate.
- `.sqlx/**` and `api/*.yaml`: generated artifacts; never hand-edit `.sqlx` query files.

## Approved UUID exception vocabulary

The policy gate may allow UUID only when the field name or exact schema entry is one of:

```text
request_id
trace_id
nonce
idempotency_key
command_id
kratos_identity_id
provider_event_id when its provider contract is UUID
```

Provider IDs stored as `text` are not converted. Any new exception requires a field-specific rationale in `scripts/id-policy-uuid-exceptions.txt`.

---

### Task 1: Add Snowflake kernel primitives and generator

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/kernel/Cargo.toml`
- Modify: `crates/kernel/src/error.rs`
- Modify: `crates/kernel/src/ids.rs`
- Modify: `crates/kernel/src/lib.rs`
- Test: inline tests in `crates/kernel/src/ids.rs`

**Interfaces:**
- Produces: `SnowflakeId`, `Id<Tag>`, `NodeId`, `RawIdGenerator`, `SnowflakeGenerator`, `IdGenerationError`.
- Consumes: `id_forge::snowflake::{Snowflake, ClockSkew, DEFAULT_EPOCH_MS}`.

- [ ] **Step 1: Write failing kernel tests for value and wire semantics**

Replace UUID-specific tests with tests equivalent to:

```rust
#[test]
fn snowflake_value_rejects_non_positive_values() {
    assert!(SnowflakeId::try_from(0_i64).is_err());
    assert!(SnowflakeId::try_from(-1_i64).is_err());
}

#[test]
fn tagged_id_serde_is_a_decimal_string_only() {
    let id = TestId::try_from(873_698_342_314_721_281_i64).unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"873698342314721281\"");
    assert_eq!(serde_json::from_str::<TestId>("\"873698342314721281\"").unwrap(), id);
    assert!(serde_json::from_str::<TestId>("873698342314721281").is_err());
    assert!(serde_json::from_str::<TestId>("\"01\"").is_err());
}

#[test]
fn generator_uses_requested_node_and_is_unique_under_concurrency() {
    let generator = Arc::new(SnowflakeGenerator::new(NodeId::new(17).unwrap()));
    let ids = (0..8)
        .map(|_| {
            let generator = Arc::clone(&generator);
            std::thread::spawn(move || {
                (0..2_000)
                    .map(|_| generator.next_id().unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .flat_map(|thread| thread.join().unwrap())
        .collect::<HashSet<_>>();
    assert_eq!(ids.len(), 16_000);
    assert!(ids.iter().all(|id| id.node_id() == 17));
}
```

- [ ] **Step 2: Run the kernel tests and verify the old implementation fails**

Run: `rtk cargo test -p sanvi-kernel ids -- --nocapture`  
Expected: FAIL because `SnowflakeId`, `NodeId`, and `SnowflakeGenerator` do not exist and numeric JSON is still UUID-based.

- [ ] **Step 3: Add the pinned dependency**

Use these manifest entries:

```toml
# workspace.dependencies
id-forge = { version = "=1.0.0", default-features = false, features = ["snowflake"] }

# crates/kernel/Cargo.toml
id-forge.workspace = true
```

Retain the existing `uuid` workspace dependency because approved exceptions still need it, but remove `uuid` from `sanvi-kernel` after `events.rs` is converted in Task 2.

- [ ] **Step 4: Implement the value objects and fallible generator**

Use these public signatures:

```rust
pub const SNOWFLAKE_EPOCH_MS: u64 = id_forge::snowflake::DEFAULT_EPOCH_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnowflakeId(NonZeroI64);

impl SnowflakeId {
    pub const fn as_i64(self) -> i64;
    pub fn node_id(self) -> u16;
}

impl TryFrom<i64> for SnowflakeId {
    type Error = KernelError;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeId(u16);

impl NodeId {
    pub fn new(value: u16) -> Result<Self, IdGenerationError>;
    pub const fn get(self) -> u16;
}

pub trait RawIdGenerator: Send + Sync {
    fn next_id(&self) -> Result<SnowflakeId, IdGenerationError>;
}

pub struct SnowflakeGenerator {
    inner: id_forge::snowflake::Snowflake,
}

impl SnowflakeGenerator {
    pub fn new(node_id: NodeId) -> Self;
}

pub struct Id<Tag> {
    raw: SnowflakeId,
    _tag: PhantomData<fn() -> Tag>,
}

impl<Tag> Id<Tag> {
    pub const fn from_snowflake(raw: SnowflakeId) -> Self;
    pub const fn as_snowflake(self) -> SnowflakeId;
    pub const fn as_i64(self) -> i64;
}

impl<Tag> TryFrom<i64> for Id<Tag> {
    type Error = KernelError;
}
```

`Serialize` must call `serializer.serialize_str(&self.to_string())`.
`Deserialize` must call `deserializer.deserialize_str(...)`; it must not use
`deserialize_any`, which would admit JSON numbers. `FromStr` must reject empty
strings, signs, whitespace, leading zeroes, zero, and overflow before creating
`SnowflakeId`.

Map `ClockSkew { last_ms, now_ms }` into:

```rust
#[derive(Debug, thiserror::Error)]
pub enum IdGenerationError {
    #[error("snowflake node id must be between 1 and 1023, got {0}")]
    InvalidNode(u16),
    #[error("system clock moved backwards from {last_ms}ms to {now_ms}ms since epoch")]
    ClockSkew { last_ms: u64, now_ms: u64 },
    #[error("snowflake value exceeded signed 64-bit storage")]
    OutOfRange,
}
```

- [ ] **Step 5: Run kernel tests and formatting**

Run: `rtk cargo test -p sanvi-kernel ids -- --nocapture`  
Expected: PASS, including 16,000 concurrent IDs with no duplicates.  
Run: `rtk cargo fmt --check`  
Expected: PASS.

- [ ] **Step 6: Commit the kernel foundation**

```bash
rtk git add Cargo.toml Cargo.lock crates/kernel
rtk git commit -m "feat(kernel): add snowflake ID primitives" -m "Provide validated string-serialized IDs and fallible node-aware generation before converting bounded contexts."
```

---

### Task 2: Convert domain event envelopes and platform messaging

**Files:**
- Modify: `crates/kernel/src/events.rs`
- Modify: `crates/kernel/src/lib.rs`
- Modify: `crates/kernel/Cargo.toml`
- Modify: `crates/platform/messaging/src/consumer.rs`
- Modify: `crates/platform/messaging/src/outbox.rs`
- Modify: `crates/platform/messaging/src/writer.rs`
- Modify: `crates/platform/messaging/tests/integration/consumer_integration.rs`
- Modify: `crates/platform/messaging/tests/integration/outbox_integration.rs`
- Modify: `crates/contexts/{access,advertising,billing,domains,identity,localization,payments,platform_admin,privacy,tenancy,theming}/src/domain/events.rs`
- Modify: `crates/contexts/{access,advertising,billing,domains,identity,localization,payments,platform_admin,privacy,tenancy,theming}/src/adapters/events.rs`
- Test: inline tests in `crates/kernel/src/events.rs`

**Interfaces:**
- Produces: `EventId`, `EventEnvelope { id: EventId, aggregate_id: SnowflakeId }`.
- Consumes: `RawIdGenerator` at event-publisher adapter boundaries.

- [ ] **Step 1: Write failing envelope tests**

```rust
#[test]
fn envelope_serializes_event_and_aggregate_ids_as_strings() {
    let envelope = EventEnvelope::new(
        EventId::try_from(101_i64).unwrap(),
        "tenancy.tenant",
        SnowflakeId::try_from(202_i64).unwrap(),
        "tenancy.tenant_created",
        serde_json::json!({}),
    );
    let json = serde_json::to_value(envelope).unwrap();
    assert_eq!(json["id"], "101");
    assert_eq!(json["aggregate_id"], "202");
}
```

- [ ] **Step 2: Run the kernel event test to verify failure**

Run: `rtk cargo test -p sanvi-kernel events -- --nocapture`  
Expected: FAIL because the envelope still accepts and generates UUIDv7 values.

- [ ] **Step 3: Make event creation explicit and fallible at publishers**

Implement:

```rust
pub enum EventTag {}
pub type EventId = Id<EventTag>;

impl EventEnvelope {
    pub fn new(
        id: EventId,
        aggregate: impl Into<String>,
        aggregate_id: SnowflakeId,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self;
}
```

Each publisher adapter stores `Arc<dyn RawIdGenerator>`, calls
`next_id().map(EventId::from_snowflake)`, then passes that ID into the context
event conversion. Context domain event conversions accept `EventId` and never
call the system clock or generator themselves.

- [ ] **Step 4: Convert messaging SQL row types and Redis payload tests**

Use `i64` for outbox and processed event query fields. Convert through
`EventId::try_from(row.id)` and `SnowflakeId::try_from(row.aggregate_id)`.
Redis payload JSON remains quoted decimal strings because the envelope owns
Serde.

- [ ] **Step 5: Run scoped tests**

Run: `rtk cargo test -p sanvi-kernel events`  
Expected: PASS.  
Run: `rtk cargo test -p sanvi-messaging --lib`  
Expected: PASS without requiring the not-yet-converted database schema. Task 4 reruns messaging integration tests immediately after the core migration rewrite.

- [ ] **Step 6: Commit event and messaging conversion**

```bash
rtk git add crates/kernel crates/platform/messaging crates/contexts/*/src/domain/events.rs crates/contexts/*/src/adapters/events.rs
rtk git commit -m "refactor(events): use snowflake envelopes" -m "Generate event IDs through injected ports and keep heterogeneous aggregate references as validated Snowflake values."
```

---

### Task 3: Make transactions and test support generator-aware

**Files:**
- Modify: `crates/platform/persistence/Cargo.toml`
- Modify: `crates/platform/persistence/src/tx.rs`
- Modify: `crates/platform/persistence/src/lib.rs`
- Modify: `crates/platform/persistence/tests/postgres_integration.rs`
- Modify: `crates/platform/testsupport/src/lib.rs`
- Modify: `crates/platform/testsupport/src/postgres.rs`
- Modify: all `TxManager::new(...)` call sites returned by `rtk rg -l 'TxManager::new\(' apps crates`

**Interfaces:**
- Produces: `TxManager::new(pool, Arc<dyn RawIdGenerator>)` and `sanvi_testsupport::test_ids()`.
- Consumes: `SnowflakeId` for `platform.audit_log.id`.

- [ ] **Step 1: Add a failing platform-audit ID integration test**

```rust
#[tokio::test]
async fn platform_scope_writes_a_snowflake_audit_id() {
    let db = sanvi_testsupport::test_db().await;
    let manager = TxManager::new(db.app_pool(), sanvi_testsupport::test_ids());
    manager.begin_unscoped("test", "tester").await.unwrap().commit().await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM platform.audit_log LIMIT 1")
        .fetch_one(&db.admin_pool())
        .await
        .unwrap();
    assert!(SnowflakeId::try_from(id).is_ok());
}
```

- [ ] **Step 2: Change the constructor and audit insertion**

```rust
#[derive(Clone)]
pub struct TxManager {
    pool: PgPool,
    ids: Arc<dyn RawIdGenerator>,
}

impl TxManager {
    pub fn new(pool: PgPool, ids: Arc<dyn RawIdGenerator>) -> Self;
}
```

`begin_unscoped` generates the audit ID before opening the transaction and
inserts `id, actor, action, reason, detail`. Map generator errors into
`anyhow::Error` with context `failed to generate platform audit id`.

- [ ] **Step 3: Add deterministic test generation**

`test_ids()` returns one shared atomic generator per test process:

```rust
pub fn test_ids() -> Arc<dyn RawIdGenerator> {
    Arc::new(TestIdGenerator::new(1_000_000))
}
```

`TestIdGenerator` uses `AtomicI64::fetch_add` and rejects no positive output.
It is test support only and must never be exported from production crates.

- [ ] **Step 4: Update every constructor call explicitly**

Application composition roots pass their process generator. Tests and fixtures
pass `sanvi_testsupport::test_ids()`. Do not add a one-argument compatibility
constructor or hidden default node.

- [ ] **Step 5: Verify persistence**

Run: `rtk cargo test -p sanvi-persistence`  
Expected: PASS for unit tests; integration test passes after core migration conversion.  
Run: `rtk cargo c`  
Expected: remaining errors identify not-yet-converted first-party UUID types, not missing `TxManager` arguments.

- [ ] **Step 6: Commit transaction injection**

```bash
rtk git add crates/platform/persistence crates/platform/testsupport apps crates
rtk git commit -m "refactor(persistence): inject ID generator" -m "Platform audit rows need configured Snowflakes, so every transaction manager receives an explicit generator and tests use deterministic IDs."
```

---

### Task 4: Rewrite core, tenancy, identity, and access schemas

**Files:**
- Modify: `migrations/20260101000000_platform_bootstrap.sql`
- Modify: `migrations/20260101000000_platform_bootstrap.down.sql`
- Modify: `migrations/20260816000000_tenancy_core.sql`
- Modify: `migrations/20260816000000_tenancy_core.down.sql`
- Modify: `migrations/20260816010000_identity_access.sql`
- Modify: `migrations/20260816010000_identity_access.down.sql`
- Modify: `migrations/20260817000000_platform_admin_entitlements.sql`
- Modify: `migrations/20260817000000_platform_admin_entitlements.down.sql`
- Test: `crates/platform/persistence/tests/postgres_integration.rs`
- Test: `crates/platform/messaging/tests/integration/consumer_integration.rs`
- Test: `crates/platform/messaging/tests/integration/outbox_integration.rs`

**Interfaces:**
- Produces: `BIGINT` platform/tenant/identity/access/admin schema and bigint RLS GUC.
- Consumes: positive `i64` IDs from adapters.

- [ ] **Step 1: Add failing schema assertions**

Add a helper that queries `information_schema.columns` and assert:

```rust
assert_column_type(&admin, "tenancy", "tenants", "id", "bigint").await;
assert_column_type(&admin, "identity", "users", "id", "bigint").await;
assert_column_type(&admin, "identity", "users", "kratos_identity_id", "uuid").await;
assert_column_type(&admin, "platform", "outbox", "id", "bigint").await;
assert_column_type(&admin, "platform", "idempotency_keys", "key", "text").await;
```

- [ ] **Step 2: Rewrite IDs and RLS casts**

Change first-party `uuid` columns to `bigint`, including nullable actor and
foreign-key columns. Change only RLS tenant casts to:

```sql
NULLIF(current_setting('sanvi.tenant_id', true), '')::bigint
```

Keep `kratos_identity_id uuid`, `nonce uuid`, UUID idempotency columns, and
request-correlation fields unchanged.

- [ ] **Step 3: Replace identity-generated audit IDs**

Use:

```sql
CREATE TABLE platform.audit_log (
  id bigint PRIMARY KEY,
  actor text NOT NULL,
  action text NOT NULL,
  reason text,
  detail jsonb NOT NULL DEFAULT '{}',
  at timestamptz NOT NULL DEFAULT now()
);
```

Keep `audit.entries.seq bigserial` unchanged.

- [ ] **Step 4: Reset and migrate a clean test database**

Run: `rtk scripts/test-db.sh down`  
Run: `rtk scripts/test-db.sh up`  
Run: `rtk scripts/test.sh -p sanvi-persistence --quiet`  
Run: `rtk scripts/test.sh -p sanvi-messaging --quiet`  
Expected: migrations apply cleanly; schema assertions and Snowflake outbox/consumer integration tests pass.

- [ ] **Step 5: Commit core schema rewrite**

```bash
rtk git add migrations/20260101000000_platform_bootstrap* migrations/20260816000000_tenancy_core* migrations/20260816010000_identity_access* migrations/20260817000000_platform_admin_entitlements* crates/platform/persistence/tests
rtk git commit -m "refactor(db): use bigint core IDs" -m "Database reset permits first-party platform, tenancy, identity, and access keys to move directly to Snowflake storage while external UUID columns remain intact."
```

---

### Task 5: Convert tenancy, identity, access, and platform-admin contexts

**Files:**
- Modify: every UUID-bearing Rust file under `crates/contexts/tenancy/**`
- Modify: every UUID-bearing Rust file under `crates/contexts/identity/**`
- Modify: every UUID-bearing Rust file under `crates/contexts/access/**`
- Modify: every UUID-bearing Rust file under `crates/contexts/platform_admin/**`
- Modify: `crates/platform/http/src/{authn.rs,authz.rs,features.rs,impersonation.rs,tenancy.rs}`
- Test: context unit and integration tests in those four crates

**Interfaces:**
- Produces: typed `TenantId`, `UserId`, `RoleId`, `MembershipId`, `InvitationId`, `ApprovalId`, and `ImpersonationGrantId` throughout first-party paths.
- Preserves: `kratos_identity_id: Uuid`, request IDs, signed-token nonces, and idempotency keys.

- [ ] **Step 1: Move shared identity tags into the kernel**

Define and export:

```rust
pub enum TenantTag {}
pub enum UserTag {}
pub enum RoleTag {}
pub type TenantId = Id<TenantTag>;
pub type UserId = Id<UserTag>;
pub type RoleId = Id<RoleTag>;
```

Owning contexts re-export these aliases. Cross-context ports use these shared
types instead of raw UUIDs. Context-specific IDs keep context-local tags.

- [ ] **Step 2: Make context generators fallible**

Example contract:

```rust
pub trait IdGenerator: Send + Sync {
    fn next_tenant_id(&self) -> Result<TenantId, TenancyError>;
}
```

Add `IdGeneration(#[from] sanvi_kernel::IdGenerationError)` to each context
error. HTTP mappers classify it as retryable service unavailable.

- [ ] **Step 3: Convert SQLx rows and query arguments**

Use this adapter pattern everywhere:

```rust
let tenant_id = TenantId::try_from(row.tenant_id)
    .map_err(|error| TenancyError::Persistence(error.to_string()))?;

sqlx::query!("SELECT ... WHERE tenant_id = $1", tenant_id.as_i64());
```

Do not use `as` casts. Invalid database IDs must be observable adapter errors.

- [ ] **Step 4: Keep external identity UUIDs explicit**

Names must make exceptions obvious:

```rust
pub struct User {
    id: UserId,
    kratos_identity_id: uuid::Uuid,
    // ...
}
```

Signed internal tenant tokens carry `TenantId` as a decimal string but retain
their random/request nonce UUID where present.

- [ ] **Step 5: Run each context gate before continuing**

Run:

```bash
rtk cargo test -p sanvi-tenancy
rtk cargo test -p sanvi-identity
rtk cargo test -p sanvi-access
rtk cargo test -p sanvi-platform-admin
rtk scripts/test.sh -p sanvi-tenancy --quiet
rtk scripts/test.sh -p sanvi-identity --quiet
rtk scripts/test.sh -p sanvi-access --quiet
rtk scripts/test.sh -p sanvi-platform-admin --quiet
```

Expected: all pass; UUID assertions remain only for approved exception fields.

- [ ] **Step 6: Commit identity-plane conversion**

```bash
rtk git add crates/kernel crates/contexts/tenancy crates/contexts/identity crates/contexts/access crates/contexts/platform_admin crates/platform/http
rtk git commit -m "refactor(identity): adopt typed snowflake IDs" -m "Use shared tenant, user, and role identifiers across bounded-context ports while preserving Ory and security UUID contracts."
```

---

### Task 6: Convert billing and audit storage and contexts

**Files:**
- Modify: `migrations/20260818000000_platform_billing.sql`
- Modify: `migrations/20260818000000_platform_billing.down.sql`
- Modify: `migrations/20260824000000_billing_hardening.sql`
- Modify: `migrations/20260824000000_billing_hardening.down.sql`
- Modify: every UUID-bearing file under `crates/contexts/billing/**`
- Modify: every UUID-bearing file under `crates/contexts/audit/**`
- Test: `crates/contexts/billing/tests/billing_integration.rs`
- Test: `crates/contexts/audit/tests/audit_integration.rs`

**Interfaces:**
- Produces: typed plan, price, subscription, invoice, and audit entry IDs.
- Preserves: Stripe IDs as `String`, webhook IDs as provider-owned strings, UUID idempotency keys.

- [ ] **Step 1: Add failing seed and type tests**

Assert plan seeds use node-zero constants `1`, `2`, `3`, are positive, and
deserialize from strings. Assert `audit.entries.id` is `bigint` while `seq`
remains sequence-backed.

- [ ] **Step 2: Rewrite billing/audit migrations and seed literals**

Use reserved seed IDs:

```sql
INSERT INTO billing.plans (id, key, name, tier, stripe_product_id, visibility, sort_order) VALUES
  (1, 'starter', 'Starter', 'starter', 'prod_starter_placeholder', 'public', 1),
  (2, 'professional', 'Professional', 'professional', 'prod_professional_placeholder', 'public', 2),
  (3, 'enterprise', 'Enterprise', 'enterprise', 'prod_enterprise_placeholder', 'public', 3);
```

Every reference to these plans uses the same bigint literal.

- [ ] **Step 3: Convert domain and adapters**

Replace raw tenant/actor UUIDs with `TenantId`/`UserId`. Keep provider values
opaque. Stripe metadata parsing becomes:

```rust
let tenant_id = metadata
    .get("tenant_id")
    .ok_or(BillingError::MissingTenant)?
    .parse::<TenantId>()?;
```

Rename tests such as `tenant_id_from_metadata_accepts_uuids_only` to assert
canonical Snowflake decimal strings.

- [ ] **Step 4: Verify billing and audit**

Run: `rtk scripts/test.sh -p sanvi-billing --quiet`  
Run: `rtk scripts/test.sh -p sanvi-audit --quiet`  
Expected: PASS, including seed and chain verification.

- [ ] **Step 5: Commit financial platform conversion**

```bash
rtk git add migrations/20260818000000_platform_billing* migrations/20260824000000_billing_hardening* crates/contexts/billing crates/contexts/audit
rtk git commit -m "refactor(billing): use snowflake entity IDs" -m "Convert billing and audit ownership to BIGINT Snowflakes while retaining Stripe and idempotency identifiers at their external formats."
```

---

### Task 7: Convert privacy, localization, theming, and domains

**Files:**
- Modify migrations and paired down files:
  - `migrations/20260819000000_privacy_compliance.sql`
  - `migrations/20260824010000_privacy_rejection_reviewer.sql`
  - `migrations/20260825000000_payments_retention_override.sql`
  - `migrations/20260820000000_localization_i18n.sql`
  - `migrations/20260821000000_theming_core.sql`
  - `migrations/20260822000000_domains_core.sql`
- Modify: every UUID-bearing file under `crates/contexts/{privacy,localization,theming,domains}/**`
- Test: integration suites in all four contexts

**Interfaces:**
- Produces: typed privacy request/subject, theme/version, custom-domain/order/certificate/DNS-record IDs.
- Preserves: challenge tokens, registrar/provider IDs, certificate serial text, UUID idempotency keys.

- [ ] **Step 1: Add failing boundary tests**

Add tests proving privacy request paths accept decimal IDs, theme seed IDs are
reserved node-zero Snowflakes, and domain order retries preserve UUID
idempotency while returning Snowflake order IDs.

- [ ] **Step 2: Rewrite migrations and stable system actor literals**

Replace UUID seed actors with reserved node-zero bigint constants documented
in comments. Use distinct values for the base theme, base theme version,
privacy migration actor, payments retention actor, and advertising migration
actor. Keep each value identical in its paired down migration.

- [ ] **Step 3: Convert contexts using typed adapter boundaries**

Privacy `request_id` fields that identify DSR aggregates become `DsrId`; HTTP
request correlation remains UUID. Domain `idempotency_key` fields remain UUID.
Certificate `serial` remains text because it is CA-owned, not a Sanvi entity
ID.

- [ ] **Step 4: Verify each context**

Run:

```bash
rtk scripts/test.sh -p sanvi-privacy --quiet
rtk scripts/test.sh -p sanvi-localization --quiet
rtk scripts/test.sh -p sanvi-theming --quiet
rtk scripts/test.sh -p sanvi-domains --quiet
```

Expected: PASS.

- [ ] **Step 5: Commit content/platform conversion**

```bash
rtk git add migrations crates/contexts/privacy crates/contexts/localization crates/contexts/theming crates/contexts/domains
rtk git commit -m "refactor(platform): convert remaining core IDs" -m "Move privacy, localization, theming, and domain aggregates to typed Snowflakes while preserving token and provider identifier contracts."
```

---

### Task 8: Refactor payments to generated Snowflakes and persisted idempotency

**Files:**
- Modify: `migrations/20260823000000_tenant_payments.sql`
- Modify: `migrations/20260823000000_tenant_payments.down.sql`
- Modify: `migrations/20260824050000_payments_refunds_disputes.sql`
- Modify: `migrations/20260824050000_payments_refunds_disputes.down.sql`
- Modify: `migrations/20260824060000_payments_payouts_tax_fees.sql`
- Modify: `migrations/20260824060000_payments_payouts_tax_fees.down.sql`
- Modify: every UUID-bearing file under `crates/contexts/payments/**`
- Test: `crates/contexts/payments/tests/payments_integration.rs`

**Interfaces:**
- Produces: typed connection, checkout, payment, refund, dispute, and payout IDs.
- Preserves: caller UUID idempotency keys and Stripe-owned string IDs.

- [ ] **Step 1: Write failing replay and concurrency tests**

Required assertions:

```rust
assert_ne!(fresh.id, command.idempotency_key); // distinct formats and meanings
assert_eq!(replayed.id, fresh.id);
assert_eq!(provider.last_idempotency_key(), command.idempotency_key.to_string());
```

Add concurrent same-key tests for connection, checkout, and refund creation;
both callers must observe one Snowflake row.

- [ ] **Step 2: Rewrite payment schemas**

Convert first-party columns to bigint. Ensure these constraints exist:

```sql
UNIQUE (tenant_id, idempotency_key)
```

on connection, checkout, and refund ownership tables. Keep
`idempotency_key uuid` and provider/webhook IDs in their external types.

- [ ] **Step 3: Remove deterministic UUIDv5 functions**

Delete `connection_id_for`, `checkout_id_for`, `refund_id_for`, their namespace
constants, and deterministic-ID tests. Add a payments `IdGenerator` port:

```rust
pub trait IdGenerator: Send + Sync {
    fn next_connection_id(&self) -> Result<ConnectionId, PaymentsError>;
    fn next_checkout_id(&self) -> Result<CheckoutId, PaymentsError>;
    fn next_refund_id(&self) -> Result<RefundId, PaymentsError>;
    fn next_payment_id(&self) -> Result<PaymentId, PaymentsError>;
    fn next_dispute_id(&self) -> Result<DisputeId, PaymentsError>;
    fn next_payout_id(&self) -> Result<PayoutId, PaymentsError>;
}
```

- [ ] **Step 4: Reserve rows before provider calls**

For each workflow:

```rust
if let Some(existing) = repository.find_by_idempotency_key(&mut scope, tenant_id, key).await? {
    return resume_or_return(existing).await;
}

let id = ids.next_checkout_id()?;
repository.insert_pending(&mut scope, &pending).await.or_else(load_unique_winner)?;
```

Provider commands receive `idempotency_key.to_string()`, not the entity ID.
Connection creation must persist its pending reservation before Stripe account
creation so a crash can resume by unique key.

- [ ] **Step 5: Replace conversion UUID derivation**

Return the stable `PaymentId` or order Snowflake plus normalized event name to
advertising. Do not hash or truncate it into a UUID.

- [ ] **Step 6: Run payments gates**

Run: `rtk cargo test -p sanvi-payments`  
Run: `rtk scripts/test.sh -p sanvi-payments --quiet`  
Expected: PASS, including localhost Stripe mocks and all replay/concurrency tests.

- [ ] **Step 7: Commit payment idempotency migration**

```bash
rtk git add migrations/20260823000000_tenant_payments* migrations/20260824050000_payments_refunds_disputes* migrations/20260824060000_payments_payouts_tax_fees* crates/contexts/payments
rtk git commit -m "refactor(payments): generate snowflake IDs" -m "Persist idempotency ownership separately from entity identity so retries converge on one Snowflake row without UUIDv5 derivation."
```

---

### Task 9: Convert advertising IDs and conversion deduplication

**Files:**
- Modify migrations and paired down files:
  - `migrations/20260826000000_advertising_core.sql`
  - `migrations/20260826010000_advertising_oauth_states.sql`
  - `migrations/20260826040000_advertising_campaign_audit.sql`
  - `migrations/20260826050000_advertising_token_vault.sql`
  - `migrations/20260826080000_advertising_conversion_tracking.sql`
- Modify: every UUID-bearing file under `crates/contexts/advertising/**`
- Test: `crates/contexts/advertising/tests/integration/advertising_integration.rs`
- Test: `crates/contexts/advertising/tests/integration/tracking_integration.rs`

**Interfaces:**
- Produces: typed connection, campaign, ad group, creative, ad, conversion, and change IDs.
- Preserves: OAuth nonce UUID, provider account/campaign IDs, caller idempotency UUIDs.

- [ ] **Step 1: Write failing conversion-dedup tests**

Assert identical `(tenant_id, payment_id, event_name)` captures deduplicate even
when client event IDs differ, and different event names remain distinct.
Expected server event ID is the stable decimal payment/order Snowflake contract,
not UUIDv5.

- [ ] **Step 2: Rewrite advertising migrations**

Convert first-party IDs and partition keys to bigint. Keep
`advertising.oauth_states.nonce uuid`. Update dynamic RLS SQL to use bigint.
Update seed actor literals to their reserved bigint constants.

- [ ] **Step 3: Add advertising ID ports and typed repositories**

Use one context `IdGenerator` port with a method per aggregate. Repository row
structs use `i64`, domain models use typed IDs, and HTTP DTOs use typed IDs or
decimal strings with explicit parsing.

- [ ] **Step 4: Replace UUIDv5 capture logic**

Delete `CONVERSION_DEDUPE_NAMESPACE` and `Uuid::new_v5`. Define the database
dedup key as `(tenant_id, source_id, event_name)` where `source_id` is the
stable first-party Snowflake.

- [ ] **Step 5: Verify advertising**

Run: `rtk scripts/test.sh -p sanvi-advertising --quiet`  
Expected: PASS for campaigns, OAuth exception handling, partitions, and conversion dedup.

- [ ] **Step 6: Commit advertising conversion**

```bash
rtk git add migrations/202608260* crates/contexts/advertising
rtk git commit -m "refactor(advertising): use snowflake IDs" -m "Convert first-party advertising aggregates and deduplication while retaining OAuth and provider-owned identifiers."
```

---

### Task 10: Wire node configuration into API, worker, and CLI

**Files:**
- Modify: `apps/api/src/config.rs`
- Modify: `apps/api/src/main.rs`
- Modify: `apps/api/src/lib.rs`
- Modify: `apps/worker/src/main.rs`
- Modify: `apps/cli/src/config.rs`
- Modify: `apps/cli/src/main.rs`
- Modify: context wiring files under `apps/cli/src/*.rs`
- Modify: `configs/base.toml`
- Modify: `configs/local.toml`
- Modify: `configs/staging.toml`
- Modify: `configs/production.toml`
- Modify: `.env.example`
- Modify: `docker-compose.yml`
- Modify: `docker-compose.override.yml`
- Modify: `docker-compose.staging.yml`
- Test: config tests in API, worker, and CLI modules

**Interfaces:**
- Produces: one shared `Arc<dyn RawIdGenerator>` per process.
- Consumes: explicit staging/production `ids.node_id`; local defaults 1/2/3.

- [ ] **Step 1: Write failing configuration tests**

Test invalid nodes `0` and `1024`, missing production nodes, and local process
defaults. Expected resolved nodes are API `1`, worker `2`, CLI `3`.

- [ ] **Step 2: Add shared resolution helper**

Use:

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IdsConfig {
    pub node_id: Option<u16>,
}

impl IdsConfig {
    pub fn resolve(&self, environment: &str, local_default: u16) -> anyhow::Result<NodeId>;
}
```

Production and staging reject `None`; local uses the binary-specific default.
The epoch is not configurable.

- [ ] **Step 3: Construct and inject one generator per process**

Immediately after config validation:

```rust
let node_id = config.ids.resolve(&config.telemetry.environment, 1)?;
let ids: Arc<dyn RawIdGenerator> = Arc::new(SnowflakeGenerator::new(node_id));
```

Use local default `2` in worker and `3` in CLI. Clone this same `Arc` into all
context ID adapters, event publishers, and `TxManager` instances.

- [ ] **Step 4: Set explicit compose node IDs**

Set API `SANVI__IDS__NODE_ID=1` and worker `SANVI__IDS__NODE_ID=2` in local
compose. Staging examples use distinct non-zero values and document that each
replica needs a unique override.

- [ ] **Step 5: Verify startup configuration**

Run: `rtk cargo test -p sanvi-api config`  
Run: `rtk cargo test -p sanvi-worker config`  
Run: `rtk cargo test -p sanvi-cli config`  
Run: `rtk cargo c`  
Expected: PASS with no missing generator wiring.

- [ ] **Step 6: Commit process wiring**

```bash
rtk git add apps configs .env.example docker-compose*.yml
rtk git commit -m "feat(config): assign snowflake nodes" -m "Construct one explicit generator per process, reserve local nodes by binary, and require unique configured nodes outside local development."
```

---

### Task 11: Convert HTTP, OpenAPI, CLI, cache, and cross-context contracts

**Files:**
- Modify: `crates/platform/http/src/openapi.rs`
- Modify: remaining UUID-bearing files under `crates/platform/http/**`
- Modify: remaining UUID-bearing files under `apps/api/**`, `apps/worker/**`, and `apps/cli/**`
- Modify: remaining UUID-bearing cache, Keto, privacy-provider, and cross-context adapter files returned by `rtk rg -l '\bUuid\b|uuid::' crates apps`
- Modify: generated `api/*.yaml` through the CLI generator
- Test: `apps/api/tests/integration/http_contract.rs`
- Test: `apps/api/src/openapi.rs`

**Interfaces:**
- Produces: decimal-string Snowflake HTTP contracts and reusable schema annotations.
- Preserves: request IDs as UUIDv7 and approved external UUID fields.

- [ ] **Step 1: Add failing HTTP contract tests**

Assert a representative tenant, user, payment, and campaign response contains
quoted decimal IDs. Assert malformed IDs and JSON numbers receive `400`.
Keep the existing `x-request-id` UUIDv7 assertion unchanged.

- [ ] **Step 2: Add a reusable HTTP schema helper**

In `sanvi-http`, define a schema function returning:

```rust
ObjectBuilder::new()
    .schema_type(SchemaType::Type(Type::String))
    .format(Some(SchemaFormat::Custom("snowflake-id".to_string())))
    .pattern(Some("^[1-9][0-9]{0,18}$"))
    .examples([serde_json::json!("873698342314721281")])
```

Transport DTO fields and path annotations reference this helper. Do not add
Utoipa to the kernel.

- [ ] **Step 3: Convert cross-cutting strings and cursors**

Redis keys, Keto tuple subjects/objects, Stripe metadata, audit filters, and
cursor payloads call typed ID `Display`. Cursor decoding parses the decimal
string into the correct typed ID. Do not parse through JavaScript-sized or
`u32` intermediates.

- [ ] **Step 4: Regenerate OpenAPI**

Run: `rtk cargo run -p sanvi-cli -- openapi --write`  
Expected: changed `api/*.yaml`. First-party IDs are `type: string` with custom
Snowflake format/pattern; approved UUID exceptions remain UUID.

- [ ] **Step 5: Verify API and remaining UUID inventory**

Run: `rtk cargo test -p sanvi-api --test integration`  
Run: `rtk scripts/check-openapi.sh`  
Run:

```bash
rtk rg -n '\bUuid\b|uuid::' crates apps
```

Expected: every remaining match maps to the approved exception vocabulary or
UUID implementation tests. No `from_uuid`, `as_uuid`, or `Uuid::new_v5`
first-party path remains.

- [ ] **Step 6: Commit boundary contracts**

```bash
rtk git add crates/platform apps api crates/contexts
rtk git commit -m "refactor(api): expose snowflakes as strings" -m "Keep 64-bit first-party IDs precise across HTTP, messaging, caches, Keto, cursors, and provider metadata while preserving UUID request correlation."
```

---

### Task 12: Add the ID-policy gate, regenerate SQLx, update docs, and prove completion

**Files:**
- Create: `scripts/check-id-policy.sh`
- Create: `scripts/id-policy-uuid-exceptions.txt`
- Modify: `justfile`
- Modify: `CONTRIBUTING.md`
- Modify: `README.md`
- Modify: UUID-specific runbooks under `docs/runbooks/**`
- Modify: phase documents under `docs/phase-*` only where they state the active ID contract
- Modify: `.sqlx/**` through `cargo sqlx prepare --workspace`
- Modify: `api/*.yaml` through OpenAPI generation if Task 11 left changes
- Test: self-test mode in `scripts/check-id-policy.sh`

**Interfaces:**
- Produces: permanent CI proof that first-party UUIDs and numeric JSON Snowflakes cannot return.
- Consumes: field-specific exception file with rationale.

- [ ] **Step 1: Write the failing policy script self-test first**

The script supports:

```bash
./scripts/check-id-policy.sh
./scripts/check-id-policy.sh --self-test
```

Self-test fixtures must prove the gate rejects:

```sql
CREATE TABLE bad (id uuid PRIMARY KEY);
```

and Rust calls to `Id::from_uuid`, `Id::as_uuid`, or `Uuid::new_v5` in a
first-party fixture. It must accept `kratos_identity_id uuid` and
`idempotency_key uuid` only because exact exception entries name them.

- [ ] **Step 2: Add the gate to repository CI recipes**

Add `id-policy` to both `ci` and `ci-quiet` after layering and before tests:

```make
id-policy:
    ./scripts/check-id-policy.sh

id-policy-self-test:
    ./scripts/check-id-policy.sh --self-test
```

- [ ] **Step 3: Reset the database and regenerate SQLx metadata**

Run:

```bash
rtk scripts/test-db.sh down
rtk scripts/test-db.sh up
rtk cargo sqlx prepare --workspace
```

Expected: `.sqlx` reflects `INT8` query parameters/results and no stale
first-party UUID query metadata remains.

- [ ] **Step 4: Update operational documentation**

Document:

- decimal-string CLI/API ID examples;
- node assignment and collision prevention;
- node `0` seed reservation;
- NTP/clock-regression failure behavior;
- reset-only rollback;
- approved UUID exceptions; and
- dependency note: `id-forge 1.0.0`, zero runtime dependencies,
  `Apache-2.0 OR MIT`.

- [ ] **Step 5: Run focused invariant and artifact checks**

Run:

```bash
rtk ./scripts/check-id-policy.sh --self-test
rtk ./scripts/check-id-policy.sh
rtk git diff --check
rtk cargo run -p sanvi-cli -- openapi --write
rtk scripts/check-openapi.sh
rtk cargo c
```

Expected: PASS and OpenAPI generation produces no further diff on the second run.

- [ ] **Step 6: Run the full repository gate**

Run: `rtk just ci-quiet`  
Expected: PASS for formatting, clippy, layering, connect invariants,
advertising invariants, all tests, OpenAPI, i18n, and the new ID policy.

- [ ] **Step 7: Audit the final diff and inventories**

Run:

```bash
rtk rg -n 'from_uuid|as_uuid|Uuid::new_v5' crates apps
rtk rg -n --glob '*.sql' '\buuid\b' migrations
rtk rg -n 'format: uuid' api
rtk rg -n --glob '*.sql' 'GENERATED .*IDENTITY|\bbigserial\b' migrations
rtk git diff --check
rtk git status --short
```

Expected:

- no removed UUID conversion or deterministic UUIDv5 entity path;
- migration UUID matches only approved exceptions;
- OpenAPI UUID matches only approved exceptions;
- only audit ordering `seq bigserial` remains identity-generated;
- no whitespace errors or untracked generated artifacts.

- [ ] **Step 8: Commit policy, artifacts, and documentation**

```bash
rtk git add scripts justfile CONTRIBUTING.md README.md docs .sqlx api
rtk git commit -m "ci(ids): enforce snowflake policy" -m "Lock the reset migration with schema, Rust, OpenAPI, and serialization checks; refresh generated artifacts and document node operations and rollback."
```

- [ ] **Step 9: Record final verification evidence**

Run: `rtk git status --short --branch`  
Expected: clean `feat/snowflake-ids`.  
Run: `rtk git log --oneline main..HEAD`  
Expected: design, plan, foundation, context migrations, boundary conversion,
and policy commits are present in dependency order.
