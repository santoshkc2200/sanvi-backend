# TASK-015: 10.6 Conversion upload, dedupe & diagnostics

**Phase:** 10
**Status:** in-progress
**Requirement(s):** FR-1010, FR-1011, NFR-1001, NFR-1002
**Depends on:** TASK-014
**Created:** 2026-08-20

**Slice:** 10.6 — Conversion upload, dedupe & diagnostics
**Prerelease:** `v0.11.0-alpha.7` · **Flag:** `advertising.conversion_tracking`

## Context

Events that TASK-014's gate permitted reach Google Ads and Meta exactly once, and every event —
uploaded, suppressed, or failed — has a reason a tenant can read. The diagnostics projection is the
deliverable that turns "the numbers look wrong" into a specific, fixable cause, and separates the
causes a tenant can fix from the ones they must respect.

**The two rules:**

1. **The queue reads the gate; it never re-evaluates it.** An event carries its decision from
   TASK-014. The worker uploads only what is already marked permitted and re-checks only for
   *withdrawal* since capture — it never upgrades a suppressed event, whatever the current directive
   state says. Consent is not retroactive.
2. **Dedupe on the shared `event_id`.** The browser pixel and the server-side upload send the same
   server-issued id. Without it both platforms count the purchase twice, ROAS doubles, and the number
   the tenant makes budget decisions with is wrong in the flattering direction.

## What to do

- [ ] **Contract** — `GET /ads/conversions` gains its upload columns (per-platform state, attempt
      count, last error, dedupe outcome, suppression reason);
      `GET /ads/conversions/{id}/diagnostics` returns the full per-event story — captured at, click ids
      present/absent, value source, directive snapshot, and each platform's attempt history with the
      platform's own error text; `POST /ads/conversions/{id}/retry` (`advertising.campaign.write`);
      `GET/POST /ads/audiences`; `POST /ads/audiences/{id}/refresh`. Hashed identifiers are shown as
      hashes; raw customer data appears nowhere.
- [ ] **`ConversionSinkPort` implementations** — Google Ads offline/enhanced conversions and Meta
      Conversions API. Both take the `ConsentSnapshot` as a **required argument** (TASK-009's gate makes
      the alternative uncompilable) and both send hashed identifiers only.
- [ ] **Upload workers** — batched per platform to the platform's preferred batch size, exponential
      backoff with jitter. Distinguish permanent failures (bad identifier format, unknown conversion
      action, revoked token) from transient ones: permanent failures stop retrying and surface,
      transient ones retry to a bounded ceiling and then park in a DLQ.
- [ ] **Withdrawal re-check** — before each attempt, confirm the directive still permits. A withdrawal
      between capture and upload stops the upload and records `suppressed_late` with the source, which
      is a different diagnostic message from suppressed at capture.
- [ ] **Per-platform upload state** — an event succeeds on Google and fails on Meta independently.
      State is per platform, never a single boolean, or a partial success reports as total failure.
- [ ] **Audience sync** — customer-list build and refresh requiring `sale_or_share` **and**
      `targeted_advertising` per subject. Opted-out subjects are excluded from a new list and **removed
      from existing ones on the next refresh**, not merely skipped. Refresh is scheduled, not manual.
- [ ] **Retraction propagation** — a withdrawal or opt-out after upload triggers a deletion/suppression
      request to each platform that supports one, recorded with its outcome. Where a platform offers no
      such API, the failure to propagate is recorded **explicitly** rather than silently accepted.
- [ ] **Diagnostics projection** — a read model assembling capture, gate, and attempt history per event
      without a fan-out of queries per row.
- [ ] **Manual retry** — for parked events only, permission-gated, idempotent, and audited. It
      re-checks the directive like any other attempt.
- [ ] **Observability** — `conversion_upload_total{platform,result}`,
      `conversion_suppressed_total{reason,source}`, `conversion_upload_lag_seconds`, DLQ depth, with a
      DLQ depth alert.

## Acceptance criteria

- [ ] A storefront purchase produces exactly one conversion on **each** connected platform, valued from
      the server-side payment record — asserted against test accounts with the browser pixel also
      firing.
- [ ] An event suppressed at capture is never uploaded, **including after the subject later consents**.
- [ ] A directive retracted between capture and attempt stops the upload and records `suppressed_late`
      with its source.
- [ ] Google succeeds and Meta fails on the same event: states are independent and both are reported.
- [ ] A malformed-identifier rejection stops retrying; a 503 retries and eventually succeeds; a token
      expiry parks the event and surfaces as a connection problem, not a data problem.
- [ ] An opted-out subject is absent from a newly built list **and removed from an existing one on the
      next refresh**, verified against the platform's own list state.
- [ ] A post-upload withdrawal issues a deletion request where supported and records an explicit
      unpropagated outcome where not.
- [ ] Every taxonomy category renders its own diagnostic reason; the enum is asserted exhaustively so
      no category falls through to a generic string.
- [ ] No raw PII appears in the diagnostics payload or its export.

## Verification

```bash
just fmt
just clippy
just layering
just ads-invariants
just ads-invariants-self-test
cargo test -p sanvi-advertising
just test
just openapi
```

## Out of scope

Metrics ingestion and the conversions column in the dashboard (TASK-016), the `PersonalDataProvider`
and sub-processor registry entries (TASK-018).

## Files likely touched

- `api/advertising.yaml` (diagnostics, retry, audiences)
- `crates/contexts/advertising/src/application/uploads/**`, `.../audiences/**`
- `crates/contexts/advertising/src/adapters/{google_ads,meta}/conversions.rs`
- `crates/contexts/advertising/src/adapters/{repos,metrics}.rs` (diagnostics projection, metrics)
- `apps/worker/**` (upload workers, DLQ, audience refresh schedule)

## Notes / gotchas

- Rollback: `advertising.conversion_tracking` off → workers stop and the queue holds. Events already
  captured are **not** discarded here (unlike TASK-014's capture rollback) because their snapshots are
  stored and still provable; on re-enable, each is re-checked for withdrawal before it is sent. If the
  flag stays off past the platforms' conversion-upload windows, park the backlog and report it rather
  than uploading late data that will restate every affected day's ROAS.
- A retry affordance on a directive-suppressed event is a compliance trap. The API must refuse it, not
  merely the UI.
- An audience list is the easiest place for an opt-out to leak past and the hardest to retract — treat
  the refresh path as the primary one, not the exception.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
