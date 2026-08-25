# TASK-030: 11.11 GA release gate, flag cleanup, staged rollout & v1.0.0

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1136, FR-1137, NFR-1101, NFR-1102
**Depends on:** TASK-024, TASK-025, TASK-026, TASK-027, TASK-029
**Created:** 2026-08-21

**Slice:** 11.11 — GA release: e2e completion, browser matrix, staged rollout & `v1.0.0`
**Prerelease:** `v1.0.0-rc.12` → **release `v1.0.0`**

## Context

Close the GA checklist opened in TASK-019, prove the whole product one more time, remove the scaffolding
phases 00–10 accumulated, and cut `v1.0.0` in a rollout that can be reversed at any point.

**This task adds nothing. Its entire output is confidence, plus a tag.**

It cannot tag until the frontend's TASK-030 is also done — this is the one cross-repo edge that runs
frontend-to-backend rather than the other way round.

## What to do

- [ ] **Contract** — frozen in TASK-026 and unchanged here. Two release-time obligations: **public API
      documentation and the changelog are published with the tag**, generated from the spec and from
      Conventional Commits, covering everything since `0.11.0`; and **the deprecation registry ships
      empty** — nothing is deprecated at 1.0.0. If something needs to be, it is fixed now rather than
      shipped with a sunset date on day one.
- [ ] **Release gate re-run** — every phase 00–11 acceptance criterion re-executed as an automated or
      documented gate, with the owner from `docs/release/ga-checklist.md` signing the line. Any line that
      cannot be re-run automatically is marked manual with the date it was last verified.
- [ ] **Full benchmark re-run** — the TASK-019 harness across all scenarios on the release candidate,
      with `perf-compare` against `rc.1` **and against each intervening slice**. A regression introduced
      by a later slice is caught here rather than by a customer.
- [ ] **Feature flag cleanup** — execute the dead-flag list from TASK-019: flags for shipped phases
      removed from code and configuration; phase 11 flags set to their GA defaults —
      `platform.rate_limits`, `platform.quotas`, `platform.ssr_cache`, `platform.load_shedding`,
      `platform.archival`, `platform.tail_sampling`, `platform.status_probes` **on**;
      `platform.degraded_mode` operator-controlled. **Every removal is a separate commit** so it is
      separately revertable.
- [ ] **Migration and rollback rehearsal** — the release's migrations applied and reverted on a staging
      clone carrying scale data, timed; the rollback procedure rehearsed by someone who did not write
      the release.
- [ ] **Staged rollout** — canary, then a percentage ramp with an automatic halt on SLO burn or error
      rate, with each stage's dwell time **defined in advance** so nobody has to decide during the ramp.
- [ ] **Version and changelog** — tag `v1.0.0`, publish the changelog, the SBOM, and the provenance
      attestation from TASK-024.
- [ ] **Post-GA backlog** — everything time-boxed out of phase 11 (accepted pen-test mediums, deferred
      findings, chaos-drill follow-ups, quarterly SLO review) recorded with owners and dates, and the
      first quarterly SLO review scheduled.

## Acceptance criteria

- [ ] `docs/release/ga-checklist.md` has every line owned, signed, and dated.
- [ ] Both parent plans' acceptance criteria are fully checked.
- [ ] No high or critical pen-test finding is open; external a11y findings are remediated and the
      accessibility statement is published.
- [ ] Every SLO has an SLI, dashboard, burn-rate alert, and owner; the error budget policy is published.
- [ ] Restore, PITR, single-tenant restore, and rotation drills are all completed with recorded timings.
- [ ] `/api/v1` is frozen, documented, rate-limited, and green on the replay suite.
- [ ] Load, spike, and soak reports are attached to the release with no regression against the stored
      baselines, `perf-compare` green against `rc.1` and every intervening rc.
- [ ] Migration up/down is rehearsed and timed on scale data, and a deployed release is reverted under
      live traffic with zero dropped requests.
- [ ] Dead flags are removed (one commit each) and phase 11 flags are at their GA defaults.
- [ ] `v1.0.0` is tagged with the changelog, public API docs, SBOM, and provenance published, and the
      deprecation registry is empty.
- [ ] The post-GA backlog is recorded with owners and dates, and the first quarterly SLO review is on a
      calendar.

## Verification

```bash
just ci
just openapi
just replay-suite rc.12
just perf
just perf-compare rc.1 rc.12
just ga-checklist-check
just migrate && just migrate-down && just migrate
just sbom
```

## Out of scope

Any new capability whatsoever. Anything discovered here that is not on the GA checklist goes to the
post-GA backlog with an owner and a date — that is the mechanism that makes the phase end.

## Files likely touched

- `docs/release/ga-checklist.md`, `CHANGELOG.md`, `docs/release/post-ga-backlog.md`
- flag definitions in `configs/**` and every call site of a removed flag (one commit per flag)
- release workflow (staged rollout stages, dwell times, automatic halt conditions)
- `benchmarks/**` (`rc.12` artifacts and the full comparison report)

## Notes / gotchas

- **The staged rollout is the rollback plan**: each stage can halt and revert to the previous release,
  rehearsed under load in this task. Migrations are expand/contract (TASK-023), so the previous binary
  runs against the new schema — that property is what makes reverting a deploy safe, and it is verified
  in the rehearsal rather than trusted.
- After the tag, reverting is a **new release** rather than a rollback, and TASK-026's deprecation policy
  applies to anything the revert would change in the public contract.
- One flag per commit is not bureaucracy. A single "remove dead flags" commit that has to be reverted
  takes the good removals with it.
- Cross-repo: this task cannot tag `v1.0.0` until `sanvi-frontend` TASK-030 is done.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
