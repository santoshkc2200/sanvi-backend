# Domains extraction

Domains runs from the separate sibling `domains-service` repository, with
its own database, image, migrations, certificate authority, registrar integrations,
and background sweep workers (verification, issuance, renewal, health, order advancement).
Its README and `docs/cutover.md` document configuration, verified data transfer and rollback.

The platform binary uses a gateway for existing `/api/v1/tenant/domains/*` and
`/api/v1/platform/domains/*` URLs. `features.domains` still gates these routes. Configure
`SANVI__DOMAINS_SERVICE__BASE_URL`, `SANVI__DOMAINS_SERVICE__SIGNING_SECRET` (signs
requests to the service) and `SANVI__DOMAINS_SERVICE__INBOUND_SECRET` (verifies its
deliveries), each at least 32 bytes and never equal. Each delegation carries a unique ID.
The platform retains session/tenant resolution, authoritative permissions, fresh MFA,
and entitlement enforcement (`domains.custom` for claiming custom domains, `domains.purchase`
for domain search and orders). Unentitled tenants are refused with `403 Forbidden` (`reason: "domain_entitlement_required"`)
before signing or contacting the service. Request-bound thirty-second delegations convey only
verified identity and granted Domains permissions; browser credentials are stripped.
There is no automatic local fallback. Unconfigured/unreachable service returns 503.

Domains owns DNS records, orders, certificates, registrar accounts and all writes to
its own database. The platform worker no longer runs domains background sweep jobs
(`domains.verification_poll`, `domains.cert_issue`, `domains.renewal_sweep`,
`domains.health_check`, `domains.order_sweep`).

Domains forwards audit commands to `/internal/domains/audit` and domain events to
`/internal/domains/events`. These endpoints require request-bound delegation; receipts,
keyed by audit command ID or event ID, commit atomically with the audit append/outbox insert.
A retry after a lost acknowledgement produces one durable platform effect, and an ID reused
with different content is rejected with 409 Conflict. Privacy orchestration calls the
service's signed `/internal/privacy/collect`, `/internal/privacy/erase` and
`/internal/privacy/rectify`, which run the embedded `DomainsPrivacyProvider` and return its real
bundle, erase report or rectification result. Unlike accounting, a 204 is never read as "nothing
held": registrant data is found by user ID or email across tenants, so every subject is asked.
The platform provider fails closed (processing pending) while the service is unavailable.

Edge TLS termination in Caddy (`deployment/Caddyfile.domains` and `docker-compose.domains.yml`)
points the `/ask` rewrite and `forward_auth` directly at the service via `DOMAINS_UPSTREAM`
(default: `domains_service:8083`).

Historical Domains migrations and in-process repositories remain for pre-cutover
rollback and existing regression fixtures. They are not selected by the production binary.
After service traffic starts, keep the service database authoritative even when rolling
application images back. Dropping old data/source and reclaiming its schema are separate
contraction changes, after explicit cutover verification.

No production deployment, data transfer or destructive contraction is performed by
this change. Apply platform receipt migrations before starting service delivery;
freeze old writers before copying data.

## Service changes required

The standalone `domains-service` must implement and consume the following contracts
to complete the platform extraction boundary:

### 1. Storefront Live Hostname Lookup (`GET /internal/domains/lookup`)

Storefront tenant resolution by custom hostname (`LiveDomainLookupService` / `DomainsTenantLookupAdapter`)
queries this endpoint to resolve custom domains to tenant IDs.

* **Method**: `GET`
* **Path**: `/internal/domains/lookup?host={hostname}`
* **Query Parameters**:
  * `host`: The normalized custom domain hostname, RFC 3986 percent-encoded by the platform.
* **Authentication / Delegation**:
  * Header `x-domains-delegation`
  * HMAC-SHA256 signed using `domains_service.signing_secret`
  * Audience: `"sanvi-domains"`
  * Purpose: `"platform"`
  * Request binding: Method `GET`, Target `/internal/domains/lookup?host={hostname}`, empty body
* **Expected Responses**:
  * `200 OK`: `application/json` payload `{ "tenant_id": <TenantId Snowflake> }`. The platform caches this positive mapping in `LiveDomainCache` for `host_cache_ttl_secs`.
  * `404 Not Found`: Hostname has no live verified claim. The platform caches negative lookup (`None`) in `LiveDomainCache`.
  * `401 Unauthorized`: Invalid delegation token, expired token, or mismatched signature.
  * `502 / 503 / Network Error`: Platform fails closed (resolves to `None`), logging a warning without caching the error, allowing future retries to succeed once the service recovers.

### 2. Edge TLS Active Tenant Verification (`POST /internal/domains/tenant-state`)

The standalone Domains service edge TLS gate must verify whether a tenant is active before
authorizing certificate issuance or traffic admission, replacing the embedded `AlwaysActiveTenantState` stub.

* **Method**: `POST`
* **Path**: `/internal/domains/tenant-state`
* **Request Body**: `application/json`
  ```json
  {
    "tenant_id": "100000000000000000"
  }
  ```
* **Authentication / Delegation**:
  * Header `x-domains-delegation`
  * HMAC-SHA256 signed using `domains_service.inbound_secret`
  * Audience: `"sanvi-platform-domains-audit"`
  * Purpose: `"tenant-state"` (or `"tenant_state"`)
  * Request binding: Method `POST`, Target `/internal/domains/tenant-state`, JSON body
* **Expected Responses**:
  * `200 OK`: `application/json` payload:
    ```json
    {
      "active": true,
      "status": "active"
    }
    ```
    If the tenant is suspended or archived:
    ```json
    {
      "active": false,
      "status": "suspended"
    }
    ```
    If the tenant ID does not exist:
    ```json
    {
      "active": false,
      "status": null
    }
    ```
  * `401 Unauthorized`: Invalid delegation token, expired token, or signature mismatch.
  * `400 Bad Request`: Malformed JSON or invalid snowflake ID.
