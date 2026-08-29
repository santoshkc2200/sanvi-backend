# Snowflake Identifier Migration Design

**Date:** 2026-08-27  
**Status:** Approved  
**Repository:** `sanvi-backend`  
**Branch:** `feat/snowflake-ids`

## 1. Goal

Replace every Sanvi-owned backend entity and event identifier with a strongly
typed, application-generated Snowflake ID. Store these IDs as PostgreSQL
`BIGINT` values and expose them as decimal strings at API and messaging
boundaries.

The database may be reset. Existing UUID data does not need a compatibility
window or backfill.

## 2. Scope

Snowflake IDs apply to identifiers owned by Sanvi, including:

- tenant, user, membership, invitation, role, approval, and impersonation IDs;
- plan, price, subscription, invoice, payment, refund, payout, and checkout IDs;
- privacy, theming, domain, advertising, audit, and other aggregate IDs;
- transactional outbox event IDs and aggregate IDs;
- internal platform audit-row IDs; and
- first-party foreign keys, actor references, cursors, cache keys, and event
  references derived from those identifiers.

UUID or opaque string values remain when unpredictability or an external
contract owns the format:

- HTTP request and trace correlation IDs;
- OAuth state nonces and verification/download tokens;
- caller-supplied idempotency keys;
- Ory Kratos identity IDs and other Ory-owned identifiers;
- Stripe, advertising-provider, registrar, certificate, and other provider IDs;
- webhook event IDs owned by providers; and
- cryptographic material, hashes, secrets, and tokens.

The frontend repository is outside implementation scope. Its existing
`string` ID types remain compatible with decimal-string Snowflakes. The media
service is also outside scope because it already uses `int64` Snowflake IDs and
serializes them as strings.

## 3. Non-goals

- Preserving or translating existing backend UUID data.
- Dual-read, dual-write, or mixed-version database operation.
- Making Snowflake IDs cryptographically unpredictable.
- Replacing third-party identifiers or caller-controlled idempotency keys.
- Introducing a central ID service or database-side ID generation.
- Changing sequence fields whose purpose is ordering rather than identity,
  such as the audit hash-chain `seq` value.

## 4. Chosen architecture

### 4.1 Layout and epoch

Use the classic positive 63-bit Snowflake layout:

| Field | Bits | Capacity |
| --- | ---: | ---: |
| Timestamp since epoch, milliseconds | 41 | about 69 years |
| Node | 10 | 1,024 nodes |
| Per-millisecond sequence | 12 | 4,096 IDs per node per millisecond |

The epoch is fixed at `2026-01-01T00:00:00Z`. It is a wire-format constant,
not runtime configuration. IDs remain representable through 2095.

Node `0` is reserved for stable seed IDs stored in migrations. Runtime
generators accept nodes `1..=1023` only. Seed IDs occupy the epoch-zero,
node-zero range and cannot collide with runtime IDs.

### 4.2 Generator implementation

Add `id-forge` version `1.0.0` with default features disabled and only the
`snowflake` feature enabled. The crate has no runtime dependencies, supports
the required 41/10/12 layout and epoch, exposes fallible clock-skew handling,
requires Rust 1.75 or newer, and is licensed `Apache-2.0 OR MIT`.

The shared kernel owns:

- `SnowflakeId`, a validated positive `i64` value;
- `Id<Tag>`, the existing strongly typed identifier wrapper backed by
  `SnowflakeId`;
- `RawIdGenerator`, an object-safe port that returns a fallible raw Snowflake;
- `SnowflakeGenerator`, the production adapter around `id_forge`; and
- `IdGenerationError`, covering invalid configuration, clock regression,
  exhausted timestamp range, and invalid generated values.

One generator instance is created per process and shared through `Arc`. API,
worker, and CLI composition roots inject it into context-specific ID adapters
and the persistence transaction manager.

`Id<Tag>` exposes only format-neutral operations:

- validation from a positive `i64`;
- parsing from a decimal string;
- `as_i64()` at persistence boundaries;
- `Display`, ordering, hashing, and copy semantics; and
- string-based Serde behavior. HTTP transport adapters apply the shared
  Snowflake OpenAPI schema without adding `utoipa` to the kernel.

Remove `Id::new()`, `Id::from_uuid()`, and `Id::as_uuid()`. ID creation must
flow through an injected generator, and UUID conversion must not remain as a
compatibility shortcut.

### 4.3 Context ports

Keep each bounded context's existing typed `IdGenerator` port. Change its
methods to return `Result<TypedId, ContextError>` so generator failure remains
observable. Production adapters delegate to the shared `RawIdGenerator`;
tests continue to inject deterministic sequence generators.

Contexts that currently create raw UUID identifiers without a port gain the
same local port pattern. Domain constructors receive generated typed IDs and
do not depend directly on `id_forge`, configuration, SQLx, or process-global
state.

## 5. Configuration and process lifecycle

Add an `ids` configuration section containing `node_id`.

- Local API defaults to node `1`.
- Local worker defaults to node `2`.
- Local CLI defaults to node `3`.
- Staging and production require an explicit `ids.node_id` in `1..=1023`.
- Each concurrently running replica must receive a distinct node ID.
- Docker Compose and deployment examples assign distinct nodes explicitly.

The epoch and bit allocation are compile-time constants. Allowing them to
vary by environment would make one database contain incompatible ID formats.

Configuration is validated before database or network initialization. An
invalid or missing required node exits with configuration status `2`.

The generator uses fallible generation. A backward wall-clock movement does
not produce an ID:

- API maps the failure to a retryable RFC 9457 `503` problem;
- worker jobs leave work retryable and record the failure;
- CLI returns a non-zero error; and
- telemetry records the process node and clock-drift magnitude without
  logging secrets or fabricating an ID.

Sequence exhaustion waits for the next millisecond as defined by the chosen
generator. Generator failures are never converted to zero, random fallbacks,
or database sequences.

## 6. Persistence design

### 6.1 Schema rewrite

Because reset is approved, rewrite the existing forward and reverse
migrations in place:

- Sanvi-owned `uuid` primary keys become `bigint` primary keys.
- Matching foreign keys, nullable references, arrays, indexes, partitions,
  and unique constraints become `bigint`-compatible.
- `platform.outbox.id`, `aggregate_id`, and
  `platform.processed_events.event_id` become `bigint`.
- `platform.audit_log.id` becomes a caller-supplied Snowflake `bigint`.
- Audit chain `seq bigserial` remains unchanged because it provides strict
  insertion order for hash verification, not identity.
- RLS policies cast `sanvi.tenant_id` with `::bigint` instead of `::uuid`.
- Seed UUID literals become reserved node-zero Snowflake constants.
- Every modified forward migration has a consistent reverse migration.

Opaque/external UUID columns remain `uuid` only when their external contract
is UUID-based. Other provider identifiers remain `text`.

### 6.2 Adapter boundary

SQLx row structs and query arguments use `i64`. Repository adapters convert
between `i64` and typed `Id<Tag>` at their boundary and reject invalid
non-positive database values. Domain and application interfaces do not expose
raw `i64`.

Regenerate committed SQLx offline metadata after all query changes.

### 6.3 Transactions and audit

Generate entity and event IDs before their transaction begins. A failure to
generate an ID prevents any write. Entity state, idempotency ownership,
outbox events, and platform audit records continue to commit atomically.

`TxManager` receives the shared raw generator so `begin_unscoped` can insert a
Snowflake ID into `platform.audit_log` without a database sequence.

## 7. Idempotent workflow changes

UUIDv5-derived IDs are removed. A deterministic UUID is not a Snowflake and
must not remain as an entity identifier.

Payment connection, checkout, and refund workflows use this pattern:

1. Validate the caller's UUID `Idempotency-Key`.
2. Look up an existing row by its tenant-scoped idempotency constraint.
3. Return or resume that row when found.
4. Otherwise generate a Snowflake entity ID.
5. Reserve/persist the pending row with a unique
   `(tenant_id, idempotency_key)` constraint before the provider call.
6. Use the retained UUID idempotency key for provider-side retries.
7. Finalize the same Snowflake row after the provider responds.

Concurrent inserts resolve through the unique constraint and load the winning
row. Crash recovery therefore depends on persisted idempotency ownership, not
on reproducible entity IDs.

Advertising conversion dedup uses the stable first-party payment or order
Snowflake together with the event name. It does not derive or accept a UUIDv5
conversion entity ID.

## 8. Wire contracts

Snowflakes are integers internally but decimal strings externally.

### 8.1 JSON and messaging

`Id<Tag>` serializes as a quoted decimal string and accepts only a quoted,
canonical positive decimal string. It does not accept JSON numbers, signs,
leading zeroes, zero, negative values, whitespace, or values above `i64::MAX`.

Redis keys, Keto relation subjects/objects, cursor payloads, event payloads,
and log fields use the same decimal representation.

### 8.2 OpenAPI

First-party IDs use:

```yaml
type: string
format: snowflake-id
pattern: '^[1-9][0-9]{0,18}$'
example: '873698342314721281'
```

UUID exceptions continue to use `type: string, format: uuid`. Generated API
documents must contain no first-party identifier marked as UUID. Keeping the
wire type as `string` prevents JavaScript precision loss and preserves the
frontend's existing TypeScript types.

The reusable schema helper lives in `sanvi-http`. Context transport adapters
reference it through field and parameter annotations; `sanvi-kernel` remains
independent of `utoipa` and every HTTP framework.

## 9. Cross-cutting consumers

Update every reader and writer of first-party IDs:

- HTTP paths, queries, bodies, response DTOs, and middleware subjects;
- CLI arguments and printed records;
- worker commands, schedulers, outbox dispatch, and event consumers;
- Redis cache keys and invalidation patterns;
- Keto relationship tuple strings;
- pagination cursors and audit filtering;
- structured logs, metrics labels where IDs are already allowed, and tracing;
- Stripe metadata/client references that contain Sanvi IDs; and
- documentation and runbooks that currently instruct users to provide a UUID.

No Snowflake is treated as a secret. Authorization remains tenant- and
permission-based; possession or guessability of an ID grants nothing.

## 10. Policy enforcement

Add an ID-policy quality gate and include it in the repository's existing
invariant/CI gate path. The gate fails when:

- a migration introduces a first-party UUID column outside an explicit,
  reviewed exception list;
- `Id::new`, `Id::from_uuid`, or `Id::as_uuid` reappears;
- first-party OpenAPI IDs regain `format: uuid`; or
- a first-party ID DTO serializes as a JSON number.

The exception list names fields and rationale. It is narrow enough to review
changes rather than allowing broad file-level suppression.

## 11. Verification

### 11.1 Kernel tests

- Generate at least 10,000 unique IDs on one node.
- Generate concurrently through a shared generator and prove uniqueness.
- Prove later IDs sort after earlier IDs for one node.
- Decompose IDs and verify epoch, node, and sequence fields.
- Reject node `0`, node `1024`, zero, negative values, overflow, malformed
  decimal strings, leading zeroes, and JSON numbers.
- Round-trip string Serde and typed-ID parsing.
- Prove distinct `Id<Tag>` types cannot be interchanged through compile-time
  usage tests where practical.

### 11.2 Context tests

- Deterministic fake generators still drive application tests.
- Generator failures produce context errors without writes.
- Payment connection, checkout, and refund retries return/resume one
  Snowflake row.
- Concurrent same-key attempts converge on one row.
- Provider-side retries continue to use the caller's UUID idempotency key.
- Conversion capture deduplicates by stable first-party IDs.

### 11.3 Migration and integration tests

- Apply every migration to a clean PostgreSQL database.
- Exercise forward and reverse migration pairs where the repository supports
  reverse validation.
- Verify representative primary keys and foreign keys are `bigint`.
- Verify approved external UUID columns remain `uuid`.
- Verify RLS accepts the decimal tenant GUC and isolates tenants.
- Verify outbox publication, consumer deduplication, platform audit, seeds,
  caches, and cursor pagination with Snowflakes.
- Regenerate SQLx metadata and OpenAPI contracts.

### 11.4 Required gates

Run the repository-defined gates in this order while iterating:

1. `cargo c`
2. scoped crate tests
3. `cargo sqlx prepare --workspace`
4. `cargo run -p sanvi-cli -- openapi --write`
5. `just ci-quiet`

The final gate may need local socket and Docker access for existing test
infrastructure.

## 12. Migration sequence

Implementation follows an expand-by-layer sequence even though database data
is reset:

1. Add the kernel value type, generator port/adapter, configuration, and tests.
2. Convert event envelopes and persistence transaction infrastructure.
3. Convert schemas, seed constants, RLS helpers, and SQLx row boundaries.
4. Convert bounded contexts one at a time, keeping each crate compiling and
   tested before moving on.
5. Refactor UUIDv5 idempotent workflows.
6. Convert composition roots, CLI, worker, caches, messaging, and HTTP/OpenAPI.
7. Add the ID-policy gate and update docs/runbooks.
8. Regenerate SQLx/OpenAPI artifacts and run all quality gates.

No later destructive contraction exists: the reset database starts directly
on the Snowflake schema.

## 13. Rollback

Before deployment, rollback is branch reversal: revert the Snowflake commits
and recreate the database from the UUID migrations on `main`.

After a reset database has accepted Snowflake data, rollback requires another
database reset because there is intentionally no UUID backfill or dual-schema
compatibility. Deployment notes must state this explicitly. No migration may
silently cast Snowflakes into UUIDs or vice versa.

## 14. Acceptance criteria

- All Sanvi-owned backend IDs are typed Snowflakes backed by positive `i64`.
- PostgreSQL stores those IDs and references as `BIGINT`.
- External/API serialization uses canonical decimal strings.
- Approved UUID exceptions remain functional and documented.
- No UUIDv5-derived entity ID remains.
- API, worker, CLI, outbox, audit, RLS, caches, Keto, and provider metadata use
  the new representation consistently.
- Clean database migration, SQLx metadata, OpenAPI generation, and all
  repository quality gates pass.
- Current uncommitted work in the original backend checkout remains untouched.
