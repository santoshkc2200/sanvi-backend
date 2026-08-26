# TASK-014: 10.5 Conversion capture & the directive gate

**Phase:** 10
**Status:** in progress — review hardening is implemented: `PaymentLookupPort` now supplies the PaymentRecord-derived conversion id, immutable order/name dedupe prevents a pre-webhook beacon and post-webhook retry from double-counting, and click-id extraction preserves client landing time/URL while decoding query values and redacting Referer query, fragment, and user-info data. Docker-backed integration and CI gates must verify this revision; it is not recorded as code-complete and tested. Not yet done: mounting `/public/track` on the tenant's own phase-08 domain (currently a shared-domain endpoint authenticated by site key + Origin) — deferred, needs frontend/infra coordination on which host the storefront pixel posts to and cert/routing coverage for that path on tenant custom domains.
**Requirement(s):** FR-1008, FR-1009, NFR-1001, NFR-1002, NFR-1010
**Depends on:** TASK-011
**Created:** 2026-08-20

**Slice:** 10.5 — Conversion capture & the directive gate
**Prerelease:** `v0.11.0-alpha.6` · **Flag:** `advertising.conversion_tracking`

## Context

A purchase on the tenant's own domain becomes a stored conversion event with click ids, a
server-sourced value, hashed identifiers, and a **decision already recorded** about whether it may ever
be uploaded. Nothing uploads in this task. The gate ships before the pipe — reversed, the gate becomes
a filter bolted onto a working pipeline, and a bolted-on privacy gate is the one that grows an
emergency bypass flag six months later.

This is the task the phase's invariant lives in, and the one where a shortcut becomes a regulatory
finding rather than a bug.

**The three rules:**

1. **The gate is at capture, and its answer is persisted.** Every event stores the `ConsentSnapshot`
   it was captured under — purposes asked, answers, signal source, jurisdiction, resolver version. An
   event with no snapshot is never uploadable, and the snapshot is the proof when someone asks a year
   later.
2. **Measurement consent is not the whole question.** Every event asks for `ads_measurement` **and**,
   where the receiving vendor is registered as a third party rather than a service provider,
   `sale_or_share` and `targeted_advertising`. A consent-only gate honours the EU rules and quietly
   violates the US ones; the resolver already knows the difference, so ask it.
3. **The value comes from our payment record.** Where a phase-09 payment exists, its amount is the
   conversion value. A client-supplied value is a hint used only when no server-side record exists, and
   it is marked as such — otherwise inflated client values poison ROAS and nobody notices.

## What to do

- [ ] **Contract** — `POST /api/v1/public/track` (no auth; tenant-domain, signed site key,
      rate-limited), `GET/PUT /ads/tracking/settings`, `POST /ads/tracking/test-event`,
      `GET /ads/conversions` (`advertising.metrics.read`). `POST /track` accepts an event name, a
      stable `event_id`, an optional order reference, click ids, and an optional value; it responds
      `202` with nothing useful in the body — it reflects no input, confirms no state, and is
      deliberately uninformative to a prober. Settings map Sanvi event names to each platform's
      conversion actions.
- [ ] **Server-issued `event_id`** — generated per order where an order exists (phase 09's conversion
      event id from TASK-005), so the browser pixel and the later server-side upload dedupe on a value
      the browser cannot fabricate or duplicate across a refresh.
- [ ] **First-party endpoint** — mounted on the tenant's own verified phase-08 domain so it survives
      ITP and ad blockers. Same-origin by design; no third-party script, no CSP relaxation.
- [ ] **Site key** — per tenant-domain, signed, rotatable, with strict `Origin` checks. It
      authenticates the *site*, not a user; it is public by nature, so it is paired with origin checks
      and rate limits rather than treated as a secret.
- [ ] **Abuse controls** — payload size caps, per-IP and per-domain rate limits, bot filtering
      (user-agent plus behavioural heuristics), and no reflection of any input in any response. Assume
      it is being probed on day one.
- [ ] **Click-id capture** — `gclid`, `gbraid`, `wbraid`, `fbc`, `fbp` from URL and first-party
      cookies, with capture time and landing URL. A missing click id is stored as **missing**, not as
      empty — TASK-015's diagnostics needs the distinction.
- [ ] **Directive resolution** — call the phase-05 `DirectiveResolver` for `ads_measurement`, and for
      `sale_or_share` / `targeted_advertising` per the receiving vendor's registered role. Persist the
      full snapshot. When any required purpose is denied: store the event **analytics-only**, set
      `upload_state.suppressed_by_directive` with the purpose *and* the signal source that blocked it
      (consent absent, opt-out, GPC), and never queue it.
- [ ] **Identifier hashing** — normalise then SHA-256 (lowercase, trim, strip dots and plus-tags for
      email; E.164 for phone). Raw PII is never persisted in this context and never leaves the hashing
      function; TASK-009's type gate makes the alternative uncompilable.
- [ ] **Value enrichment** — join the phase-09 payment record by order reference and use its amount and
      currency. Record the value source (`payment_record` | `client_reported`) on the event.
- [ ] **Idempotent storage** — `UNIQUE (tenant_id, event_id)` from TASK-009. A retried beacon, a
      refreshed confirmation page, and a duplicated pixel all collapse to one row.
- [ ] **Test event** — an authenticated endpoint that emits a synthetic event end to end and returns
      what was captured, what the resolver decided, and why. Flagged as a test so it never enters
      metrics.

## Acceptance criteria

- [ ] A storefront purchase on a tenant's own domain produces exactly one conversion event with the
      value taken from the phase-09 payment record.
- [ ] All five directive scenarios behave correctly and record the purpose and source that decided
      them: EU no-consent (suppressed), US no-signal (permitted), US opt-out (suppressed), GPC
      (suppressed), withdrawal after capture (future uploads stop).
- [ ] The same subject and event with the vendor registered as a service provider versus a third party
      yields different gate outcomes.
- [ ] Every stored event has a resolver version, jurisdiction, purposes, answers, and signal source; a
      missing field fails the test.
- [ ] A full-capture assertion over the events table and the tracing output finds no raw email or
      phone; `A.User+tag@Example.COM` and `auser@example.com` hash identically.
- [ ] A client-reported value of 1,000,000 against a real ¥4,980 payment is overridden by the payment
      record and marked `payment_record`; the JPY value is zero-decimal-correct end to end.
- [ ] The same `event_id` posted five times yields one row.
- [ ] Oversized payloads, a bad origin, an invalid site key, and a rate-limit breach are each rejected
      without reflecting input or revealing whether the tenant exists.
- [ ] Click ids present, partially present, and wholly absent are distinguishable in storage.
- [ ] No `ConversionSinkPort` implementation exists yet — nothing has been uploaded anywhere.

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

Every upload path, dedupe against the platforms, diagnostics projection, audience sync, and retraction
propagation (all TASK-015). Metrics ingestion (TASK-016).

## Files likely touched

- `api/advertising.yaml` (`/public/track`, tracking settings, test event, conversions list)
- `crates/contexts/advertising/src/application/tracking/**`
- `crates/contexts/advertising/src/adapters/{http,repos}.rs` (public route, site-key verification)
- phase-05 `DirectiveResolver` integration, phase-09 payment lookup port
- rate-limit and bot-filter middleware configuration

## Notes / gotchas

- Rollback: `advertising.conversion_tracking` off → `/public/track` returns `202` and **discards**, the
  storefront beacon is not emitted, and the setup screen shows a disabled state. Rolling back loses
  events for the duration rather than deferring them — deliberately, because a queue of events captured
  under a directive state we may no longer be able to prove is worse than a gap in the data.
- The gate's answer is stored, not recomputed later. TASK-015 reads it; it never re-derives it upward.
- `202` on everything, including rejections, is intentional. Do not "improve" the endpoint's error
  reporting.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
