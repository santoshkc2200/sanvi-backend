# TASK-029: 11.10 Compliance evidence generation & propagation proof

**Phase:** 11 · **Status:** todo · **Size:** M
**Requirement(s):** FR-1132, FR-1133, FR-1134, FR-1135, NFR-1112
**Depends on:** TASK-028, **phase 10 merged**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** none — evidence slice

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Produce, from **live configuration** rather than from a document someone wrote once, the evidence each
jurisdiction expects — and prove by test that the obligations can be met, in every jurisdiction, inside
that jurisdiction's own clock.

## Why this shape

Phase 05 built jurisdiction profiles so that a new US state law is a data change plus a test. This task
is where that claim is tested.

**Evidence generation is an operation, not a document.** Each pack is produced by a named job from live
configuration with the generating build sha and timestamp embedded. A pack that cannot say which build
produced it is not evidence, and **hand-editing a generated pack destroys the only property that made
it worth generating**.

The original task ran the DSR work as live-fire drills with named human runners and a breach tabletop,
and ended with counsel reviewing the notice diff. Those are in
[`../../release/needs-humans.md`](../../release/needs-humans.md). The machinery they would exercise is
built here and asserted by automated tests timed against the statutory deadlines — which is what
actually catches a broken erasure path. The human judgement they would add is genuinely lost, and is
named as lost rather than quietly dropped.

## Constraints

- Determinism is the point: the same configuration and period produce an identical pack, and a
  configuration change is reflected in the next pack **without code changes**.
- Time the automated DSR path assertions against the **statutory** clock, not an internal SLA. An
  internal 14-day target that comfortably beats a 30-day deadline tells you nothing about the
  jurisdiction with a 15-day one.
- A gap this task exposes is a defect in the phase it came from and is fixed there, not papered over in
  the pack.

## File ownership map

- a new evidence-generation module reading the phase-05 data map, jurisdiction profiles, retention
  rules, and sub-processor registry
- `apps/cli/**` — `evidence-pack`, `dsr-check`, `notice-diff`
- the phase-05 `PersonalDataProvider` registry and its completeness test
- `docs/compliance/evidence/**` — generated artifacts
- erasure propagation adapters for archives, caches, and suppression lists

## Steps

### Step 1: Write the provider completeness test and let it fail

**Files:** Create `tests/compliance/provider_completeness.rs`

**Do:** enumerate registered `PersonalDataProvider`s and fail when one is missing from either the access
or the erasure path. **The most likely real-world defect is a context that quietly never joined** —
payments arrived in phase 09 and advertising in phase 10, both after the privacy centre was written.

**Verify:** `just test-provider-completeness` → fails on any context not wired into both paths; then
passes once wired. Add a throwaway context without registering a provider and confirm it fails.

### Step 2: Build the evidence pack generator

**Files:** Create the evidence-generation module; Create `apps/cli/src/commands/evidence_pack.rs`

**Do:** per jurisdiction — EU/UK, Japan, each US state in scope:
- Art. 30-style records of processing, generated from the phase-05 data map;
- risk assessments for the high-risk processing phases 09 and 10 added (payment data, advertising
  identifiers, conversion uploads);
- the sub-processor list with each party's role, purpose, location, and transfer mechanism, **including
  the ad platforms and Stripe, whose roles differ per jurisdiction**;
- retention rules as published *and* as executed, taken from TASK-028's per-class report so the two
  columns can be compared;
- DSR and appeal counters from TASK-020.

Stamp every pack with the generating build sha and period.

**Verify:** `just evidence-pack --jurisdiction all --period last-quarter` generates one pack per
jurisdiction. Run it twice on the same configuration and period → **byte-identical output**. Change one
configuration value → the next pack reflects it with no code change.

### Step 3: Assert the DSR paths against statutory clocks

**Files:** Create `tests/compliance/dsr_paths.rs`; Create `apps/cli/src/commands/dsr_check.rs`

**Do:** access, erasure, US opt-out including a GPC signal arriving on a storefront under a tenant
custom domain, and an appeal. Run each end to end on scale data and assert completion inside that
jurisdiction's statutory deadline.

**Verify:**
- `just dsr-check --jurisdiction all` passes, with each measured duration committed.
- GPC on a tenant custom domain suppresses tracking **on the very first request**, with the directive
  snapshot recorded. This is the case most likely to be quietly broken: the signal arrives on the
  tenant's own domain, a code path phase 05 never saw.

### Step 4: Verify erasure propagation reaches every destination

**Files:** Create erasure propagation adapters; Create `tests/compliance/erasure_propagation.rs`

**Do:** prove an erasure request reaches archives (TASK-028), backup policy notes, caches, search
indexes, ad-platform suppression lists (phase 10), and the payment provider's retained records — each
destination either purged or documented as lawfully retained with an end date.

**Verify:** a seeded subject is verifiably gone from every destination, with retained classes
enumerated and justified.

### Step 5: Assert the breach obligation calculator against a fixture

**Files:** Create `tests/compliance/breach_calculator.rs`

**Do:** the tabletop is parked, but the calculator it would exercise is testable. Assert it against a
deliberately awkward fixture: a multi-state US incident with one EU data subject and encrypted data at
rest, so the encryption safe-harbour assessment and the differing notification clocks all have to be
evaluated rather than assumed.

**Verify:** the calculator produces the correct obligations, deadlines, and safe-harbour conclusion for
the fixture.

### Step 6: Generate the notice diff

**Files:** Create `apps/cli/src/commands/notice_diff.rs`

**Do:** the generators produce the current notice text; the diff against the last approved version is
produced **by the generator**, not by someone reading two documents side by side. The approval step
waits on counsel (parked); the diff does not.

**Verify:** `just notice-diff` produces a diff against the stored approved version and is deterministic.

## Definition of done

- [ ] `just test-provider-completeness` fails on a context registered in neither path, proven in the PR.
- [ ] An evidence pack generates for every jurisdiction, stamped with build and period.
- [ ] Two runs on the same configuration and period are byte-identical; a configuration change is
      reflected without code changes.
- [ ] Every DSR path completes inside its jurisdiction's statutory deadline, with timings committed.
- [ ] GPC on a tenant custom domain suppresses tracking on the first request, with the snapshot recorded.
- [ ] A seeded subject is verifiably gone from every erasure destination; retained classes are justified.
- [ ] The breach calculator is correct on the multi-state / EU-subject / encrypted-at-rest fixture.
- [ ] `just notice-diff` is generated and deterministic.

## Verification

```bash
just fmt
just clippy
just test
just evidence-pack --jurisdiction all --period last-quarter
just test-provider-completeness
just dsr-check --jurisdiction all
just notice-diff
```

## Out of scope

Fixing whatever gap this task exposes — a gap found here is a defect in the phase it came from.
Frontend privacy-centre re-verification, notice diff surfacing, and evidence pack download (frontend
TASK-029).

## Parked

Live-fire DSR drills with named human runners, the breach tabletop, and counsel's approval of the
notice diff — see [`../../release/needs-humans.md`](../../release/needs-humans.md). Human judgement on
the notice text is a real loss, not an equivalent substitution.

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
