# freee OAuth adapter and provider verification

Delivery step "Provider authorization and credentials" from
`2026-10-26-accounting-connections.md`. Backend library only: no HTTP routes,
OpenAPI, or composition into `apps/api` (that is the next step, "Authenticated
API and admin screens").

## Provider facts this design depends on

From freee's developer start guide and its published OpenAPI schema
(`freee/freee-api-schema`, `v2020_06_15`):

- Authorize: `https://accounts.secure.freee.co.jp/public_api/authorize`
  (`response_type=code`, `client_id`, `redirect_uri`, `state`,
  `prompt=select_company`). With `select_company` the grant covers exactly one
  company and the token response carries `company_id`.
- Token: `https://accounts.secure.freee.co.jp/public_api/token`,
  `authorization_code` and `refresh_token` grants. Access tokens last 21600 s.
- **Refresh tokens are single-use** and expire 90 days after issue. Every
  refresh returns a new refresh token that must replace the old one.
- Revoke: `https://accounts.secure.freee.co.jp/public_api/revoke` with
  `token=`. Only confirmed from community sample code, so the call is
  best-effort and local revocation is authoritative.
- `GET https://api.freee.co.jp/api/1/companies/{id}` → `company.role`.
- `GET /api/1/account_items?company_id=` → `account_items[].{id, available}`.
- `GET /api/1/taxes/companies/{company_id}?available=true` → `taxes[].code`.

## Decisions

1. **Shared crypto primitives move to `sanvi-crypto`** (`crates/platform/crypto`):
   AES-256-GCM `seal`/`open` (`nonce || ciphertext`) and HMAC-SHA256 signed
   state (`v1.<payload>.<sig>`). Advertising already carried both, and privacy
   carries the AES half. Advertising's private copies are replaced by calls to
   the shared crate with unchanged byte formats. Privacy is left for a separate
   change. Tables and key rows stay per context (one schema per context).
2. **OAuth state** is bound to tenant, initiating user, book, connection and
   provider, lasts 10 minutes, and is single-use. It is redeemed before the
   code exchange.
3. **Company binding.** The connection's `organization_id` is registered first.
   The callback's `company_id` must equal it, or the grant is discarded and
   nothing is stored.
4. **Credentials** are one row per connection in
   `accounting.provider_credentials`: per-tenant-DEK-encrypted refresh and
   access tokens, access expiry, refresh-token issue time, and a status of
   `active`, `reauthorization_required` or `revoked`. Revoked rows keep no
   ciphertext.
5. **Refresh concurrency.** A refresh locks the credential row
   (`SELECT … FOR UPDATE`), reuses a cached access token that is still valid
   for more than 60 s, and otherwise refreshes while holding the lock, storing
   the rotated pair in the same transaction. The cached access token lives in
   the row, so every process shares it. `invalid_grant` moves the credential
   to `reauthorization_required`. A timeout leaves the row unchanged, and the
   next attempt finds out from freee whether the old token was burnt.
6. **Verification** (`FreeeVerifier: ProviderVerifier`) requires:
   - an active credential;
   - a company role of `admin` or `simple_accounting`;
   - mapped revenue, refund and clearing accounts that exist and are
     `available`;
   - every mapped tax code present among the company's available codes.

   Non-freee connections still fail closed.
7. **Outbox events** `accounting.connection_authorized`,
   `accounting.credential_revoked` and
   `accounting.reauthorization_required` share the credential transaction.
   They carry book and connection IDs only, never tokens or user IDs.

## Out of scope

HTTP callback/start routes, permission catalog, feature flag, admin UI,
refresh sweep job, privacy-adapter changes for credentials, Money Forward/Xero.
