# TASK-013: 10.4 Meta adapter & creatives

**Phase:** 10
**Status:** in-progress — CBO variant landed; live-credential verification pending
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

- [x] **Contract** — no new campaign endpoints; TASK-012's surface is platform-agnostic and Meta
      arrives as data. New here: `GET/POST /creatives`, `GET/DELETE /creatives/{id}`,
      `GET /creatives/{id}/previews?placement=`. The capability matrix gains its creative half for
      **both** platforms: placements, asset specs (dimensions, aspect ratios, file size, duration),
      text fields with per-locale limits, and which combinations are valid together. `previews` returns
      per-placement render metadata — proxied where a platform offers a native preview, rendered from
      the spec where it does not.
- [ ] **Meta adapter** — Marketing API campaign/ad-set/ad CRUD against the pinned version, implementing
      the same `AdPlatformPort` and passing the same contract suite as the fake and Google. Still blocked
      on Meta app review, same as TASK-011/012's real Google adapter; the fake adapter's `meta` matrix
      covers the interim.
- [x] **Meta capability matrix** — broadened from 2 objectives/1 budget type/1 placement to 6 objectives,
      2 budget types (`daily`, `lifetime`), 6 targeting dimensions, and 3 placements
      (`feed`/`stories`/`reels`), each with its own asset spec. **Done 2026-09-16:** the
      campaign-budget-optimization `BudgetType::CampaignBudgetOptimization` variant exists — Meta's
      matrix declares it (matrix_version bumped to 2026-09-16 for the frontend schema cache), Google's
      does not, so validation rejects it for Google on the `budget.kind` path; the Meta adapter maps it
      to the campaign-level daily cap, and the exported frontend fixture JSON carries it. **Still
      open:** explicit ad-set-level scheduling.
- [x] **Creative model** — `Creative`/`StoredCreative` persisted via `CreativeRepository` (new port +
      Postgres impl), with assets, per-locale copy, and a placement link. Asset storage reuses
      `course-media` references (`asset_references: Vec<String>`) rather than a second uploader.
- [x] **Spec validation** — `CreativeAssetSpec::validate_asset` / `CapabilityMatrix::validate_creative_text`
      (shipped earlier in 4d59e4b/add5edf) now run inside `CreateCreative::execute`, at creative-create
      time — before persistence, not at ad publish time.
- [x] **Publish & review** — `add_ad`/`update_ad` now validate, before `apply_mutation`, that
      `fields.creative.id` parses as a UUID, resolves via `CreativeRepository::find`, and fits the
      campaign's platform: its placement must exist in that platform's `CapabilityMatrix` and match the
      stored creative's own placement (a payload cannot claim a different placement than the creative it
      references). Missing/unparseable id → `AdvertisingError::CreativeNotFound` /
      `InvalidInput`; placement mismatch or unsupported placement → `AdvertisingError::Validation`. Review
      *state* (per-ad rejection reasons surfaced from the platform) is still unimplemented — that has no
      meaning until a real adapter exists, same blocker as TASK-012's Google adapter.
- [x] **Drift for Meta** — added
      `reconciliation_sweep_detects_natively_paused_meta_ad_set_as_drift` running the existing engine
      against the Meta fixture; `reconciliation.rs`/`reconciliation_sweep.rs` are untouched by this task's
      diff (verified via `git diff --stat`).
- [x] **Rate limiting** — `rate_limited_registry` now takes `Fn(&PlatformKey) -> (usize, usize)` instead
      of one shared pair; new test proves Google and Meta get independently-enforced concurrency caps.

## Acceptance criteria

- [ ] A campaign created in Sanvi appears live in a Meta test account with matching budget, schedule,
      and objective. **Blocked** — no live Meta test account access.
- [ ] The adapter contract suite is green against recorded Meta fixtures and a Meta test account, with
      **no** suite exceptions added for Meta. **Blocked** — same reason; the fake-adapter contract suite
      is green, but that is not this criterion.
- [x] The reconciliation engine detects a natively paused Meta ad set as drift with **no engine
      changes** — verified: the new test passes and `reconciliation.rs`/`reconciliation_sweep.rs` have
      zero diff in this task's commits.
- [x] An asset failing one placement's spec is rejected for that placement only, at upload — covered by
      existing `domain::matrix::tests` (shipped in 4d59e4b) plus the new
      `creative_crud_and_matrix_validation_lifecycle` integration test.
- [x] Identical copy passes in English and fails in Japanese where Meta's limit is shorter, per field —
      `domain::matrix::tests::identical_headline_passes_english_and_fails_shorter_japanese_limit`.
- [x] `just layering` and the platform-literal grep gate stay green — verified locally
      (`./scripts/check-layering.sh`, `./scripts/check-ads-invariants.sh`; the latter's frontend-bundle
      scan still needs `ADVERTISING_FRONTEND_BUNDLE`, which only CI can supply — see Notes).
- [x] The fake adapter's third matrix (`asymmetric_demo`) still validates unchanged — covered by
      `adapters::stubs::tests::every_validator_capability_has_complete_matrix_data` and
      `third_fixture_registers_through_the_same_catalog_path`.

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
- Verification run locally on 2026-08-26 (no docker frontend bundle available): `cargo fmt --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `./scripts/check-layering.sh`,
  `cargo test -p sanvi-advertising` (all green, 92 tests), `cargo run -p sanvi-cli -- openapi --check`
  all pass. `./scripts/check-ads-invariants.sh` passes every check except `scan_frontend_bundle`, which
  is intentionally fail-closed without a CI-built `sanvi-frontend` artifact — not something this task can
  satisfy outside CI. `cargo nextest run --workspace` had 15 unrelated failures, all
  `sanvi-privacy::privacy_integration` docker `WaitContainer(StartupTimeout)` — confirmed environmental:
  `cargo nextest run -p sanvi-privacy` alone passes 57/57. No `sanvi-advertising` test failed in either
  run.
  - Superseded 2026-08-27: the clippy gate no longer passes `--all-features`. No crate in the workspace
    defines a `[features]` table, so the flag only added resolver overhead and split the build cache
    between invocations that passed it and ones that did not. The command above is left as-run for the
    record; the current gate is `cargo clippy --workspace --all-targets -- -D warnings`, defined in
    `justfile` and `.github/workflows/pr.yml`.
- **Publish & review (creative_id validation), verified 2026-08-27**: `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `./scripts/check-layering.sh`,
  `cargo test -p sanvi-advertising` (106 tests: 70 lib + 36 integration, all green — integration run
  against the shared `scripts/test-db.sh` Postgres/Redis pair, not per-test testcontainers; the default
  testcontainers Postgres image is too old for the `CREATE DATABASE ... STRATEGY` clause `testsupport`
  uses, an unrelated pre-existing environment gap, not a regression), and
  `cargo run -p sanvi-cli -- openapi --check` all pass. `./scripts/check-ads-invariants.sh` still fails
  only `scan_frontend_bundle` for the same CI-only-artifact reason as the 2026-08-26 run above.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
