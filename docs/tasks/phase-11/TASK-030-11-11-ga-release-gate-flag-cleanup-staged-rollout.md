# TASK-030: 11.11 GA release gate, flag cleanup & v1.0.0

**Phase:** 11 · **Status:** todo · **Size:** M
**Requirement(s):** FR-1136, FR-1137, NFR-1101, NFR-1102
**Depends on:** TASK-024, TASK-025, TASK-026, TASK-027, TASK-029, **frontend TASK-030**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Release:** `v1.0.0`

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Close the GA checklist opened in TASK-019, prove the whole product one more time, remove the
scaffolding eleven phases accumulated, and cut `v1.0.0` in a release that can be reverted.

**This task adds nothing. Its entire output is confidence, plus a tag.**

## Why this shape

The original task shipped through a canary and a percentage ramp with an automatic halt. There is no
orchestrator that can route a percentage of traffic, so the rollout is a single-host deploy whose
rollback property is **rehearsed under load** rather than assumed — which is the part that actually
protects anything. Expand/contract migrations (TASK-023) are what make reverting a deploy safe: the
previous binary runs against the new schema. That property is verified in the rehearsal, not trusted.

## Constraints

- **Every flag removal is a separate commit.** A single "remove dead flags" commit that has to be
  reverted takes the good removals with it.
- The deprecation registry ships **empty**. Nothing is deprecated at 1.0.0; if something needs to be, it
  is fixed now rather than shipped with a sunset date on day one.
- After the tag, reverting is a **new release** rather than a rollback, and TASK-026's deprecation
  policy applies to anything the revert would change in the public contract.
- Anything discovered here that is not on the GA checklist goes to the post-GA backlog with a date.
  That is the mechanism that makes the phase end.

## File ownership map

- `docs/release/ga-checklist.md`, `CHANGELOG.md`, `docs/release/post-ga-backlog.md`
- flag definitions in `configs/**` and every call site of a removed flag — one commit per flag
- release workflow
- `benchmarks/**` — final artifacts and the full comparison report

## Steps

### Step 1: Re-run every gate

**Files:** none

**Do:** `just ci` plus every gate this phase added.

**Verify:** all green:
`ci-test-coverage`, `trace-coverage`, `log-pii-scan`, `runbook-xref`, `alert-rules-test`,
`route-matrix`, `rls-matrix`, `test-idor`, `check-timeouts`, `check-migration-expand-contract`,
`spec-lint`, `spec-freeze-check`, `replay-suite`, `partition-check`, `archival-roundtrip`,
`retention-report`, `test-provider-completeness`, `audit`, `sbom`.

### Step 2: Close the GA checklist

**Files:** Modify `docs/release/ga-checklist.md`

**Do:** every phase 00–11 acceptance criterion re-executed. Automated lines run; manual lines are
verified and dated; parked lines link to `needs-humans.md` and are listed in the release notes as known
gaps — **the penetration test and the external accessibility audit are named explicitly**, because a
GA that quietly omits them is dishonest.

**Verify:** `just ga-checklist-check` → no line without a state; no `automated` line unrun.

### Step 3: Re-run the full benchmark harness

**Files:** `benchmarks/**`

**Do:** every scenario on the release candidate, compared against the TASK-019 baseline **and against
each intervening candidate**. A regression introduced by a later slice is caught here rather than by a
customer.

**Verify:** `just perf-compare` green against the baseline and every intervening tag.

### Step 4: Remove dead flags, one commit each

**Files:** Modify `configs/**` and every call site

**Do:** execute the dead-flag list. Phase 11 flags to GA defaults: `platform.rate_limits`,
`platform.quotas`, `platform.ssr_cache`, `platform.load_shedding`, `platform.archival`,
`platform.tail_sampling` **on**; `platform.degraded_mode` operator-controlled.

**Verify:** `git log --oneline` shows one commit per removed flag; `just ci` green after each.

### Step 5: Rehearse migration up, down, and revert under load

**Files:** `docker-compose.staging.yml`

**Do:** apply and revert the release's migrations on a clone carrying scale data, timed. Then deploy
and revert the release itself under k6 load.

**Verify:**
- `just migrate && just migrate-down && just migrate` on scale data, with the duration recorded.
- The revert under load produces zero dropped requests, and the previous binary runs against the new
  schema.

### Step 6: Tag and publish

**Files:** `CHANGELOG.md`, release workflow

**Do:** tag `v1.0.0`. Publish the changelog generated from Conventional Commits covering everything
since `0.11.0`, the generated public API docs, and the SBOM from TASK-024. Confirm the deprecation
registry is empty.

**Verify:** the tag exists; changelog, API docs, and SBOM are attached; the deprecation registry has
zero entries.

### Step 7: Record the post-GA backlog

**Files:** Create `docs/release/post-ga-backlog.md`

**Do:** everything time-boxed out of phase 11 — deferred findings, injection-suite follow-ups, the
quarterly review of the lab budgets, and every line in `needs-humans.md` — recorded with a date.

**Verify:** every parked GA-checklist line appears in the post-GA backlog with a date.

## Definition of done

- [ ] Every gate from every phase-11 task is green.
- [ ] `docs/release/ga-checklist.md` has no line without a state; parked lines are named in the release
      notes as known gaps, including the absent penetration test and accessibility audit.
- [ ] `just perf-compare` is green against the baseline and every intervening candidate.
- [ ] Dead flags are removed one commit each; phase 11 flags are at GA defaults.
- [ ] Migration up/down is rehearsed and timed on scale data.
- [ ] A deployed release is reverted under load with zero dropped requests.
- [ ] `v1.0.0` is tagged with changelog, public API docs, and SBOM published; the deprecation registry
      is empty.
- [ ] `docs/release/post-ga-backlog.md` records every deferred item with a date.
- [ ] Frontend TASK-030 is done.

## Verification

```bash
just ci
just openapi
just replay-suite
just perf
just perf-compare
just ga-checklist-check
just migrate && just migrate-down && just migrate
just sbom
```

## Out of scope

Any new capability whatsoever.

## Parked

Staged canary rollout with a percentage ramp and automatic halt, provenance attestation, and a rollback
rehearsed by someone who did not write the release — see
[`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
