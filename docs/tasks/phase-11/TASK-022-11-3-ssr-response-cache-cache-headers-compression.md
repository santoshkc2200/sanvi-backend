# TASK-022: 11.3 SSR response cache, cache headers & compression

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1111, NFR-1101
**Depends on:** TASK-020
**Created:** 2026-08-21

**Slice:** 11.3 — Frontend performance & Core Web Vitals (backend half)
**Prerelease:** `v1.0.0-rc.4` · **Flag:** `platform.ssr_cache`

## Context

The backend half of the frontend performance slice. Frontend TASK-022 owns bundles, fonts, images, and
INP; this task owns the three server-side levers that move field LCP and that the frontend cannot pull
on its own: a cached SSR response, correct cache headers per app, and compression with a priority hint
for the LCP resource.

It is contract-first despite changing no shapes, because it changes **cache semantics for a whole class
of responses** — and a caching bug does not look like a bug, it looks like another tenant's storefront.

## What to do

- [ ] **Contract** — anonymous storefront SSR caching keyed by `(tenant, locale, theme revision, device
      class)`, behind `platform.ssr_cache`, with explicit `Vary` and a documented invalidation path on
      theme publish and content change. **Authenticated responses are never cached, asserted by a test
      rather than by a code comment.** Documented in the spec alongside the storefront read endpoints.
- [ ] **SSR response cache** — implement the keyed cache in `crates/platform/{http,cache}`, with
      single-flight on miss (a cold cache during a traffic spike must not become a thundering herd on
      the render path) and a hit-ratio metric on TASK-020's storefront dashboard.
- [ ] **Invalidation** — theme publish and content change invalidate within a documented window, driven
      by the existing domain events rather than by a TTL that hopes for the best.
- [ ] **Cache headers per app** — immutable, hashed static assets; sensible HTML lifetimes per app
      (marketing long with revalidation, storefront short, admin `no-store`); `Vary` correct on every
      locale- and theme-dependent response.
- [ ] **Compression and priority** — Brotli where the CDN allows it, early hints or preload headers for
      the LCP resource of the storefront's top templates, and a check that compression never applies to
      an already-compressed payload.

## Acceptance criteria

- [ ] An authenticated request is never served from or written to the SSR cache, proven by a test.
- [ ] A cache-key test proves two tenants cannot share an entry, and that locale, theme revision, and
      device class each partition the key.
- [ ] A theme publish invalidates the affected entries inside the documented window.
- [ ] 500 concurrent misses on one key produce exactly one render (single-flight holds).
- [ ] The SSR cache hit ratio appears on the storefront dashboard with the flag on.
- [ ] Static assets are immutable and hashed; each app's HTML lifetime matches the documented policy;
      `Vary` is correct on every locale- and theme-dependent response.
- [ ] Brotli applies above the size floor and never to an already-compressed payload.
- [ ] The LCP resource of each of the storefront's top templates is preloaded or early-hinted, and the
      improvement is visible in the frontend's `rc.4` benchmark against `rc.1`.

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just test-cache-keys
just perf
just perf-compare rc.3 rc.4
```

## Out of scope

Everything the browser does: bundles, splitting, prefetch, fonts, images, streaming SSR, deferred
hydration, third-party scripts, and INP — all frontend TASK-022. CDN configuration beyond what the
response headers instruct.

## Files likely touched

- `crates/platform/http/**` (cache middleware, `Vary`, compression, early hints)
- `crates/platform/cache/**` (keyed SSR entries, single-flight)
- storefront read handlers and their invalidation subscriptions (theme publish, content change)
- `configs/**` (per-app cache lifetimes, flag definition)
- `api/storefront.yaml` (documented cache semantics)

## Notes / gotchas

- Rollback: `platform.ssr_cache` off returns to uncached SSR — slower, correct, and the safe default.
  This flag is the one piece of the slice that can serve a **wrong** response, so it ships last and
  alone, after the header and compression work is already on staging.
- Device class in the key is there because the storefront serves different markup to mobile; if that
  ever stops being true, remove it from the key rather than leaving a dimension that only multiplies
  entries.
- Do not cache anything with a `Set-Cookie` or an `Authorization` on the request. The test that asserts
  this is the most valuable test in the task.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
