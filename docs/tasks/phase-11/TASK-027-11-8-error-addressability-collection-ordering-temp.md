# TASK-027: 11.8 Error addressability, collection ordering & template accessibility

**Phase:** 11 · **Status:** todo · **Size:** S
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

- [ ] Every validation error from every context carries its field path; the enumeration test fails on a
      flat one.
- [ ] A wizard payload rejected at step three names the step-three field.
- [ ] Every collection endpoint has a specified ordering, enforced and tie-broken by id, stable across
      repeated fetches and page boundaries.
- [ ] Templates pass contrast and structure checks in both locales, with an equivalent plain-text
      alternative.
- [ ] No `/api/v1` response shape changed except error field addressing.

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

Everything the browser renders — the axe sweep, manual passes, focus management, forms, charts, the
theme contrast gate, and the accessibility statement — all frontend TASK-027.

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
