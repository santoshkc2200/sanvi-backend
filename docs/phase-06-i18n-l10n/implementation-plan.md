# Phase 06 — Internationalization & Localization (backend)

**Target version:** 0.7.0
**Depends on:** phases 01, 02 (locale on user + tenant), 04 (localized invoices/plans), 05 (localized notices)
**Unlocks:** Japanese market launch; phase 07 themes carry locale-aware typography

## Goal

A `localization` bounded context that owns locale negotiation, message catalogs, and formatting
policy for everything the backend emits: API problem details, emails, notifications, PDFs, exports,
and localized content fields. Release scope is **English (`en`) and Japanese (`ja`)**, with adding a
third locale being a data operation, not a code change.

## Scope

**In**
- Locale negotiation and propagation (request → use case → notification → template).
- ICU MessageFormat catalogs for backend-emitted text, with pluralisation/gender/number handling.
- Localized *content* storage pattern for tenant-authored data (plans, themes, product copy).
- Formatting: dates, times, numbers, currency (incl. JPY zero-decimal), addresses, names, sorting.
- Timezone handling and per-user/tenant timezone preference.
- Translation workflow: extraction, catalog validation, missing-key detection, fallback policy.

**Out**
- Frontend UI translation (frontend phase 06) — the backend serves the catalogs it owns and the
  locale it negotiated; the frontend owns its own message set.
- Machine translation of tenant content (future).

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Message format | ICU MessageFormat, catalogs as `.ftl`-style or JSON per locale under `crates/contexts/localization/catalogs/{locale}/{domain}.json` | Handles Japanese counters and English plurals with one syntax |
| Locale identifiers | BCP-47, normalised and validated (`en`, `en-GB`, `ja`, `ja-JP`) | Interop with `Accept-Language`, Stripe, Kratos, ad platforms |
| Negotiation order | explicit `?locale=` (allow-listed) → authenticated user preference → tenant default → `Accept-Language` best match → platform default (`en`) | Deterministic, testable, and never surprises a signed-in user |
| Fallback | Per-key fallback to the base locale, **never** an empty string or the raw key in production | A missing translation must degrade to readable English |
| Missing keys | Build fails in CI; runtime logs a metric and falls back | Catch at build time, survive at runtime |
| Content localization | `jsonb` locale map (`{"en": "...", "ja": "..."}`) for short fields; sidecar `*_translations` table for long/searchable fields | Avoids column explosion, keeps search indexable where it matters |
| Storage of times | Always `timestamptz` in UTC; render in the subject's timezone at the edge | One correct way |
| Currency | Minor units as `i64` + ISO-4217 code; exponent from the currency table, **never assume 2** | JPY has 0 decimals; the frontend `billing-elements` package already models this |
| Emails | Subject + body per locale, rendered from the same template with locale-specific typography and date formats | One template, many locales |

## Domain model

```rust
pub struct Locale(String);                 // validated BCP-47, canonicalised
pub struct LocaleSet { supported: Vec<Locale>, default: Locale }   // platform-wide and per-tenant
pub struct MessageKey(&'static str);       // "billing.invoice.paid.subject"
pub struct Translation { key, locale, value, updated_at, source: TranslationSource }
pub struct FormattingPolicy { locale, timezone: Tz, first_day_of_week, number_format, name_order,
                              address_format, currency_display }
pub trait Translator {
    fn translate(&self, key: MessageKey, locale: &Locale, args: &MessageArgs) -> String;
}
```

Events: `TenantLocalesChanged`, `TranslationUpdated`, `CatalogPublished`.

## Deliverables

### Schema

```sql
CREATE SCHEMA localization;
CREATE TABLE localization.supported_locales (
  code text PRIMARY KEY, name_native text NOT NULL, name_en text NOT NULL,
  enabled boolean NOT NULL DEFAULT true, sort_order int NOT NULL);
INSERT INTO localization.supported_locales VALUES
  ('en','English','English',true,1), ('ja','日本語','Japanese',true,2);

CREATE TABLE localization.tenant_locales (
  tenant_id uuid PRIMARY KEY, default_locale text NOT NULL, enabled_locales text[] NOT NULL,
  timezone text NOT NULL DEFAULT 'UTC', updated_at timestamptz NOT NULL DEFAULT now());

CREATE TABLE localization.translations (            -- runtime-editable overrides only
  scope text NOT NULL,        -- 'platform' | tenant uuid
  key text NOT NULL, locale text NOT NULL, value text NOT NULL,
  updated_by uuid, updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (scope, key, locale));
```

Static catalogs ship with the binary (compile-time embedded, zero I/O on the hot path); the
`translations` table only holds *overrides* (tenant tone-of-voice, urgent copy fixes), merged over
the static catalog with a cached resolution per (scope, locale).

### Locale propagation

- `LocaleContext { locale, fallback_chain, timezone, formatting }` resolved by middleware right after
  tenant resolution, added to request extensions and the tracing span.
- Carried into async work explicitly: every job/event payload includes `locale` so a notification
  sent from a worker is in the right language (never re-negotiated from a missing request).
- `Content-Language` response header and `Vary: Accept-Language` set on localized responses.

### Localized surfaces

| Surface | Approach |
|---|---|
| Problem+json errors | `title`/`detail` from the catalog; machine-readable `type`/`code` stay locale-independent |
| Emails & notifications | Locale-specific templates; Japanese templates use a CJK-safe layout and no reliance on word-spacing |
| Invoices & receipts | Stripe `locale` on Checkout/Portal; our invoice mirror renders localized labels |
| Plans & marketing copy | `jsonb` locale maps, edited in platform admin |
| Exports (phase 05) | Human-readable HTML in the subject's locale; machine JSON stays keyed in English |
| PDFs | Font embedding for CJK (Noto Sans JP) — a real failure mode if forgotten |

### Japanese specifics that are actually implemented

- Zero-decimal currency handling end to end (`JPY` amounts never divided by 100).
- Name order (`family given`), and honorific suffix `様` in email salutations via a
  `name_order`-aware formatter rather than string concatenation.
- Address format: postal code (`〒123-4567`), prefecture → city → block, rendered from a
  locale-driven address template, and validated per country.
- Full-width/half-width normalisation (NFKC) on input for search and comparison; the original is
  preserved for display.
- Japanese collation for sorting lists (ICU collator, kana-aware) rather than byte order.
- Era-free, unambiguous date rendering (`2026年8月15日`) with the timezone defaulting to `Asia/Tokyo`
  for `ja` tenants.

### API surface (`api/localization.yaml`)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/v1/public/locales` | Supported locales for the current tenant/platform |
| GET | `/api/v1/public/messages/{domain}?locale=` | Catalog delivery for the frontend, ETag-cached |
| GET/PUT | `/api/v1/tenant/localization/settings` | Default locale, enabled locales, timezone |
| GET/PUT | `/api/v1/tenant/localization/overrides` | Tenant copy overrides |
| GET/PUT | `/api/v1/platform/localization/translations` | Platform catalog overrides |

## Work breakdown

1. `Locale` value object, negotiation middleware, fallback chain, `LocaleContext` propagation.
2. Catalog format, loader, compile-time embedding, override merge + cache + invalidation.
3. `Translator` port and adapter; `t!("key", args)` macro with **compile-time key existence check**.
4. Catalog tooling: `cargo xtask i18n extract|check|report` — extracts keys, finds missing/orphaned
   keys, produces per-locale coverage; wired into CI as a hard gate.
5. Formatting service: dates, numbers, currency (exponent table), addresses, names, collation.
6. Localize: problem+json, notification templates, invoice mirror, privacy notices, plan copy.
7. Tenant/platform locale settings + events + caches.
8. CJK font embedding for PDF/export rendering; visual snapshot test for a Japanese invoice.
9. Translation workflow docs: how a string gets from code to a translated catalog, and the review gate.

## Testing

- Negotiation matrix test: all five sources × conflicting values → expected locale.
- Catalog completeness test per locale (CI gate at 100 % for `en`/`ja` on shipped domains).
- ICU rendering tests for plurals (en) and counters (ja), including `0`/`1`/`many`.
- Currency tests: JPY (0 exponent), USD (2), and a 3-exponent currency; round-tripping minor units.
- Formatting golden tests for dates/addresses/names per locale.
- Snapshot tests for one email and one PDF per locale (catches missing CJK glyphs — the classic
  "tofu boxes" bug).
- Async-path test: a job enqueued by an `en` request but processed later still renders `en`.

## Security & compliance

- Locale is user input: allow-list only; no path/format-string injection through locale values.
- Tenant copy overrides are escaped on render (no HTML injection into emails); a strict allow-list of
  inline markup.
- Notice versioning from phase 05 is per locale **and per jurisdiction** — a Japanese notice update
  re-prompts `ja` users, and a US-section update re-serves the notice at collection to US subjects
  without clearing anyone's existing opt-out. Content keys are `{notice}/{jurisdiction}/{locale}`, so
  a missing translation degrades to the notice's source locale rather than to the wrong jurisdiction's
  text.

## Observability

Metrics: `i18n_missing_key_total{key,locale}` (should be 0 in prod), `locale_negotiated_total{locale,source}`,
`translation_cache_hit_ratio`. Alert on any missing-key increase after a deploy.

## Acceptance criteria

- [ ] Every backend-emitted string a user can see is in the catalog — no string literals in handlers
      (lint enforced).
- [ ] Switching a tenant to `ja` changes emails, errors, invoices, and notices, including from the
      worker.
- [ ] A JPY plan bills, renders, and exports with no decimal artefacts.
- [ ] CI fails when a new key lacks a `ja` translation.
- [ ] A Japanese invoice PDF renders with correct glyphs and a Japanese-formatted address.

## Risks

| Risk | Mitigation |
|---|---|
| Translations lag code and block releases | `en` fallback + coverage report; only *shipped* domains are gated at 100 % |
| Locale lost across async boundaries | Locale is a required field on every job/event payload; a test asserts it |
| Hardcoded strings creep back | Clippy lint / custom lint on user-facing formatting macros + PR checklist |
| Text expansion breaks layouts | Frontend phase 06 owns pseudo-localisation; backend keeps subjects within length budgets |
