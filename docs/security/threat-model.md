# Threat model v0 (STRIDE)

Scope: the backend platform as delivered by phase 00 (workspace, API,
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
| T14 | Unauthenticated access to privileged endpoints (future) | Spoofing | — | Critical | Phase 02 (Ory Kratos sessions); the "no route without a permission check" test ships with phase 03 |

## Residual risks accepted in phase 00

- Ory containers run in the local stack but no authentication exists yet —
  the API is dev-only; production deploy is gated on phase 02.
- `Migrations run as a pre-deploy job` uses staging credentials from GitHub
  secrets; secret rotation is manual until the secret manager integration
  lands.
