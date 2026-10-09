# TASK-027: 11.8 Error addressability, collection ordering & template accessibility

**Phase:** 11 · **Status:** done · **Size:** S
**Requirement(s):** FR-1127
**Depends on:** TASK-022, **phase 10 merged**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** none

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

The three API-side affordances accessibility depends on and that a client **cannot fix on its own**.

## Why this shape

All three are cheap here and impossible to do well downstream. A flat error string forces the client
into a colour-only or position-only presentation. A list whose order changes between fetches breaks
keyboard position and screen-reader context on refresh. An inaccessible transactional email is the one
accessibility failure a user cannot work around, because they cannot re-render it.

Do this **before** frontend TASK-027 starts its remediation, not alongside it. A client that has
already worked around a flat error string will have built a presentation that the fix then breaks.

It depends on TASK-022 because ordering and error shapes interact with the cached SSR responses;
changing them after the cache lands invalidates a warm cache for a cosmetic reason.

## Constraints

- Nothing here is flagged. Field-addressed errors are additive (the message stays), and ordering
  changes are behaviour-preserving for any client that was not relying on an unspecified order — which
  is the point of specifying it.
- No `/api/v1` response shape changes except the addition of field addressing on errors.

## File ownership map

- `crates/platform/http/**` — problem-details field addressing
- every context's `application/**` validation error types, especially the phase 08–10 wizards
- every context's list handlers and their `api/*.yaml` ordering documentation
- notification and email templates plus their locale catalogs

## Steps

### Step 1: Write the enumeration test and let it fail

**Files:** Create `tests/contract/error_addressability.rs`

**Do:** enumerate every validation error type across the thirteen contexts and assert each carries the
field path it belongs to.

**Verify:** `just test-error-addressability` → fails, listing the flat ones.

### Step 2: Fix the stragglers, wizards first

**Files:** Modify every context's `application/**` validation error types

**Do:** the phase 08–10 wizards are the usual offenders, because their multi-step payloads flatten on
the way out and lose the path to the field that failed.

**Verify:** the enumeration test passes, and a multi-step wizard payload rejected at step three names
**the step-three field, not the envelope**.

### Step 3: Specify and enforce deterministic ordering

**Files:** Modify `api/*.yaml`, every context's list handlers; Create
`tests/contract/collection_ordering.rs`

**Do:** every collection endpoint gets a specified ordering in its spec, enforced in the handler, tied
to TASK-021's cursor pagination and **tie-broken by id** so equal sort keys cannot reshuffle.

**Verify:** `just test-collection-ordering` — insert rows with identical sort keys and assert a stable
order across repeated fetches **and across page boundaries**.

### Step 4: Make the templates accessible

**Files:** Modify notification and email templates and their locale catalogs

**Do:** semantic headings, real text rather than text-in-image, meaningful link text, table structure
where tabular, and a plain-text alternative carrying the same information. Contrast and structure
checks in both locales.

**Verify:** `just i18n` passes; a template test asserts the plain-text alternative contains every fact
the HTML version does.

## Definition of done

- [x] Every validation error from every context carries its field path; the enumeration test fails on a
      flat one.
- [x] A wizard payload rejected at step three names the step-three field.
- [x] Every collection endpoint has a specified ordering, enforced and tie-broken by id, stable across
      repeated fetches and page boundaries.
- [x] Templates pass contrast and structure checks in both locales, with an equivalent plain-text
      alternative.
- [x] No `/api/v1` response shape changed except error field addressing.

## Verification

```bash
just fmt
just clippy
just test
just openapi
just test-error-addressability
just test-collection-ordering
just i18n
```

## Execution record (2026-10-10)

- **Field addressing.** `sanvi_http::FieldIssue` and `ApiError::validation` (same 400 and
  `bad-request` class as before; the flat message stays, the RFC 9457 `fields` extension member is
  additive). Each of the thirteen HTTP contexts gained an exhaustive classifier
  (`adapters/http/validation.rs`) that owns the field paths and the unchanged flat detail, one
  constructed sample per error variant, and unit tests pinning the message-keyed grab-bag variants
  (payments money/connection shapes, billing prices, advertising inputs, access
  definitions/entitlements). The advertising campaign wizard's creative-parse failure now raises a
  `Validation` violation carrying `ad_groups[i].ads[j].creative.id` instead of flattening; the
  payments checkout addresses `amount_minor`, `currency`, `reference`, `cursor`, `status`.
- **Enumeration gate.** `apps/api/tests/integration/error_addressability.rs` walks all thirteen
  inventories and fails on a classified-validation sample with no field, an empty path, or an empty
  message; `all_thirteen_http_contexts_are_enumerated` pins the context set;
  `the_gate_fails_on_a_planted_flat_validation_error` is the self-test. `just test-error-addressability`.
- **Wizard step three.** `patch_campaign_validates_and_hydrates_creatives_in_ad_groups` now proves a
  payload rejected at the third ad group names `ad_groups[2].ads[0].creative.id` (integration, real DB).
- **Ordering.** Every bounded collection's annotation declares `Ordered by … .` in its doc comment
  (landed in the operation `summary`/`description`); identity memberships tie-break by `user_id`,
  theming theme versions by `id` (the two SQL gaps). `apps/api/tests/integration/collection_ordering.rs`
  parses the clause out of the merged document (final key must be the row's unique key; documented
  exceptions `list_audit → seq`, `list_members → user_id`) and serves `GET /api/v1/tenant/payments`
  through the real stack with five rows sharing one `created_at`: two independent page walks are
  byte-identical and ids fall through id-DESC across page boundaries. `just test-collection-ordering`.
- **Templates.** `sanvi_localization::adapters::email_html` derives the HTML alternative from the
  rendered plain text (one `<h1>`, `<p>` paragraphs, `lang`, HTML-escaped, bare-URL lines become links
  labelled with the locale's action — `emails.invitation.action` added to both catalogs; unlabelled
  URLs fall back to the host). The WCAG-AA palette is measured by unit test, not eyeballed. Catalog
  rules for both locales: templates are plain text, URLs sit on their own line, and the HTML
  alternative must contain every sentinel fact the text carries. `RenderedNotification` carries
  `html_body`; the dispatcher, the worker's budget/notification lane and the direct Lane-B sends
  (identity invitation, DSR verification, export passphrase) all ship it.
  `just i18n` green (catalog parity incl. the new key).
- **Harness repairs the suite exposed (pre-existing defects, fixed here).** The contract harness's
  lazy pool stub retried refused connects for sqlx's full 30 s acquire window inside the 30 s request
  budget, making `cache_headers_snapshot` a coin flip (measured 30.0 s at HEAD too); the stub now
  acquires with a 1 s window and the test runs in ~1 s. The regenerated `payments.yaml` made
  `/connections/current` collection-shaped like its already-exempted sibling; the same exemption line
  was added. The freeze gate's `allOf` comparison descends member-by-member so the additive `fields`
  member is not misread as a changed schema alternative.
- **Pre-existing, surfaced.** Main's committed `api/*.yaml` and `docs/api/generated/*` were stale
  against merged code (`openapi --check` fails on a pristine main checkout: access components,
  advertising path set). This branch regenerates them per CONTRIBUTING §3; the regeneration surfaces
  post-freeze required-field additions (`requested_scopes`, `is_system`, …) to
  `spec-freeze-check`, which needs the repository's reviewed-exception flow — not TASK-027's
  verification list.
- **Final verification.** `cargo fmt --check` clean; `cargo clippy --workspace --all-targets --
  -D warnings` clean; full workspace suite 1,508/1,508; `openapi --check`, `spec-lint`,
  `api-docs --check`, `i18n check`, `replay-suite`, `route-matrix`, trace-coverage, log-PII,
  secret-scan, layering, timeouts, id-policy all green; `test-error-addressability` 4/4 and
  `test-collection-ordering` 3/3.

## Out of scope

Everything the browser renders — the axe sweep, manual passes, focus management, forms, charts, the
theme contrast gate, and the accessibility statement — all frontend TASK-027.

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
