# TASK-032: 10.4a Media asset gateway

**Phase:** 10
**Status:** in-progress
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

- [ ] **New context** — `crates/contexts/media` (`sanvi-media` crate), mirroring the shape of
      `advertising`'s outbound adapters (`adapters/hitox.rs` alongside `google.rs`/`meta.rs`'s
      pattern): a `MediaGatewayPort` trait, a `HitoxMediaAdapter` `reqwest` implementation, thin
      application use cases (one per route below), and an `adapters/http` router.
- [ ] **Routes** — same-origin, session-authenticated, mounted so a deployment can point
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
- [ ] **Tenant → namespace mapping** — the existing tenant-resolution middleware (used by every other
      context) puts `TenantContext` in request extensions before the handler runs; the adapter sends
      `X-Namespace-ID: <tenant_id>`, 1:1, no separate registration step. `hitox-media-service` treats
      the namespace as an opaque caller-supplied value by design, so this needs nothing on its side.
- [ ] **Config** — `MediaConfig { base_url: String, service_api_key: String }` in
      `apps/api/src/config.rs`, same shape as `AdvertisingConfig`'s static secrets (plain `String`
      from env, not envelope-encrypted — this is our secret, not a tenant's). Never logged, traced,
      or echoed in an error body (NFR-1003's redaction rule applies here too even though it's outside
      the `advertising` context).
- [ ] **Error passthrough** — relay `hitox-media-service`'s `problem+json` status and body close to
      verbatim rather than remapping into `sanvi-backend`'s own `ApiError` vocabulary — the frontend's
      `courseApiRequest` already parses the media service's error shape directly today and must not
      need to change. Adapter-level failures the media service never sent (timeout, connection
      refused, 5xx) map to `502 media/provider-unavailable`.

## Acceptance criteria

- [ ] Each of the 6 routes, called with a valid tenant session, reaches `hitox-media-service` with
      `Authorization: Bearer <service_api_key>` and `X-Namespace-ID: <tenant_id>` set, and relays a
      successful response body/status unchanged.
- [ ] A request without a valid tenant session gets the same `401` shape every other session-gated
      route already returns — never a passthrough of media-service's own "valid service API key
      required" body, which would leak that a shared secret is involved.
- [ ] `Idempotency-Key` on `POST /v1/assets` reaches `hitox-media-service` unchanged; a request
      missing it gets media-service's own `400` passed through (the header is optional at this
      gateway's own layer — media-service is the one that requires it).
- [ ] A `hitox-media-service` error response (404/409/4xx) passes through with its status and
      `problem+json` body intact.
- [ ] `service_api_key` never appears in a log line, trace attribute, or error response body —
      `HitoxMediaAdapter`'s `Debug` impl redacts it by hand, the same pattern `MetaAdsAdapter` already
      uses for `client_secret` (there is no repo-wide grep gate for this; each adapter owns its own
      redaction).
- [ ] `just layering` stays green — the new context depends only on `kernel`/`platform`, same as
      every other context.

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

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
