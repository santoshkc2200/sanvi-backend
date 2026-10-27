# Connector Import Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the tenant admin reach `ecommerce-connectors` (Shopify/Amazon catalog imports) through `sanvi-backend`, which authorizes each request and mints the connector's identity token.

**Architecture:** Extend the `product_access` context, which already proxies to ecommerce-service. Add a second product, `CONNECTORS`. It reuses `HttpProductUpstream`, the Ed25519 signer, the entitlement resolver and the forwarding allow-lists. The connector authorizes nothing by permission, so unlike the ecommerce proxy the platform declares a permission on every route through `PermissionMap`, and the routes are ordinary permissioned routes rather than matrix exemptions. The connector token has its own claim shape (connector spec §6.2), so it gets its own claims type next to `DelegationClaims`.

**Tech Stack:** Rust, actix-web, utoipa, reqwest, jsonwebtoken (EdDSA), the existing `sanvi_http` permission middleware.

**Spec:** `ecommerce-connectors/docs/superpowers/specs/2026-10-23-ecommerce-connectors-design.md` (§3 callers, §6.2 identity token, the OAuth section, §9 API); the pattern being extended: `docs/superpowers/specs/2026-10-27-ecommerce-proxy-design.md`. The connector contract is `ecommerce-connectors/api/openapi.yaml`.

**Branch:** `feat/connector-proxy` from `main` (f5d9107), worktree `../worktrees/backend-connector-proxy`.

## Global Constraints

- Route family: `GET|POST|DELETE /api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}` forwards to `{product_access.connectors.base_url}/v1/{tail}`, mounted in the shared tenant scope (same middleware chain as `/ecommerce/{tail}`).
- Upstream headers: `Authorization: Bearer <service key>` and `X-Identity-Token: <jwt>`. The existing `HttpProductUpstream` sends both already.
- Token: compact JWT, `alg: EdDSA`, header `kid`. Claims, exactly as the connector deserializes them (`crates/service/src/adapters/http/auth.rs:63`):
  `tenant_id: i64`, `user_id: i64`, `destination_store_id: String`, `aud: "ecommerce-connectors"`, `iat: i64`, `exp: i64`, `jti: String`.
  Ids are JSON **numbers** here, not the decimal strings of identity token v2.
- Lifetime 60 s (`TOKEN_LIFETIME_SECS`). The connector rejects lifetimes above 120 s and replayed `jti`s. `jti` is a fresh `Uuid::now_v7().simple()` per request.
- One platform signing key (`[product_access] signing_key_id/seed`) for all products. No second key.
- Feature: `products.ecommerce.enabled`, checked in the use case so the proxy fails closed (the same reason the ecommerce-proxy spec gives).
- New permissions, tenant scope: `ecommerce.import.read` and `ecommerce.import.manage`. `tenant_owner` and `tenant_admin` get both. `tenant_member` gets neither.
- Permission per route: GET → `ecommerce.import.read`, POST/DELETE → `ecommerce.import.manage`, and the callback GET (`connections/{provider}/callback`) → `ecommerce.import.manage`, because it creates a connection.
- `store_id`: canonical positive decimal `i64` (`1..=9223372036854775807`, no sign, no leading zero), passed to the connector as the string `destination_store_id`. The platform does not check that the store exists. The tenant comes from the session, so a wrong store can only name another store of the *same* tenant, and ecommerce-service's consumer validates tenant and store on ingestion.
- Forwarding limits are unchanged from the ecommerce proxy: 256 KiB request body, 4 MiB response, 10 s timeout, header allow-lists, upstream 401 → 502, unconfigured or unreachable → 503, no retries.
- Config: `[product_access.connectors] base_url, service_key`. Env `SANVI__PRODUCT_ACCESS__CONNECTORS__BASE_URL` and `SANVI__PRODUCT_ACCESS__CONNECTORS__SERVICE_KEY` (the latter equals the connector's `CONNECTOR_SERVICE_KEY_CURRENT`). Empty is valid and answers 503.
- Gates: `cargo c`, `cargo k`, `cargo fmt --check`, `scripts/test.sh -p <crate>` while iterating, `just ci-quiet` at the end. Never use bare `cargo test` on crates with `tests/`.
- Contracts: `cargo run -p sanvi-cli -- openapi --write` and `just route-matrix-write`, with the outputs committed.

## Review Focus

1. **The callback reached by a read-only user.** If the generic `{tail:.+}` resource is registered before the callback resource, a GET with only `ecommerce.import.read` would complete an OAuth connection. Expect 403. Task 4 pins this.
2. **Path tricks around the store slot.** `stores/0/...`, `stores/007/...`, `stores/-1/...`, `stores/%31/...` and `stores/1/../2/jobs` must be 400 and must never reach the connector. Tasks 1 and 4 pin this.
3. **Shopify callback query fidelity.** The connector verifies Shopify's `hmac` over the full query, so the query string must pass through byte-for-byte, including its order and encoding. Task 4 pins it with an `hmac`/`shop`/`timestamp` query.
4. **Signing-key drift between repos.** The connector's dev public key (`79b5…`) is not the platform dev seed's (`ea4a…`). Local stacks get 401, which the proxy turns into 502. Task 3 adds a pinning test and Task 6 aligns the connector.
5. **A tenant without the product.** With `products.ecommerce.enabled` off, every route must answer 403 `feature not enabled: products.ecommerce.enabled` before anything is signed or sent. Task 2 pins this.

---

### Task 1: Connector product, store id and claims (domain)

**Files:**
- Modify: `crates/contexts/product_access/src/domain/product.rs`
- Modify: `crates/contexts/product_access/src/domain/delegation.rs`
- Modify: `crates/contexts/product_access/src/domain/ports.rs`
- Modify: `crates/contexts/product_access/src/domain/mod.rs` (re-exports)
- Modify: `crates/contexts/product_access/src/adapters/signer.rs` (`Ed25519Signer`, `DisabledSigner`, plus the test `JsonSigner` in `application/mod.rs`)

**Interfaces:**
- Produces:
  - `pub const CONNECTORS: Product = Product { key: "ecommerce-imports", audience: "ecommerce-connectors", feature: "products.ecommerce.enabled", permission_prefix: "ecommerce.import.", limits: &[] }`
  - `pub struct DestinationStoreId(i64)`, with `DestinationStoreId::parse(&str) -> Result<Self, ProductAccessError>` (`BadRequest` on failure) and `as_i64()`
  - `pub struct ConnectorClaims { tenant_id: i64, user_id: i64, destination_store_id: String, aud: &'static str, iat: i64, exp: i64, jti: String }` (`Serialize`), with `ConnectorClaims::grant(product: &Product, tenant_id: TenantId, user_id: UserId, store: DestinationStoreId, permissions: &[String], entitlements: &[EntitlementView], jti: String, now: DateTime<Utc>) -> Result<Self, ProductAccessError>`
  - `fn authorize(product: &Product, permissions: &[String], entitlements: &[EntitlementView]) -> Result<Vec<String>, ProductAccessError>`, extracted from `DelegationClaims::grant` (entitlement check, then the prefix filter, sort and dedup). `DelegationClaims::grant` and `ConnectorClaims::grant` both call it.
  - `TokenSigner::sign_connector(&self, claims: &ConnectorClaims) -> Result<String, ProductAccessError>`

- [ ] **Step 1: Write the failing tests** in `delegation.rs` `mod tests`:
  - `connector_claims_match_the_connector_contract`: grant with `["ecommerce.import.read", "ecommerce.store.read"]` and the feature enabled, then serialize to JSON. Assert the keys are exactly `{tenant_id, user_id, destination_store_id, aud, iat, exp, jti}`, `tenant_id == 1001` (number), `user_id == 3001` (number), `destination_store_id == "42"` (string), `aud == "ecommerce-connectors"`, and `exp - iat == 60`.
  - `connector_claims_refuse_without_the_product_or_an_import_permission`: feature off gives `NotEntitled { feature: "products.ecommerce.enabled" }`. Only `ecommerce.store.read` gives `NoPermissions { product: "ecommerce-imports" }`.
  - `store_ids_are_canonical_positive_decimals`: ok for `"1"`, `"42"` and `"9223372036854775807"`. Err for `""`, `"0"`, `"007"`, `"-1"`, `"+1"`, `"1.0"`, `"9223372036854775808"`, `"%31"` and `"a"`.
  - The existing `DelegationClaims` tests must stay green unchanged. They prove the `authorize` extraction preserved behavior.

- [ ] **Step 2: Run, expect FAIL** (unresolved `ConnectorClaims` / `DestinationStoreId`): `just test-fast`

- [ ] **Step 3: Implement** the items listed under Produces. `DestinationStoreId::parse` accepts input only when `s.parse::<i64>()` is `> 0` *and* `n.to_string() == s`. Implement `sign_connector` in `Ed25519Signer` through the existing `sign_json`. `DisabledSigner` returns the same error as its `sign`.

- [ ] **Step 4: Run, expect PASS:** `just test-fast`, then `cargo k`

- [ ] **Step 5: Commit**

```bash
git add crates/contexts/product_access/src
git commit -m "feat(product_access): connector product, store id and identity claims"
```

### Task 2: `ProxyImportRequest` use case

**Files:**
- Create: `crates/contexts/product_access/src/application/imports.rs`
- Modify: `crates/contexts/product_access/src/application/mod.rs` (`mod imports; pub use imports::ProxyImportRequest;`, and the test `JsonSigner` gains `sign_connector`)

**Interfaces:**
- Consumes: `Caller<'_>`, the `TenantEntitlements`, `RolePermissions`, `TokenSigner` and `ProductUpstream` ports, `ConnectorClaims::grant`, and `DestinationStoreId` (Task 1).
- Produces: `ProxyImportRequest::new(entitlements, permissions, signer, upstream) -> Self` (same four `Arc<dyn …>` as `ProxyRequest::new`), and `async fn execute(&self, caller: Caller<'_>, store: DestinationStoreId, request: &ForwardRequest) -> Result<ForwardResponse, ProductAccessError>`.

- [ ] **Step 1: Write the failing tests** in `imports.rs`, with fakes shaped like those in `application/mod.rs` tests:
  - `forwards_with_a_connector_token_for_this_tenant_user_and_store`: the recorded token JSON has `tenant_id == 1001`, `user_id == 3001`, `destination_store_id == "42"` and a 32-char `jti`. Two calls give different `jti`s.
  - `nothing_is_forwarded_without_the_product_or_an_import_permission`: feature off gives `NotEntitled`, an empty `role_ids` gives `NoPermissions`, and in both cases the recorder is empty.
  - `a_failing_entitlement_lookup_fails_closed`: the resolver returns `Storage`, the error propagates and the recorder is empty.

- [ ] **Step 2: Run, expect FAIL:** `just test-fast`

- [ ] **Step 3: Implement** `execute` in the same order as `ProxyRequest::execute`: resolve entitlements, resolve permissions (skip the lookup when `role_ids` is empty), grant `ConnectorClaims`, `sign_connector`, then `upstream.forward`. Use `#[tracing::instrument(name = "product_access.import_proxy", skip_all, fields(tenant_id = %caller.tenant_id, store_id = store.as_i64()))]`.

- [ ] **Step 4: Run, expect PASS:** `just test-fast`

- [ ] **Step 5: Commit** `feat(product_access): proxy import requests with a connector identity token`

### Task 3: Configuration, wiring and the dev-key pin

**Files:**
- Modify: `crates/contexts/product_access/src/config.rs` (`connectors: UpstreamConfig` field, `#[serde(default)]`, validated by the same rule as `ecommerce`)
- Modify: `configs/base.toml` (add a `[product_access.connectors]` block with `base_url = ""` and `service_key = ""`, plus a comment in the style of the ecommerce block)
- Modify: `crates/contexts/product_access/src/adapters/http/mod.rs` (`ProductAccessState { ecommerce, imports: Arc<ProxyImportRequest> }`)
- Modify: `apps/api/src/product_access.rs` (build `HttpProductUpstream::new(&CONNECTORS, …)` with the same fallback-to-unconfigured logic as ecommerce, and the shared signer)
- Modify: `crates/contexts/product_access/src/adapters/signer.rs` tests

**Interfaces:**
- Consumes: `ProxyImportRequest::new` (Task 2), `CONNECTORS` (Task 1).
- Produces: `ProductAccessState.imports`, used by Task 4.

- [ ] **Step 1: Write the failing tests:**
  - `config.rs`: extend `half_configured_products_are_refused` with a connectors `base_url` set but a short `service_key` (`validate()` returns Err), and extend `secrets_never_print` to check that the connectors `service_key` is absent from `Debug`/`Serialize` output.
  - `signer.rs`: `connector_token_verifies_with_the_platform_dev_key`. Sign `ConnectorClaims` with seed `07`×32 and kid `dev-ed25519-1`, then decode with `jsonwebtoken` against public key hex `ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c` and audience `ecommerce-connectors`. Assert header `kid == "dev-ed25519-1"` and that `tenant_id` round-trips as a number.

- [ ] **Step 2: Run, expect FAIL:** `just test-fast`

- [ ] **Step 3: Implement** the config field, `base.toml` block, state field and wiring.

- [ ] **Step 4: Run, expect PASS:** `just test-fast`, then `cargo c`

- [ ] **Step 5: Commit** `feat(product_access): configure and wire the connector upstream`

### Task 4: Routes, permissions and HTTP tests

**Files:**
- Modify: `crates/contexts/access/src/domain/permissions.rs` (two `PermissionDef`s placed after `ecommerce.product.manage`. Descriptions: `"See the tenant's catalog import connections, jobs, previews and failures"` and `"Connect Shopify or Amazon accounts and run, confirm, cancel or retry catalog imports"`)
- Modify: `crates/contexts/access/src/domain/roles.rs` (add both to `tenant_owner` and `tenant_admin`, and extend the existing role tests: owner and admin have both, member has neither)
- Modify: `crates/contexts/product_access/src/adapters/http/handlers.rs`
- Modify: `crates/contexts/product_access/src/adapters/http/routes.rs`
- Modify: `apps/api/src/openapi.rs` (register the handlers, and extend the `product_access` tag text with one sentence on `/ecommerce-imports/stores/{store_id}/{tail}`)
- Test: `crates/contexts/product_access/tests/integration/proxy_http.rs`

**Interfaces:**
- Consumes: `ProductAccessState.imports`, `DestinationStoreId::parse`, `ProductPath::parse`, `ForwardRequest::new`.
- Produces: handlers `proxy_import_get`, `proxy_import_post`, `proxy_import_delete` and `proxy_import_callback`, all `(state, tenant: CurrentTenant, subject: CurrentSubject, request: HttpRequest, path: web::Path<(String, String)>, body: web::Bytes) -> Result<HttpResponse, ApiError>`, delegating to a shared `proxy_import`. The callback's path is `(store_id, provider)`, and it builds the tail `connections/{provider}/callback`. Also `pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)]` in `routes.rs`, shaped like advertising's.

Registration in `product_access_tenant_scope_routes`, **in this order**:

```rust
web::resource("/ecommerce-imports/stores/{store_id}/connections/{provider}/callback")
    .app_data(PermissionMap::new(vec![(Method::GET, manage)]))
    .route(web::get().to(proxy_import_callback));
web::resource("/ecommerce-imports/stores/{store_id}/{tail:.+}")
    .app_data(PermissionMap::new(vec![(Method::GET, read), (Method::POST, manage), (Method::DELETE, manage)]))
    .route(web::get().to(proxy_import_get))
    .route(web::post().to(proxy_import_post))
    .route(web::delete().to(proxy_import_delete));
```

- [ ] **Step 1: Write the failing integration tests** in `proxy_http.rs`. Copy that file's existing harness and wiremock upstream.
  - `import_get_forwards_to_v1_with_the_store_in_the_token`: `GET .../ecommerce-imports/stores/42/jobs?limit=5` reaches the mock as `GET /v1/jobs?limit=5`, with `Authorization: Bearer <key>` and a decodable `X-Identity-Token` whose `destination_store_id == "42"`.
  - `delete_revokes_a_connection`: `DELETE .../stores/42/connections/9` reaches the mock as `DELETE /v1/connections/9`, and the upstream 204 passes through.
  - `callback_query_passes_through_verbatim`: `GET .../stores/42/connections/shopify/callback?code=c&hmac=ab%2Bcd&shop=x.myshopify.com&state=s&timestamp=1` reaches the mock with the query string byte-equal.
  - `read_only_member_cannot_complete_a_callback`: with only `ecommerce.import.read`, the callback returns 403, `GET .../stores/42/jobs` returns 200, and the mock received exactly one request.
  - `malformed_store_ids_are_refused_before_forwarding`: for `0`, `007`, `-1`, `%31`, the response is 400 and the mock received nothing.
  - `tenant_without_ecommerce_gets_the_feature_problem`: 403 with detail `feature not enabled: products.ecommerce.enabled`, and nothing forwarded.

- [ ] **Step 2: Run, expect FAIL:** `scripts/test.sh -p sanvi-product-access`

- [ ] **Step 3: Implement** the permissions, role seeds, handlers (utoipa `path = "/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}"`, `tag = "product_access"`, the same 400/403/502/503 rows as `proxy_ecommerce_*`, with 403 text `"Tenant lacks products.ecommerce.enabled, or the user lacks the ecommerce.import.* permission"`), `PERMISSIONED_ROUTES`, and the registration above. `proxy_import` parses `store_id` with `DestinationStoreId::parse` before `ProductPath::parse`, maps errors through the existing `problem()`, and builds the response exactly as `proxy_ecommerce` does. Extract that response-building tail into a shared `fn respond(ForwardResponse) -> Result<HttpResponse, ApiError>` rather than copying it.

- [ ] **Step 4: Run, expect PASS:** `scripts/test.sh -p sanvi-product-access` and `scripts/test.sh -p sanvi-access`

- [ ] **Step 5: Commit** `feat(product_access): permissioned proxy routes for catalog imports`

### Task 5: Contracts and route matrix

**Files:**
- Modify: `api/product_access.yaml` (generated)
- Modify: `docs/security/route-metadata.json` and `docs/security/route-matrix.md` (generated)
- Modify: `apps/api/tests/integration/permission_catalog.rs` (add product_access's `PERMISSIONED_ROUTES` to `catalogs()`)
- Modify: `apps/api/tests/integration/idor_sweep.rs` (one exemption row per method for `/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}` and one for the callback, with the reason: `"{store_id} names an ecommerce-service store, not a platform entity; the tenant comes from the session and ecommerce-connectors isolates connections and jobs by the token's tenant"`)

- [ ] **Step 1: Regenerate:** `cargo run -p sanvi-cli -- openapi --write` and `just route-matrix-write`. Check the new rows show the declared permission, not `exempt:`.
- [ ] **Step 2: Run:** `just openapi`, `just route-matrix`, and `scripts/test.sh -p sanvi-api`. Expect all green.
- [ ] **Step 3: Commit** `chore(product_access): regenerate contracts and route matrix for import routes`

### Task 6: Companion change in ecommerce-connectors (separate repo, separate branch)

Branch `chore/platform-dev-identity-key` in `ecommerce-connectors`, created from its `main` once the in-progress `normalize.rs`/`tests/amazon.rs` edits are committed or stashed by their owner.

**Files:**
- Modify: `.env.example`: set `CONNECTOR_IDENTITY_PUBLIC_KEYS=dev-ed25519-1:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c`, and update the comment to say it matches the sanvi-backend dev seed.
- Modify: `crates/service/src/bin/mint_identity.rs`: change the default seed to `07` repeated 32 times, so `scripts/smoke.sh` and the platform agree.
- Modify: `.env.example` comment on `SHOPIFY_REDIRECT_URI` (and the Amazon equivalent, if one is configured). In a platform deployment it must point at the admin SPA's import-callback page. That page calls `GET /api/v1/tenant/ecommerce-imports/stores/{store_id}/connections/{provider}/callback?<provider query>`. The SPA keeps `store_id` from before the redirect.

- [ ] **Step 1:** Run `scripts/ci.sh` there and expect green, then run `docker compose --profile stack up -d --build --wait && scripts/smoke.sh` and expect the smoke test to pass with the new key.
- [ ] **Step 2: Commit** `chore(auth): trust the sanvi-backend dev identity key`

### Task 7: Document and gate

**Files:**
- Modify: `docs/superpowers/specs/2026-10-27-ecommerce-proxy-design.md`: add a "Catalog imports (ecommerce-connectors)" section covering the route, permission map, token claims (and how they differ from v2), config, callback flow, and store-id trust decision. Under "Not done" add: connector personal data (it stores `user_id` in connections, jobs and audit) is not yet in the export/erasure pipelines; the admin import screens.

- [ ] **Step 1:** `just ci-quiet`. Expect every gate green, and report any failure with its output.
- [ ] **Step 2: Commit** `docs(product_access): document the catalog import proxy`

## Not in this plan

- Admin import screens and `packages/api-client` methods (sanvi-frontend).
- Data-subject export and erasure for connector-held `user_id`s. That needs connector-side internal privacy routes first.
- Store-existence checks against ecommerce-service (see the `store_id` constraint above for why they are not needed for isolation).
