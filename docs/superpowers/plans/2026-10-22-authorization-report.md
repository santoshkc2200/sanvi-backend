# Authorization remediation report

Implemented task 1 of `2026-10-22-architecture-remediation.md`. No commits or deployments.

## Changes

- `CheckPermission` requires `TenantRoleSource`. `SqlxTenantRoleSource` reads the current tenant membership through the identity repository and returns roles only for an active membership. Permission candidates must also match those current role IDs. The current role catalog determines which roles still grant the requested permission. Platform role candidates remain Keto-owned and continue to apply across tenants.
- Tenant reads cannot bypass authoritative storage through a cached allow. Tenant outage-cache entries include a canonical fingerprint of the current eligible permission-granting role objects. `DegradedAuthorizer` reloads that fingerprint before bounded read grace. Revocation, suspension, removed roles, and removed role permissions cannot reuse old grants during a Keto outage. Unchanged tenant grants and elevated platform operators retain the existing bounded read grace; writes still fail closed. Authorizers without explicit revision support cannot opt tenant reads into cached outage grace.
- Identity session verification also omits suspended-membership roles. It continues authenticating the global user, preserving self-service/platform workflows. Effective impersonation identity remains supplied by existing middleware; the authorization check uses that effective user's current membership rather than stale transported role IDs.
- Replaced Redis version-before-effect handling and event-delta application with `SqlxAccessProjection`. Events identify an aggregate and wake reconciliation. PostgreSQL advisory transaction locks serialize each membership or role across independent workers. The reconciler reads current authoritative rows after taking the lock, removes obsolete tuples, writes desired tuples, and commits the completed snapshot checkpoint only after successful external effects. Partial effects repeat safely after failure/crash. Old, duplicate, reversed, revoke/regrant, and deletion events converge to current rows rather than replaying old instructions.
- Added reversible migration `20261022000300_access_projection_checkpoints`. Membership checkpoints use tenant FORCE RLS and a composite tenant/user key; role checkpoints contain global projection metadata. First-run cleanup considers the role/permission catalogs to repair pre-checkpoint projections. Membership reconciliation preserves unrelated platform assignments.
- Access privacy collection includes the new user-referencing membership checkpoints. Erasure deletes their known tenant/role tuples and checkpoint references, and does not commit checkpoint erasure when the external deletion fails.
- Identity webhook client IP extraction now uses the shared trusted-proxy helper wired by the root implementation.
- Added seven `.sqlx` metadata files for the new projection/privacy query macros. Used SQLx 0.9's `SQLX_OFFLINE_DIR` live-compilation path because `cargo-sqlx` is not installed. A subsequent ordinary offline check passed.

Composition changes in API/worker and the narrowly scoped architecture-gate adapter exception were coordinated with and implemented by the root agent. No HTTP/OpenAPI contract changed in this task.

## Red evidence

Observed expected assertion failures before implementing the corresponding fixes:

1. Same-event revocation retry left the original Keto memberships present because the event version was already acknowledged.
2. Tenant cached allow returned `Ok(Allowed)` despite unavailable authoritative membership storage.
3. Reversed role deltas restored a permission removed from the latest role state.
4. Keto outage plus membership removal reused read grace: `Allowed` instead of `Denied`.
5. New membership checkpoint references were absent from the subject's privacy export.

The original version/delta-specific unit fixtures were replaced with regressions against real PostgreSQL snapshots/checkpoints; the closed-pool cache regression lives in `tests/fresh_authorization.rs`, keeping application modules independent of adapters.

## Final verification

Completed successfully with the shared Postgres/Redis harness:

`eval "$(./scripts/test-db.sh up)" && cargo test -p sanvi-access -- --nocapture`

- 51 library tests passed.
- 1 standalone fresh-authorization regression passed.
- 15 integration tests passed, including 7 new authorization consistency regressions.
- 0 doc-test failures.

New integration coverage includes stopped projection with stale Keto/cache, suspended/revoked memberships, removed roles/permissions, platform operators, same-event transient revocation retry, reversed role updates and late role creation after deletion, late membership grant after revocation, role partial-write retry without a premature checkpoint, actual cross-worker advisory-lock waiting (`pg_locks`), latest snapshot reload after waiting, outage-grace proof revalidation, and checkpoint privacy export/erasure. Only the external Keto service is controlled in-memory; authoritative rows, repositories, transactions, RLS application role, and checkpoint storage are real PostgreSQL.

`cargo check -p sanvi-access --all-targets --message-format short` passed after metadata generation, and scoped `git diff --check` passed. Windows incremental-cache hard-link warnings were present; no test failures remained. Root owns final workspace formatting, clippy, full suites, and assembled HTTP/impersonation gates.

## Operational notes

- Reconciliation deliberately holds one database transaction/aggregate lock while idempotent external calls run. Existing Keto request deadlines bound individual calls. Permission-registry cleanup may issue multiple calls during rare role-authoring reconciliation; worker lag should remain monitored.
- Authoritative membership/catalog reads now occur for tenant authorization and cached outage proof. PostgreSQL unavailability fails closed rather than reusing an unproven grant.
- Disposable SQLx preparation database: `sanvi_access_sqlx_20261022` on the existing shared test PostgreSQL at port 59246, cloned from the verified migrated template. It is retained temporarily for root verification; explicitly drop this known database after final verification. Do not remove the shared harness or template.
