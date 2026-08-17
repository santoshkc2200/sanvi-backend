# Translation workflow — how a string gets from code to a translated catalog

Phase 06 ships **English (`en`) and Japanese (`ja`)**. This is the process for
every backend-emitted string, from authoring to release.

## The two catalogs

1. **Static catalogs** — `crates/contexts/localization/catalogs/{locale}/{domain}.json`.
   Flat JSON maps from dotted keys to ICU-style patterns. Compiled into the
   binary at build time; the `translations` table never replaces them, only
   *overrides* them (tone-of-voice, urgent copy fixes) at runtime.
2. **The override table** — `localization.translations` (`scope`, `key`,
   `locale`, `value`). Tenant overrides are edited via
   `PUT /api/v1/tenant/localization/overrides`; platform overrides via
   `PUT /api/v1/platform/localization/translations`.

## Authoring a new string

1. **Write the `en` entry first.** Pick a domain by surface:
   `errors` (problem+json), `emails` (notifications), `billing`
   (invoices/plans), `privacy` (notices/exports). Key shape:
   `{domain}.{noun}.{property}` — `emails.invitation.subject`,
   `billing.invoice.amount_due`.
2. **Use ICU patterns.** `{name}` substitutes; `{count, plural, one {# item}
   other {# items}}` pluralises (en) and counts (ja); `{gender, select, …}`
   selects. Apostrophes are doubled (`You''re`). Dates/numbers render through
   the formatting service (`{date, date}`, `{n, number}`).
3. **Reference it with `t!`** — never a raw string:

   ```rust
   let subject = t!("emails.invitation.subject", tenant = tenant_name);
   let rendered = subject.render(translator.as_ref(), scope, locale);
   ```

   `t!` fails the build if the key is not in the `en` catalog, and CI fails
   if it lacks a `ja` translation — a missing translation can never ship.
4. **Write the `ja` entry.** Same placeholders, same order. CJK-safe text:
   no reliance on word-spacing, no era names in dates (the formatter renders
   `2026年8月15日`).

## The review gate

`cargo run -p sanvi-cli -- i18n check` (CI job `i18n`) asserts:

- every catalog parses, and every value is non-empty;
- `ja` covers 100 % of `en` on every shipped domain (no missing, no extra keys);
- every translation references exactly the same placeholders as its source;
- every `t!` key in the source tree exists in the `en` catalog.

`i18n report` prints per-domain coverage; `i18n extract` maps keys to files.

## Locale negotiation (what your string renders as)

1. explicit `?locale=` (allow-listed against the platform set)
2. the signed-in user's preference (`identity.users.locale`)
3. the tenant default (`localization.tenant_locales`)
4. best `Accept-Language` match
5. the platform default (`en`)

Per-key fallback walks `locale → base language → en` — rendering never
returns an empty string or a raw key; a runtime miss logs
`i18n_missing_key_total` (should be 0 in production).

## Adding a third locale

A **data operation**, not a code change:

1. `INSERT INTO localization.supported_locales (code, name_native, name_en,
   enabled, sort_order) VALUES ('de', 'Deutsch', 'German', true, 3);`
2. Add `catalogs/de/*.json` with 100 % coverage (the CI gate now includes it).
3. Formatting policy handles it via `FormattingPolicy` (dates/numbers use
   locale-independent patterns; timezone defaults to UTC until configured).
4. Add it to `localization.explicit_locale_allowlist` if `?locale=de` should
   work.

## Rules of the road

- User-visible strings live in catalogs; handlers must not hard-code them.
- Every event/job that produces user-visible output carries `metadata.locale`
  (`EventEnvelope::with_locale`); workers never re-negotiate from a request.
- Currency amounts are minor units + ISO-4217 code; the exponent comes from
  `currency_exponent` (JPY 0, USD 2, BHD 3) — never assumed.
- Timestamps are `timestamptz` UTC; they render in the subject's timezone.
- Overrides must reference existing keys (a typo'd key is rejected at the
  API, so it can never silently render).
