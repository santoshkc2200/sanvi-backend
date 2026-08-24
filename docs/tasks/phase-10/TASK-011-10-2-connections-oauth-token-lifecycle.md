# TASK-011: 10.2 Connections, OAuth & token lifecycle

**Phase:** 10
**Status:** in-progress
**Requirement(s):** FR-1002, FR-1003, NFR-1003
**Depends on:** TASK-010
**Created:** 2026-08-20

**Slice:** 10.2 — Connections, OAuth & token lifecycle
**Prerelease:** `v0.11.0-alpha.3` · **Flags:** `advertising.google_ads`, `advertising.meta`

## Context

A tenant connects a Google Ads account and a Meta ad account, picks the right account when they have
several, and the connection stays healthy without anyone thinking about it. Tokens are encrypted,
never logged, refreshed before expiry, and a revocation degrades to a clear "reconnect" prompt rather
than a cascade of failing jobs.

Ad account access is money access. Treat this as a credential slice, not a settings slice.

**The two rules:**

1. **The `state` parameter is a signed, single-use, tenant-bound nonce.** Every OAuth surface, both
   platforms, no exceptions. Without it the callback is a CSRF endpoint that grants an attacker's ad
   account to a tenant, or a tenant's ad account to an attacker's session.
2. **Revocation pauses, it does not thrash.** A dead token sets `status = Disconnected`, notifies the
   tenant once, and *pauses* dependent jobs. It does not retry a broken credential every five minutes
   until the platform rate-limits the whole tenant.

## What to do

- [ ] **Contract** — `POST /connections/{platform}/oauth/start` (`advertising.connect` + step-up),
      `GET /connections/{platform}/oauth/callback` (signed `state` only),
      `GET /connections/{platform}/accounts`, `POST /connections` (selects the account),
      `GET /connections` (`advertising.read`), `DELETE /connections/{id}` (`advertising.connect` +
      step-up). `start` returns the authorization URL and an opaque state handle — the frontend never
      constructs an OAuth URL and never sees a token. `GET /connections` carries **server-computed**
      health: `can_sync`, `can_upload_conversions`, `token_expires_at`, `scopes_missing`, `last_error`,
      `last_synced_at`.
- [ ] **Google Ads OAuth** — offline access, developer token, `login-customer-id` for manager
      accounts, account listing after consent. A manager account resolves to many customer ids; the
      picker is not optional for real advertisers.
- [ ] **Meta OAuth** — Facebook Login for Business, system-user token where the tenant's business
      permits it, Business and ad-account selection, and permission checks against what was **granted**
      rather than what was requested.
- [ ] **Signed state** — HMAC over `(tenant_id, platform, nonce, expiry)`, stored and burned on
      redemption. A forged, expired, replayed, or cross-tenant state is rejected and audited.
- [ ] **Token storage** — refresh tokens through TASK-009's envelope scheme with the per-tenant DEK.
      Access tokens are cached in memory only. Nothing token-shaped is persisted outside the envelope
      and nothing token-shaped reaches a log line or a trace attribute.
- [ ] **Refresh scheduler** — renews before expiry with jitter, per connection. On permanent failure
      (revoked, app uninstalled, password change): `status = Disconnected`, `AdAccountDisconnected`
      emitted, tenant notified once, dependent jobs paused.
- [ ] **Scope drift** — on each sync, compare granted scopes against required. A missing scope surfaces
      as a re-consent state naming what is missing and what stopped working, not a generic error.
- [ ] **Connection uniqueness** — `UNIQUE (tenant_id, platform, external_account_id)` from TASK-009. A
      reconnect of the same account reuses the row and keeps campaign history; connecting a *different*
      account is a new row that inherits nothing.
- [ ] **Step-up auth & audit** — connect and disconnect require fresh MFA and write an audit entry with
      the actor, the account, and the scopes granted.
- [ ] **Adapter account listing** — `list_accounts` on both real adapters. These are the first live
      calls in the phase; everything else still runs against the fake.

## Acceptance criteria

- [ ] A tenant connects Google Ads and Meta end to end in test/sandbox accounts and both show healthy
      with a last-sync time.
- [ ] A callback with a forged, missing, expired, replayed, or cross-tenant `state` is rejected and
      audited; a state minted for tenant A cannot be redeemed in tenant B's session.
- [ ] A tracing capture across start → callback → refresh contains no token substring, and audit
      entries exist for every decrypt-path access (CI-asserted).
- [ ] A token near expiry is renewed; a revoked token sets `Disconnected`, notifies once, and pauses
      dependent jobs — asserted by **counting retry attempts**, not by absence of an error.
- [ ] A granted-scope subset produces a re-consent state naming the missing scope.
- [ ] A Google manager account resolves to multiple customer ids and all are listed.
- [ ] Reconnecting the same external account reuses the row; a different account creates a new one and
      inherits no campaigns.
- [ ] Connect and disconnect are both refused without fresh MFA.

## Verification

```bash
just fmt
just clippy
just layering
just ads-invariants
cargo test -p sanvi-advertising
just test
just openapi
```

## Out of scope

Campaign CRUD and the Google campaign-tree adapter (TASK-012), Meta campaign tree (TASK-013),
conversion capture (TASK-014), metrics ingestion (TASK-016).

## Files likely touched

- `api/advertising.yaml` (connection endpoints + health fields)
- `crates/contexts/advertising/src/application/connections/**`
- `crates/contexts/advertising/src/adapters/{http,repos,google_ads,meta}.rs`
- `apps/worker/**` (refresh scheduler)
- `configs/` (OAuth client credentials, developer token), audit + notifications wiring

## Notes / gotchas

- Rollback is per-platform flag off → the platform card shows unavailable and OAuth start returns
  `503 advertising/platform-unavailable`. **Leave existing connections, their tokens, and the refresh
  scheduler alone** — disabling refresh to "fully roll back" strands every token past its expiry and
  forces every tenant to re-consent when the flag comes back on.
- Store what the account picker needs at selection time: currency and timezone decide what "today"
  means in every later dashboard number.
- Meta's granted-vs-requested permission gap is normal, not an error path to log and forget.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-011 done --note "<PR or commit>"`
