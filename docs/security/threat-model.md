# Threat model v0 (STRIDE)

Scope: the backend platform as delivered by phase 02 (workspace, API,
worker, CLI, local stack, CI/CD). Refreshed at the end of every phase.

| # | Threat | STRIDE | Likelihood | Impact | Mitigation (phase 00) |
|---|--------|--------|------------|--------|------------------------|
| T1 | Cross-tenant data access via a missing `WHERE` predicate | Information disclosure | Medium | Critical | RLS architecture decided (ADR 0002); app role `NOBYPASSRLS` from day one; per-table isolation tests land with phase 01 |
| T2 | SQL injection through unvalidated input | Tampering | Low | Critical | SQLx bound parameters everywhere; no string-built SQL in the codebase (lint-enforced in later phases) |
| T3 | Secret leakage in logs/errors/traces | Information disclosure | Medium | High | `Redacted<T>` wrapper; JSON logs; error bodies are problem+json with no internals; gitleaks in pre-commit + CI |
| T4 | Secret committed to the repository | Information disclosure | Low | High | gitleaks pre-commit + CI; `.gitignore`; `.env.example` placeholders only; deny.toml |
| T5 | Unbounded requests exhausting the API | DoS | High | Medium | Concurrency limit, per-request timeout, body size caps, per-IP Redis token bucket with burst/refill |
| T6 | Panic leaks stack traces to clients | Information disclosure | Low | Medium | Panic-catch middleware → generic 500 problem+json; span logs the panic server-side only |
| T7 | Clickjacking / MIME sniffing / referrer leakage on API responses | Various | Low | Low | Security headers on every response (X-Frame-Options DENY, nosniff, referrer-policy, CSP, permissions-policy, HSTS off-localhost) |
| T8 | Cross-origin abuse (stolen credentials, CSRF) | Spoofing | Medium | Medium | CORS explicit allow-list, credentials, no wildcards — rejected at startup if misconfigured |
| T9 | Malicious/injected dependency in the supply chain | Tampering | Medium | High | cargo audit + cargo deny (advisories/bans/licenses) in CI; lockfile committed; SBOM (CycloneDX) published; container image scanned (trivy) and signed (cosign keyless) |
| T10 | Tampered container image deployed to staging | Tampering | Low | High | cosign keyless signing on main; digest-pinned deploys; OCI labels with git SHA |
| T11 | Migrator/API role privilege escalation | Elevation of privilege | Low | High | Least-privilege role split; app role cannot create tables or bypass RLS; migrations run as a separate pre-deploy job |
| T12 | Forged request correlation / request smuggling | Spoofing | Low | Medium | Request-id accepted only as a UUIDv7; actix normalizes paths; body/head limits |
| T13 | Rate limiter outage opens the floodgates (or closes the API) | DoS | Low | Medium | Fail-open/fail-closed per environment (`rate_limit.fail_open`); production fails closed |
| T14 | Unauthenticated access to privileged endpoints | Spoofing | Low | Critical | Phase 02: Kratos session verification on every non-public route (401 via extractors) + the permission-catalog CI test (no route without a declared permission) |
| T15 | Account takeover via auto-linking an unverified provider email | Spoofing | Low | Critical | Auto-link requires the provider on `trusted_email_verified_providers` AND both sides verified; everything else gets the proof-of-ownership challenge (single-use, 10-minute, session-bound nonce); `AccountLinkRejected` audit events |
| T16 | Forged Kratos webhook creating/merging projections | Spoofing | Low | High | `/internal/kratos/*` is not public: constant-time shared-secret header + client-IP allow-list; handlers parse defensively; a colliding registration email never binds |
| T17 | Session replay after logout/revocation | Spoofing | Low | High | Session cache TTL ≤ 60 s; revocation invalidates by session id; revoke via the API also revokes in Kratos |
| T18 | Cross-tenant membership escalation | Elevation of privilege | Low | Critical | RLS FORCE on `identity.memberships*`, invitations; scoped transactions by construction; RLS matrix test in CI |
| T19 | Invitation token abuse (replay/guess) | Elevation of privilege | Low | High | 256-bit random tokens, SHA-256 at rest, single use, expiry, email must match the session; acceptance idempotent |
| T20 | Authorization cache serves stale grants after revocation | Elevation of privilege | Low | High | Decision cache TTL ≤ 30 s; worker deletes Keto tuples on `MembershipRevoked` within seconds; acceptance criterion tested |
| T21 | Keto outage grants/denies incorrectly | DoS / Elevation | Low | High | Fail closed: the gate maps backend errors to 503, never to Allowed; decision cache never caches errors |

## Residual risks accepted in phase 02

- Platform-role *assignments* live in Keto tuples only (`cli membership
  grant --platform`); a full `seed access` rebuild wipes them — re-run the
  grant. Phase 03 gives platform memberships a database home with audit.
- The registration webhook runs post-persist and cannot interrupt the flow
  (v26 hook semantics: interrupting registration hooks fire pre-persist,
  where the identity id does not exist yet). The login webhook is
  interrupting; projection drift is backstopped by the ≤ 60 s session-cache
  TTL, the login hook, and the CLI rebuild (`seed access`).
- Kratos v26's logout flow has no webhook support; session-cache
  invalidation on logout happens via `DELETE /api/v1/me/sessions/{id}` with
  the cache TTL as the backstop.
- Operator CLI writes (`cli membership grant`) use the CLI's environment;
  command-line auditing of those actions is manual until the platform admin
  console (phase 03).

## Residual risks accepted in phase 02

- Platform-role *assignments* live in Keto tuples only (`cli membership
  grant --platform`); a full `seed access` rebuild wipes them — re-run the
  grant. Phase 03 gives platform memberships a database home with audit.
- The registration webhook runs post-persist and cannot interrupt the flow
  (v26 hook semantics: interrupting registration hooks fire pre-persist,
  where the identity id does not exist yet). The login webhook is
  interrupting; projection drift is backstopped by the ≤ 60 s session-cache
  TTL, the login hook, and the CLI rebuild (`seed access`).
- Kratos v26's logout flow has no webhook support; session-cache
  invalidation on logout happens via `DELETE /api/v1/me/sessions/{id}` with
  the cache TTL as the backstop.
- Operator CLI writes (`cli membership grant`) use the CLI's environment;
  command-line auditing of those actions is manual until the platform admin
  console (phase 03).

## Phase 05: privacy & compliance

Threats introduced by the privacy context, with their mitigations:

| # | Threat | STRIDE | Likelihood | Impact | Mitigation (phase 05) |
|---|--------|--------|-----------|--------|-----------------------|
| T31 | DSR disclosure to the wrong person (export/status exfiltration) | Information disclosure | Medium | Critical | Verification before disclosure (session auth or hashed OTP + token); "not found" for unowned requests (no existence oracle); token/passphrase stored hashed (HMAC-peppered); download TTL + download cap |
| T32 | An opt-out is silently dropped or gated on verification | Repudiation | Low | High | Opt-out path has no verification gate; ledger row + directive materialisation in the submitting transaction; propagation sweep re-asserts to downstream processors |
| T33 | Crypto-shred is ineffective (key survives, ciphertext recoverable) | Tampering | Low | Critical | Per-subject DEK wrapped by a KEK outside the DB; shred deletes the wrapped DEK; decrypt returns the typed `KeyShredded` error; integration test proves post-erasure decrypt fails |
| T34 | A jurisdiction misresolved toward a permissive default | Information disclosure | Medium | Critical | Evidence chain stored per resolution; unknown ⇒ most protective profile; matrix test pins every profile's defaults |
| T35 | Consent/opt-out ledger forged or edited | Tampering | Low | High | Append-only ledger (no UPDATE paths); every privacy action lands in the phase-03 hash-chained audit trail |
| T36 | GPC honoured in UI but not enforced server-side | Information disclosure | Medium | High | `Sec-GPC` recorded pre-auth in the backend directive engine and bound to the subject on the next authenticated request; integration test proves the denial survives binding |
| T37 | Tenant operator abuses processor-mode requests against other tenants' subjects | Information disclosure | Low | High | Requests scoped by RLS to the tenant's own end-user subjects; submission audited with the operator id |

## Residual risks accepted in phase 05

- Bound GPC signals influence directives but are not (yet) re-asserted into
  the session itself on every request; the directive engine re-reads the
  ledger per resolution, which is the enforcement point.
- Export artifacts are stored on local disk (encrypted) rather than object
  storage in the tenant's region; the region pinning is enforced by
  deployment topology, not yet by the artifact layer.
- The `#[personal_data]` attribute macro from the plan is delivered as the
  per-context `data_classes()` registry instead; the CI gate (every context
  registers a provider) is the enforcement mechanism that keeps the map
  from rotting.
- Jurisdiction-profile seeds are operating defaults pending counsel review;
  the `source_note` column records this until the review happens.
