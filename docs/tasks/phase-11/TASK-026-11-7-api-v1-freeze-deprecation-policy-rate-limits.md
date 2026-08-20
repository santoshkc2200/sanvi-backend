# TASK-026: 11.7 API v1 freeze, deprecation policy, rate limits & quotas

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1122, FR-1123, FR-1124, FR-1125, FR-1126, NFR-1106
**Depends on:** TASK-021, TASK-023
**Created:** 2026-08-21

**Slice:** 11.7 — API `v1` freeze, deprecation policy, rate limits & quotas
**Prerelease:** `v1.0.0-rc.8` · **Flags:** `platform.rate_limits`, `platform.quotas`

## Context

Turn thirteen independently-grown OpenAPI documents into one public contract we are willing to be held
to for years, and put enforceable, **visible** limits around it.

**This is the last task in which breaking changes are cheap.** After it, every change costs a
deprecation window. An inconsistency found here is a fix; found later it is a six-month sunset.

It depends on TASK-021 because per-plan limits are derived from the measured capacity model, and on
TASK-023 because `429` and `503` semantics have to be decided together or clients get backoff wrong in
both directions.

## What to do

- [ ] **Contract** — this task *is* the contract work, across all thirteen documents in `api/`:
      - **Consistency pass** — resource naming, pluralisation, casing, pagination (cursor everywhere,
        one parameter set), filtering and sorting grammar, timestamp and money representation, enum
        naming, and the problem-details envelope with one namespaced type registry.
      - **Rate-limit and quota semantics** — `429` with `Retry-After`, `RateLimit-Limit`,
        `RateLimit-Remaining`, `RateLimit-Reset`; documented burst behaviour; and the explicit
        distinction from TASK-023's `503 platform/overloaded`.
      - **Deprecation policy** — six months' notice, `Deprecation` and `Sunset` headers, a changelog
        entry and migration note per deprecation, published **before** anything is deprecated.
      - **Quota endpoint** — `GET /api/v1/tenant/usage` returning current-period usage and limit per
        metered dimension, so the UI can show a number rather than an error.
- [ ] **End-to-end spec review** — all thirteen documents read as one API by two reviewers, with a
      written style guide extracted from the result and enforced by a spectral-style linter in CI.
- [ ] **Freeze `/api/v1`** — once the consistency pass merges, a CI job diffs the spec against the
      frozen snapshot and fails on any breaking change without an explicit, reviewed exception label.
- [ ] **Rate limiting** — per-plan limits enforced at the HTTP boundary, keyed by tenant **and** by API
      credential, with a separate stricter bucket for unauthenticated and public endpoints (the phase-10
      tracking endpoint and phase-08 domain verification most of all). Limits derived from TASK-021's
      capacity model, with the derivation written down next to the numbers.
- [ ] **Quotas** — metered dimensions per plan (API requests, tracked conversions, storage, custom
      domains, seats, notification sends), counted in a way that survives a restart, exposed through the
      usage endpoint, and enforced with an **entitlement-aware error that names the upgrade path**
      rather than saying "forbidden".
- [ ] **Public API documentation** — generated from the spec, with worked examples per journey and per
      locale (JPY and zero-decimal handling appear in the examples, not in a footnote), published with
      the release.
- [ ] **Backwards-compatibility replay suite** — real client requests recorded from staging (scrubbed),
      replayed against each release candidate, asserting status, shape, and header compatibility. **This
      is the gate that makes the freeze real rather than aspirational.**
- [ ] **Deprecation machinery** — header emission, changelog generation, and a registry of deprecated
      elements with their sunset dates, so the policy is enforced by the build rather than remembered.

## Acceptance criteria

- [ ] All thirteen specs pass the style linter, and the linter blocks a deliberately inconsistent
      endpoint — proven in the PR.
- [ ] `/api/v1` is frozen; the breaking-change gate fails on a removed field, a narrowed enum, and a
      changed required-ness, and passes on an additive change.
- [ ] The deprecation policy is published, with header emission and changelog generation working end to
      end against a test deprecation.
- [ ] Per-plan rate limits are enforced behind `platform.rate_limits` with correct headers, and the
      derivation from the capacity model is recorded next to the numbers.
- [ ] `429` is never returned where `503` is meant, and the public tracking endpoint's stricter bucket
      holds under a flood.
- [ ] Quota counting survives a worker restart and a Redis flush — counters reconcile from Postgres.
- [ ] `GET /api/v1/tenant/usage` returns the same numbers enforcement uses. A UI showing a different
      number from the one that blocks the request is worse than no UI.
- [ ] A quota breach returns an entitlement-aware error naming the upgrade path, not a bare `403`.
- [ ] Public API docs are published with per-journey, per-locale worked examples including JPY.
- [ ] The replay suite runs on every release candidate, is green against this one, and fails on a
      deliberately broken response shape.

## Verification

```bash
just fmt
just clippy
just test
just openapi
just spec-lint
just spec-freeze-check
just replay-suite rc.8
just test-quotas
```

## Out of scope

Frontend `429` handling, quota surfaces, limit-reached states, and the platform-admin override view
(frontend TASK-026). Abuse quotas on specific surfaces, which are a security control rather than a
commercial limit (TASK-024). Removing the flags and setting GA defaults (TASK-030).

## Files likely touched

- all thirteen `api/*.yaml` documents, plus `api/shared/*.yaml` (problem types, pagination parameters)
- `crates/platform/http/**` (rate-limit middleware, `RateLimit-*` headers, deprecation headers)
- a new quota-counting module with Postgres reconciliation and Redis fast path
- `api/tenant_usage.yaml` or the tenant document's `/usage` path
- `scripts/spec-lint.*`, `scripts/spec-freeze-check.*`, `tests/replay/**`
- `docs/api/{style-guide,deprecation-policy}.md`, generated public docs

## Notes / gotchas

- Rollback: `platform.rate_limits` and `platform.quotas` off return to unlimited (the pre-slice
  behaviour) **while keeping the counting path live**, so usage data keeps accruing and the limits can
  be switched on again with real numbers behind them. Both roll out in shadow mode first — count and
  report, do not enforce — for one full traffic cycle, with the observed would-have-blocked rate
  recorded in the PR before enforcement. **The spec freeze does not roll back.**
- The usage endpoint and the enforcement path must read the same counter. Two counters that agree today
  diverge the first time one of them is restarted.
- This task is where the phase's one permitted new user-facing capability lives. Everything else the
  team wants to add "while we're in there" goes to the post-GA backlog.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-026 done --note "<PR or commit>"`
