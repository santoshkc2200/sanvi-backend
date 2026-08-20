# TASK-029: 11.10 Compliance evidence packs & jurisdiction drills

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1132, FR-1133, FR-1134, FR-1135, NFR-1112
**Depends on:** TASK-028
**Created:** 2026-08-21

**Slice:** 11.10 — Compliance evidence packs & jurisdiction drills
**Prerelease:** `v1.0.0-rc.11` · **Flag:** none — evidence slice

## Context

Produce, from **live configuration** rather than from a document someone wrote once, the evidence each
jurisdiction expects — and then prove the obligations can actually be met by running them against the
clock, in every jurisdiction, before a regulator does it for us.

Phase 05 built jurisdiction profiles so that a new US state law is a data change plus a test. This task
is where that claim is tested: the evidence is *generated*, so it cannot drift from the system, and the
drills are *timed against the jurisdiction's own deadline*, not against a comfortable internal SLA.

It depends on TASK-028 because a retention claim is only evidence once the retention report proves the
rules run, and it consumes TASK-020's DSR counters and TASK-024's sub-processor and security posture.

## What to do

- [ ] **Contract** — no new public endpoints. One internal contract, exercised by the platform console:
      **evidence generation is an operation, not a document.** Each pack is produced by a named job from
      live configuration (jurisdiction profiles, data map, retention rules, sub-processor registry,
      directive statistics) with the generating build sha and timestamp embedded. A pack that cannot say
      which build produced it is not evidence.
- [ ] **Evidence pack generator, per jurisdiction** — EU/UK, Japan, and each US state in scope:
      - Art. 30-style records of processing, generated from the phase-05 data map;
      - risk assessments / DPIA-equivalents for the high-risk processing added in phases 09 and 10
        (payment data, advertising identifiers, conversion uploads);
      - the sub-processor list with each party's role, purpose, location, and transfer mechanism —
        including the ad platforms and Stripe, **whose roles differ per jurisdiction**;
      - retention rules as actually published *and* as actually executed, taken from TASK-028's
        per-class report so the two columns can be compared;
      - DSR and appeal metrics for the period, from TASK-020's counters.
- [ ] **Live-fire DSR drill per jurisdiction** — access, erasure, US opt-out (including a Global Privacy
      Control signal arriving on a storefront under a tenant custom domain), and an appeal. Each run end
      to end on scale data, **timed against that jurisdiction's statutory clock**, with the runner and
      result recorded.
- [ ] **Cross-context completeness check** — the access and erasure paths must cover every context's
      `PersonalDataProvider`, including the ones registered latest (payments in phase 09, advertising in
      phase 10). A test enumerates registered providers and fails when one is missing from either path —
      the most likely real-world defect is a context that quietly never joined.
- [ ] **Breach tabletop** — exercise the per-jurisdiction obligation calculator end to end on a
      deliberately awkward scenario: a multi-state US incident with one EU data subject and encrypted
      data at rest, so the encryption safe-harbour assessment and the differing notification clocks all
      have to be evaluated rather than assumed. Output is a timeline with decisions and their bases.
- [ ] **Deletion propagation verification** — an erasure request is proven to reach archives (TASK-028),
      backup policy notes, caches, search indexes, ad-platform suppression lists (phase 10), and the
      payment provider's retained records, with each destination either purged or documented as lawfully
      retained with an end date.
- [ ] **Notice and notice-at-collection review** — the generators produce the current notice text; the
      diff against what counsel last approved is produced **by the generator**, not by someone reading
      two documents side by side. Counsel reviews the diff against what the system actually does.

## Acceptance criteria

- [ ] An evidence pack generates for every jurisdiction in scope, stamped with build and period.
- [ ] Generator determinism: the same configuration and period produce an identical pack, and a
      configuration change is reflected in the next pack **without code changes**.
- [ ] Adding a context without registering a `PersonalDataProvider` fails the completeness test — proven
      in the PR.
- [ ] A live-fire DSR drill in every jurisdiction completes **inside that jurisdiction's legal
      deadline**, with the timings committed.
- [ ] GPC on a tenant custom domain suppresses tracking on the very first request, with the directive
      snapshot recorded.
- [ ] A seeded subject is verifiably gone from every erasure destination, with retained classes
      enumerated and justified.
- [ ] The breach calculator produces the correct obligations, deadlines, and safe-harbour conclusion for
      the awkward multi-state scenario, asserted against a fixture.
- [ ] The notice diff is generated rather than hand-assembled, and counsel has approved it.

## Verification

```bash
just fmt
just clippy
just test
just evidence-pack --jurisdiction all --period last-quarter
just test-provider-completeness
just dsr-drill --jurisdiction all
just breach-tabletop --scenario multi-state-eu-encrypted
just notice-diff
```

## Out of scope

Fixing whatever gap this task exposes: a gap found here is a defect in the phase it came from and is
fixed there, not papered over in the pack. Frontend privacy-centre re-verification, notice diff
surfacing, and evidence pack download (frontend TASK-029).

## Files likely touched

- a new evidence-generation module reading the phase-05 data map, jurisdiction profiles, retention
  rules, and sub-processor registry
- `apps/cli/**` (`evidence-pack`, `dsr-drill`, `breach-tabletop`, `notice-diff`)
- the phase-05 `PersonalDataProvider` registry and its completeness test
- `docs/compliance/{evidence,drills,tabletop}/**` (committed drill records with runners and timings)
- erasure propagation adapters for archives, caches, search indexes, and suppression lists

## Notes / gotchas

- Rollback: nothing deployed changes user-facing behaviour, so there is nothing to roll back — this task
  either produces evidence or exposes a gap. **The pack's value is that it cannot describe a system
  other than the one running**, so never hand-edit a generated pack to make it look right.
- Time the drills against the statutory clock, not the internal SLA. An internal 14-day target that
  comfortably beats a 30-day deadline tells you nothing about the jurisdiction with a 15-day one.
- The sub-processor role field is the field TASK-024's threat model and phase 10's upload gate both
  read. If a role is wrong here, it is wrong in an outbound upload decision too.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-029 done --note "<PR or commit>"`
