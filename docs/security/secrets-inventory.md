# Secrets inventory

**Task:** TASK-024 (slice 11.5f) · **Scope:** every secret the backend holds,
stores, or transports · **Maintenance rule:** a new secret-bearing struct
field fails `just secret-scan` until it is wrapped in `Redacted<..>`
/ `SecretRef` or carries a reasoned exemption in
`crates/platform/telemetry/tests/secret-exemptions.txt`. This document is the
*why* behind each row; the gate is the enforcement.

Three properties are recorded per secret: **storage** (where it lives at
rest), **rotation** (how it turns over — the timed drill itself is TASK-025,
fed by this table), and **blast radius** (what a leak exposes). "Render
safety" names the mechanism that keeps the secret out of logs, traces and
serialized output, with the test that proves it.

## Platform credentials

| Secret | Storage | Rotation | Blast radius | Render safety |
| --- | --- | --- | --- | --- |
| Postgres password (`SANVI__DATABASE__PASSWORD`) | env → `DatabaseConfig.password`, used to build the pool | credential rotation drill, TASK-025 | full database read/write as `sanvi_app` | manual `Debug` + `#[serde(skip_serializing)]`; gate class `manual-redaction` |
| Redis URL (may embed auth) `configs/[base].toml → server/redis.url` | config file / env override | same drill as Postgres | cache/abuse-counter/session store access; no durable data | URL is not secret-shaped by name — reviewed here: contains credentials only when the deployment URL does; keep such forms env-only |
| OTLP auth token (`SANVI__TELEMETRY__OTLP__AUTH_TOKEN`) | env → `OtlpConfig.auth_token` | OpenObserve credential rotation | write access to the log/trace pipeline (poison telemetry, not data) | manual `Debug` + `skip_serializing`; PII corpus test exercises the emitter |

## Trust boundaries the platform terminates

| Secret | Storage | Rotation | Blast radius | Render safety |
| --- | --- | --- | --- | --- |
| Kratos webhook shared secret (`SANVI__IDENTITY__WEBHOOK_SECRET`) | env → `IdentityConfig.webhook_secret` → `WebhookState.secret` (Clone-only runtime holder) | re-issue in Kratos + env swap | forge logout/registration webhooks → session invalidation floods, signup abuse counting bypass | `Redacted<String>` config field; runtime holder derives nothing |
| Stripe platform key + webhook secret (`SANVI__BILLING__*`) | env → `BillingConfig` (both `Redacted<String>`) | Stripe dashboard re-issue; boot validation rejects cross-endpoint reuse | platform refunds/coupons via API key; forged billing webhooks via secret | derived Debug/Serialize render `<redacted>`; `secrets_never_render_through_debug_or_serde` unit test |
| Stripe Connect key + webhook secret (`SANVI__PAYMENTS__*`) | env → `PaymentsConfig` (both `Redacted<String>`); boot check forces them to differ from billing's | restricted-key rotation in Stripe | tenant Connect operations; forged Connect webhooks | same as billing; cross-secret distinctness validated at boot |
| Connect onboarding `client_secret` | minted by Stripe, held ephemerally in `ProviderAccountSession` / delivered once by `ConnectionSessionView` | single-use by construction | completes one onboarding session for one tenant | domain type: manual redacting `Debug`, no `Serialize`; DTO: `capability-delivery` class (no `Debug`) |
| Domains internal secret (`SANVI__DOMAINS__*`) | env → `DomainsConfig.internal_secret` (`Redacted<String>`) | env swap | unauthorized internal domain-job calls | `Redacted` field on a derived-Debug struct |
| Tenancy header-signing keys (`SANVI__TENANCY__SIGNING_KEYS__K1..`) | env → `TenancyConfig.signing_keys: Redacted<BTreeMap<..>>`; `active_key_id` picks the minting key; verification accepts all | add K2 → swap active → drop K1 (overlap window built in) | forge internal tenant-resolution headers (internal surface only) | wrapped whole-map; validation iterates via `expose_secret()` |
| Theming preview token secret (`SANVI__THEMING__PREVIEW_TOKEN_SECRET`) | env → `ThemingConfig.preview_token_secret` (`Redacted<String>`) | env swap (invalidates outstanding preview links — acceptable, TTL-bounded) | mint preview links for any tenant's drafts | wrapped field; handler decodes via `expose_secret()` |

## Encryption keys (the secrets that protect other secrets)

| Secret | Storage | Rotation | Blast radius | Render safety |
| --- | --- | --- | --- | --- |
| Privacy KEK (`SANVI__PRIVACY__KEK`, base64 32 B) | env → `PrivacyConfig.kek` (`Redacted<String>`); plaintext bytes only inside `SqlxEnvelopeCrypto.kek: [u8; 32]` (derive-less) | KEK rotation = wrap existing DEKs under new KEK id (`kek_id` is recorded beside every wrapped DEK); persisted-DEK local rehearsal complete in TASK-025; deployment-wide migration remains an operator procedure in `docs/runbooks/rotation/envelope-keys.md` | decrypt every subject's envelope-encrypted DSR data; **cannot** reach ad-account tokens (separate envelope, TASK-011) | wrapped config field; runtime carrier exempt with no-derives rule |
| Per-subject DEKs | DB `privacy.subject_keys.wrapped_dek` (ciphertext of the KEK) | destroyed on erasure (crypto-shredding is the DEK row's deletion) | single subject's data | `ciphertext` class: never plaintext anywhere |
| Advertising token KEK (`SANVI__ADVERTISING__TOKEN_KEK`) | env → `AdvertisingConfig.token_kek` (`Redacted<String>`); plaintext bytes only in `SqlxTenantEnvelopeCrypto.kek` (derive-less) | rotation adds a KEK id beside wrapped tenant DEKs (same shape as privacy) | decrypt every tenant's stored ad-platform refresh tokens | wrapped config field |
| Per-tenant ad DEKs | DB `advertising.tenant_secret_keys.wrapped_dek` | platform schedule (never a DSR — a subject's erasure must not reach a live ad credential, TASK-011 design) | one tenant's ad tokens | `ciphertext` class |
| Ad-platform OAuth refresh tokens | DB `advertising.connection_tokens` (AES-256-GCM under the tenant DEK); domain code sees only `SecretRef` (the connection id) | token refresh cycle (`RunTokenRefresh`); revocation transitions state machine | one tenant's ad account operations | `SecretRef` renders `[REDACTED]` in Debug/Display/Serialize — unit-tested in `domain/secrets.rs` |
| Export passphrase (privacy DSR artifacts) | never stored: PBKDF2-derived key wraps the export; DB keeps only `passphrase_verifier` | per-artifact, delivered out of band | one export download | `ciphertext` class for the verifier; passphrase DTOs are `capability-input` (no Serialize) |

## Third-party service credentials

| Secret | Storage | Rotation | Blast radius | Render safety |
| --- | --- | --- | --- | --- |
| Google Ads developer token + OAuth client secret (`SANVI__ADVERTISING__GOOGLE_ADS_*`) | env → `AdvertisingConfig` (`Redacted<String>` each) | Google Cloud re-issue | operate every tenant's Google Ads connection (developer token is org-wide) | wrapped fields; adapter structs are derive-less runtime carriers |
| Meta client secret (`SANVI__ADVERTISING__META_CLIENT_SECRET`) | env → `AdvertisingConfig` (`Redacted<String>`) | Meta app re-issue | operate every tenant's Meta connection | same |
| OAuth state / tracking-site signing secrets (`SANVI__ADVERTISING__*_SIGNING_SECRET`) | env → `AdvertisingConfig` (`Redacted<String>` each) | env swap | forge OAuth state CSRF tokens / tracking-site keys | same |
| hitox media service API key (`SANVI__MEDIA__SERVICE_API_KEY`) | env → `MediaConfig.service_api_key` | re-issue in the media service | media upload/read for the platform | manual redacting Debug **and** Serialize (`manual-redaction` class) |
| Notification service send/read keys (`SANVI__NOTIFICATION__*`) | env → `NotificationConfig` (struct derives neither Debug nor Serialize) | re-issue in the notification service | send/read tenant email | `config-carrier` class (no-derives rule) |
| Kratos admin/session surface | operated by Kratos itself; the backend holds only the session token presented per request (never a field) and the webhook shared secret above | Kratos-side | — | session tokens are request-scoped `&str` parameters, invisible to the field scan by design |

## What the backend deliberately does **not** hold

- **TLS certificate private keys** — terminated at the edge proxy; the domains
  context's own documentation states the app never holds them
  (`crates/contexts/domains/src/domain/certificate.rs`).
- **Card data** — Stripe-hosted; the platform stores no PANs (the PII corpus
  carries a specimen only to prove the emitter would catch one).
- **User passwords** — Kratos owns credential storage; the backend sees
  Kratos-issued sessions.

## Findings fixed by this slice (regression tests in place)

1. **`Redacted<T>` serialized its plaintext.** `Serialize` forwarded the
   inner value while `Debug`/`Display` redacted, so any wrapped secret inside
   a serializable struct (response DTO, persisted document, config dump)
   would have leaked through the one render path nobody tests casually.
   `Serialize` now emits `<redacted>`; `all_four_render_paths_redact` in
   `crates/platform/telemetry/src/redaction.rs` fails against the vulnerable
   commit.
2. **Every secret-bearing config struct derived `Debug` and/or `Serialize`
   with plaintext fields** — billing, payments, privacy, identity, domains,
   theming, advertising, tenancy (`SigningKey.secret`, the internal-header
   HMAC key, had `Debug + Serialize + PartialEq` on the secret itself). All
   are now `Redacted<String>` fields: derived `Debug` and `Serialize` render
   `<redacted>` with no per-struct manual impls to keep in sync. The gate's
   `config-carrier` rule is the regression test — a reverted wrap fails
   `just secret-scan`.
3. **`ExchangedAuthorization` derived `Serialize` on a struct carrying a raw
   ad refresh token.** The derive was unused; removed (compiler-enforced).
4. **`KratosIdentity`/`WebhookIdentity` derived `Debug` over Kratos credential
   material.** `Credentials` now has a manual redacting `Debug`;
   `credentials_never_render` proves it directly and nested.
5. **`ProviderAccountSession` derived `Debug` over the Connect
   `client_secret`.** Manual redacting `Debug`; `client_secret_never_renders`.
6. **Capability-token DTOs derived `Debug`** (DSR verification tokens, export
   download token/passphrase, preview tokens, invitation token) — dropped
   where unused, so no error path can format them.

## Relationship to the other gates

- The **log emitter** side (values that *are* emitted) is TASK-020's
  `log-pii-scan` corpus; this inventory covers the type-level guarantee that
  nothing renders even by accident.
- **Rotation drills** for every row above are TASK-025's scope; this table is
  the input that scopes them (including "every old credential asserted
  rejected afterwards").
  TASK-025's 2026-10-09 user-approved scope completes the local rehearsal:
  PostgreSQL, Redis, tenant/tracking/OAuth/preview signing, billing/Connect
  webhook verification and privacy/advertising KEK rewrap. Secret-free timings
  and rejection evidence: `benchmarks/dr/rotation-local.json`. Live provider
  revocation, session invalidation and deployment-wide rollouts remain parked;
  the exhaustive credential procedures are in `docs/runbooks/rotation/`.
- The **route/permission matrix** (`just route-matrix`) covers which routes
  accept these secrets; the IDOR sweep proves foreign tenants cannot reach
  them.
