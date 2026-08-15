# Phase 08 — Custom Domains, DNS & TLS (backend)

**Target version:** 0.9.0
**Depends on:** phases 01 (host → tenant resolution hook), 03 (entitlement `domains.custom`), 06 (localized guides)
**Unlocks:** tenant-owned branded storefronts on their own domain

## Goal

A `domains` bounded context that supports two journeys from the brief:

1. **Connect** — the tenant already owns a domain: we tell them exactly what records to create at
   their registrar (with registrar-specific guidance), verify ownership, then issue and renew TLS.
2. **Purchase** — the tenant buys a domain through Sanvi: we search, quote, register, auto-configure
   DNS, verify, and issue TLS with no manual steps.

Both end in the same state: a verified, TLS-terminated, tenant-resolving hostname.

## Scope

**In**
- Domain claim lifecycle, ownership verification (DNS TXT / CNAME / HTTP), apex + www handling.
- Registrar port with a first adapter for domain search/quote/register/renew/transfer-lock.
- Authoritative DNS management port (adapter for the chosen managed DNS provider).
- ACME certificate lifecycle (issue, renew, revoke) and edge integration.
- Health monitoring of live domains (DNS drift, cert expiry, misconfiguration) with tenant alerts.
- WHOIS privacy, auto-renew, expiry warnings, and the money side (purchase is billed through phase 04
  as a one-off/recurring charge on the platform subscription).

**Out**
- Email/MX hosting for purchased domains (documented as unsupported; we only guide).
- Transferring a domain *away* automation (manual runbook, auth code exposed on request).

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Verification | `TXT _sanvi-challenge.<domain> = <token>` for ownership; `CNAME` (or `ALIAS`/`A`) for routing, checked independently | Ownership and routing fail for different reasons; users need to know which |
| Apex domains | Prefer provider `ALIAS`/`ANAME`/flattened CNAME; fall back to documented `A` records to our anycast IPs, and always offer `www` + redirect | CNAME at apex is illegal in DNS; the fallback must be explicit |
| TLS | ACME via an edge proxy with **on-demand TLS**, gated by an internal `/internal/tls/authorize?host=` endpoint that only approves verified, active domains | No cert stampede, no cert for a domain we don't serve, zero manual issuance |
| ACME challenge | HTTP-01 for connected domains (works once routing is in place); DNS-01 for purchased domains and wildcards (we control DNS) | Each journey uses the challenge it can actually pass |
| Cert storage | Edge-managed storage backed by object storage/KV with encryption at rest; private keys never enter the app database | Least privilege for the most sensitive material we hold |
| Registrar | Behind `DomainRegistrarPort` with one adapter to start; pricing, TLD availability and renewal semantics normalised | The brief says "purchase"; registrar choice is commercial and will change |
| DNS provider | Behind `DnsProviderPort`; we manage zones only for purchased domains | Connected domains stay at the customer's registrar — we never ask for their credentials |
| Verification cadence | Backoff polling (10 s → 5 min) for 72 h, then `expired` with a resumable retry | DNS propagation is slow and users walk away |
| Domain state | `pending_setup → verifying → verified → issuing_cert → live → degraded → removed` | Every state maps to a distinct UI message and remediation |

## Domain model

```rust
pub struct CustomDomain { id, tenant_id, hostname: Hostname, kind: DomainKind /*Connected|Purchased*/,
                          role: DomainRole /*Primary|Alias|Redirect*/, status: DomainStatus,
                          verification: VerificationChallenge, cert: Option<CertificateRef>,
                          last_checked_at, failure: Option<DomainFailure> }
pub struct DomainOrder { id, tenant_id, hostname, registrar: RegistrarId, term_years: u8,
                         price: Money, status: OrderStatus, auto_renew: bool, whois_privacy: bool,
                         expires_at, external_order_ref }
pub struct CertificateRef { id, hostname, issuer, serial, not_before, not_after, renewed_at }
```

Invariants: one `Primary` per tenant; hostname globally unique across tenants (first verified claim
wins, and an unverified claim expires so a domain is never permanently squatted); a domain cannot go
`live` without both verification and a valid certificate.

Events: `DomainClaimed`, `DomainVerified`, `DomainVerificationFailed`, `CertificateIssued`,
`CertificateRenewed`, `CertificateRenewalFailed`, `DomainWentLive`, `DomainDegraded`,
`DomainOrderPlaced`, `DomainOrderCompleted`, `DomainExpiringSoon`.

## Deliverables

### Schema

```sql
CREATE SCHEMA domains;
CREATE TABLE domains.custom_domains (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL, hostname text NOT NULL,
  normalized_hostname text NOT NULL UNIQUE,       -- punycode, lowercased
  kind text NOT NULL, role text NOT NULL, status text NOT NULL,
  challenge_token text NOT NULL, challenge_type text NOT NULL,
  verified_at timestamptz, last_checked_at timestamptz, failure_code text, failure_detail jsonb,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX one_primary_per_tenant ON domains.custom_domains (tenant_id)
  WHERE role = 'primary' AND status <> 'removed';

CREATE TABLE domains.orders (...);
CREATE TABLE domains.certificates (id uuid PRIMARY KEY, domain_id uuid NOT NULL,
  issuer text, serial text, not_before timestamptz, not_after timestamptz,
  status text NOT NULL, last_error text);
CREATE TABLE domains.dns_records (id uuid PRIMARY KEY, domain_id uuid, type text, name text,
  value text, ttl int, managed boolean NOT NULL DEFAULT true);
```

RLS on all tables; `normalized_hostname` is the global uniqueness key and is IDN-normalised
(punycode) to prevent homograph collisions.

### Connect journey

1. `POST /api/v1/tenant/domains {hostname, role}` → validate (public suffix list, not our own
   apex/subdomain, not an IP, not already verified elsewhere, entitlement present) → create claim
   with challenge token.
2. Response includes **exact records** and a registrar-specific guide id (phase 06 localised):
   `TXT _sanvi-challenge.example.com = sanvi-verify=…` and
   `CNAME www.example.com → edge.sanvi.app` (+ apex instructions for the detected provider).
3. Registrar detection from NS records lets us show "here's how it looks in Cloudflare / Route 53 /
   お名前.com" rather than generic advice.
4. Verification worker polls with backoff using DoH against multiple resolvers (avoid single-resolver
   cache lies) and requires agreement from ≥ 2 resolvers.
5. On verification → status `verified` → the edge is allowed to issue on demand → first request
   triggers HTTP-01 → `live`.

### Purchase journey

1. `GET /api/v1/tenant/domains/search?q=` → registrar availability + normalised pricing (cached,
   rate-limited, never fanning out uncontrolled registrar calls).
2. `POST /api/v1/tenant/domains/orders` → price re-quote → charge via phase 04 (Stripe, on the
   platform subscription, so tenants pay Sanvi) → register with the registrar (idempotent by our
   order id) → create the zone in our DNS provider → apply records → auto-verify (we control DNS) →
   DNS-01 cert issuance → `live`.
3. Auto-renew by default; renewal fires 30 days before expiry with 3 retry windows; failures escalate
   to the tenant and to platform ops. Expiry warnings at T-30/T-14/T-7/T-1 days.
4. WHOIS privacy on by default where the TLD allows; registrant contact data is personal data →
   registered in the phase 05 data map with the registrar as a sub-processor.

### Edge / TLS integration

- Edge proxy (Caddy or Traefik in the reference deployment) is configured with on-demand TLS and an
  `ask` endpoint: `GET /internal/tls/authorize?host=` returns 200 only for a hostname in
  `verified|live` for an `active` tenant. Everything else is 403, which stops abuse-driven issuance.
- Rate limits at the edge for issuance attempts per host and globally, protecting the ACME account.
- OCSP stapling, TLS 1.2+ only, HSTS with a documented preload decision *per tenant* (preload is
  irreversible-ish, so it is opt-in with a clear warning).
- Certificate renewal monitored independently of the edge: a job compares `not_after` against now and
  alerts at 21/14/7 days regardless of what the edge believes.

### Health monitoring

Hourly job per live domain: DNS records still point at us, cert valid and matching, HTTP 200 from the
storefront, redirect chain sane. Drift → `degraded` + tenant notification with the exact fix + a
platform alert if many tenants degrade at once (indicates our problem, not theirs).

### API surface (`api/domains.yaml`)

| Method | Path | Permission |
|---|---|---|
| POST/GET/DELETE | `/api/v1/tenant/domains` | `domains.manage` (+ entitlement) |
| GET | `/api/v1/tenant/domains/{id}/instructions` | `domains.read` |
| POST | `/api/v1/tenant/domains/{id}/verify` | `domains.manage` (manual re-check, rate-limited) |
| POST | `/api/v1/tenant/domains/{id}/promote` | `domains.manage` (make primary) |
| GET | `/api/v1/tenant/domains/search` | `domains.purchase` |
| POST/GET | `/api/v1/tenant/domains/orders` | `domains.purchase` |
| POST | `/api/v1/tenant/domains/orders/{id}/auto-renew` | `domains.purchase` |
| GET | `/internal/tls/authorize` | internal only (edge) |
| GET/POST | `/api/v1/platform/domains` | `platform.domain.*` (force-release, investigate) |

## Work breakdown

1. Hostname value object: IDN/punycode, public-suffix validation, reserved-name blocklist, length.
2. Claim lifecycle + uniqueness + expiry of stale claims.
3. DNS checker: multi-resolver DoH, TXT/CNAME/A/ALIAS/NS lookups, propagation-aware backoff.
4. Registrar-detection heuristics + guide catalog (localised, one per major registrar + generic).
5. `DomainRegistrarPort` + adapter (search, quote, register, renew, lock, contacts, WHOIS privacy).
6. `DnsProviderPort` + adapter (zone create, record CRUD, DNS-01 challenge records).
7. ACME integration and the `/internal/tls/authorize` gate; edge configuration as code.
8. Purchase flow with charge → register → configure saga, compensations, and idempotency.
9. Renewal, expiry warnings, auto-renew billing, and failure escalation.
10. Health monitor + degraded-state notifications + platform-wide anomaly alert.
11. Phase-01 integration: custom domain → tenant resolution cache, with instant invalidation.
12. Runbooks: stuck verification, cert issuance failure, registrar outage, domain transfer out.

## Testing

- Hostname parser property tests: unicode, homographs, trailing dots, uppercase, over-length labels,
  public-suffix edge cases (`co.uk`, `com.au`).
- DNS checker against a mock resolver: partial propagation, conflicting resolvers, NXDOMAIN, CNAME
  chains, wildcard records.
- Saga tests for purchase: failure at each step (charge ok/registration fails → refund path;
  registration ok/DNS fails → retry without double-registering).
- `/internal/tls/authorize` matrix: unverified, wrong tenant, suspended tenant, removed domain → 403.
- Cert expiry simulation with a fake clock; renewal failure escalation.
- Squatting test: tenant A claims `x.com` unverified, tenant B verifies it → B wins, A's claim closes
  with a clear reason.

## Security & compliance

- Verification tokens are high-entropy, single-domain, single-tenant, and rotated on re-issue.
- We never request or store registrar credentials for connected domains.
- Registrant contact data (purchase journey) is personal data: minimised, encrypted, exportable and
  erasable subject to registrar/ICANN retention rules (documented as a retention override).
- SSRF protection on any fetch of a tenant-supplied hostname (no internal ranges, no redirects to
  private IPs, DNS pinning).
- Abuse: purchased-domain flows are entitlement-gated and rate-limited; phishing-style hostnames are
  screened against a blocklist and flagged for review.

## Observability

Metrics: `domain_verification_duration`, `domain_status_gauge{status}`, `cert_days_to_expiry_min`,
`acme_issue_total{result}`, `tls_authorize_denied_total`, `registrar_api_latency`,
`domain_health_degraded_gauge`. Alerts: any cert < 7 days, ACME failure rate, registrar API errors,
mass degradation.

## Acceptance criteria

- [ ] Connecting an externally-registered domain succeeds end to end with copy-pasteable records and
      a working `https://` within minutes of DNS propagation.
- [ ] Purchasing a domain results in a live, TLS-secured storefront with no human step.
- [ ] `/internal/tls/authorize` refuses every hostname we do not serve (matrix test green).
- [ ] Certificates renew automatically; a simulated renewal failure pages before expiry.
- [ ] A domain removed from a tenant stops resolving and its cert is no longer served.
- [ ] Health monitor detects an intentional DNS change and notifies the tenant with the fix.

## Risks

| Risk | Mitigation |
|---|---|
| ACME rate limits during mass onboarding | On-demand issuance behind the authorize gate, per-host throttling, staged rollout, ACME account monitoring |
| Registrar API instability or repricing | Port abstraction + second adapter planned; quotes re-validated at purchase time; failures refund cleanly |
| Apex/DNS provider limitations confuse users | Provider detection + specific guides + `www`-primary fallback recommendation |
| Domain squatting / hostile takeover attempts | Verified claim wins, unverified claims expire, all claims audited |
| Purchased-domain refunds (registrations are usually non-refundable) | Explicit pre-purchase disclosure; charge only after availability re-check; failed registration auto-refunds |
