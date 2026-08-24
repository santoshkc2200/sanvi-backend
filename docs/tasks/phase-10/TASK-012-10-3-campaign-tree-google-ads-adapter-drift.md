# TASK-012: 10.3 Campaign tree, Google Ads adapter & drift

**Phase:** 10
**Status:** in-progress
**Requirement(s):** FR-1004, FR-1005, FR-1006, NFR-1007, NFR-1010
**Depends on:** TASK-011
**Created:** 2026-08-20

**Slice:** 10.3 — Campaign tree, Google Ads adapter & drift
**Prerelease:** `v0.11.0-alpha.4` · **Flag:** `advertising.google_ads`

## Context

A tenant drafts a campaign in Sanvi, it validates against the real Google capability matrix, it
publishes, and it appears live in Google Ads with matching budget and schedule. Then someone edits it
in Google's own UI — and Sanvi shows that as drift with a diff, never as something to overwrite.

This is the first task where our code causes a tenant to spend real money.

**The two rules:**

1. **The platform owns delivery; Sanvi owns intent.** Remote state is the truth about what is running
   and what it cost. Our state is the truth about what the tenant asked for. Reconciliation reports
   the difference; it never resolves it silently.
2. **Every mutation is idempotent and attributed.** A client-generated mutation id, a change log row
   with the actor, and a retry that cannot double-publish. "Who paused this campaign" must be
   answerable, and a network timeout during publish must not create two campaigns.

## What to do

- [ ] **Contract** — `GET/POST /campaigns`, `GET/PATCH /campaigns/{id}`, `POST /campaigns/{id}/publish`,
      `POST /campaigns/{id}/pause|/resume`, `GET/POST/PATCH /campaigns/{id}/ad-groups` (+ `/ads`),
      `POST /campaigns/{id}/validate`, `GET /campaigns/{id}/changes`, `GET/POST /campaigns/{id}/drift`.
      `validate` is a dry run returning the same field-addressed violations as a failed write, so the
      builder can validate a whole step without risking a partial publish. `PATCH` on a published
      campaign requires `Idempotency-Key` **and** `If-Match` on the campaign revision; a stale revision
      returns `409` with the current state rather than clobbering it. `POST /drift` carries
      `keep_theirs` or `reapply_ours` and is idempotent per drift revision.
- [ ] **Campaign tree persistence** — draft/publish lifecycle over `campaigns`, `ad_groups`, `ads`.
      Drafts are ours alone and never touch the platform; `external_id` is null until publish.
- [ ] **Google Ads adapter** — `AdPlatformPort` campaign-tree CRUD against the pinned API version,
      plus the real Google `CapabilityMatrix` replacing the fake's. Passes the same port contract
      suite the fake passes.
- [ ] **Publish path** — validate → create remotely → store `external_id` → poll review status. The
      mutation id is sent to Google where the API supports it and is otherwise recorded before the call
      and checked after, so a timeout is recoverable by lookup rather than by retry-and-hope.
- [ ] **Change log** — every mutation writes actor, timestamp, field-level before/after, source
      (`sanvi` | `platform`), and the mutation id. Append-only, covered by the phase-05 retention rules
      registered in TASK-009. This table is what answers spend disputes.
- [ ] **Reconciliation job** — nightly and on-demand pull of the remote tree per connection, compared
      field-wise against our last-known remote snapshot:
      - remote changed, ours unchanged → update the snapshot, no drift;
      - remote changed **and** contradicts our intent → set `drift` with the diff, emit
        `CampaignDrifted`, notify. **Never write back.**
- [ ] **Drift resolution** — `keep_theirs` adopts remote as intent; `reapply_ours` re-pushes our intent
      as an ordinary attributed mutation. Both clear the flag; neither happens automatically.
- [ ] **Review status** — per-ad review state with rejection reasons surfaced **verbatim** from the
      platform. A paraphrased policy rejection is worse than useless when the tenant has to fix it.
- [ ] **Rate limiting & quota** — Google Ads quota is per developer token and shared across all
      tenants. Queue and budget requests per connection so one tenant's bulk edit cannot exhaust the
      quota for every other tenant.

## Acceptance criteria

- [ ] A campaign created in Sanvi appears live in a Google Ads test account with matching budget,
      schedule, and objective.
- [ ] The adapter contract suite is green against recorded Google fixtures **and** a Google test
      account.
- [ ] A publish retried after a simulated timeout produces exactly one campaign; a replayed mutation id
      returns the original result.
- [ ] A `PATCH` with a stale revision returns `409` and does not mutate.
- [ ] A campaign paused in Google's UI shows as drifted with a correct diff, is **not** resurrected by
      the next sync, and `reapply_ours` re-pauses only when the tenant chooses it.
- [ ] A non-contradicting remote change updates the snapshot **without** raising drift — the
      false-positive test that decides whether tenants ignore the drift indicator.
- [ ] A pause is attributed in the change log to the acting user; a platform-sourced change is
      attributed to `platform`.
- [ ] Japanese ad copy is validated against the Japanese character limits, not the English ones.
- [ ] A bulk edit by one tenant does not starve another tenant's sync (quota fairness test).

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

Meta's adapter and creatives (TASK-013), conversion capture and upload (TASK-014, TASK-015), metrics
and spend numbers (TASK-016), budget caps and auto-pause (TASK-017).

## Files likely touched

- `api/advertising.yaml` (campaign, validate, changes, drift endpoints)
- `crates/contexts/advertising/src/application/campaigns/**`, `.../reconciliation/**`
- `crates/contexts/advertising/src/adapters/google_ads/**`, `.../repos.rs`, `.../http.rs`
- `apps/worker/**` (reconciliation job, review-status polling, request queue)

## Notes / gotchas

- Rollback: `advertising.google_ads` off → the builder and mutation endpoints refuse with
  `503 advertising/platform-unavailable` while campaign **reads** stay available. Campaigns already
  live on Google keep spending; the rollback stops our writes, not their delivery, and the UI must say
  so rather than implying the campaigns are paused.
- Do not "fix" a contract-suite divergence by special-casing the suite. A divergence is an adapter bug.
- Snapshot-vs-intent is three states, not two: ours, theirs, and last-known-theirs. Losing the third
  makes every remote edit look like drift.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-012 done --note "<PR or commit>"`
