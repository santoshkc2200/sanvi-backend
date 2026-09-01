# TASK-031: 11.a CI coverage repair & gate truth

**Phase:** 11 · **Status:** todo · **Size:** S
**Requirement(s):** NFR-1101, NFR-1108
**Depends on:** nothing — startable today, independent of phase 10
**Created:** 2026-09-01
**Flag:** none — CI and documentation only

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step below carries a verification command. Do not
> tick a step whose command has not been run and passed.

## Goal

Make the existing gates true. Six context crates ship integration tests that no CI job runs; the
scanner set is one scanner short of the claim made for it; and the measured test-suite baseline
contains two statements that are no longer correct. None of that is visible from a green build, which
is the problem.

## Why this shape

`pr.yml` has two test jobs. The unit job runs `--workspace --exclude sanvi-persistence --exclude
sanvi-messaging --exclude sanvi-tenancy`, because those three need Docker. The integration job then
names eight crates explicitly. Everything not in that list of eight, and not in the unit job, is
tested by nothing:

```
sanvi-access  sanvi-advertising  sanvi-audit  sanvi-identity  sanvi-platform-admin  sanvi-theming
```

All six have a `tests/` directory. A job that runs no tests still passes, so nothing reported it.

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

### Step 2: Restore the six crates to the integration job

**Files:** Modify `.github/workflows/pr.yml`

**Do:** replace the hand-written `-p` list in the integration job with the full set of
Docker-dependent workspace members. Keep the unit job's exclusions.

**Verify:** `just ci-test-coverage` → passes. Then
`cargo nextest run --profile ci -p sanvi-access -p sanvi-advertising -p sanvi-audit -p sanvi-identity -p sanvi-platform-admin -p sanvi-theming`
→ every test passes, or the failures are fixed in this task before it closes.

### Step 3: Wire the check into CI so the omission cannot recur

**Files:** Modify `.github/workflows/pr.yml`, `justfile`

**Do:** add `ci-test-coverage` to `just ci` and to the PR workflow. Add a self-test target
(`ci-test-coverage-self-test`) that removes a crate from the list in a temporary copy and asserts the
check fails, following the pattern `check-ads-invariants.sh` already uses.

**Verify:** `just ci-test-coverage-self-test` → passes (meaning: the check demonstrably fails on a
crate deliberately dropped from the list).

### Step 4: Add the missing SAST scanner

**Files:** Modify `.github/workflows/pr.yml`

**Do:** `cargo audit`, `cargo deny`, gitleaks, trivy and the CycloneDX SBOM already exist and already
block. Add SAST — `cargo clippy` with the security lint groups enabled, plus `semgrep` with the Rust
ruleset — as a blocking job. Do not add an IaC scanner: there is no IaC in this repository (recorded
in `docs/release/needs-humans.md`).

**Verify:** the SAST job fails on a deliberately introduced finding (a `unsafe` block with an
out-of-bounds index, reverted in the same PR), then passes on the clean tree.

### Step 5: Make the Gatekeeper exemption a detected prerequisite

**Files:** Modify `scripts/test.sh`, `CONTRIBUTING.md`

**Do:** the measured baseline shows ~97 % of a cold macOS run is `syspolicyd` scanning freshly linked
binaries, not testing. The exemption is per parent process, so it silently regresses whenever a new
terminal or agent host runs the suite. Have `scripts/test.sh` detect the un-exempted case on macOS and
print the one-line fix (`sudo spctl developer-mode enable-terminal`, then Developer Tools in System
Settings) as a warning before the run rather than leaving it as a subsection nobody reads.

**Verify:** `scripts/test.sh --quiet` on an exempted host prints no warning; the detection branch is
covered by a unit test of the check function.

### Step 6: Correct the measured baseline document

**Files:** Modify `docs/phase-11-hardening-ga/test-performance-baseline.md`

**Do:** the "Still open" section claims three crates are untested on every pull request. They are not —
`pr.yml` runs them in the integration job. Replace that claim with the accurate finding this task
fixes, and record `just clean-stale` as run with the resulting `target/debug/deps` file count, or
record why it was not.

**Verify:** the corrected document names six crates, not three, and cites the job that covers the
three.

## Definition of done

- [ ] `just ci-test-coverage` passes, and its self-test proves it fails on a dropped crate.
- [ ] All six previously-unrun crates' integration tests execute in CI and pass.
- [ ] SAST blocks in CI and was demonstrated to fail on a planted finding.
- [ ] `scripts/test.sh` warns on an un-exempted macOS host before a cold run.
- [ ] `test-performance-baseline.md` contains no claim contradicted by `pr.yml`.
- [ ] `just ci` is green.

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
