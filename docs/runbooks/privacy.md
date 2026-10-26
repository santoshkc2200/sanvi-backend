# Privacy runbook

Operator procedures for the privacy context (phase 05). Jurisdiction rules are **data**
(`privacy.jurisdiction_profiles`), reviewed by counsel; the mechanisms described here are the
platform's operating defaults. When in doubt: the most protective profile applies, never the
most convenient.

Prerequisites: `just`, access to the environment's database (migrator role, read-only for
inspection) and the secret manager.

## 1. Data subject requests (DSR)

Requests arrive three ways: self-service (`POST /api/v1/privacy/requests`), authorized agents
(same endpoint, `requester = authorized_agent` + evidence), and tenant operators acting for
their end users (`POST /api/v1/tenant/privacy/requests`, processor role).

- **Opt-outs take effect immediately and are never gated on verification** (CCPA). The
  directive flip and the ledger row are written in the submitting transaction; the worker
  propagates them to downstream processors within the jurisdiction's business-day window
  (`privacy.optout_propagation` job, `privacy.consents.evidence->>'propagated_at'`).
- **Everything else verifies before disclosure or deletion**: authenticated users are verified
  by their session; email requesters get a one-time code. A request stuck in
  `awaiting_verification` for more than a day is worth a look:
  `SELECT id, received_at FROM privacy.data_subject_requests WHERE status = 'awaiting_verification';`
- The saga (`privacy.dsr_advance` job) fans out to every registered provider; per-context
  progress lives in `privacy.request_participants`. A participant that failed twice parks the
  request in `partially_complete` — never silently succeed.
- Deadline extension is once per request, only where the jurisdiction allows it
  (`POST /api/v1/platform/privacy/requests/{id}/extend`); the jurisdiction table decides.
- **Never** extend, reject or edit rows by hand. Rejections must state a reason (audited), and
  US subjects must receive the appeal route and the state authority's complaint address —
  the API includes both in the outcome.

## 2. Appeal review (US states with an appeal right)

1. List open appeals: `GET /api/v1/platform/privacy/appeals`.
2. The decision is a **second-operator review**: the operator deciding must differ from the
   operator whose refusal is under appeal. Both decisions are in the audit chain.
3. Decide: `POST /api/v1/platform/privacy/appeals/{id}/decision` with `upheld` (the request
   re-opens) or `denied` (closed with the authority complaint route in the outcome).
4. The profile's `appeal_response_days` is the clock; the internal SLA alert fires well before
   it.

## 3. Breach response

1. Record the incident once: `POST /api/v1/platform/privacy/incidents` (affected counts,
   jurisdictions, data classes, whether the data was encrypted at rest). The response lists
   the computed obligations per jurisdiction and audience — GDPR 72h to the SA, APPI report to
   the PPC, per-state consumer/AG thresholds and deadlines.
2. The calculator creates conservative review tasks. Storage encryption alone never suppresses notices. Assess risk, availability, key compromise, recipient counts, and applicable exceptions; retain the decision and supporting evidence. Japan has separate initial and final authority tasks.
3. Record each discharge as it happens: `POST /api/v1/platform/privacy/incidents/{id}/notify`
   with jurisdiction + audience + artifact. Obligations within 24h of due alarm.
4. Verify the current local requirements and incident facts. A missing or incorrect profile requires review and correction; a generic baseline or an empty deadline is never evidence that notification is unnecessary.

## 4. Legal holds

- Apply: `POST /api/v1/platform/privacy/holds` with `scope` (`{"subject_key": ...}` or
  `{"tenant_id": ...}`) and a reason. Holds block erasure; the DSR parks `on_hold`.
- Release: `POST /api/v1/platform/privacy/holds/{id}/release`. Every hold is audited both ways.
- Litigation/statutory retention always beats erasure — the response to the subject states
  what was retained and why (the erasure report carries the retention reasons).

## 5. Key destruction verification (crypto-shred)

Erasure destroys the per-subject DEK (`privacy.subject_keys`). To verify:

```sql
-- Should return 0 rows for an erased subject (the wrapped key is gone).
SELECT * FROM privacy.subject_keys WHERE subject_id = '<subject id>';
```

- Each decrypt checks shared revocation state. Backups may contain wrapped keys and plaintext records; restoring a database alone does not erase them. The API recovery gate must pass before traffic resumes. See `docs/compliance/minimum-release.md` for ledger initialization and replay.
- The KEK itself (`SANVI__PRIVACY__KEK`) lives in the secret manager. Rotation = add a new key,
  flip `privacy.kek_id`, and re-wrap outstanding DEKs with the migration tooling — the old KEK
  must stay available until every DEK row references the new `kek_id`.
- **Never** destroy a KEK before its wrapped DEKs are re-wrapped or their subjects erased.

## 6. Retention sweep

- Runs daily, **dry-run by default** (`privacy.retention_dry_run = true`); the report logs
  `scanned` vs `would_delete`. Production enables deletes only after a verified dry-run.
- Rules are rows in `privacy.retention_rules` (counsel adjusts periods without a release);
  the privacy notice's retention table is generated from the same rows.
- Invoices are statutory (`statutory_retention`, `archive`) and survive erasure requests —
  the response says so.

## 7. Jurisdiction changes

A new US state law is **configuration**: `PUT /api/v1/platform/privacy/jurisdictions` with the
new profile row and `evidence_codes` mappings (reviewed), plus a matrix-test expectation row — CI fails until the
new jurisdiction's defaults, deadlines and appeal rights are explicitly asserted. No code
change, no release.

## 8. Manual rights, complaints, corrections, and restriction release

`other` requests remain open for a human response. This includes complaints, recipient-specific disclosures, and requests for human review. Record actual fulfillment through `POST /api/v1/platform/privacy/requests/{id}/fulfill` with a response summary and evidence reference. Only verified, open `other` or rectification requests qualify. The subject can read the summary through their authenticated/token-bound status page. Do not mark a response complete before supplying the information or completing the correction.

A verified restriction or objection denies optional processing independently of later consent. Release only after verifying the subject's instruction or legal exception, and notify the subject before lifting a restriction. Record the reason through `POST /api/v1/platform/privacy/requests/{id}/release`. Essential storage, security, rights handling, and legally necessary processing still require their own applicable basis.

Extensions require a justification, must be recorded before the original deadline, and queue a subject notification in the transactional outbox. Watch delivery failures and ensure the explanation reaches the subject within the original response period. EU/UK clocks use calendar months; the day count in a legacy profile is not the statutory month definition.
