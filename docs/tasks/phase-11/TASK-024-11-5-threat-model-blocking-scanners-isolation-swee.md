# TASK-024: 11.5 Threat model, blocking scanners, isolation sweep & pen-test remediation

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1116, FR-1117, NFR-1108, NFR-1109, NFR-1110, NFR-1113, NFR-1114
**Depends on:** TASK-019
**Created:** 2026-08-21

**Slice:** 11.5 — Security: threat model, scanning, isolation sweep & pen-test remediation
**Prerelease:** `v1.0.0-rc.6` · **Flag:** none — findings-driven

## Context

Have someone other than the authors try to break it, and fix what they find. Around that, close the
gaps an external test is bad at finding: authorization coverage across every route, cross-tenant
isolation through every ID-bearing parameter, and a supply chain that is reviewed rather than assumed.

**This task runs on its own track.** The external test executes against the frozen staging build from
TASK-019 while TASK-021 through TASK-023 continue; only the booking and the frozen environment are
shared. The remediation window is scheduled, not hoped for.

## What to do

- [ ] **Contract** — no new endpoints, two contract-level outcomes. **The route/permission matrix becomes
      a generated artifact** derived from the OpenAPI documents plus handler metadata: every route, its
      required permission, its tenant scoping, and its rate-limit class. Reviewed by two people and
      checked in; a route that appears without a permission fails CI. **Security headers are specified,
      not incidental** — HSTS, frame-ancestors, referrer policy, permissions policy, and the CSP —
      asserted per app in a snapshot test.
- [ ] **Threat-model refresh** — `docs/security/threat-model.md` updated with STRIDE per trust boundary
      across all thirteen contexts, explicitly covering the surfaces phases 08–10 added: tenant custom
      domains (and the subdomain-takeover class), the public tracking endpoint, ad-platform OAuth
      tokens, Stripe Connect keys and webhooks, and the impersonation path from phase 03.
- [ ] **Penetration test executed** — third-party, against frozen staging with scale data, scoped to
      include multi-tenant isolation, authentication and session handling, the payment and advertising
      surfaces, and the public tracking endpoint. All findings triaged within 48 hours of receipt;
      high/critical fixed before GA; the remainder scheduled with an owner and a date.
- [ ] **CI scanning, all blocking** — SAST, dependency audit (`cargo audit` / `cargo deny`), container
      scan, IaC scan, and secret scan. **None advisory** — an advisory scanner is a way of documenting
      that nobody reads the output. Accepted findings carry an expiry.
- [ ] **Authorization review** — generate the route matrix above; assert each route's permission, tenant
      scoping, and rate-limit class in a test that walks the router. Two-person review of the generated
      matrix is a merge requirement for this task.
- [ ] **Multi-tenant isolation sweep** — re-run the phase-01 RLS matrix test against the full schema
      including every table added in phases 08–10 and **every child partition by name**, then a manual
      **IDOR sweep**: every ID-bearing path, query, header, and body field tried cross-tenant with a
      valid session for the wrong tenant. Findings become regression tests, not just fixes.
- [ ] **Abuse controls** — per-tenant and per-IP quotas on the surfaces that cost money or reputation:
      signup, invitation, password reset, the public tracking endpoint, domain purchase, and outbound
      notification sends. Bot filtering on tracking; fraud checks on domain purchase; velocity limits on
      invitations.
- [ ] **Secrets and key handling review** — every secret's storage, rotation path, and blast radius
      recorded; envelope-encrypted material (phase-05 DEKs, ad tokens, Connect keys) verified unlogged
      and unserialised, with the redaction tests from phases 09 and 10 extended to every new type.
- [ ] **Supply chain** — SBOM generated and published per release, dependencies pinned, provenance
      attestation on release artifacts, a written review policy for new dependencies, and `cargo vet`
      style auditing for the crates on the security-critical path (crypto, HTTP parsing, SQL, template
      rendering).

## Acceptance criteria

- [ ] Threat model refreshed and reviewed, covering the phases 08–10 surfaces explicitly.
- [ ] Pen-test report received; **no high or critical finding open**; medium and below scheduled with
      owners and dates.
- [ ] All five CI scanners blocking with zero unaccepted findings; every accepted finding has an expiry.
- [ ] The route/permission matrix is generated, reviewed by two people, and enforced — a route added
      without a permission, tenant scoping, or rate-limit class fails CI, proven in the PR.
- [ ] Cross-tenant isolation verified three ways: RLS matrix across all contexts and child partitions,
      IDOR sweep, and the external test.
- [ ] A wrong-tenant request for every ID-bearing parameter returns 404 or 403 and never leaks existence
      through a timing or message difference.
- [ ] Abuse controls live on every money- or reputation-costing surface, with legitimate traffic
      unaffected at 3× peak.
- [ ] Redaction tests pass across every secret-bearing type — debug, display, serde, and tracing capture.
- [ ] Security header and CSP snapshots pass per app.
- [ ] SBOM published, dependencies pinned, provenance attested, review policy documented.
- [ ] **Every finding — internal or external — has a regression test that fails against the vulnerable
      commit.**

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just route-matrix
just rls-matrix
just test-idor
just audit
just sbom
```

## Out of scope

Credential rotation drills (TASK-025 — this task produces the secrets inventory that scopes them).
Per-plan rate limits and quotas (TASK-026 — this task's abuse controls protect specific surfaces; those
are commercial limits). Frontend auth-surface review, CSP tightening, and bundle secret scanning
(frontend TASK-024).

## Files likely touched

- `docs/security/threat-model.md`, `docs/security/secrets-inventory.md`, `docs/security/dependency-policy.md`
- `scripts/generate-route-matrix.*` and its committed output
- `crates/platform/http/**` (security headers, abuse quotas middleware)
- `tests/security/**` (IDOR sweep, RLS matrix extension, redaction, header snapshots)
- `deny.toml`, CI workflow definitions (five blocking scanners, SBOM, provenance)
- remediation commits across whichever contexts the pen test names

## Notes / gotchas

- **Security fixes do not roll back; they roll forward.** The two changes that can break legitimate
  traffic are abuse quotas and CSP tightening, and both ship the same way: report-only or shadow mode
  first, a week of observed traffic on staging, then enforcement, with the threshold and the observed
  false-positive rate recorded in the PR.
- Child partitions are the isolation gap most likely to be real. RLS does not automatically apply to a
  partition created after the policy; assert it **by name**, per partition, not by parent.
- Book the test in TASK-019, not here. By the time this task starts, the engagement should already have
  a date, a scope, and a named build.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
