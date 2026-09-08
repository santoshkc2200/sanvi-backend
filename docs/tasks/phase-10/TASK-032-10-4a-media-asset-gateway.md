# TASK-032: 10.4a Media asset gateway

**Phase:** 10
**Status:** done
**Requirement(s):** FR-1007, NFR-1003
**Depends on:** TASK-013
**Created:** 2026-09-07

**Slice:** 10.4a — Media asset gateway (addendum to 10.4, same shape as TASK-031's "-a" slice in
phase 11)
**Ships behind:** none (no tenant-visible behavior change until the frontend has something that
was 401ing before; this task doesn't touch `advertising.meta`)

## Context

TASK-013's own execution notes recorded a deferred finding: `hitox-media-service` authenticates
every `/v1/assets` route with `Bearer <service API key>` only, and the admin SPA's session is a
Kratos cookie — it has nothing valid to send. `course-media`'s `courseApiRequest` already ships a
same-site-gateway shape (`credentials: 'include'`, no `Authorization` header sent), on the
assumption that *something* same-site exchanges the cookie for the service key. That something has
never existed — `sanvi-backend` has no proxy or token-minting endpoint for media, and grepping the
codebase for `hitox`/media-service integration returns nothing. The creative uploader and the
saved-creative preview image (`POST /v1/assets/{id}/delivery`) both silently depend on it.

This task builds that gateway. It is not Meta-specific and not advertising-specific — the contract
it fronts (`course-media`'s `api/assets.ts`) is already written generically for reuse, and
`hitox-media-service` deliberately owns no consumer concepts — but advertising creatives are the
only real caller today, which is why this rides phase 10 as an addendum to 10.4 rather than opening
its own phase.

## What to do

- [x] **New context** — `crates/contexts/media` (`sanvi-media` crate), mirroring the shape of
      `advertising`'s outbound adapters (`adapters/hitox.rs` alongside `google.rs`/`meta.rs`'s
      pattern): a `MediaGatewayPort` trait, a `HitoxMediaAdapter` `reqwest` implementation, thin
      application use cases (one per route below), and an `adapters/http` router.
- [x] **Routes** — same-origin, session-authenticated, mounted so a deployment can point
      `VITE_MEDIA_ORIGIN` at `sanvi-backend` itself and get the same-site shape `course-media`
      already assumes, with no frontend change:
      - `POST /v1/assets` (mint upload; `Idempotency-Key` required, passed through)
      - `POST /v1/assets/{id}/parts` (presign parts)
      - `POST /v1/assets/{id}/complete` (commit upload)
      - `GET /v1/assets/{id}` (asset state)
      - `DELETE /v1/assets/{id}` (abort)
      - `POST /v1/assets/{id}/delivery` (mint delivery URL — this is what unblocks the saved-creative
        preview image; wiring the preview's `<img src>` to it is `sanvi-frontend` work, out of scope
        here)
      Captions/chapters routes are not proxied — nothing calls them today.
- [x] **Tenant → namespace mapping** — the existing tenant-resolution middleware (used by every other
      context) puts `TenantContext` in request extensions before the handler runs; the adapter sends
      `X-Namespace-ID: <tenant_id>`, 1:1, no separate registration step. `hitox-media-service` treats
      the namespace as an opaque caller-supplied value by design, so this needs nothing on its side.
- [x] **Config** — `MediaConfig { base_url: String, service_api_key: String }` in
      `apps/api/src/config.rs`, same shape as `AdvertisingConfig`'s static secrets (plain `String`
      from env, not envelope-encrypted — this is our secret, not a tenant's). Never logged, traced,
      or echoed in an error body (NFR-1003's redaction rule applies here too even though it's outside
      the `advertising` context).
- [x] **Error mapping, not passthrough** — CONTRIBUTING.md rule #2: `sanvi_http::ApiError` is the only error body this API produces. Map hitox-media-service's `{status, code, detail}` onto the matching `ApiError` variant by status (404→`NotFound`, 409/`conflict`|`concurrent_modification`|`invalid_state`→`Conflict`, 400→`BadRequest`, anything else including unreachable/timed-out upstream→`ServiceUnavailable`) AND thread the media-service's own `code` string into `ApiError::problem().extensions` (the `campaign_validation_problem` pattern), so the body still carries top-level `code`; `detail` carries the message through unchanged.

## Acceptance criteria

- [x] Each of the 6 routes, called with a valid tenant session, reaches `hitox-media-service` with
      `Authorization: Bearer <service_api_key>` and `X-Namespace-ID: <tenant_id>` set, and relays a
      successful response body/status unchanged. (Verified against mock HTTP servers in
      `adapters/hitox.rs` tests plus handler tests — not against a live media-service; see the
      unchecked live criterion below.)
- [x] A request without a valid tenant session gets the same `401` shape every other session-gated
      route already returns — never a surface of media-service's own "valid service API key
      required" body, which would leak that a shared secret is involved. (Inherited: the routes
      mount inside the shared `/api/v1/tenant` scope behind `sanvi_http::TenantMiddleware` and use
      the shared `CurrentTenant`/`RequiredPermission` extractors; no dedicated test by design — see
      Notes/gotchas.)
- [x] `Idempotency-Key` on `POST /v1/assets` reaches `hitox-media-service` unchanged; a request
      missing it gets media-service's rejection surfaced as the mapped `400` (`ApiError::BadRequest`
      carrying the upstream `code`) rather than a verbatim upstream body (the header is optional at
      this gateway's own layer — media-service is the one that requires it).
- [x] A `hitox-media-service` error response surfaces as the matching `ApiError` status
      (404→`NotFound`, 409→`Conflict`, 400→`BadRequest`, anything else including
      unreachable/timed-out upstream→`ServiceUnavailable`) with the media-service's own `code` in
      the body's top-level `code` extension member and `detail` as the message — never a verbatim
      upstream body (CONTRIBUTING.md rule #2).
- [x] Client-supplied `X-Tenant-ID`/`X-Namespace-ID` headers have zero effect: the tenant always
      comes from the session (`CurrentTenant` — no handler reads a tenant header) and the gateway
      always sets its own `X-Namespace-ID` from that tenant id (`create_ignores_a_spoofed_namespace_header_and_uses_the_session_tenant`).
- [x] `service_api_key` never appears in a log line, trace attribute, or error response body —
      `HitoxMediaAdapter`'s `Debug` impl redacts it by hand, the same pattern `MetaAdsAdapter` already
      uses for `client_secret` (there is no repo-wide grep gate for this; each adapter owns its own
      redaction).
- [x] `just layering` stays green — the new context depends only on `kernel`/`platform`, same as
      every other context.
- [ ] Live round trip against a real `hitox-media-service`: `POST` a real `create_media_asset`
      through `apps/api` with a valid tenant session and confirm a `201` with a real `asset_id`.
      NOT run — no local media-service instance was reachable in this environment (see
      Notes/gotchas). The wire contract it would exercise is covered by the mock-server adapter
      tests and handler tests above.

## Verification

```bash
cargo c
just fmt
just clippy
just layering
cargo nextest run -p sanvi-media
just openapi
```

## Out of scope

- Wiring the saved-creative preview's `<img src>` to `POST .../delivery`'s response — that's
  `sanvi-frontend`, tracked against the finding already recorded in `sanvi-frontend`'s TASK-013 doc.
- Any change to `hitox-media-service` (auth, CORS, or otherwise) — this task's whole point is that
  none is needed.
- A `courses`/content bounded context consuming this gateway — no such context exists in
  `sanvi-backend` yet; `course-media`'s legacy course-upload path is untouched.
- Captions/chapters proxying, and the public/private delivery *read* routes
  (`GET /v1/public/...`, `GET /v1/delivery/...`) — those are unauthenticated-by-token on
  `hitox-media-service`'s own side already and need no gateway.

## Files likely touched

- `crates/contexts/media/**` (new crate)
- `Cargo.toml` (workspace members)
- `apps/api/src/config.rs`, `apps/api/src/lib.rs` (wiring)
- `api/media.yaml` (new OpenAPI file, one per context per repo convention)

## Notes / gotchas

- Ruled out at design time: a token-minting endpoint enabling direct cross-origin browser →
  media-service calls. It would touch `hitox-media-service` (new token type/verification path,
  CORS: specific origin instead of `*`, `Access-Control-Allow-Credentials`) **and**
  `sanvi-frontend` (`getToken` would need a real fetched token, `VITE_MEDIA_ORIGIN` becomes
  genuinely cross-origin, TASK-013's e2e same-site assumption would need redoing) for no benefit
  over the gateway — bulk upload bytes already bypass both servers via `hitox-media-service`'s
  presigned S3 part URLs, so the gateway only ever proxies small JSON control calls.
- Rollback: revert the crate/route registration; `course-media` degrades exactly as it does today
  (401, same-site gateway assumed but absent) — no data migration, nothing stateful added.
- Closeout notes (Task 7, 2026-09-08):
  - Spec correction (authorized at closeout): the "Error passthrough" bullet above was replaced
    with "Error mapping, not passthrough" per CONTRIBUTING.md rule #2 (`sanvi_http::ApiError` is
    the only error body this API produces), matching the shipped implementation reviewed in Tasks
    4 and 6 (`MediaError` → `ApiError` by status in `handlers.rs::media_problem`, upstream `code`
    threaded through `problem().extensions`, `detail` unchanged). The old bullet's `502
    media/provider-unavailable` never existed in code — unreachable/timed-out upstream maps to
    `ServiceUnavailable` (503) with code `media_unavailable`.
  - Live round trip NOT run: no local `hitox-media-service` is reachable in this environment.
    Probed 2026-09-08 — `:8080` answers 404 `page not found` and belongs to the `hitox-backend`
    container (a different service); no media-service API container exists (only the
    `hitox-backend-media-worker`), and nothing listens on candidate ports
    8081/8082/8085/8090/9002/3001/3002/5001. Standing up a live round trip would additionally need
    a valid tenant session through `apps/api` (Kratos/session infra), which this closeout does not
    scaffold. Per the TASK-013 honesty pattern this is recorded, not claimed: the corresponding
    acceptance criterion is left unchecked, and the wire contract it would exercise is covered by
    `HitoxMediaAdapter` mock-server tests (Bearer key, `X-Namespace-ID`, `Idempotency-Key`,
    per-status error mapping, connection-failure → `Unavailable`) plus 13 handler tests (routes,
    `code`-in-body mapping, spoofed-`X-Namespace-ID` ignored).
  - 401 criterion: no dedicated media test by design — `CurrentTenant`/`RequiredPermission` are the
    exact extractors every other context's routes use, the routes mount in the shared tenant scope
    behind `TenantMiddleware` (`apps/api/src/lib.rs`), and the 401/403 path is that shared
    infrastructure's contract (covered by `crates/platform/http/tests/integration/*.rs`).
  - Gate at closeout, all green: `cargo c`, `cargo fmt --check`, `cargo k` (clippy `-D warnings`),
    `./scripts/check-layering.sh`, `cargo nextest run -p sanvi-media` (28/28),
    `scripts/test.sh -p sanvi-access` (50/50), `just openapi` (no-op, no diff).

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
