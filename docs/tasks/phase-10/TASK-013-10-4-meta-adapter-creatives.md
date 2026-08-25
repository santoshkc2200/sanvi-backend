# TASK-013: 10.4 Meta adapter & creatives

**Phase:** 10
**Status:** todo
**Requirement(s):** FR-1007, NFR-1008
**Depends on:** TASK-012
**Created:** 2026-08-20

**Slice:** 10.4 — Meta adapter & creatives
**Prerelease:** `v0.11.0-alpha.5` · **Flag:** `advertising.meta`

## Context

The second network. Meta's campaign tree lands through the same port, the same validation engine, and
the same builder — with **no new frontend code**, only a new capability matrix. If that is not true,
the abstraction from TASK-010 failed and this is where we find out cheaply.

Creatives land here too, because creative specs are the most platform-divergent part of the model and
Meta's placement matrix is what makes the generic version honest.

**The rule: every frontend line added for Meta is a bug in TASK-010.** Deviations are permitted only
for genuinely visual concerns (a placement preview frame), never for field logic, validation, or
availability. Any `if (platform === 'meta')` in a form path is fixed in the matrix instead.

Runs in parallel with TASK-014 once TASK-012 is on staging.

## What to do

- [ ] **Contract** — no new campaign endpoints; TASK-012's surface is platform-agnostic and Meta
      arrives as data. New here: `GET/POST /creatives`, `GET/DELETE /creatives/{id}`,
      `GET /creatives/{id}/previews?placement=`. The capability matrix gains its creative half for
      **both** platforms: placements, asset specs (dimensions, aspect ratios, file size, duration),
      text fields with per-locale limits, and which combinations are valid together. `previews` returns
      per-placement render metadata — proxied where a platform offers a native preview, rendered from
      the spec where it does not.
- [ ] **Meta adapter** — Marketing API campaign/ad-set/ad CRUD against the pinned version, implementing
      the same `AdPlatformPort` and passing the same contract suite as the fake and Google.
- [ ] **Meta capability matrix** — objectives, budget types including campaign budget optimization,
      ad-set-level scheduling, targeting dimensions, placements, and per-locale text limits. Where Meta
      expresses something Google cannot (and vice versa), the matrix carries it and the domain does not
      branch.
- [ ] **Creative model** — `Creative` persisted with assets, per-locale copy, and placement links.
      Asset storage reuses the `course-media` upload path rather than a second uploader.
- [ ] **Spec validation** — an asset is checked against the target placements' specs **at upload time**,
      not at publish time. Discovering a wrong aspect ratio after a failed publish is the worst possible
      moment to discover it.
- [ ] **Publish & review** — creatives publish with their ads; Meta's review status and rejection
      reasons flow through TASK-012's review pipeline verbatim.
- [ ] **Drift for Meta** — TASK-012's reconciliation engine runs **unchanged** against the Meta adapter.
      This is the assertion that drift detection is platform-agnostic, not Google-shaped.
- [ ] **Rate limiting** — Meta's per-app and per-account limits differ in kind from Google's quota; the
      request queue gains a Meta-specific budget rather than inheriting a shared assumption.

## Acceptance criteria

- [ ] A campaign created in Sanvi appears live in a Meta test account with matching budget, schedule,
      and objective.
- [ ] The adapter contract suite is green against recorded Meta fixtures and a Meta test account, with
      **no** suite exceptions added for Meta.
- [ ] The reconciliation engine detects a natively paused Meta ad set as drift with **no engine
      changes** (diff the engine's source in the PR to prove it).
- [ ] An asset failing one placement's spec is rejected for that placement only, at upload.
- [ ] Identical copy passes in English and fails in Japanese where Meta's limit is shorter, per field.
- [ ] `just layering` and the platform-literal grep gate stay green — no Meta branch entered the domain
      or application layers.
- [ ] The fake adapter's third matrix still validates unchanged, proving the abstraction absorbed a
      second real platform.

## Verification

```bash
just fmt
just clippy
just layering
just ads-invariants
cargo test -p sanvi-advertising
just test
just openapi
```

## Out of scope

Meta Insights ingestion (TASK-016) and the Meta Conversions API sink (TASK-015) — this task is the
campaign tree and creatives only.

## Files likely touched

- `api/advertising.yaml` (creative endpoints, creative half of the matrix schema)
- `crates/contexts/advertising/src/adapters/meta/**`
- `crates/contexts/advertising/src/domain/creative.rs`, matrix extensions
- `crates/contexts/advertising/src/application/creatives/**`
- request-queue configuration in `apps/worker/**`

## Notes / gotchas

- Rollback: `advertising.meta` off → Meta disappears from the catalog and its endpoints return
  `503 advertising/platform-unavailable`; Google is untouched. Meta connections and tokens are left
  intact and refreshing, per TASK-011's rollback rule. Live Meta campaigns keep spending.
- If this task needs a domain change to fit Meta, that change belongs in the matrix — reopen TASK-010's
  design rather than branching here.
- Reuse `course-media`; a second uploader in this context is a maintenance liability and a CSP surprise.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
