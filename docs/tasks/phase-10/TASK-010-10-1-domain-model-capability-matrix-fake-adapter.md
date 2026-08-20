# TASK-010: 10.1 Domain model, capability matrix & fake adapter

**Phase:** 10
**Status:** todo
**Requirement(s):** FR-1001, NFR-1008
**Depends on:** TASK-009
**Created:** 2026-08-20

**Slice:** 10.1 — Domain model, capability matrix & fake adapter
**Prerelease:** `v0.11.0-alpha.2` · **Flag:** `advertising.enabled`

## Context

Fix the unified campaign model and the shape of platform difference **before** either real adapter
exists. The deliverable is a domain that can express a Google campaign and a Meta campaign without
either one being the template, plus a fake adapter that proves it.

This is the task that decides whether "adding a third network is an adapter, not a rewrite" is true.
It is cheap now and unaffordable later. Build Google first and Google's shape *becomes* the
abstraction; Meta then arrives as a pile of special cases.

**The rule: a capability matrix, not a lowest common denominator.** The model does not shrink to what
both platforms share; it declares, per platform, which objectives, budget types, targeting dimensions,
creative specs, and character limits exist. A field Meta cannot express is *absent* for Meta, not
present-and-broken. The matrix is **backend-provided data**, so a platform adding an objective is a
data update, not a frontend release.

The fake adapter is a deliverable to `sanvi-frontend`, not test scaffolding: it is what lets the
frontend build TASK-011–TASK-013's screens before Google developer-token approval and Meta app review
land.

## What to do

- [ ] **Contract** — `GET /api/v1/tenant/ads/platforms` now returns the real catalog: per platform,
      display metadata, entitlement key, connection state (`not_connected` until TASK-011), and its
      **capability matrix** — objectives, budget types, minimum budgets by currency, schedule
      granularity, targeting dimensions, creative placements and asset specs, and per-locale text
      limits. The matrix is versioned (`matrix_version`) and cacheable.
- [ ] **Domain model** — `Campaign`, `AdGroup`, `Ad`, `Creative`, `Budget`, `Schedule`, `Objective`,
      `Targeting`, `BidSettings`, `DriftState` per the implementation plan. `Objective` and `Targeting`
      carry platform-specific payloads without a `serde_json::Value` escape hatch in the middle of the
      domain.
- [ ] **`CapabilityMatrix`** — a declarative per-platform structure covering everything the validation
      engine consults, with text field limits **per locale** (a Japanese headline limit is not the
      English one, and phase 06 makes that testable).
- [ ] **Validation engine** — takes a draft campaign tree plus a matrix and returns a structured list
      of violations with **field paths**, so the frontend can attach each one to its input. Not
      booleans, not strings: field-addressable violations, or the form engine cannot render them.
- [ ] **`AdPlatformPort`** — auth, list accounts, campaign-tree CRUD, fetch metrics, upload
      conversions. Every method takes an explicit connection handle; none of them reads ambient tenant
      state.
- [ ] **Port contract suite** — the shared test suite every adapter must pass. TASK-012 and TASK-013
      run this same suite; a divergence is an adapter bug, not a suite exception.
- [ ] **Fake adapter** — a complete in-process implementation with a deliberately *asymmetric*
      capability matrix: one objective Google-like, one Meta-like, one supported by neither, and a
      lifetime budget type one of them lacks. Serves the frontend, the tests, and the "third network"
      proof.
- [ ] **Catalog endpoint** — assembles matrices, entitlement state, and connection state. A
      non-entitled platform is listed with its upgrade state, never omitted.
- [ ] **Domain events** — declare the phase's event set with payloads and register them on the outbox:
      `AdAccountConnected/Disconnected`, `CampaignCreated/Updated/Paused/Resumed`, `CampaignDrifted`,
      `ConversionRecorded`, `ConversionUploaded/UploadFailed`, `SpendThresholdReached`,
      `MetricsIngested`. Later tasks emit them; the shapes stop moving here.

## Acceptance criteria

- [ ] The campaign tree, capability matrix, and validation engine are merged with **no**
      platform-specific branching in the domain layer (`just layering` plus a grep gate for platform
      literals under `src/domain/` and `src/application/`).
- [ ] The fake adapter passes the full `AdPlatformPort` contract suite.
- [ ] An objective/budget-type combination the matrix forbids is rejected with a field path; a
      per-locale character limit rejects a Japanese string that passes the English limit.
- [ ] Matrix symmetry test: for every platform, every capability the validation engine consults exists
      in the matrix — a missing key fails the test rather than silently permitting.
- [ ] `GET /ads/platforms` serves matrices for entitled platforms and lists non-entitled ones with
      their upgrade state.
- [ ] Every declared event payload is registered on the outbox and round-trips through serialization.
- [ ] The fake adapter's matrices are exported as fixtures consumable by `sanvi-frontend`.

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

Any real platform call, OAuth, tokens (TASK-011), the Google adapter (TASK-012), creatives
(TASK-013), and anything conversion- or metrics-shaped (TASK-014+).

## Files likely touched

- `api/advertising.yaml` (catalog response + matrix schema)
- `crates/contexts/advertising/src/domain/**` (model, matrix, validation, ports, events)
- `crates/contexts/advertising/src/application/**` (catalog query)
- `crates/contexts/advertising/src/adapters/{http,stubs,events}.rs`
- port contract suite under `crates/contexts/advertising/tests/`

## Notes / gotchas

- The matrix must be data the frontend consumes, not a shape the frontend mirrors. If a frontend PR
  ever needs a literal from it, the matrix is missing a field.
- Keep the fake's third matrix genuinely unlike both real platforms; a fake that resembles Google
  proves nothing in TASK-013.
- Violations without field paths are the single most common way this design fails — reject them in
  review.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-010 done --note "<PR or commit>"`
