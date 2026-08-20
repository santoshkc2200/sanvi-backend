# TASK-027: 11.8 Error addressability, collection ordering & template accessibility

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1127
**Depends on:** TASK-022
**Created:** 2026-08-21

**Slice:** 11.8 — Accessibility (WCAG 2.2 AA) & external audit (backend half)
**Prerelease:** `v1.0.0-rc.9` · **Flag:** none

## Context

The small backend half of the accessibility slice. Frontend TASK-027 owns the audit, the manual passes,
and the remediation; this task owns the three API-side affordances accessibility depends on and that a
client **cannot fix on its own**.

All three are cheap to do here and impossible to do well downstream. A flat error string forces the
client into a colour-only or position-only presentation. A list whose order changes between fetches
breaks keyboard position and screen-reader context on refresh. An inaccessible transactional email is
the one accessibility failure a user cannot work around, because they cannot re-render it.

## What to do

- [ ] **Contract** — no `/api/v1` shape changes beyond making existing error and collection responses
      honest about what they already intend: **every validation error names the field it belongs to**,
      so a client can associate messages programmatically and build an error summary that links to the
      offending input; and **every collection endpoint specifies a deterministic ordering**, tie-broken
      by id.
- [ ] **Error addressability audit** — walk every validation error emitted by the thirteen contexts and
      fix the stragglers. The wizards from phases 08–10 are the usual offenders, because their
      multi-step payloads flatten on the way out and lose the path to the field that failed.
- [ ] **Deterministic ordering** — specify and enforce an ordering on every collection endpoint, tied to
      the cursor pagination TASK-021 established, tie-broken by id so equal sort keys cannot reshuffle
      between pages or between fetches.
- [ ] **Template accessibility** — phase-06 notification and email templates meet the same contrast and
      structure rules as the apps: semantic headings, real text rather than text-in-image, meaningful
      link text, table structure where tabular, and a plain-text alternative that carries the same
      information.

## Acceptance criteria

- [ ] Every validation error from every context carries the field path it belongs to; a test enumerates
      the contexts' error types and fails on a flat one.
- [ ] A multi-step wizard payload rejected at step three names the step-three field, not the envelope.
- [ ] Every collection endpoint has a specified ordering in its spec, enforced in the handler, and
      tie-broken by id — proven by a test that inserts rows with identical sort keys and asserts a
      stable order across repeated fetches and across page boundaries.
- [ ] Notification and email templates pass contrast and structure checks in both locales, with a
      plain-text alternative carrying the same information.
- [ ] No `/api/v1` response shape changed except the addition of field addressing on errors.

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

## Out of scope

Everything the browser renders: the axe sweep, manual keyboard/screen-reader/zoom/reduced-motion
passes, focus management, forms, charts, the theme contrast gate, the external audit, and the
accessibility statement — all frontend TASK-027.

## Files likely touched

- `crates/platform/http/**` (problem-details field addressing)
- every context's `application/**` validation error types, especially the phase 08–10 wizards
- every context's list handlers and their `api/*.yaml` ordering documentation
- notification and email templates plus their locale catalogs

## Notes / gotchas

- Rollback: nothing here is flagged. Field-addressed errors are additive (the message stays), and
  ordering changes are behaviour-preserving for any client that was not relying on an unspecified order
  — which is the point of specifying it.
- This task depends on TASK-022 rather than on the frontend performance work directly: ordering and
  error shapes interact with the cached SSR responses, and changing them after the cache lands means
  invalidating a warm cache for a cosmetic reason.
- Do this before frontend TASK-027 starts its remediation, not alongside it. A client that has already
  worked around a flat error string will have built a presentation that the fix then breaks.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-027 done --note "<PR or commit>"`
