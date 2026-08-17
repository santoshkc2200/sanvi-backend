# Phase 07 — Theming (backend) — Implementation notes

**Version:** 0.8.0 target. Implemented against `docs/phase-07-theming/implementation-plan.md`.

## What shipped

- **`theming` bounded context** (`crates/contexts/theming`): manifest schema +
  validation, DTCG token engine (parse/merge/override with the `overridable`
  allow-list), WCAG AA contrast gate, CSS sanitizer + scoping, resolved-theme
  builder with CSS custom-property emission, draft → publish → rollback state
  machine with immutable snapshots, brand-asset validation (type sniffing,
  dimension caps, EXIF strip, SVG rejection), signed preview tokens.
- **Schema** (`migrations/20260821000000_theming_core.sql`): `themes`,
  `theme_versions` (immutable, trigger-enforced; yank via `yanked_at`),
  `tenant_themes` (RLS FORCE, one row per tenant per state), and
  `tenant_theme_revisions` (RLS, append-only snapshots). The `base` theme is
  seeded as platform reference data.
- **API** (`api/theming.yaml`): the public resolved-theme endpoint
  (ETag-cached), tenant draft/available/preview/publish/rollback/assets, and
  the platform registry (list/install/yank).
- **CLI** (`sanvi theme publish|list|versions|yank`): the installation
  pipeline — validate → contrast gate → content-addressed zip artifact →
  registry row. The same code path the API's install endpoint runs, so
  "installable" is proven by CI for first-party themes.
- **Reference themes**: `base` (seeded) and `aurora` (premium, requires the
  `theming.premium_themes` entitlement) under `crates/contexts/theming/themes/`.
- **Observability**: `theme_resolution_duration`, `theme_cache_hit/miss`,
  `theme_publish_total{result}`, `theme_validation_failures_total{reason}`,
  `custom_css_size_bytes`.
- **Worker consumer**: `theming.*` events invalidate every instance's
  resolved-theme cache (Redis + local fallback).

## Documented deviations from the plan

1. **Asset upload is a direct (base64 JSON) upload, not a presigned URL.**
   There is no object-store client in the repo yet, and a presign flow needs
   one to sign against. The upload endpoint applies the full validation
   pipeline (sniffing, caps, EXIF strip, SVG rejection) and stores
   content-addressed files under the configured `artifact_dir`; swapping the
   storage port for S3/GCS later does not change the API.
2. **Artifacts land on local disk, not object storage.** `LocalFileStore`
   implements the `ArtifactStore`/`BrandAssetStore` ports (same layout a CDN
   or object store would mirror); `artifact_uri` + digest are still recorded
   and the digest is pinned at install.
3. **Token documents are stored in the DB** (`theme_versions.tokens`) in
   addition to the content-addressed artifact, so resolution never depends on
   storage availability at request time. The artifact remains the source of
   truth for rollback/pipeline tooling.
4. **Asset serving is not part of the API.** Resolved asset URLs point at
   `asset_base_url` (+ `/themes/{key}/{version}/…` or `/brand/{tenant}/…`);
   production points that prefix at a CDN/static bucket. Serving local files
   from the API would re-introduce a path-traversal surface for zero phase
   value.
5. **Rollback is append-only with monotonic revisions** (a rollback is itself
   a revision). Publish snapshots the state it replaces, so rollback restores
   revision N−1 exactly; the first publish has nothing to roll back to
   (`409`). ETags therefore always move forward, which is what the frontend
   cache contract needs.
6. **SVG uploads are rejected, not sanitised** ("sanitised or converted" in
   the plan): without a real XML processor the platform cannot prove an SVG
   is script-free, so it fails closed.
7. **Custom CSS at-rules are rejected entirely** (not just `@import`): the
   sanitizer accepts only allow-listed selectors/properties, no `url()`, no
   escapes, no HTML, sized-capped and re-scoped under `.sanvi-theme-scope`.

## Security notes

- No theme-supplied code exists in the schema by construction (declarative
  manifest only); no remote CSS/font origins are representable.
- Theme rows are immutable after insert; yank is the kill switch and pinned
  tenants keep rendering.
- Resolution fails closed to the base theme when a pinned version is missing
  (logged + counted, never silent).
- The preview endpoint requires `tenant.theming.read` **and** a short-lived
  HMAC token; responses carry `no-store` + `noindex`. Without a configured
  secret the endpoint fails closed (503).
- The CSS sanitizer and asset validators carry adversarial unit tests (XSS
  corpus, script-bearing SVG, oversized/decompression-bomb inputs).

## Testing

- Unit: manifest/token/contrast/sanitizer/asset/preview/cache (34 tests).
- Integration (testcontainers, real RLS): publish→override→resolve,
  rollback-exactness, entitlement filtering, immutability trigger, yank vs
  pinned tenants, contrast gate, custom-CSS entitlement + scoping, brand
  asset upload/rejection, tenant isolation, public host resolution (7 tests).
- Contract: `api/theming.yaml` generated and CI-checked; permission catalog
  test covers every theming route.
