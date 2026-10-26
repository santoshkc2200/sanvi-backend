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
reads as 0; an enabled quota with no limit is sent as `i64::MAX`), `iat`,
`exp`. Ids are decimal strings. The service's side is documented in
`ecommerce-service/docs/architecture.md`.

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

## Not done

- No `jti` replay store on either side; a captured token plus the service key
  is reusable for its 60 s lifetime.
- Plans that include ecommerce (`ecommerce-starter`, bundle) and their
  billing wiring.
- Admin frontend screens calling these routes.
- Restaurant: add a `Product` constant and its upstream when that service
  exists; the delegation and forwarding code is product-neutral.
