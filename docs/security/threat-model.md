# Threat model — STRIDE per trust boundary

**Version:** 1 · **Refreshed:** 2026-10-02 (TASK-024 slice 11.5g; previous: v0, last extended phase 09)
**Scope:** the backend platform — workspace, API, worker, CLI, local/staging stack, CI/CD — across
every context crate (fifteen at refresh time) and every serving surface. Refreshed at the end of
every phase.

## Method

v0 listed threats phase by phase. That read as history, not as a model: nothing forced a surface
added in phase 08 to name the boundary it crosses, and nothing stopped a mitigation from being a
paragraph instead of an artifact. v1 is organised the other way round — **one section per trust
boundary**, each threat STRIDE-classified, and (the rule this refresh adds, TASK-024 step 1):

> **Every trust boundary names at least one mitigation that resolves to a test file path or a CI
> gate** — an artifact that can be opened and run, not a claim. The gate scripts carry `--self-test`
> modes proving they fail on a planted violation, so "the gate exists" is itself falsifiable.

Likelihood/impact are judgement labels (`L/I`), not measurements; the evidence column is the part
this document stands on. Threat ids are stable across versions: T1–T45 are carried from v0 (some
re-homed to their boundary), T46+ are new at this refresh.

System shape in one paragraph: three binaries (API, worker, CLI) over Postgres + Redis; tenancy is
resolved per request (signed internal header, custom domain, or subdomain); identity is Ory Kratos
with backend projection webhooks; authorization is Ory Keto with a permission gate on every
non-public route; money runs on Stripe Connect (direct charges — Sanvi is never merchant of record);
advertising capture is a public beacon with consent directives; domains/TLS at the edge; the compose
stack adds Kratos, Keto, MinIO, mailpit, the notification service, and (billing profile) stripe-mock.

## TB1 — Public internet → API edge

What crosses: every request, authenticated or not, before any tenant or session exists.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T5 | Unbounded requests exhausting the API | DoS | H/M | Concurrency limit, per-request timeout, body caps, per-IP token bucket, fail-open/fail-closed per env (T13) — `crates/platform/http/tests/integration/middleware_rejections.rs` (fail-closed, timeout, panic, concurrency on a real socket) |
| T6 | Panic leaks stack traces to clients | Information disclosure | L/M | Panic-catch middleware → generic 500 problem+json; span logs server-side only — `apps/api/tests/integration/http_contract.rs` (error envelope on the assembled app) |
| T7 | Clickjacking / MIME sniffing / referrer leakage | Various | L/L | Specified six-header set pinned **per serving surface** (probe, public JSON, 404, openapi, metrics, tracking) in both TLS postures; duplicates fail; `server`/`x-powered-by` must stay absent — `apps/api/tests/integration/headers_snapshot.rs` |
| T8 | Cross-origin abuse (stolen credentials, CSRF) | Spoofing | M/M | CORS explicit allow-list, no wildcards, misconfiguration rejected at startup; preflight non-coverage pinned so a stack reorder is a conscious snapshot update — `apps/api/tests/integration/headers_snapshot.rs` |
| T12 | Forged request correlation / path smuggling | Spoofing | L/M | Request-id accepted only as UUIDv7; actix normalises paths; body/head limits — `crates/platform/http/tests/integration/middleware_rejections.rs`, id shape pinned by `scripts/check-id-policy.sh` |
| T13 | Rate limiter outage opens (or closes) the API | DoS | L/M | `rate_limit.fail_open` per environment; production fails closed — `crates/platform/http/tests/integration/middleware_rejections.rs` |
| T55 | Security config that exists but never takes effect (the HSTS dead-code class: header gated on the bind address, which is `0.0.0.0` everywhere) | Tampering | M/H | HSTS is now explicit config (`server.security.hsts`) with startup validation (max-age ≥ 1; preload must qualify); snapshot asserts the TLS posture per surface — `apps/api/tests/integration/headers_snapshot.rs`; the API binary refuses `SANVI_ENV=production` with `max_age_secs=0` (startup validation path) |

## TB2 — Host → tenant resolution

What crosses: the Host header, `<slug>.<base>` subdomains, custom domains, and the internal signed
tenant header used by the edge/worker. This boundary decides which tenant every later RLS predicate
will use, so forgery here is forgery of the isolation key itself.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T60 | Forged `x-sanvi-tenant` internal header (attacker picks the tenant the whole stack believes) | Spoofing | M/C | `v1.<key>.<tenant>.<ts>.<HMAC-SHA256>` tokens, constant-time verify, ±60 s TTL, key rotation via `active_key_id`; a present-but-invalid header is rejected 403 and **never falls through** to subdomain resolution; header with no keys configured rejects — `crates/platform/http/tests/integration/tenant_resolution.rs`, signer/verifier in `crates/contexts/tenancy/src/adapters/resolution/signed.rs` |
| — | Tenant spoofing via hostile Host input (homoglyph slugs, ports, trailing dots, multi-label prefixes) | Spoofing | M/H | Suffix match, multi-label rejection, hostile slugs resolve NotFound; unresolvable host answers 404 (never 403) so tenants cannot be enumerated — `crates/platform/http/tests/integration/tenant_resolution.rs` |
| T46 | Subdomain/hostname takeover: wildcard edge serves a hostname whose DNS the attacker controls, or a tenant permanently squats a hostname | Elevation / Spoofing | M/H | Custom domains require a TXT challenge at `_sanvi-challenge.<host>` (plus CNAME/A) verified across ≥2 agreeing DoH resolvers; claims expire (72 h) so a hostname is never squatted forever; partial unique index `one_live_domain_per_hostname` — first verified tenant wins, losers close with `verified_by_other_tenant`; `platform_domain` and its subdomains are unclaimable; `Hostname::parse` rejects IPs/localhost/bare TLDs — `crates/contexts/domains/tests/domains_integration.rs` |
| — | Deleted domain keeps serving (stale projection) | Tampering | L/M | Remove deletes the `live_hostnames` row ("the hostname must stop resolving"), revokes the certificate, and platform operators can force-release — `crates/contexts/domains/tests/domains_integration.rs`, `crates/contexts/domains/src/application/remove.rs` |

## TB3 — Identity provider ↔ backend (Kratos)

What crosses: session cookies verified via whoami, and the `/internal/kratos/*` projection webhooks.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T14 | Unauthenticated access to privileged endpoints | Spoofing | L/C | Session verification on every non-public route via extractors; backend authn failure never fails open — `apps/api/tests/integration/permission_catalog.rs` + the route gate (`TB4`) |
| T15 | Account takeover via auto-linking an unverified provider email | Spoofing | L/C | Auto-link only for `trusted_email_verified_providers` with both sides verified; everything else gets a single-use, 10-minute, session-bound challenge and the existing owner is notified — `crates/contexts/identity/tests/integration/identity_application.rs` |
| T16 | Forged Kratos webhook creating/merging projections | Spoofing | L/H | `/internal/kratos/*`: constant-time shared-secret header + client-IP allow-list, **empty allow-list fails closed**; hooks are interrupting so no Kratos identity exists without a projection — `crates/contexts/identity/tests/integration/identity_application.rs`; the registration webhook is also an abuse-quota surface (signup) — `apps/api/tests/integration/abuse_quotas.rs` |
| T17 | Session replay after logout/revocation | Spoofing | L/H | Session cache TTL ≤ 60 s (config-validated 1..=60); revocation deletes by session id and by token hash; API revoke also revokes in Kratos — `crates/contexts/identity/src/adapters/session_cache.rs` (TTL bound validated) + `crates/contexts/identity/tests/integration/identity_application.rs` |
| T19 | Invitation token abuse (replay/guess) | Elevation | L/H | 256-bit random tokens, SHA-256 at rest, single use, expiry, email must match the session, acceptance idempotent — `crates/contexts/identity/tests/integration/identity_application.rs`; invitation sends are abuse-counted — `apps/api/tests/integration/abuse_quotas.rs` |

## TB4 — Authorization decisions (Keto + route gates)

What crosses: every authenticated request's permission check, and the catalogs that decide what a
check even looks like.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T14 | Route mounted without a permission check | Elevation | L/C | Every non-public route declares a permission and every declared permission exists in the registry, both directions off the same catalogs registration iterates — `apps/api/tests/integration/permission_catalog.rs` |
| T59 | Route landing (or changing) without its security declaration — permission, tenant scope, rate class, abuse quota | Elevation | M/H | Generated route/permission matrix from OpenAPI + handler metadata; committed output with source fingerprints; unmapped route, missing field, or stale output fails the gate; matrix diff must be acknowledged in the PR — `scripts/generate-route-matrix.sh` (+ mutation self-test), committed `docs/security/route-matrix.md`, pr.yml `route-matrix` job |
| T18 | Cross-tenant membership escalation | Elevation | L/C | RLS FORCE on `identity.memberships*` and invitations; scoped transactions by construction — `crates/contexts/identity/tests/integration/identity_rls.rs` and the schema-wide matrix (`TB5`) |
| T20 | Authorization cache serves stale grants after revocation | Elevation | L/H | Decision cache TTL ≤ 30 s, never caches errors; worker deletes Keto tuples on `MembershipRevoked` — `crates/contexts/access/src/adapters/decision_cache.rs` (inline tests: TTL, error non-caching) |
| T21 | Keto outage grants/denies incorrectly | DoS / Elevation | L/H | Fail closed: backend errors map to 503, never Allowed — `crates/contexts/access/src/adapters/keto.rs` (inline tests) |
| — | Privilege escalation through custom roles ("grant what you lack") | Elevation | L/C | Authoring use case refuses grants beyond the author's own scope; system roles immutable — `crates/contexts/access/tests/integration/role_authoring.rs`; entitlement overrides flip one tenant only and quota consumption is atomic under parallelism — `crates/contexts/access/tests/integration/entitlements_integration.rs` |
| — | Entitlement gate referencing a feature that does not exist (or vice versa) | Tampering | L/M | Every gated route references a declared feature, both directions — `apps/api/tests/integration/feature_catalog.rs` |

## TB5 — Row isolation inside Postgres (RLS & scoped transactions)

What crosses: every SQL statement. This is the boundary that makes multi-tenancy real; it is asserted
structurally (schema) and behaviourally (requests), from the live catalog so new tables are covered
as they land.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T1 | Cross-tenant read via a missing `WHERE` predicate | Information disclosure | M/C | Every `tenant_id` table: ENABLE + FORCE RLS and exactly one canonical `tenant_isolation` policy, matched exactly; **every child partition asserted by name** (a partition created after the policy is the likely gap); `sanvi_app` is `NOBYPASSRLS` (asserted first, because a flipped bit decorates every other check); the one no-RLS exemption (`domains.live_hostnames`, public-DNS data) is frozen to its column list — `apps/api/tests/integration/rls_matrix.rs`, per-context `crates/contexts/identity/tests/integration/identity_rls.rs` |
| T54 | Existence oracle on id-bearing routes (foreign id answers differently from unknown id) | Information disclosure | M/M | Matrix-derived IDOR sweep: foreign-host sweep, cross-tenant real-id sweep (404/403, foreign-row marker never echoed, 204 only for idempotent deletes with the row proven intact), and a nonexistent-id twin asserting identical status — `apps/api/tests/integration/idor_sweep.rs` |
| T2 | SQL injection through unvalidated input | Tampering | L/C | SQLx bound parameters everywhere; SAST (clippy security groups piercing crate allows + pinned semgrep ruleset) — pr.yml `sast` job; cross-context SQL banned — `scripts/check-layering.sh` |
| T11 | Migrator/API role privilege escalation | Elevation | L/H | `sanvi_app` cannot create tables or bypass RLS; migrations run as a separate role (pr.yml `sqlx-prepare` applies them as the non-BYPASSRLS app role before checking the SQLx cache) — `apps/api/tests/integration/rls_matrix.rs` (role bits) + pr.yml `sqlx-prepare` job |
| — | Unscoped (platform) access escaping audit | Repudiation | L/H | Every unscoped transaction must go through `begin_unscoped(reason, actor)`, which sets `sanvi.platform_scope` and writes an audit row; the matrix's behavioural half asserts platform scope sees everything *and* writes those rows — `apps/api/tests/integration/rls_matrix.rs`, `crates/platform/persistence/src/tx.rs` |

## TB6 — Stripe → backend (webhooks: platform billing & Connect)

What crosses: provider events that become money state. Governing invariant: **Sanvi is never the
merchant of record** — direct charges on the connected account, funds never touch the platform
balance.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T39 | Forged Connect/platform webhooks creating false money state | Spoofing | M/C | Signature over the **raw** body before any parse, endpoint-specific secrets that must differ from each other (boot invariant), timestamp tolerance, client-IP allow-list failing closed when empty, store-first dedupe on provider event id — `crates/contexts/payments/tests/payments_integration.rs`, `crates/contexts/billing/tests/billing_integration.rs`, distinct-secret boot check in `apps/api/src/config.rs` |
| T40 | Webhook replay or reordering corrupting state | Tampering | M/H | Event-id PK dedupe; per-connection out-of-order anchor drops strictly older events; handlers idempotent (upserts by provider ids, forward-only transitions), proven in the version-upgrade replay corpus — `crates/contexts/billing/tests/billing_integration.rs`, `crates/contexts/payments/tests/payments_integration.rs` |
| T41 | Poison event blocking the money pipeline | DoS | M/H | One transaction per event; 5 attempts then dead-letter; DLQ depth pages — `crates/contexts/payments/tests/payments_integration.rs`; paging rule pinned in `observability/alerts/rules.json` by `crates/platform/telemetry/tests/alert_rules.rs` |
| T38 | Cross-tenant money routing (charging on another tenant's connected account) | Elevation | L/C | Connections load through RLS-scoped reads; outbound command must carry exactly that tenant's account id; webhook events route by provider account id to exactly one row — `crates/contexts/payments/tests/payments_integration.rs` + `TB5` matrix |
| — | Merchant-of-record drift (fee config, transfer shapes) | Repudiation / Fraud | L/H | Application fees disabled by default; fee config is platform-admin only and audited; the banned-pattern gate rejects Stripe merchant-of-record shapes in source — `scripts/check-connect-invariants.sh` (+ self-test, pr.yml `connect-invariants` job); runtime alarm `payments_platform_balance_movements_total` pinned by `crates/platform/telemetry/tests/alert_rules.rs` |

## TB7 — Backend → Stripe (outbound keys & payment architecture)

What crosses: the restricted API key, the hosted-page redirect, and nothing else — by architecture.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T45 | Provider credentials over-scoped or leaked | Information disclosure | L/H | Connect uses a restricted `rk_` key (boot-validated shape, distinct from the billing key); secrets env-only, `Redacted`-wrapped — `crates/contexts/payments/src/config.rs` (`validate()`), `crates/platform/telemetry/tests/secret_inventory.rs`, inventory in `docs/security/secrets-inventory.md` |
| T42 | Card data / KYC PII landing in Sanvi storage | Information disclosure | L/C | Hosted-page checkout and embedded onboarding keep PAN/KYC at Stripe; schema has no columns for either — architecture re-confirmed each phase that touches payments (PCI note below) |
| T43 | Platform becoming a party to tenant money flows | Repudiation / Fraud | L/H | See T38/T41 row in `TB6` (fee invariants + alarm) |
| T44 | Tenant disconnect stranding money with no local record | Tampering | L/H | Disconnect guarded by named blockers (in-flight payments, disputes, pending payouts); handlers route by account regardless of lifecycle — `crates/contexts/payments/tests/payments_integration.rs` |

## TB8 — Ad platforms ↔ backend (OAuth, tokens, uploads)

What crosses: the OAuth redirect on connect, stored access/refresh tokens, and conversion/audience
uploads. Consent is a privacy rule enforced at this boundary, not a UI preference.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T51 | OAuth state forgery / callback swap (attacker binds their platform account into a tenant connection, or MITMs the code) | Spoofing | M/H | OAuth `state` is an HMAC-signed claim (tenant, platform, nonce, expiry) with a **single-use server-side nonce registry** — consume deletes the nonce — `crates/contexts/advertising/tests/integration/advertising_integration.rs` (OAuth state manager), `crates/contexts/advertising/src/adapters/oauth_state_store.rs` |
| T52 | Ad tokens stolen at rest (DB read, dump, log) | Information disclosure | L/H | AES-256-GCM envelope: per-tenant DEK wrapped by a config KEK; ciphertext + wrapped DEK in dedicated tables; empty KEK disables connect rather than storing plaintext; KEK is `Redacted` and render-gated — `crates/contexts/advertising/src/adapters/token_vault.rs`, `crates/platform/telemetry/tests/secret_inventory.rs`, `docs/security/secrets-inventory.md` |
| T50 | Conversion captured/uploaded despite withheld consent | Repudiation / Information disclosure | M/H | Capture consults the directive engine (unknown platform ⇒ third-party purpose check); uploads carry the directive snapshot; consent withdrawal actively retracts/suppresses; audience lists require both purposes and exclude opted-out subjects — `crates/contexts/advertising/tests/integration/tracking_integration.rs`; vendor role defaults to protective `ThirdParty` for unknown platforms — `crates/contexts/advertising/src/adapters/vendor_role.rs` |
| — | Refresh-tokens loaned forever (worker keeps refreshing a revoked grant) | Tampering | L/M | Refresh sweep mints through the tenant vault; permanently failed refreshes transition the connection to `disconnected` — `crates/contexts/advertising/src/application/token_refresh.rs` (worker), vault tests in `crates/contexts/advertising/src/adapters/token_vault.rs` |

## TB9 — Browser/beacons → public tracking endpoint

What crosses: anonymous `POST /api/v1/public/track` beacons — the one unauthenticated write surface,
and the one where enforcement is already on.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T48 | Beacon flood costing reputation (spam into ad platforms) or the database | DoS | H/M | The endpoint answers a uniform empty `202` regardless of outcome (no oracle for probers); bot-UA blocklist; `Origin` required; per-IP and per-Origin permits; site-key HMAC (tenant+origin, ≥32-byte secret, constant-time); abuse quota **enforced** (per-IP 120/min, per-Origin 600/min) with the gate deciding before the router — `apps/api/tests/integration/abuse_quotas.rs`, `crates/contexts/advertising/tests/integration/tracking_integration.rs`, `crates/platform/http/src/abuse.rs` (flood-holds unit tests), measured posture committed in `benchmarks/abuse-shadow-2026-09-30.md` |
| T49 | Raw customer identifiers (email/phone) flowing to ad platforms or storage | Information disclosure | M/H | Identifiers normalised then SHA-256-hashed (`HashedIdentifier`); landing URL sanitised (query/fragment/userinfo stripped); the source gate bans raw email/phone anywhere in advertising adapters **except the single tracking handler** — `scripts/check-ads-invariants.sh` (+ self-test), hashing unit tests in `crates/contexts/advertising/src/domain/hashing.rs` |

## TB10 — Platform operator → impersonation & approvals

What crosses: a support operator acting inside a tenant. The most privileged routine path in the
system.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T53 | Impersonation abuse: silent, overbroad, stale, or targeting a privacy-locked tenant | Elevation / Repudiation | L/C | Creation guardrails (target must be active member, privacy lock refuses outright, duration clamped 30/60 min, reason required) with audit entry + `ImpersonationStarted/Ended` in the same transaction; runtime middleware resolves `x-sanvi-impersonate: {grant_id}` to an active grant, `ReadOnly` grants reject unsafe methods, responses carry the banner header, the real actor is recorded; fresh-AAL2 window gates dangerous platform actions — `crates/contexts/platform_admin/tests/impersonation.rs`; the runtime read-only clamp has **no assembled-app test yet** (named residual below) |
| — | Tenant-scoped session reaching platform routes | Elevation | L/C | Every mounted platform route must 403 for a tenant session — `apps/api/tests/integration/idor_sweep.rs` (sweep 3) |

## TB11 — Subject & DSR surfaces (privacy)

What crosses: data-subject requests, consent/opt-out signals, and the erasure path. Threats T31–T37
carried from phase 05.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T31 | DSR disclosure to the wrong person | Information disclosure | M/C | Verification before disclosure (session or hashed OTP+token); "not found" for unowned requests; download token + passphrase verifier + TTL (7 d) + download cap (3) — `crates/contexts/privacy/tests/integration/privacy_integration.rs`; export credentials never traverse the outbox nor logs (`apps/worker/src/main.rs`) |
| T32 | Opt-out silently dropped or gated on verification | Repudiation | L/H | No verification gate on opt-out; ledger row + directive materialised in the submitting transaction; propagation sweep re-asserts downstream — `crates/contexts/privacy/tests/integration/privacy_integration.rs` |
| T33 | Crypto-shred ineffective | Tampering | L/C | Per-subject DEK wrapped by a KEK outside the DB; shred deletes the wrapped DEK; decrypt returns typed `KeyShredded`; post-erasure decrypt failure proven — `crates/contexts/privacy/tests/integration/privacy_integration.rs`, `crates/contexts/privacy/src/adapters/crypto.rs` |
| T34 | Jurisdiction misresolved toward a permissive default | Information disclosure | M/C | Unknown ⇒ most protective profile; evidence chain per resolution; every seeded profile pinned by an explicit expectation row — `crates/contexts/privacy/tests/integration/jurisdiction_matrix.rs` |
| T35 | Consent/opt-out ledger forged or edited | Tampering | L/H | Append-only ledger (no UPDATE paths); every privacy action in the hash-chained audit trail — `crates/contexts/audit/tests/audit_integration.rs` |
| T36 | GPC honoured in UI but not server-side | Information disclosure | M/H | `Sec-GPC` recorded pre-auth, bound to the subject on next authentication, binding never unbinds — `crates/contexts/privacy/src/adapters/gpc.rs` (inline tests) + `privacy_integration.rs` |
| T37 | Operator abusing processor-mode requests against other tenants' subjects | Information disclosure | L/H | Requests RLS-scoped to the tenant's own subjects; submission audited with operator id — `TB5` matrix + `privacy_integration.rs` |
| — | A context shipping personal data without joining the access/erasure paths | Information disclosure | M/H | Provider registry must exactly cover the workspace's context set; privacy state can never affect pricing/entitlements (dependency ban) — `apps/api/tests/integration/privacy_providers.rs` |

## TB12 — Telemetry, logs & secret render paths

What crosses: everything that renders a value — Debug, Display, Serialize, log emitters, traces.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T3 | Secret leakage in logs/errors/traces | Information disclosure | M/H | Log PII corpus: every PII shape the platform holds (incl. base64-wrapped tokens, hashed identifiers) emitted through the real emitter must leave zero raw occurrences — `crates/platform/telemetry/tests/pii_corpus.rs` (+ `scripts/check-log-pii.sh`, pr.yml via `just ci` gates) |
| T56 | Secret render-path leak: a plaintext field reachable through a derived `Debug`/`Serialize` (the class that shipped in eight config structs before 11.5f, including `Redacted`'s own `Serialize`) | Information disclosure | M/H | The scanner enumerates **secret-shaped fields from the source tree**; each must be `Redacted`-wrapped or carry a mechanically-verified class exemption (derives constrained); unclassified, rule-violating, or stale entries fail; self-test plants an unredacted `webhook_secret` in a real src tree and expects the named failure — `crates/platform/telemetry/tests/secret_inventory.rs` (+ `tests/secret-exemptions.txt`, `scripts/check-secret-scan.sh`, pr.yml `secret-scan` job); the four-render-paths regression test lives in `crates/platform/telemetry/src/redaction.rs` |
| T4 | Secret committed to the repository | Information disclosure | L/H | gitleaks over full history in CI; `.env.example` placeholders only — pr.yml `secrets` job; render-safety for what *is* configured is `TB12`'s gate above; rotation paths per secret in `docs/security/secrets-inventory.md` |

## TB13 — Async: outbox, worker jobs, retention

What crosses: domain events (transactional outbox → Redis Streams) and every scheduled job.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| — | Live credentials crossing the outbox (events replayed to a dev environment or read from the stream) | Information disclosure | L/H | Credentials never traverse the outbox by policy; worker mints export credentials at use time and never logs them — `apps/worker/src/main.rs`, render gate in `TB12` |
| — | At-least-once delivery double-applying an effect | Tampering | M/M | Outbox/consumer semantics on real Postgres+Redis: handlers idempotent (webhook dedupe in `TB6`, upserts elsewhere) — `crates/platform/messaging/tests/integration/outbox_integration.rs`, `crates/platform/messaging/tests/integration/consumer_integration.rs` |
| — | Retention/partition job dropping or keeping the wrong rows | Tampering | M/H | Monthly partitions pre-created and **asserted by name** in the RLS matrix; retention sweep drops/anonymises per `privacy.retention_rules` — `apps/api/tests/integration/rls_matrix.rs` (partition coverage), `apps/worker/src/main.rs` (partition maintenance job) |
| — | Audit history edited after the fact | Repudiation | L/H | Global hash chain (prev_hash → hash), UPDATE/DELETE rejected at the database level; CLI verifier `just audit-verify` — `crates/contexts/audit/tests/audit_integration.rs` |

## TB14 — Edge TLS issuance & reachability probing

What crosses: the edge proxy asking "may I serve/certificate this hostname", and the platform
probing tenant storefronts.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T47 | Forged TLS authorization (edge issued a certificate for a host the platform never verified, or for an inactive tenant) | Spoofing / Elevation | L/H | `GET /internal/tls/authorize` allows only verified/issuing/live hostnames of an **active** tenant; shared secret (`x-sanvi-internal`, constant-time) + optional CIDR allow-list, empty secret fails closed; denial reasons are machine-readable — `crates/contexts/domains/tests/domains_integration.rs`, handler in `crates/contexts/domains/src/adapters/http/handlers.rs` |
| — | SSRF via reachability probing (operator-supplied host used to scan internal network) | Information disclosure | L/M | The storefront prober refuses non-public addresses — `crates/contexts/domains/src/adapters/prober.rs` (inline tests) |
| — | Certificate private keys stored in the application database | Information disclosure | L/C | Keys live only in the edge proxy (schema comment is the contract); app stores verification state and status — `migrations/20260822000000_domains_core.sql`, issuance sweep in `crates/contexts/domains/tests/domains_integration.rs` |

## TB15 — Egress: media gateway & notifications

What crosses: asset uploads/downloads proxied to the media upstream, and outbound notification sends.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T61 | Media namespace crossover (tenant A reading/writing tenant B's assets via the upstream) | Information disclosure / Elevation | L/H | All media routes under `media.assets.manage` (tenant scope, route-matrix row); every upstream call carries `X-Namespace-ID: <tenant_id>` from the resolved context; empty service key fails closed — `apps/api/tests/integration/idor_sweep.rs` (media assets in the cross-tenant sweep), `docs/security/route-matrix.md`, `crates/contexts/media/src/adapters/hitox.rs` (inline tests) |
| T62 | Outbound notification flood (a tenant drowning the shared email reputation) | DoS / Repudiation | M/M | Sends count per tenant at the gateway port on both lanes; **no enforcement path by design** — dropping delivered mail is a policy decision the `would_block` rate informs, and no shipped config can trigger it — `crates/contexts/notifications/src/adapters/http.rs` (inline tests: counted per tenant, never blocked), `crates/platform/http/src/abuse.rs` unit tests; gateway fails closed when unconfigured |
| T63 | Product-proxy delegation forged or widened (a browser steering ecommerce-service to another tenant, or gaining permissions it does not hold) | Spoofing / Elevation | L/H | `/api/v1/tenant/ecommerce/{tail}` signs a 60 s Ed25519 delegation from the *resolved* tenant and the active membership's roles only (`ecommerce.*` permissions, plan limits); refuses tenants without `products.ecommerce.enabled` and users holding no `ecommerce.*` permission before any upstream call; browser `Authorization`, cookies, tenant headers and any client `X-Identity-Token` are dropped by an allow-list; paths are plain segments under `/v1/` — `crates/contexts/product_access/tests/integration/proxy_http.rs`, `crates/contexts/product_access/src/domain/{delegation,forwarding}.rs` (inline tests), `apps/api/tests/integration/idor_sweep.rs` (route in the foreign-host sweep) |
| T64 | Product-service internals leaking to browsers (upstream cookies/headers, or its credential rejection read as session expiry) | Information disclosure | L/M | Response headers pass an allow-list (`content-type`, `idempotency-replayed`); an upstream 401 becomes 502, unreachable or unconfigured upstreams 503 — `crates/contexts/product_access/tests/integration/proxy_http.rs` |

## TB16 — Build, CI/CD & supply chain

What crosses: code and configuration on their way to a tagged, deployed image.

| # | Threat | STRIDE | L/I | Mitigation — verified by |
|---|--------|--------|-----|--------------------------|
| T9 | Malicious/vulnerable dependency | Tampering | M/H | cargo-audit + cargo-deny (advisories/bans/licenses), trivy fs scan (CRITICAL/HIGH exit 1, version pinned post-compromise), lockfile committed, SBOM generated — pr.yml jobs `audit`, `deny`, `trivy`, `sbom` |
| T58 | Advisory acceptance rot (an "accepted" risk accepted forever) | Tampering | M/H | `deny.toml` `[advisories]` is a dated ledger — every acceptance ends `expires:YYYY-MM-DD` and an expired/missing/unparsable date fails the build — `scripts/check-audit-acceptances.sh` (+ self-test, pr.yml `audit` job); release.yml re-runs the advisory gates **on the tagged tree** |
| T10 | Tampered image deployed | Tampering | L/H | cosign keyless signing on main, digest-pinned deploys, OCI labels with git SHA, post-deploy smoke, manual rollback — `.github/workflows/main.yml` |
| T57 | Security tests that exist but no job runs | Tampering | M/H | Every workspace member with a `tests/` dir must be selected by a CI job; the gate fails on a dropped crate and its crate list is derived, not hand-written — `scripts/check-ci-test-coverage.sh` (+ self-test, pr.yml `ci-test-coverage` job) |
| — | SAST-blind spots / layered-architecture violations / id-shape drift entering main | Tampering | M/M | pr.yml `sast` (clippy security groups piercing crate-level allows + pinned semgrep), `layering` + `scripts/check-layering.sh` (+ self-test), `scripts/check-id-policy.sh` (+ self-test), `openapi` contract drift gate, `i18n` gate, GA-checklist state gate `scripts/check-ga-checklist.sh` |
| — | Observability gates skipped (unspanned use case, PII in logs, alert without runbook) | Repudiation | M/M | `just ci`/`ci-quiet` run `trace-coverage`, `log-pii-scan`, `runbook-xref`, `alert-rules-test` (each with a self-test; wired into pr.yml through the `secret-scan`/`route-matrix` pattern — TASK-020's own pr.yml wiring is that task's recorded follow-up) |

## Coverage map — every context, at least one boundary and one artifact

| Context | Boundaries | Named evidence |
|---|---|---|
| tenancy | TB2 | `crates/platform/http/tests/integration/tenant_resolution.rs` |
| identity | TB3, TB5 | `crates/contexts/identity/tests/integration/{identity_application,identity_rls}.rs` |
| access | TB4 | `crates/contexts/access/tests/integration/{role_authoring,entitlements_integration}.rs`, `apps/api/tests/integration/permission_catalog.rs` |
| audit | TB5, TB13 | `crates/contexts/audit/tests/audit_integration.rs` |
| billing | TB6 | `crates/contexts/billing/tests/billing_integration.rs` |
| payments | TB6, TB7 | `crates/contexts/payments/tests/payments_integration.rs`, `scripts/check-connect-invariants.sh` |
| advertising | TB8, TB9 | `crates/contexts/advertising/tests/integration/{advertising,tracking,budget}_integration.rs`, `scripts/check-ads-invariants.sh` |
| domains | TB2, TB14 | `crates/contexts/domains/tests/domains_integration.rs` |
| privacy | TB11 | `crates/contexts/privacy/tests/integration/{privacy_integration,jurisdiction_matrix}.rs`, `apps/api/tests/integration/privacy_providers.rs` |
| platform_admin | TB10 | `crates/contexts/platform_admin/tests/impersonation.rs` |
| theming | TB1, TB12 | preview token sign/verify inline tests `crates/contexts/theming/src/domain/preview.rs`, `apps/api/tests/integration/headers_snapshot.rs` |
| localization | TB1 | locale allow-list `crates/contexts/localization/src/config.rs` (untrusted `?locale=` ignored), `crates/platform/http/tests/integration/locale_resolution.rs`, i18n gate |
| media | TB15 | `apps/api/tests/integration/idor_sweep.rs`, `crates/contexts/media/src/adapters/hitox.rs` |
| notifications | TB15 | `crates/contexts/notifications/src/adapters/http.rs` inline tests, `crates/platform/http/src/abuse.rs` |
| product_access | TB15 | `crates/contexts/product_access/tests/integration/proxy_http.rs`, `apps/api/tests/integration/idor_sweep.rs` |
| system | TB1 | `crates/contexts/system/tests/build_probe.rs` (probe carries no tenant-scoped field) |
| platform (http, persistence, messaging, telemetry, cache) | TB1, TB5, TB12, TB13 | `crates/platform/http/tests/integration/{middleware_rejections,tenant_resolution}.rs`, `crates/platform/persistence/tests/postgres_integration.rs`, `crates/platform/telemetry/tests/{secret_inventory,pii_corpus,alert_rules}.rs` |
| apps (api, worker, cli) | TB1, TB5, TB13, TB16 | `apps/api/tests/integration/` (assembled-app suites), pr.yml job set, `scripts/check-ci-test-coverage.sh` |

The route gate (`docs/security/route-matrix.md`, 213 OpenAPI operations + eight documented runtime
endpoints at refresh time) is the per-route index into these boundaries: every route carries its
permission, tenant scope, rate class, and abuse quota, and a route that appears without them fails
CI.

## Residual risks accepted in phase 02

- Platform-role *assignments* live in Keto tuples only (`cli membership grant --platform`); a full
  `seed access` rebuild wipes them — re-run the grant. Phase 03 gives platform memberships a database
  home with audit.
- The registration webhook runs post-persist and cannot interrupt the flow (v26 hook semantics:
  interrupting registration hooks fire pre-persist, where the identity id does not exist yet). The
  login webhook is interrupting; projection drift is backstopped by the ≤ 60 s session-cache TTL, the
  login hook, and the CLI rebuild (`seed access`).
- Kratos v26's logout flow has no webhook support; session-cache invalidation on logout happens via
  `DELETE /api/v1/me/sessions/{id}` with the cache TTL as the backstop.
- Operator CLI writes (`cli membership grant`) use the CLI's environment; command-line auditing of
  those actions is manual until the platform admin console (phase 03).

## Residual risks accepted in phase 05

- Bound GPC signals influence directives but are not (yet) re-asserted into the session itself on
  every request; the directive engine re-reads the ledger per resolution, which is the enforcement
  point.
- Export artifacts are stored on local disk (encrypted) rather than object storage in the tenant's
  region; the region pinning is enforced by deployment topology, not yet by the artifact layer.
- The `#[personal_data]` attribute macro from the plan is delivered as the per-context
  `data_classes()` registry instead; the CI gate (every context registers a provider) is the
  enforcement mechanism that keeps the map from rotting.
- Jurisdiction-profile seeds are operating defaults pending counsel review; the `source_note` column
  records this until the review happens.

## Residual risks accepted in phase 09

- Sandbox-mode Stripe accounts are indistinguishable from live ones in our projection (the provider
  reports neither); environment separation relies on per-environment keys and the registry refusing
  the fake provider in production.
- The reconciliation job's staleness window is configuration, not contract: a badly tuned window
  delays (never loses) truth, since webhooks remain the fast path.

## Residual risks accepted at this refresh (phase 11, TASK-024)

- **The isolation sweep is written and run by the author of the code.** No automation removes that;
  what it buys is that every finding becomes a regression test, so the suite only grows. The external
  penetration test is the largest single unmitigated gap, parked in
  [`../release/needs-humans.md`](../release/needs-humans.md).
- The **timing half** of the no-existence-leak requirement is not automated (flaky in CI); the
  idor sweep's nonexistent-id twin (status equality) is the automatable half.
- The **runtime impersonation clamp** (`x-sanvi-impersonate` read-only enforcement in
  `crates/platform/http/src/impersonation.rs`) has no assembled-app test; creation guardrails are
  tested at use-case level (`crates/contexts/platform_admin/tests/impersonation.rs`). First phase
  that touches the middleware owes it one.
- `notifications` and `media` ship no `tests/` directory; their evidence is inline unit tests plus
  assembled-app coverage (route matrix, IDOR sweep). `scripts/check-ci-test-coverage.sh` binds only
  crates that have a `tests/` dir.
- Abuse thresholds are untuned (no traffic): counting is live on every FR-1117 surface, enforcement
  only on the tracking endpoint. Tuning is a configuration change deferred to
  `needs-humans.md`; the measured shadow posture is in `benchmarks/abuse-shadow-2026-09-30.md`.
- RUSTSEC-2026-0258 (h2 0.3 via actix-http 3.x) is accepted with expiry 2026-11-15; HTTP/2
  terminates at the edge. Tracked in `deny.toml`'s dated ledger.
- **CI has never run** — this checkout has no git remote. Every gate above has been executed locally
  (with its self-test), but runner-level workflow correctness (action pinning in practice, service
  containers, caching) is unproven until a remote exists.
- The domains registrar runs hard-validated `sandbox` mode; no production registrar adapter exists
  yet, so domain *purchase* is sandbox-scoped while verification/TLS run for real.

## PCI DSS scope confirmation (re-confirmed at this refresh)

Scope remains **SAQ-A** (card-not-present, fully outsourced): card data is captured only on
Stripe-hosted surfaces (Checkout hosted page, embedded Account Sessions); Sanvi stores no PAN, no
CVV, no sensitive authentication data anywhere (T42's architectural controls); no Sanvi system is in
the cardholder data flow. The phase-09 review (TASK-008) found no deviation. Re-confirm at every
phase that touches the payment path.

---
*Refresh history: v0 phase 02 (T1–T21), extended phase 05 (T31–T37), phase 09 (T38–T45); v1
2026-10-02 restructured per trust boundary with resolvable evidence (TASK-024 11.5g, T46–T62).*
