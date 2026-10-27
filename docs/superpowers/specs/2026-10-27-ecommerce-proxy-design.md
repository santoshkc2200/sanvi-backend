# Product access: platform proxy to ecommerce-service

Status: implemented on `feat/ecommerce-proxy`. Step 2 ("shared product access")
of `sanvi-infrastructure/docs/saas-design.md`, for the ecommerce product.

## Why

`ecommerce-service` is a separate process that trusts only the platform. It
needs to know which tenant and user a request acts for, what the user may do,
and the tenant's plan limits, without reading platform tables. The platform
already resolves all of that per request (Kratos session, tenancy, active
membership roles, entitlements), so it signs a short-lived delegation and
forwards the request.

## Route

`GET|POST /api/v1/tenant/ecommerce/{tail}` forwards to
`{product_access.ecommerce.base_url}/v1/{tail}`, inside the shared tenant
scope, so the canonical middleware chain (tenant resolution, authentication,
impersonation, CORS allow-list, rate limits and quotas) runs first. Only GET and POST are proxied because those are all the methods
ecommerce-service serves; add the next method with the first service route
that needs it.

Per request, in `ProxyRequest` (`crates/contexts/product_access`):

1. Resolve the tenant's entitlements (access context, cached resolver). No
   `products.ecommerce.enabled`: 403 `feature not enabled: …`, the same
   detail the platform feature gate uses. This check lives in the use case,
   not in `RequiredFeature`, because that extractor treats an unwired gate
   as enabled and the proxy must fail closed.
2. Turn the active membership's roles (`Subject.role_ids`, empty for
   non-members) into permissions with the access role catalog; keep
   `ecommerce.*`. None: 403, nothing forwarded.
3. Sign the delegation and forward with the service key.

No per-route permission is declared: ecommerce-service authorizes each use
case from the delegated permissions. The route-matrix rows record this as an
explicit exemption.

## Delegation token (identity token v2)

Compact JWT, `alg: EdDSA`, header `kid`, 60 s lifetime (the service accepts at
most 120 s). Claims: `iss` `sanvi-backend`, `aud` `ecommerce-service`, `sub`
user id, `tenant_id`, `permissions` (sorted `ecommerce.*`), `limits` (e.g.
`{"ecommerce.stores.max": 3}`; a disabled quota is omitted, which the service
reads as 0; an enabled quota with no limit is sent as `i64::MAX`), `jti`
(unique per mint, 32 hex chars), `iat`, `exp`. Ids are decimal strings. The
service's side is documented in `ecommerce-service/docs/architecture.md`.

## What crosses

Request headers forwarded: `accept`, `accept-language`, `content-type`,
`idempotency-key`, plus `X-Request-Id`. Cookies, the browser's
`Authorization`, tenant headers and any client `X-Identity-Token` never pass.
Paths must be plain segments (`[A-Za-z0-9._~-]`, no `.`/`..`, no
percent-encoding). Bodies up to 256 KiB; responses up to 4 MiB.

Response headers returned: `content-type`, `idempotency-replayed`. Upstream
status and body pass through, including its problem documents. An upstream
401 (the platform's own key rejected) becomes 502 so a browser never reads it
as session expiry; unreachable, timed out (10 s) or unconfigured upstreams
become 503. Nothing is retried.

## Catalog changes

- Permissions (tenant scope): `ecommerce.store.read`, `ecommerce.store.manage`,
  `ecommerce.product.read`, `ecommerce.product.manage`. `tenant_owner` and
  `tenant_admin` get all four; `tenant_member` gets the two reads. System
  roles are re-seeded idempotently.
- Features: `products.ecommerce.enabled` (boolean, off by default) and
  `ecommerce.stores.max` (quota, default limit 1). A tenant gets ecommerce
  through a platform-operator entitlement override until plans that include
  it exist.

## Configuration

`[product_access]` in `configs/base.toml`; secrets are environment-only:

| Variable | Value |
| --- | --- |
| `SANVI__PRODUCT_ACCESS__SIGNING_KEY_ID` | key id, e.g. `prod-ed25519-1` |
| `SANVI__PRODUCT_ACCESS__SIGNING_KEY_SEED` | 32-byte Ed25519 seed, hex |
| `SANVI__PRODUCT_ACCESS__ECOMMERCE__BASE_URL` | e.g. `http://ecommerce-service:8080` |
| `SANVI__PRODUCT_ACCESS__ECOMMERCE__SERVICE_KEY` | ecommerce's `ECOMMERCE_SERVICE_KEY_CURRENT` |

ecommerce-service trusts the matching public key through
`ECOMMERCE_IDENTITY_PUBLIC_KEYS=kid:hex` (up to two, for rotation). Boot
validation refuses a configured base URL without a 16+ byte service key and a
usable signing key. With nothing configured every proxy request answers 503
after the entitlement and permission checks.

Local development: the seed `07` repeated 32 times with key id
`dev-ed25519-1` matches ecommerce-service's `.env.example`; a unit test
(`matches_the_ecommerce_dev_key`) fails if the two drift. Never use it outside
a laptop.

Rotation: deploy the new public key to ecommerce-service as the second
`ECOMMERCE_IDENTITY_PUBLIC_KEYS` entry, switch the platform's seed and key id,
then drop the old public key after the token lifetime has passed.

## Personal data

The platform's data-subject request pipelines (export and erasure) reach
ecommerce-service through `ProductAccessPrivacyProvider`. Requests call the
service's internal privacy routes (`/internal/v1/privacy/users/{id}/records` and
`/internal/v1/privacy/users/{id}/erasure`) authenticated with the service key
and a privacy-scoped delegation token (`aud: ecommerce-service/privacy`,
`sub: user_id`, fresh `jti`). User-created records are exported under the
`EcommerceAttribution` data class and anonymised across all tenants on erasure.

## Catalog imports (ecommerce-connectors)

`ecommerce-connectors` handles catalog imports from external platforms (Shopify, Amazon). It is reached through the platform proxy under a dedicated route family:

### Routes and Permission Map

- `GET|POST|DELETE /api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}` forwards to `{product_access.connectors.base_url}/v1/{tail}`.
- `GET /api/v1/tenant/ecommerce-imports/stores/{store_id}/connections/{provider}/callback` forwards to `{product_access.connectors.base_url}/v1/connections/{provider}/callback`.

Unlike `ecommerce-service`, the connector authorizes nothing by permission. The platform declares permissions on every route via `PermissionMap` in `product_access_tenant_scope_routes`:
- `GET .../stores/{store_id}/{tail}` requires `ecommerce.import.read`.
- `POST|DELETE .../stores/{store_id}/{tail}` requires `ecommerce.import.manage`.
- `GET .../stores/{store_id}/connections/{provider}/callback` requires `ecommerce.import.manage` because the callback establishes a connection. It is registered before the generic `{tail:.+}` route so read-only users cannot complete an OAuth connection (fails with 403).

In the access context, `tenant_owner` and `tenant_admin` receive both `ecommerce.import.read` and `ecommerce.import.manage`. `tenant_member` receives neither.

### Token claims (vs identity token v2)

The connector token is a compact JWT signed with `alg: EdDSA` using the shared platform signing key (`[product_access] signing_key_id/seed`). It differs from identity token v2 in its claim shape (connector spec §6.2):
- `tenant_id`: JSON integer (`i64`), not a decimal string.
- `user_id`: JSON integer (`i64`), not `sub` string.
- `destination_store_id`: string representing the canonical store ID.
- `aud`: `"ecommerce-connectors"`.
- `iat`, `exp`: UNIX timestamps (lifetime 60 s; connector ceiling is 120 s).
- `jti`: fresh UUIDv7 string per request (`Uuid::now_v7().simple()`), preventing replay.
- No `permissions` or `limits` lists: the platform enforces all permissions before minting.

### Store ID trust decision

The `store_id` slot must be a canonical positive decimal `i64` (`1..=i64::MAX`, no leading zero, no sign, no path traversal or URL encoding). Malformed values like `0`, `007`, `-1`, `%31` or `../` are rejected with 400.
The platform passes `store_id` as `destination_store_id` without verifying that the store exists in `ecommerce-service`: the tenant is derived from the authenticated session, so a wrong store ID can only name another store within the *same* tenant, and `ecommerce-service` validates tenant and store during catalog ingestion.

### Configuration

`[product_access.connectors]` in `configs/base.toml`:
- `base_url`: e.g. `http://ecommerce-connectors:8080`.
- `service_key`: matching `CONNECTOR_SERVICE_KEY_CURRENT` (env `SANVI__PRODUCT_ACCESS__CONNECTORS__SERVICE_KEY`).
Empty values are valid and cause the proxy to fail closed with 503.

### Callback flow

During OAuth connection (e.g. Shopify), the admin SPA redirects the merchant to the provider. The provider returns to the SPA's callback page with query parameters (`code`, `hmac`, `shop`, `state`, `timestamp`). The SPA forwards this to `GET /api/v1/tenant/ecommerce-imports/stores/{store_id}/connections/{provider}/callback?<provider query>`. The query string is forwarded byte-for-byte to preserve HMAC verification.

## Not done

- Plans that include ecommerce (`ecommerce-starter`, bundle) and their
  billing wiring.
- Admin frontend screens calling these routes.
- Connector personal data: `ecommerce-connectors` stores `user_id` in connections, jobs and audit logs, but is not yet wired into the platform export and erasure pipelines.
- Restaurant: add a `Product` constant and its upstream when that service
  exists; the delegation and forwarding code is product-neutral.
