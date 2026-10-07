# TASK-022: 11.3 SSR response cache, cache headers & compression

**Phase:** 11 · **Status:** todo · **Size:** M
**Requirement(s):** FR-1111, NFR-1101
**Depends on:** TASK-020, **phase 10 merged**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** `platform.ssr_cache`

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

The three server-side levers that move storefront first paint and that the frontend cannot pull on its
own: a cached SSR response, correct cache headers per app, and compression with a priority hint for
the largest above-the-fold resource.

## Why this shape

This is contract-first despite changing no response shapes, because it changes **cache semantics for a
whole class of responses** — and a caching bug does not look like a bug, it looks like another tenant's
storefront.

**This is the one piece of the phase that can serve a wrong response.** It ships last within its slice,
alone, after the header and compression work is already on staging, behind a flag whose off position is
the correct-but-slower behaviour.

## Constraints

- Authenticated responses are never cached, asserted by a test rather than by a code comment.
- Do not cache anything with a `Set-Cookie` on the response or an `Authorization` on the request. **The
  test that asserts this is the most valuable test in the task.**
- Invalidation is driven by domain events, not by a TTL that hopes for the best.

## Execution slices

Added 2026-10-07, when the task was picked up (the phase README's convention: tasks are sliced as
they are picked up). Two groundings the table assumes, both verified against the tree on the day:

- **There is no `api/storefront.yaml`.** The storefront read surface is the `/api/v1/public*`
  resources across theming, localization, billing (public plans) and privacy (disclosures) — the
  surfaces the 11.2c profile ranked. Step 1 therefore documents the cache semantics on the specs
  that own those operations, not on a spec that does not exist.
- **11.2h pre-paid part of Steps 5–6.** `SelectiveCompression` (Brotli-first, 1024-byte floor,
  already-compressed and encoded responses refused, `Vary` on every encoded response) and
  `ConditionalEtag` are in `crates/platform/http` behind `[server.compression]` /
  `server.keep_alive_secs` with wire tests. This task must not re-land them; its remaining transport
  work is the per-app HTML lifetimes and the storefront preload hint.

| Slice | Steps | Shape | Scoped verification for the session |
| --- | --- | --- | --- |
| **11.3a** cache semantics in the specs + the flag definition | 1 | contract-led, small | `scripts/check-openapi.sh` clean over the touched specs; flag absent/off is a no-op (`scripts/test.sh -p sanvi-api` config tests); ga-checklist OK |
| **11.3b** cache-key type + the key-partition negative tests | 2 (key half) | test-led, small | the four partition tests (tenant, locale, theme revision, device class) green; a tenant-less key fails to construct (type-level, not by review) |
| **11.3c** keyed cache, single-flight, metrics, the response negatives | 2 (response half) + 3 | the slice that can serve a wrong response | written-first negatives shown failing on the no-cache stub in-session, green after; 500 concurrent misses → render counter = 1; `scripts/test.sh -p sanvi-cache -p sanvi-http -p sanvi-api` |
| **11.3d** invalidation on domain events | 4 | event-led | publish-a-theme test asserts the affected entries are gone inside the documented window; `scripts/test.sh -p <touched contexts> -p sanvi-api` |
| **11.3e** per-app cache headers + `Vary` snapshot | 5 | small, config + middleware | snapshot test per serving surface (extends 11.2h's `headers_snapshot`); admin `no-store` asserted; `scripts/test.sh -p sanvi-http -p sanvi-api` |
| **11.3f** storefront preload hint (compression already landed) | 6 remainder | small, judgement | preload header asserted on the storefront read responses; already-compressed refusal re-proven by 11.2h's wire tests (no re-landing) |
| **11.3g** flag on in the lab, measure, close | 7 | measurement-led, no code | `scripts/perf.sh <tag>` with the flag on; `scripts/perf-compare.sh` against the stored candidate; the task's full `## Verification` block once, then the status flip here and in `../backlog.md` |

**Order.** 11.3a → the rest (the flag and the documented semantics are what everything else cites).
11.3b → 11.3c (the middleware constructs the key type). 11.3c → 11.3d (there is nothing to
invalidate before entries exist). 11.3e and 11.3f are independent of 11.3b–d and of each other.
**11.3g runs last and alone** — it is the slice where the cache can first serve a response to a
request that did not render it, so it ships behind the flag with the off position
verified as the pre-cache behaviour.

**Host coordination.** Sliced 2026-10-07 while TASK-021's 24 h soak (attempt 3) is mid-run on this
host, completing ~09:57 2026-10-08. No build or test tier runs against the host until the soak ends —
a full-CPU build is co-tenant noise the soak's pass conditions (0 failed / 0 dropped, flat
curves) cannot absorb. Implementation starts 11.3a after the soak verdict lands.

## File ownership map

- `crates/platform/http/**` — cache middleware, `Vary`, compression, early hints
- `crates/platform/cache/**` — keyed SSR entries, single-flight
- storefront read handlers and their invalidation subscriptions
- `configs/**` — per-app cache lifetimes, flag definition
- `api/storefront.yaml` — documented cache semantics

## Steps

### Step 1: Document the cache semantics in the spec

**Files:** Modify `api/storefront.yaml`

**Do:** anonymous storefront SSR caching keyed by `(tenant, locale, theme revision, device class)`,
behind `platform.ssr_cache`, with explicit `Vary` and a documented invalidation path on theme publish
and content change.

Device class is in the key because the storefront serves different markup to mobile. If that ever stops
being true, remove the dimension rather than leaving one that only multiplies entries.

**Verify:** `just openapi` clean.

### Step 2: Write the negative tests first

**Files:** Create `tests/cache/ssr_cache_negative.rs`

**Do:** assert, before the cache exists, that an authenticated request is never served from or written
to it; that a response carrying `Set-Cookie` is never stored; and that two tenants cannot share an
entry.

**Verify:** the tests compile and fail for the right reason (no cache yet), then pass once Step 3
lands.

### Step 3: Implement the keyed cache with single-flight

**Files:** Modify `crates/platform/{http,cache}/**`

**Do:** the keyed cache with single-flight on miss — a cold cache during a traffic spike must not become
a thundering herd on the render path — and a hit-ratio metric on TASK-020's alert surface.

**Verify:** 500 concurrent misses on one key produce **exactly one** render, asserted by a render
counter.

### Step 4: Wire invalidation to domain events

**Files:** Modify storefront read handlers and their subscriptions

**Do:** theme publish and content change invalidate the affected entries inside a documented window.

**Verify:** a test publishes a theme and asserts the affected entries are gone within the window.

### Step 5: Set cache headers per app

**Files:** Modify `configs/**`, `crates/platform/http/**`

**Do:** immutable hashed static assets; HTML lifetimes per app (marketing long with revalidation,
storefront short, admin `no-store`); `Vary` correct on every locale- and theme-dependent response.

**Verify:** a snapshot test asserts each app's HTML lifetime and `Vary` set.

### Step 6: Add compression and priority hints

**Files:** Modify `crates/platform/http/**`

**Do:** Brotli above a size floor; preload or early-hint the largest above-the-fold resource of the
storefront's top templates.

**Verify:** compression never applies to an already-compressed payload; the preload appears on each top
template.

### Step 7: Enable the flag last and measure

**Files:** `configs/**`, `benchmarks/**`

**Do:** turn `platform.ssr_cache` on only after Steps 5 and 6 are on staging, and re-run the harness.

**Verify:** `just perf-compare` shows the improvement, and the cache-key negative tests are green with
the flag on.

## Definition of done

- [ ] An authenticated request is never served from or written to the cache, proven by a test.
- [ ] A cache-key test proves two tenants cannot share an entry, and that locale, theme revision, and
      device class each partition the key.
- [ ] A response carrying `Set-Cookie` is never stored.
- [ ] A theme publish invalidates the affected entries inside the documented window.
- [ ] 500 concurrent misses on one key produce exactly one render.
- [ ] The hit-ratio metric is emitted with the flag on.
- [ ] Static assets are immutable and hashed; each app's HTML lifetime matches the documented policy;
      `Vary` is correct on every locale- and theme-dependent response.
- [ ] Brotli applies above the floor and never to an already-compressed payload.
- [ ] `just perf-compare` records the improvement against the previous candidate.

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
hydration, third-party scripts, INP — all frontend TASK-022.

## Parked

CDN configuration and edge image transforms — see
[`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
