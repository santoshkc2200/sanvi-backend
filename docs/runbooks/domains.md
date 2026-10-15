# Domains runbook

Operator procedures for the domains context (phase 08): custom-domain claims, DNS
verification, the TLS authorize gate, purchases, renewals and health monitoring.

Sources of truth:

- `domains.custom_domains` — the claim/state machine (`pending_setup → verifying →
  verified → issuing_cert → live → degraded`, terminal `removed`). **Never hand-edit
  statuses** — transitions happen in use cases that also maintain the projection and
  emit events.
- `domains.live_hostnames` — the host→tenant projection the tenancy resolver and the
  `/internal/tls/authorize` gate read. Maintained transactionally with every transition;
  if it ever disagrees with `custom_domains`, rebuild with the repair statement in §1.
- `domains.orders`, `domains.certificates`, `domains.dns_records` — the purchase saga,
  certificate facts, and managed DNS records (sandbox deployment).
- The edge proxy (Caddy/Traefik) is the TLS terminator; the app only vouches for
  hostnames. Private keys never enter the application database.

## Production customer domains

Customer-owned domains serve the **storefront**. The tenant admin console stays
on the platform admin origin. Registration/purchase is a separate capability;
the live customer-domain overlay disables the sandbox registrar and charger.

`ca_mode = "live"` uses the edge's on-demand ACME issuance. The worker connects
to the configured `anycast_ips` with the customer's hostname as SNI, verifies
the public certificate chain and hostname, and records the actual issuer,
serial and validity. It never follows redirects or uses environment proxies.
An issuance failure leaves the domain retryable, rather than marking it live
with synthetic certificate facts. Every configured edge IP must be public;
the documentation IPs in the default config deliberately fail live startup.

The supplied `deployment/Caddyfile.domains` contains:

- A catch-all HTTPS storefront with on-demand ACME and persistent key storage.
- A loopback-only bridge translating Caddy's `?domain=` ask protocol into the
  authenticated `/internal/tls/authorize?host=` endpoint. Secrets travel in
  headers, never query strings. Neither the bridge nor the Caddy admin API is
  published by Docker.
- An authorization check before **every HTTP request**, so cached certificates
  cannot keep removed or suspended tenants serving. Gate/backend failure denies
  access. `/internal` paths are never forwarded publicly.
- Host preservation and strict SNI/Host matching for tenant resolution.

Deployment procedure (on the deployment host, after the existing API blue/green
proxy and separately deployed storefront are healthy):

1. Supply through the host secret manager/environment:
   `SANVI_DOMAIN_INTERNAL_SECRET` (random shared secret), `ACME_EMAIL`,
   `SANVI_PLATFORM_DOMAIN`, `SANVI_EDGE_HOSTNAME`, `SANVI_EDGE_IPS` (Figment array
   syntax, e.g. `["<public IPv4>"]`), and `STOREFRONT_UPSTREAM` (reachable
   storefront hostname and port). The overlay passes identical domain settings
   to API, next API slot and worker. Keep existing database/Redis/privacy and
   other deployment secrets configured as usual.
2. Point the edge DNS hostname at these same public IPs. Make TCP 80/443 and
   optionally UDP 443 reachable at Caddy; HTTP-01/TLS-ALPN-01 must reach this
   edge. Configure the storefront's existing API, media and identity origins
   and permit customer origins where those separate services require them.
   Run its Node adapter with `PROTOCOL_HEADER=x-forwarded-proto` and
   `HOST_HEADER=x-forwarded-host`, reachable only through the trusted edge;
   leave `ORIGIN` unset so it can serve different customer origins.
3. Validate before applying:
   `docker compose -f docker-compose.staging.yml -f docker-compose.domains.yml config --quiet`.
   Start the edge and apply API/worker configuration with the normal deployment
   procedure using both files. For a first install:
   `docker compose -f docker-compose.staging.yml -f docker-compose.domains.yml up -d`.
   Do not expose either API slot directly to the internet.
4. Grant the tenant `domains.custom` and an appropriate domain quota. In the
   admin Domains screen, connect the customer hostname, publish the generated
   TXT and routing records, then request verification. DNS record names are
   relative to the registrable zone: for `shop.example.com`, TXT name is
   `_sanvi-challenge.shop`, CNAME name is `shop`.
5. Check `verifying`, `verified`/`issuing_cert`, then `live`; inspect the real
   HTTPS certificate and storefront for that hostname. Unknown/unverified
   hostnames must be denied. Remove a test domain and confirm HTTP access is
   denied even when Caddy still has its certificate cached.

Caddy renews certificates independently. The worker observes them near expiry;
an unchanged certificate does not emit a renewal event or clear expiry warnings.
Tune `cert_request_timeout_secs` (default 60) for initial ACME handshakes.
Do not delete the `domains_edge_data` volume on rollout or rollback. Multiple
edges must share supported certificate storage or be deployed with an explicit
per-edge issuance/renewal strategy; the overlay describes one edge instance.

Removal is **service withdrawal**, not CA-level revocation: the projection and
request gate stop service immediately, while Caddy owns certificate retirement.
For key compromise, revoke using the CA/edge's incident procedure. Rollback:
remove the edge overlay from deployment, restore the previous routing and domain
configuration, and retain the edge data volume. No schema migration is needed.

Local proof: `scripts/test.sh --quiet -p sanvi-domains` exercises certificate
observation and DNS instructions. `python3 scripts/test-domains-edge.py` runs the
actual Caddy routes in a disposable container with a local CA and gate fixture;
it proves ask translation, trusted HTTPS, Host preservation, SNI isolation,
secret stripping and denial for unknown/removed domains without public ACME.

## 1. Live-hostname projection repair

The projection is updated in the same transaction as every state change; drift is only
possible after a manual data edit. Rebuild it:

```sql
-- run as sanvi_migrator against the environment database
TRUNCATE domains.live_hostnames;
INSERT INTO domains.live_hostnames (normalized_hostname, tenant_id, status, updated_at)
SELECT normalized_hostname, tenant_id, status, updated_at
FROM domains.custom_domains
WHERE status IN ('verified', 'issuing_cert', 'live', 'degraded');
```

Then restart one worker (or wait a poll interval) so the host caches repopulate.

## 2. Stuck verification

A claim stays `pending_setup`/`verifying` longer than expected:

1. `SELECT id, tenant_id, hostname, status, failure_code, failure_detail, next_check_at,
   challenge_expires_at FROM domains.custom_domains WHERE id = '<snowflake-id>';`
2. `failure_code = txt_missing` → the tenant hasn't published (or published wrong) the
   TXT record. Check the record independently: `dig +short TXT _sanvi-challenge.<host>`.
   Compare with the value in `challenge_token` (`sanvi-verify=<token>`).
3. `failure_code = routing_missing` → CNAME/A records don't point at
   `domains.edge_hostname` / the anycast IPs. `dig +short <host>` and `dig +short
   CNAME <host>`.
4. The backoff schedule is visible in `next_check_at`; nothing to do but wait for
   propagation (up to a few hours for some TLDs). The tenant can trigger an immediate
   re-check with `POST /api/v1/tenant/domains/{id}/verify` (throttled by the schedule).
5. Expired challenge (`challenge_expires_at < now()`): the claim closes itself on the
   next sweep and the tenant re-claims — never un-expire a challenge by hand.

Resolver-layer trouble (all DoH resolvers erroring) appears in the worker logs as
`DNS verification failed at the resolver layer; will retry`; the claim stays
`verifying`. Check egress to `domains.doh_resolvers`.

## 3. Certificate issuance / renewal failures

- `acme_issue_total{result="error"}` climbing, or domains stuck in `issuing_cert`:
  check the worker's `domains cert issuance pass` logs and `failure_detail` on the row.
  With the sandbox CA this should not happen — a real CA outage is the production
  adapter's failure mode, and the sweep retries every interval.
- Renewal failures (`acme_renewal_total{result="error"}`, `CertificateRenewalFailed`
  events, `domains.certificates.last_error`): the current certificate keeps serving
  until `not_after`; escalate when the alert fires (cert < 7 days). Inspect
  `last_error`, confirm the CA/edge is healthy, and the next sweep retries — no
  manual issuance exists by design; the edge renews on demand once the CA recovers.
- A certificate for a domain we no longer serve (orphan): the renewal sweep revokes
  the row automatically (`domains.certificates.status = 'revoked'`).

## 4. Registrar / purchase saga repair

The saga states are `pending → charged → registered → configuring → active` with
`failed`/`refunded` terminals. The worker's `domains order sweep` resumes anything
stuck mid-saga; the API request path already attempts everything once.

- Order stuck `charged` (no `external_order_ref`): the registration call never
  confirmed. The sweep retries it (idempotent on the order id). If it keeps failing,
  check `registrar_api_requests_total{result=...}`; with the sandbox registrar this is
  configuration drift only.
- Order stuck `configuring`: DNS application is retrying. Check `dns_records` rows for
  the domain — the sweep upserts them idempotently.
- Order `failed` with `failure_detail.refund_error`: money was charged but the refund
  didn't land. The sweep retries the refund every interval; if it never lands, treat
  it as a billing incident (runbook: billing) and page the billing owner.
- Renewals: `renewal_attempts`/`next_renewal_at` on `domains.orders` track the retry
  windows; exhaustion emits `DomainOrderFailed` and notifies the tenant. A registry
  renewal that must happen *now* (e.g. within 48h of expiry) is a manual registrar
  action in production — document the external reference in `external_order_ref` and
  update `expires_at` via the provider's normal flow.

## 5. Domain transfer out (manual)

Transfers away are deliberately manual (no automation in phase 08):

1. Verify the requester: tenant owner or platform-ops authorization. Log the request
   and approval in the ticket.
2. The tenant disables transfer lock at the registrar (for sandbox deployments there
   is no lock to release; for a production adapter this is a registrar action).
3. Provide the auth code via the registrar's existing channel (never via support chat
   logs that are not retention-scoped).
4. After the registrar confirms the transfer, force-release locally:
   `POST /api/v1/platform/domains/{id}/release` with the audited reason. This removes
   the claim, revokes the certificate row, deletes the managed zone and invalidates
   resolution caches.

## 6. The TLS authorize gate

`GET /internal/tls/authorize?host=` returns 200 only for a hostname in the live
projection of an active tenant; everything else is 403. The route is
self-authenticated: `x-sanvi-internal` must match `domains.internal_secret` (empty
secret = the gate fails closed and issuance stops — configure it before onboarding
any domain), plus the optional `internal_allowed_ips` CIDR list.

- `tls_authorize_denied_total` climbing → either an edge misconfiguration (wrong
  secret/header) or genuine abuse probing. The per-host reason is logged at warn
  level.
- Deliberately refusing a hostname the edge requests is *normal* (unverified claims,
  removed domains). Investigate only denials for hostnames you expect to serve.

## 7. Health degradation

The hourly sweep checks DNS routing (connected domains), certificate validity and the
storefront's HTTP response. Drift marks the domain `degraded` with a failure code and
notifies the tenant.

- `dns_drift` → the tenant moved their DNS (or their registrar expired). Guide them
  back to the instructions endpoint; the sweep recovers automatically once records
  point at us again.
- `cert_invalid` → the certificate row is expired/absent. Check the edge and the
  renewal job (§3).
- `probe_failed` → the storefront isn't answering. If *many* tenants degrade at once
  (`domain_health_degraded_gauge` spikes, mass-degradation error log), the problem is
  platform-side (edge/anycast), not the tenants'.
