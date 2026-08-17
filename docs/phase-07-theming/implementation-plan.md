# Phase 07 — Theming & Pluggable Templates (backend)

**Target version:** 0.8.0
**Depends on:** phases 01, 03 (entitlements gate premium themes), 06 (locale-aware typography)
**Unlocks:** tenant-branded storefronts; the marketplace model for future themes

## Goal

A `theming` bounded context that treats a theme as an **installable, versioned artifact** described by
a manifest, not as code baked into the frontend. Tenants switch themes, override tokens (colors,
fonts, logos), and choose layouts, with the platform controlling what is installable and what a
tenant may change. Adding a new theme is publishing an artifact, not deploying the app.

## Scope

**In**
- Theme/template registry: packages, versions, compatibility ranges, availability rules.
- Tenant theme selection, token overrides, asset (logo/favicon/OG image) management.
- Resolved-theme API the frontend consumes for SSR, with strong caching and instant invalidation.
- Draft → preview → publish workflow with rollback.
- Validation and safety: schema-checked manifests, token allow-list, no arbitrary code execution.

**Out**
- The rendering runtime and component/layout implementation (frontend phase 07).
- A public third-party marketplace with revenue share (future phase).

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Theme = manifest + tokens + layout composition + assets. **No arbitrary JS.** | Declarative only in this release | An executable theme is a stored XSS/supply-chain vector across every tenant |
| Token source | DTCG JSON, the same format the existing `design-tokens` package already uses (Style Dictionary) | Reuses a built, tested pipeline instead of inventing a second one |
| Override surface | Allow-list of *semantic* tokens (`color.background.primary`, `font.family.base`, radii, spacing scale) — not primitives, not arbitrary CSS | Tenants can be on-brand without being able to break accessibility or layout |
| Custom CSS | Entitlement-gated (`theming.custom_css`), sanitised, scoped, and size-capped | Power users want it; it must be opt-in and contained |
| Distribution | Versioned artifact (tokens + layout manifest + assets) in object storage, immutable, content-addressed; DB holds metadata | Cacheable, rollback-able, CDN-friendly |
| Resolution | Server resolves `theme + tenant overrides + locale` → a single `ResolvedTheme` document + a precompiled CSS custom-property block, ETag'd | One request for SSR, one cache key, instant swap |
| Versioning | SemVer per theme; tenant pins a version, upgrades are explicit; compatibility declared against a platform `theme_api` version | A theme upgrade must never silently change a live storefront |
| Assets | Uploaded via presigned URLs, validated (type, dimensions, size), stripped of EXIF, served from CDN | Standard hardening; EXIF strip is also a privacy item (phase 05) |

## Theme manifest (`theme.json`)

```jsonc
{
  "id": "aurora",
  "version": "1.4.0",
  "themeApi": "^1.0.0",
  "name": { "en": "Aurora", "ja": "オーロラ" },
  "capabilities": ["dark-mode", "rtl-ready", "cjk-typography"],
  "tokens": { "light": "tokens/light.json", "dark": "tokens/dark.json" },
  "overridable": ["color.brand.*", "color.background.*", "font.family.*", "radius.*", "spacing.scale"],
  "layouts": {
    "storefront.home":    { "slots": ["hero", "featured", "testimonials", "cta"] },
    "storefront.product": { "slots": ["gallery", "details", "related"] },
    "storefront.checkout":{ "slots": ["summary", "payment"], "locked": true }
  },
  "assets": { "preview": "preview.png", "screenshots": ["shots/1.png"] },
  "fonts": [{ "family": "Noto Sans JP", "subsets": ["latin", "japanese"], "source": "self-hosted" }]
}
```

`locked: true` on a layout means slot composition cannot be changed by the tenant — checkout stays
predictable regardless of theme, which matters for conversion and for PCI-adjacent surfaces.

## Domain model

```rust
pub struct Theme { id: ThemeId, key: String, publisher: PublisherRef, visibility: Visibility,
                   required_feature: Option<FeatureKey>, status: ThemeStatus }
pub struct ThemeVersion { id, theme_id, semver: Version, theme_api_range: VersionReq,
                          manifest: Manifest, artifact_digest: String, published_at, yanked_at }
pub struct TenantTheme { tenant_id, theme_id, version: Version, state: ThemeState /*Draft|Live*/,
                         token_overrides: TokenOverrides, layout_overrides: LayoutOverrides,
                         assets: BrandAssets, custom_css: Option<SanitizedCss>, published_at }
pub struct ResolvedTheme { /* fully merged tokens, layout tree, asset URLs, font links, css_vars */ }
```

Invariants: overrides are validated against the version's `overridable` list *and* the token schema;
a `Draft` can be previewed by authorized users only; publishing is atomic and produces a new
immutable revision so rollback is a pointer move.

Events: `ThemePublished`, `ThemeYanked`, `TenantThemeChanged`, `TenantThemePublished`,
`TenantThemeRolledBack`, `BrandAssetUpdated`.

## Deliverables

### Schema

```sql
CREATE SCHEMA theming;
CREATE TABLE theming.themes (id uuid PRIMARY KEY, key text UNIQUE NOT NULL, publisher text NOT NULL,
  visibility text NOT NULL, required_feature text, status text NOT NULL);
CREATE TABLE theming.theme_versions (id uuid PRIMARY KEY, theme_id uuid NOT NULL,
  semver text NOT NULL, theme_api_range text NOT NULL, manifest jsonb NOT NULL,
  artifact_uri text NOT NULL, artifact_digest text NOT NULL,
  published_at timestamptz NOT NULL DEFAULT now(), yanked_at timestamptz,
  UNIQUE (theme_id, semver));
CREATE TABLE theming.tenant_themes (tenant_id uuid NOT NULL, state text NOT NULL,
  theme_id uuid NOT NULL, semver text NOT NULL, token_overrides jsonb NOT NULL DEFAULT '{}',
  layout_overrides jsonb NOT NULL DEFAULT '{}', assets jsonb NOT NULL DEFAULT '{}',
  custom_css text, revision int NOT NULL, updated_by uuid, updated_at timestamptz,
  PRIMARY KEY (tenant_id, state));
CREATE TABLE theming.tenant_theme_revisions (tenant_id uuid, revision int, snapshot jsonb,
  published_at timestamptz, published_by uuid, PRIMARY KEY (tenant_id, revision));
```

RLS on all `tenant_*` tables. `theme_versions` rows are immutable after insert (trigger-enforced);
withdrawal is `yanked_at`, never deletion, so a pinned tenant keeps rendering.

### Resolution & delivery

- `GET /api/v1/public/theme?host=…` returns `ResolvedTheme` + `css_vars` (a ready-to-inline
  `:root{--…}` block) with `ETag` = hash(theme version, overrides revision, locale, api version).
- Cached in Redis for 24 h, invalidated by `TenantThemePublished`; the frontend also caches by ETag,
  so a publish is visible within one request and costs one round trip.
- Preview: `GET /api/v1/tenant/theme/preview?token=` with a short-lived signed token that resolves the
  `Draft` state — no cookie, no cache, `noindex` headers, permission-checked.
- Font strategy: self-hosted, subset per locale, `font-display: swap`, preloaded for the locale the
  request negotiated (phase 06). Japanese subsets are large — the manifest declares subsets so the
  frontend only preloads what the page needs.

### Installation pipeline

`apps/cli theme publish <dir>`:
1. Validate manifest against the JSON schema and the `themeApi` range.
2. Validate tokens against the DTCG schema; reject unknown token types and unresolved references.
3. Contrast check: every foreground/background semantic pair must meet WCAG AA, or publishing fails
   (`--allow-contrast-warnings` requires an explicit override and is recorded).
4. Scan assets (type sniffing, dimension/size caps, EXIF strip, malware scan).
5. Build the artifact, compute the digest, upload immutably, insert the version row.

Same pipeline runs in CI for first-party themes, so "installable" is proven, not asserted.

### API surface (`api/theming.yaml`)

| Method | Path | Permission |
|---|---|---|
| GET | `/api/v1/public/theme` | none (host-resolved) |
| GET | `/api/v1/tenant/themes/available` | `theming.read` (filtered by entitlements) |
| GET/PUT | `/api/v1/tenant/theme/draft` | `theming.write` |
| POST | `/api/v1/tenant/theme/publish` \| `/rollback` | `theming.publish` |
| POST | `/api/v1/tenant/theme/assets` (presign) | `theming.write` |
| GET/POST/DELETE | `/api/v1/platform/themes` (+ versions, yank) | `platform.theme.write` |

## Work breakdown

1. Manifest schema + validator + `themeApi` compatibility rules.
2. Token merge engine (base → theme → tenant overrides) with allow-list enforcement and schema check.
3. Artifact storage, content addressing, immutability triggers, yank semantics.
4. `ResolvedTheme` builder + CSS custom-property emitter + ETag/caching + invalidation on events.
5. Draft/publish/rollback state machine with revision snapshots.
6. Brand asset upload (presign, validate, strip, CDN URLs) and defaults per locale.
7. Custom CSS: entitlement gate, sanitiser (allow-list of properties/selectors, no `url()` to foreign
   origins, no `@import`), size cap, scoping wrapper.
8. Contrast/accessibility validation in the publish pipeline.
9. Publish CLI + first-party themes ("base" + one alternative) as reference implementations.
10. CSP coordination: theme font/image origins are fed into the frontend `csp` package's config.

## Testing

- Manifest fixtures: valid, unknown token type, out-of-range `themeApi`, disallowed override key.
- Merge tests: precedence, deep merge of token groups, rejection of non-overridable keys.
- Publish/rollback: concurrent publishes serialise; rollback restores the exact previous snapshot.
- Cache tests: publish invalidates within 1 s; ETag stable when nothing changed.
- Security: CSS sanitiser fuzzed against an XSS corpus; SVG logo upload with embedded script rejected.
- Accessibility: contrast validator unit tests on known-good and known-bad palettes.
- Load: 10k tenants each on a distinct theme version, resolution p99 under budget with cold cache.

## Security & compliance

- No theme-supplied JavaScript, no remote CSS/font `@import`; everything served from our origins.
- SVG uploads are sanitised or converted; type sniffing, not extension trust.
- Theme artifacts are digest-pinned; a mismatched digest fails closed to the previously cached theme.
- Publisher identity recorded for future third-party themes; yank is the kill switch.

## Observability

Metrics: `theme_resolution_duration`, `theme_cache_hit_ratio`, `theme_publish_total{result}`,
`theme_validation_failures_total{reason}`, `custom_css_size_bytes`. Alert on resolution p99 or a spike
in fail-closed digest mismatches.

## Acceptance criteria

- [x] A tenant switches theme, overrides brand color/font/logo, previews it, publishes it, and the
      live storefront reflects it within one request.
- [x] Rollback restores the previous look exactly, including assets.
- [x] Publishing a theme whose semantic pairs fail WCAG AA is refused by default.
- [x] A new theme is installed by publishing an artifact — no application deploy (demonstrated).
- [x] A premium theme is invisible to tenants without the entitlement.
- [x] No path allows theme-supplied script execution (verified by the security test suite).

Implemented (see `implementation-notes.md` for the documented deviations:
base64-JSON asset upload instead of presigned URLs, local artifact storage,
tokens stored in the DB, append-only rollback, SVG rejection instead of
sanitisation).

## Risks

| Risk | Mitigation |
|---|---|
| Tenants create unusable/inaccessible designs | Semantic-token-only overrides + contrast gate + "reset to theme defaults" |
| Theme upgrade breaks a live storefront | Version pinning, explicit upgrade with preview, one-click rollback |
| Custom CSS becomes an XSS vector | Entitlement-gated, allow-list sanitiser, fuzzed, scoped, size-capped |
| CJK font weight hurts LCP | Subsetting declared in the manifest, locale-aware preload, `font-display: swap` |
| Theme sprawl becomes unmaintainable | `themeApi` compatibility range + yank + deprecation policy |
