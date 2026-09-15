# TASK-031: 11.a CI coverage repair & gate truth

**Phase:** 11 · **Status:** done · **Size:** S
**Requirement(s):** NFR-1101, NFR-1108
**Depends on:** nothing — startable today, independent of phase 10
**Created:** 2026-09-01
**Flag:** none — CI and documentation only

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step below carries a verification command. Do not
> tick a step whose command has not been run and passed.

## Goal

Make the existing gates true. This plan opened believing six context crates shipped integration
tests that no CI job ran; measurement showed the unit job's `--workspace` selects them after all —
but by an accident nothing could see, because the integration job's crate list is hand-written, the
scanner set was one scanner short of the claim made for it, and the measured test-suite baseline
contained statements that were no longer correct. None of that is visible from a green build, which
is the problem. (Details and evidence in *Execution notes* and the per-step records below.)

## Why this shape

`pr.yml` has two test jobs. The unit job runs `--workspace --exclude sanvi-persistence --exclude
sanvi-messaging --exclude sanvi-tenancy`, because those three need Docker. The integration job then
names eight crates explicitly.

> **Measured correction (2026-09-15, see Execution notes):** the six crates named below *are*
> selected by the unit job's `--workspace` — `cargo nextest list` with that job's exact arguments
> runs their integration targets. The claim this plan was built on was wrong twice over (first
> about three crates, then about six); what was never wrong is the mechanism defect: the
> integration job's hand-written crate list, which nothing checked and which five of its eight
> entries duplicated. The task's deliverable — derive the list, and fail when any member with
> tests is not covered — stands; only its premise needed correcting.

The defect is the hand-maintained crate list, not the six missing entries. Fixing the list without
fixing the mechanism means the seventh crate is missed the same way. This task derives the list from
the workspace and fails when a crate with an integration target is not covered.

This task comes first in the phase because every later task adds a gate, and a gate wired into a job
that does not run the code it gates is decoration.

## Constraints

- No product behaviour changes. No `crates/contexts/**/src` edits except to fix a test that the
  restored coverage reveals as failing.
- If restoring a crate's integration tests turns the build red, **fix the test or the code — do not
  re-exclude the crate.** A red build here is the task working.
- Keep the two-job split. Docker-dependent crates stay in the integration job; the split exists for
  runtime, not for coverage.

## File ownership map

- `.github/workflows/pr.yml` — the two test jobs and the new coverage check
- `scripts/check-ci-test-coverage.sh` — asserts every crate with an integration target is run by CI
- `justfile` — `ci-test-coverage` target, wired into `just ci`
- `docs/phase-11-hardening-ga/test-performance-baseline.md` — corrected claims
- `CONTRIBUTING.md` and `scripts/test.sh` — Gatekeeper exemption as a detected prerequisite

## Steps

### Step 1: Write the coverage check and prove it fails today

**Files:** Create `scripts/check-ci-test-coverage.sh`, modify `justfile`

**Do:** the script enumerates workspace members that have a `tests/` directory, extracts the crate
names CI actually runs from the `cargo nextest` invocations in `.github/workflows/pr.yml`, and exits
non-zero listing any crate in the first set and not the second. Add `just ci-test-coverage`.

**Verify:** `just ci-test-coverage` → **FAILS**, naming exactly `sanvi-access`, `sanvi-advertising`,
`sanvi-audit`, `sanvi-identity`, `sanvi-platform-admin`, `sanvi-theming`.

> **Recorded 2026-09-15:** `just ci-test-coverage` → **passes**. The expected failure named a
> defect that does not exist in `pr.yml`: the unit job's `--workspace` minus three excludes selects
> every member's tests, including these six crates' integration targets — proven with `cargo
> nextest list` using the unit job's exact arguments. The failure mode the check exists for is real
> and is proven by the self-test instead, which drops a covered crate from a temporary copy of the
> workflow and asserts the check fails naming it (see Step 3).

### Step 2: Restore the six crates to the integration job

**Files:** Modify `.github/workflows/pr.yml`

**Do:** replace the hand-written `-p` list in the integration job with the full set of
Docker-dependent workspace members. Keep the unit job's exclusions.

**Verify:** `just ci-test-coverage` → passes. Then
`cargo nextest run --profile ci -p sanvi-access -p sanvi-advertising -p sanvi-audit -p sanvi-identity -p sanvi-platform-admin -p sanvi-theming`
→ every test passes, or the failures are fixed in this task before it closes.

> **Recorded 2026-09-15:** the integration job's crate list is now derived at run time by
> `check-ci-test-coverage.sh --integration-packages`: fifteen Docker-dependent members — the
> original eight plus `sanvi-access`, `sanvi-advertising`, `sanvi-audit`, `sanvi-identity`,
> `sanvi-platform-admin`, `sanvi-theming` and `sanvi-api`. `sanvi-http` is correctly excluded: its
> tests need no Docker. The six crates' suites ran locally with the job's exact nextest invocation
> against the shared test database: **424 tests run, 424 passed**.

### Step 3: Wire the check into CI so the omission cannot recur

**Files:** Modify `.github/workflows/pr.yml`, `justfile`

**Do:** add `ci-test-coverage` to `just ci` and to the PR workflow. Add a self-test target
(`ci-test-coverage-self-test`) that removes a crate from the list in a temporary copy and asserts the
check fails, following the pattern `check-ads-invariants.sh` already uses.

**Verify:** `just ci-test-coverage-self-test` → passes (meaning: the check demonstrably fails on a
crate deliberately dropped from the list).

> **Recorded 2026-09-15:** passes — the self-test drops `sanvi-http` (the first member shipping
> tests that no job excludes) from a temporary workflow copy and asserts the check names it, and
> asserts a workflow with no `cargo nextest run` invocation fails rather than passing silently.
> `ci-test-coverage` is wired into `just ci`/`just ci-quiet` and runs in its own PR job, with the
> self-test as a second step.

### Step 4: Add the missing SAST scanner

**Files:** Modify `.github/workflows/pr.yml`

**Do:** `cargo audit`, `cargo deny`, gitleaks, trivy and the CycloneDX SBOM already exist and already
block. Add SAST — `cargo clippy` with the security lint groups enabled, plus `semgrep` with the Rust
ruleset — as a blocking job. Do not add an IaC scanner: there is no IaC in this repository (recorded
in `docs/release/needs-humans.md`).

**Verify:** the SAST job fails on a deliberately introduced finding (a `unsafe` block with an
out-of-bounds index, reverted in the same PR), then passes on the clean tree.

> **Recorded 2026-09-15:** the clippy half failed on the planted `unsafe` block with a constant
> out-of-bounds index (`clippy::out_of_bounds_indexing`, correctness group); the canary was
> removed and the clean workspace passes the exact command. semgrep (`p/rust`, pinned 1.176.1)
> found 4 blocking `temp-dir` findings on the clean tree — all `#[cfg(test)]` fixtures named
> uniquely per run (uuid v7 / pid), not security operations — annotated with `nosemgrep` on the
> matching lines; the scan then passed with 0 findings. Blocking the security groups explicitly
> also surfaced two real, committed findings the existing `-D warnings` job cannot see (the
> `sanvi-payments` crate carries a blanket `#![allow(warnings)]`, which command-line group denies
> pierce): a duplicated attribute and an orphaned, mid-sentence doc comment attaching itself to
> `CheckoutRepository`. Both fixed in this task.

### Step 5: Make the Gatekeeper exemption a detected prerequisite

**Files:** Modify `scripts/test.sh`, `CONTRIBUTING.md`

**Do:** the measured baseline shows ~97 % of a cold macOS run is `syspolicyd` scanning freshly linked
binaries, not testing. The exemption is per parent process, so it silently regresses whenever a new
terminal or agent host runs the suite. Have `scripts/test.sh` detect the un-exempted case on macOS and
print the one-line fix (`sudo spctl developer-mode enable-terminal`, then Developer Tools in System
Settings) as a warning before the run rather than leaving it as a subsection nobody reads.

**Verify:** `scripts/test.sh --quiet` on an exempted host prints no warning; the detection branch is
covered by a unit test of the check function.

> **Recorded 2026-09-15:** this host reports `Developer mode is currently disabled.`, and
> `scripts/test.sh --quiet` prints the warning with the fix before anything runs — the
> un-exempted branch observed live. `scripts/test.sh --self-test` covers both branches of the
> decision function (warn on "disabled", silent on "enabled" and on an unavailable
> `DevToolsSecurity`) and passes.

### Step 6: Correct the measured baseline document

**Files:** Modify `docs/phase-11-hardening-ga/test-performance-baseline.md`

**Do:** the "Still open" section claims three crates are untested on every pull request. They are not —
`pr.yml` runs them in the integration job. Replace that claim with the accurate finding this task
fixes, and record `just clean-stale` as run with the resulting `target/debug/deps` file count, or
record why it was not.

**Verify:** the corrected document names six crates, not three, and cites the job that covers the
three.

> **Recorded 2026-09-15:** the document's "Still open" section now states the measured truth (the
> unit job selects all six crates' integration targets; what was missing was the mechanism, not
> the coverage) and records `just clean-stale` as run on 2026-09-15 — `cargo sweep --time 14`
> removed nothing (every artifact is under 14 days old); `target/debug/deps` holds 65,532 files
> and 122 executables against the 327,615 / 5,430 recorded on 2026-08-27.

## Execution notes (2026-09-15, branch `feat/task-031-ci-coverage-repair`)

- **The premise was stale; the mechanism was not.** `git log -L` on the unit job's nextest line
  shows its exclude set was never larger than `{sanvi-persistence, sanvi-messaging, sanvi-tenancy}`,
  so the six crates' integration tests have been selected by the unit job all along. What no job
  and no check guaranteed: that the integration job's hand-written list (five of whose eight
  entries duplicated the unit job) named everything that needs it, or that any future crate with
  `tests/` would be covered at all. The gate and the derived list are the deliverable, and they are
  proven by self-test rather than by today's accident of coverage.
- **SAST found real rot the existing gate cannot see.** `sanvi-payments/src/lib.rs` opens with a
  blanket `#![allow(warnings)]`, which silences the clippy job for that entire crate; explicit
  group denies (`-D clippy::suspicious`) pierce crate-level allows, which is how the duplicated
  attribute and the orphaned doc comment surfaced. **The blanket allow itself is left standing** —
  removing it is a lint-laundering pass over the whole crate and does not belong in an S-sized CI
  task. It is the first candidate for a follow-up.
- **Environment note for whoever runs the suite here:** this machine's Xcode license is
  unaccepted, so every `cc` invocation (test-binary linking, clippy build scripts) fails with
  exit 69. Fix: `sudo xcodebuild -license` (one-time, needs the user). The verifications above were
  run by putting the standalone Command Line Tools' `cc` on PATH with `SDKROOT` pointed at the
  Xcode SDK — no system state changed.
- **No CI run has executed these workflow changes yet**: this checkout has no git remote, so the
  PR workflow cannot be triggered from here. Every job's commands were run locally and green;
  the first push exercises them for real.
- **The gate and the derived list share one source.** Once the integration job's `-p` list moved
  into a run-time derivation, the gate's first battery run failed naming the three unit-excluded
  crates: the workflow text no longer contains the list it runs. The gate now expands the
  derivation marker (`steps.integration_packages.outputs.args`) with the same list
  `--integration-packages` computes, so it audits what the workflow will actually execute, not what
  it literally says.

## Definition of done

- [x] `just ci-test-coverage` passes, and its self-test proves it fails on a dropped crate.
- [x] All six previously-unrun crates' integration tests execute in CI and pass.
- [x] SAST blocks in CI and was demonstrated to fail on a planted finding.
- [x] `scripts/test.sh` warns on an un-exempted macOS host before a cold run.
- [x] `test-performance-baseline.md` contains no claim contradicted by `pr.yml`.
- [x] `just ci` is green.

*(Recorded 2026-09-15: the full gate ran as `just ci-quiet` — the same recipe set with the
low-noise test profile — exit 0: fmt, clippy `-D warnings`, layering, connect/ads/id-policy
invariants, `ci-test-coverage`, full workspace suite via `scripts/test.sh` with the `agent` profile
(**1113 tests run, 1113 passed**), OpenAPI drift (14 contexts OK), i18n (100 % ja coverage). The
verification block's `just test` is that same script without the quiet profile.)*

## Verification

```bash
just fmt
just clippy
just layering
just ci-test-coverage
just ci-test-coverage-self-test
just test
just openapi
```

## Out of scope

Any new gate that belongs to a later task (span coverage, route matrix, timeouts). Test-suite speed
work beyond the Gatekeeper warning — the measured baseline shows the suite itself is 22 seconds and
optimising it divides into that window.

## Parked

IaC scanning and release-artifact provenance attestation — see
[`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
