# Phase 02 — Identity & Access (backend)

**Target version:** 0.3.0
**Depends on:** phase 01
**Unlocks:** 03 (admin/RBAC UI), and the authorization checks every later phase calls

## Goal

Authentication and authorization live in **two separate bounded contexts** backed by Ory: `identity`
(who you are — Ory Kratos) and `access` (what you may do — Ory Keto). Google, Microsoft, and
"continue with email" all work, adding a provider is configuration rather than code, and an account
that signs in with a provider whose verified email matches an existing account is safely linked.

## Scope

**In**
- `identity` context: identity projection, sessions, memberships (user ↔ tenant ↔ role), account
  linking, invitations, MFA policy hooks.
- `access` context: permission model, role catalog, Keto relation-tuple adapter, `authorize()` used
  by every other context.
- Kratos configuration (self-service flows, schemas, OIDC providers, webhooks), Keto namespace
  configuration, and the session-verification middleware.

**Out**
- Role *authoring* UI and platform-admin-specific privileges (phase 03) — the model supports them,
  the operator seeds roles by CLI here.
- Consent records for privacy (phase 05).

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| AuthN | Ory Kratos, browser flows, session cookies (SameSite=Lax, Secure, HttpOnly) | Brief requirement; removes credential storage from our threat surface |
| Session verification | `GET /sessions/whoami` with Redis cache keyed by session token hash, TTL ≤ 60 s, invalidated on Kratos logout webhook | Cheap and correct; no JWT replay window we can't close |
| AuthZ | Ory Keto (Zanzibar relation tuples) | Brief requirement; models tenant-scoped and resource-scoped permissions uniformly |
| Providers | Kratos OIDC provider list driven by config + secrets; adding one = config entry + secret | "Scalable to add/remove providers" from the brief |
| Email login | Kratos one-time-code flow (passwordless), password optional per tenant policy | "Continue with email" without password sprawl |
| Account linking | Automatic **only** when both sides are verified for the same email; otherwise a proof-of-ownership challenge | Auto-linking on unverified email is an account-takeover primitive |
| Identity data | Kratos owns credentials & traits; we keep a local `identity.users` projection | Joins, RLS, and soft-delete need a local row; Kratos stays the source of truth for credentials |

## Domain model

**identity**

```rust
pub struct User { id: UserId, kratos_identity_id: IdentityId, primary_email: Email,
                  email_verified: bool, status: UserStatus, locale: Option<Locale> }
pub struct Membership { tenant_id: TenantId, user_id: UserId, role_ids: Vec<RoleId>,
                        status: MembershipStatus, invited_by: Option<UserId> }
pub struct LinkedAccount { user_id: UserId, provider: ProviderId, subject: String,
                           linked_at: Timestamp, linked_via: LinkMethod }
pub struct Invitation { id, tenant_id, email, role_ids, expires_at, accepted_at, token_hash }
```

Events: `UserRegistered`, `UserEmailVerified`, `AccountLinked`, `AccountLinkRejected`,
`SessionCreated`, `SessionRevoked`, `MembershipGranted`, `MembershipRevoked`, `InvitationSent`,
`InvitationAccepted`, `UserDeactivated`.

**access**

```rust
pub struct Permission(&'static str);   // "billing.subscription.update"
pub struct Role { id: RoleId, scope: RoleScope /*Platform|Tenant*/, key: String,
                  permissions: BTreeSet<Permission>, is_system: bool, tenant_id: Option<TenantId> }
pub trait Authorizer { async fn check(&self, subject: &Subject, permission: Permission,
                                      resource: &ResourceRef) -> Result<Decision, AccessError>; }
```

Permission naming: `<context>.<resource>.<action>`, declared in a single central registry so the
frontend, docs, and Keto seed all derive from one list.

## Ory configuration

**Kratos**
- Identity schema `default.schema.json`: `traits.email` (required, `credentials.password.identifier`,
  `verification.via: email`), `traits.name.{first,last}`, `traits.locale`, `metadata_public.user_id`.
- Flows enabled: registration, login, verification (code), recovery (code), settings, logout.
- Methods: `oidc` (google, microsoft), `code` (email OTP), `password` (optional), `totp`/`webauthn`
  reserved for phase 11 step-up.
- Provider config template (secrets injected, never committed):
  ```yaml
  selfservice.methods.oidc.config.providers:
    - id: google
      provider: google
      client_id: ${GOOGLE_CLIENT_ID}
      client_secret: ${GOOGLE_CLIENT_SECRET}
      scope: [openid, email, profile]
      mapper_url: file:///etc/config/kratos/oidc.google.jsonnet
      requested_claims: { id_token: { email_verified: { essential: true } } }
    - id: microsoft
      provider: microsoft
      microsoft_tenant: common
      ...
  ```
- Jsonnet mappers normalise provider claims into our trait shape and carry `email_verified` through.
- **Webhooks → our API** (`after registration`, `after login`, `after settings`, `after logout`):
  create/refresh the local `identity.users` projection inside a transaction; failure blocks the flow
  (`can_interrupt: true`) so we never end up with a Kratos identity we don't know about.

**Keto**
- Namespaces: `tenant`, `role`, `user`, plus per-resource namespaces added by later phases
  (`subscription`, `domain`, `campaign`, `theme`).
- Tuple shape: `role:{tenant}:{role_key}#member@user:{user_id}` and
  `permission:{permission_key}#granted@role:{tenant}:{role_key}#member`, with tenant containment via
  `tenant:{tenant_id}#member@user:{user_id}`.
- OPL (Ory Permission Language) file checked into `configs/keto/namespaces.ts`, applied by migration.

## Account linking algorithm

```
on OIDC callback for (provider, subject, email, email_verified):
  if linked_account(provider, subject) exists            -> sign in that user
  else if no user with email                             -> create user, link, sign in
  else                                                    -> existing user U with that email:
      if email_verified(provider) AND U.email_verified   -> link automatically,
                                                             emit AccountLinked{via: AutoVerified},
                                                             notify U by email (security notice)
      else                                                -> DO NOT link. Return a linking challenge:
                                                             user must complete an existing login
                                                             method (or email OTP) for U, then the
                                                             settings flow attaches the credential.
                                                             Emit AccountLinkRejected on abandon.
```

The challenge path is implemented as: Kratos returns the duplicate-credential error → our handler
starts a login flow with a signed `link_intent` (provider, subject, nonce, 10 min TTL, single use,
bound to the browser) → after successful login we call the Kratos settings API to link the OIDC
credential → audit + email notification. Unlinking is allowed only while ≥ 1 credential remains.

## Deliverables

### Schema

```sql
CREATE SCHEMA identity;
CREATE TABLE identity.users (
  id uuid PRIMARY KEY, kratos_identity_id uuid NOT NULL UNIQUE,
  primary_email citext NOT NULL, email_verified boolean NOT NULL DEFAULT false,
  status text NOT NULL, locale text, created_at timestamptz NOT NULL DEFAULT now(),
  deactivated_at timestamptz);
CREATE UNIQUE INDEX ON identity.users (primary_email) WHERE deactivated_at IS NULL;

CREATE TABLE identity.memberships (
  tenant_id uuid NOT NULL, user_id uuid NOT NULL REFERENCES identity.users(id),
  status text NOT NULL, invited_by uuid, created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, user_id));
CREATE TABLE identity.membership_roles (tenant_id uuid, user_id uuid, role_id uuid,
  PRIMARY KEY (tenant_id, user_id, role_id));
CREATE TABLE identity.linked_accounts (
  user_id uuid NOT NULL REFERENCES identity.users(id), provider text NOT NULL,
  subject text NOT NULL, linked_via text NOT NULL, linked_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (provider, subject));
CREATE TABLE identity.invitations (...);
CREATE TABLE identity.link_intents (nonce_hash text PRIMARY KEY, payload jsonb, expires_at timestamptz);

CREATE SCHEMA access;
CREATE TABLE access.roles (id uuid PRIMARY KEY, scope text NOT NULL, tenant_id uuid,
  key text NOT NULL, name text NOT NULL, is_system boolean NOT NULL DEFAULT false,
  UNIQUE (scope, tenant_id, key));
CREATE TABLE access.role_permissions (role_id uuid, permission text, PRIMARY KEY (role_id, permission));
```

`identity.memberships`, `identity.membership_roles`, `identity.invitations`, and tenant-scoped
`access.roles` get RLS via the phase-01 helper.

### Middleware & API

- `Authentication` middleware: extract Kratos session cookie / bearer → `whoami` (cached) →
  `Subject { user_id, session_id, aal, methods, tenant_memberships }` in extensions and span.
- `require_permission!("billing.subscription.update", resource)` extractor calling `Authorizer`;
  decisions cached ≤ 30 s and invalidated on `RoleChanged`/`MembershipRevoked` events.
- Endpoints: `GET /api/v1/me`, `GET /api/v1/me/sessions`, `DELETE /api/v1/me/sessions/{id}`,
  `POST /api/v1/auth/link/challenge`, `POST /api/v1/auth/link/complete`,
  tenant member CRUD (`/api/v1/tenant/members`, invitations accept/resend/revoke),
  `GET /api/v1/access/permissions` (registry, for the UI), Kratos webhook receivers under
  `/internal/kratos/*` (mTLS or shared-secret + IP allow-list, not public).

### Seed roles

`platform_owner`, `platform_operator`, `platform_support` (scope Platform);
`tenant_owner`, `tenant_admin`, `tenant_member`, `tenant_billing`, `tenant_viewer` (scope Tenant,
system roles, non-deletable). Seeded idempotently by `apps/cli seed roles`.

## Work breakdown

1. Kratos config, identity schema, jsonnet mappers, docker-compose wiring, Mailpit verification loop.
2. Webhook receivers + local user projection (transactional, idempotent, replay-safe).
3. Session middleware + cache + logout invalidation.
4. `access` domain: permission registry, roles, `Authorizer` port; Keto OPL + adapter + seed job.
5. Memberships and invitations (email via `notifications` context stub).
6. Account-linking challenge flow end to end, including the security-notice email.
7. Permission extractor and the "no route without a permission" CI test.
8. OpenAPI (`api/identity.yaml`, `api/access.yaml`) + problem types for auth failures.
9. Provider addition runbook (`docs/runbooks/add-oidc-provider.md`) proving the "config-only" claim.

## Testing

- Integration against real Kratos/Keto containers: register → verify → login for each method.
- Linking matrix: 8 combinations of (existing method, provider verified?, local verified?) with the
  expected outcome asserted, including the **must-not-link** cases.
- Permission tests: role grant/revoke propagates to Keto and invalidates the decision cache.
- Negative: expired/replayed `link_intent`, forged webhook, session fixation, cross-tenant membership
  escalation, invitation token reuse, IDOR on `/tenant/members/{id}`.
- Fuzz the permission-string parser; assert the registry has no orphans (permission used but not
  declared, or declared but never checked).

## Security & compliance

- Session cookies: `Secure`, `HttpOnly`, `SameSite=Lax`, `__Host-` prefix, domain-scoped per tenant
  host; CSRF handled by Kratos tokens on browser flows.
- Rate limit and lock-out on login/OTP per (ip, identifier) with exponential backoff; OTP is
  single-use and time-boxed.
- Security notifications on: new provider linked, new device login, password/OTP change, session
  revocation.
- MFA is modelled now (`aal` recorded, step-up hook available) and enforced for platform admins in
  phase 03.
- Audit every membership/role change with actor, before/after, and request id.

## Acceptance criteria

- [ ] Sign in with Google, Microsoft, and email OTP; all three land on the same user when the email
      matches and both sides are verified, and are refused (with challenge) when they do not.
- [ ] Adding a third provider requires only config + secret — demonstrated with a test provider.
- [ ] Every non-public route rejects unauthenticated requests and requires a declared permission
      (CI test enumerates the router).
- [ ] Revoking a role removes access within 30 s without a restart.
- [ ] Kratos webhook failure prevents orphan identities (verified by fault injection).

## Risks

| Risk | Mitigation |
|---|---|
| Kratos flow semantics differ between versions | Pin the Kratos image; integration tests run against the pinned version in CI; upgrade is a tracked task |
| Auto-linking abused via a provider that lies about `email_verified` | Provider allow-list with per-provider `trust_email_verified` flag, default false for anything but Google/Microsoft |
| Keto latency on hot paths | Batch checks, short-TTL decision cache, and a local role→permission fast path for coarse checks |
| Local projection drifts from Kratos | Nightly reconciliation job + `identity reconcile` CLI, drift metric alerts |
