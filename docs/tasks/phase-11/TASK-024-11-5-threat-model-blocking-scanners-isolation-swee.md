# TASK-024: 11.5 Threat model, route matrix & isolation sweep

**Phase:** 11 · **Status:** in-progress · **Size:** L
**Requirement(s):** FR-1116, FR-1117, NFR-1108, NFR-1109, NFR-1113, NFR-1114
**Depends on:** TASK-031
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** none — findings-driven

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:subagent-driven-development`.
> **Execute one slice per session, not one task per session** — see `## Execution slices` below. Each
> slice is a commit with its own scoped verification; a slice that has not been committed is not done.
> Every step carries a verification command; do not tick a step whose command has not been run and
> passed. **Run the full `## Verification` block once, in the final slice** — not per step and not per
> slice. Running the workspace suite after every step is the single largest avoidable cost in this
> phase: `just test` links every test binary, and on an un-exempted macOS host Gatekeeper scans each
> one (~15 of ~16 minutes). Use `just check` and `just test-fast` while working.

## Goal

Close the security gaps that can be closed without an external tester: authorization coverage across
every route, cross-tenant isolation through every ID-bearing parameter and every child partition, abuse
controls on the surfaces that cost money or reputation, and a specified rather than incidental set of
security headers.

## Why this shape

The original task was built around a booked third-party penetration test, and most of its acceptance
criteria depended on that report. There is no budget for one; it is recorded as an unmitigated gap in
[`../../release/needs-humans.md`](../../release/needs-humans.md) rather than pretended away.

**Say the limitation plainly: the isolation sweep here is written and run by the same person who wrote
the code being tested.** No amount of automated coverage removes that. What the automation does buy is
that every finding, from wherever it comes, becomes a regression test — so the suite only grows and a
class of defect cannot return.

The scanner set is nearly complete already: `cargo audit`, `cargo deny`, gitleaks, trivy, and a
CycloneDX SBOM all exist and all block in `pr.yml`. TASK-031 adds SAST. This task's scanner work is
therefore small: keep them blocking, give accepted findings an expiry, and publish the SBOM with the
tag rather than only as a CI artifact.

This task is in the gate half because the route matrix and the RLS sweep enumerate: they cover phase
10's remaining routes and tables automatically as those land.

## Constraints

- **Security fixes roll forward, not back.** The two changes that can break legitimate traffic are
  abuse quotas and header tightening; both ship in report-only or shadow mode first, with the observed
  would-have-blocked rate recorded in the PR before enforcement.
- Every finding — however small, wherever from — gets a regression test that fails against the
  vulnerable commit. No exceptions.
- Child partitions are asserted **by name**. RLS does not automatically apply to a partition created
  after the policy, and that is the isolation gap most likely to be real.

## File ownership map

- `docs/security/threat-model.md`, `docs/security/secrets-inventory.md`,
  `docs/security/dependency-policy.md`
- `scripts/generate-route-matrix.sh` and its committed output `docs/security/route-matrix.md`
- `crates/platform/http/**` (package `sanvi-http`) — security headers, abuse quota middleware
- `crates/platform/telemetry/src/redaction.rs` (package `sanvi-telemetry`) — the redaction the
  enumeration test drives
- `apps/api/tests/integration/**` (package `sanvi-api`) — IDOR sweep, RLS matrix, header snapshots
- `deny.toml`, `.github/workflows/pr.yml` — scanner configuration and finding expiry

**Two path corrections, found by looking rather than by assuming — do not re-derive them:**

- There is **no `tests/security/` directory and no workspace-root Rust test crate.** The workspace root
  holds only `tests/load/` (k6 JavaScript, from TASK-019). A `tests/security/*.rs` file would belong to
  no workspace member, so `cargo nextest run --workspace` would never compile it and TASK-031's
  `just ci-test-coverage` gate would never see it — a security suite that silently does not run. The
  cross-cutting tests here go in **`apps/api/tests/integration/`**, which already holds exactly this
  kind of assembled-app test (`http_contract.rs`, `permission_catalog.rs`, `privacy_providers.rs`).
- There is **no `tests/security/rls_matrix.rs` to modify.** The phase-01 matrix this step extends is
  **`crates/contexts/identity/tests/integration/identity_rls.rs`**. Extending it across every context's
  tables means a new matrix in `apps/api/tests/integration/`, with the identity file left as the
  per-context test it already is.

## Execution slices

Eight steps in one session means one context, one compaction, and re-reads of work already done. The
slices below are the commit boundaries; each is one agent session with a fresh context, and each
carries the scoped commands to run **in that session**. The task is done when the last slice is.

| Slice | Steps | Shape | Scoped verification for the session |
| --- | --- | --- | --- |
| **11.5a** route matrix generator & gate, red | 2 | small, enumerator | `just check`, then `just route-matrix` → **must fail** on any unmapped route; falsify with a throwaway unmapped route |
| **11.5b** IDOR sweep derived from the matrix | 4 | **bulk, mechanical — delegatable** | `scripts/test.sh -p sanvi-api`, `just test-idor` |
| **11.5c** RLS matrix to the full schema, partitions by name | 3 | medium, database-led | `scripts/test.sh -p sanvi-api`, `just rls-matrix`, then drop one child partition's policy in a scratch database and confirm the failure names that partition |
| **11.5d** security headers, specified not inherited | 5 | small, snapshot | `scripts/test.sh -p sanvi-http -p sanvi-api`; remove one header and confirm the snapshot fails |
| **11.5e** abuse controls in shadow mode | 6 | medium, measurement-led | `scripts/test.sh -p sanvi-http`, then the 3×-peak harness run; commit the would-have-blocked rate |
| **11.5f** secrets inventory, redaction enumeration & dependency policy | 7–8 | small, inventory-led | `scripts/test.sh -p sanvi-telemetry`, `just audit` with a past-expiry acceptance planted, `just sbom` |
| **11.5g** threat model, then close the task | 1 | judgement, no code | the full `## Verification` block, once |

**Order and parallelism.** 11.5a → 11.5b is a hard edge: the sweep enumerates *from* the committed
matrix, which is what makes phase 10's routes covered as they land rather than hand-listed. 11.5c,
11.5d, 11.5e and 11.5f are independent of the matrix and of each other, and may run concurrently in
separate sessions.

**Why the threat model moved to last.** Step 1's acceptance is that every trust boundary names a
mitigation *resolving to a test file path or a CI gate*. Written first, those are forward references to
files that do not exist yet, and they are either vague or wrong by the time the gates land. Written
last, each one is a path that can be opened. The step did not get easier; it got checkable.

**Why 11.5b is the delegation candidate.** It is the only slice whose work is bulk-mechanical with a
machine oracle: issue every ID-bearing request with a wrong-tenant session, assert 404 or 403, and let
`just test-idor` say whether the enumeration is complete. Review is reading a diff. Every other slice
turns on judgement — what the blast radius of a secret is, which surface deserves a quota, whether a
403 leaked existence through its timing.


### Slice 11.5a record — 2026-09-24

Completed the route inventory and blocking freshness/completeness gate in this
slice's commit (`security(http): gate route security matrix`). The generated
matrix includes 213 OpenAPI operations and eight explicitly documented runtime
endpoints outside OpenAPI. Reviewed handler metadata names permissions or
reasoned exemptions, tenant scope, rate class, and evidence paths. Registration,
handler, catalog, and shared enforcement fingerprints expose source changes in
the committed diff; this is declaration review, not proof of runtime isolation.
Identity-disabled permission bypass, optional/configuration-dependent limiters,
and public/credential-bound exemptions remain explicit limitations.

Verification: `just check` passed (all targets); `just route-matrix` and
`just route-matrix-self-test` passed. Mutation tests fail on an added unmapped
OpenAPI route, an undocumented runtime handler, missing security fields, removal
of a permission extractor/declaration, duplicate registration/OpenAPI keys, and
stale committed output. The PR template requires matrix-diff acknowledgment;
no PR has been opened, so the combined Definition of done item below remains
unchecked until that acknowledgment exists. Later slices remain unstarted.

Runner: Codex on the local macOS development host. Independent review found
method/path drift, unsupported registration discovery, and deleted shared-key
permission declarations that the first version missed. Regression mutations now
cover all three, nested modules, production items following test modules, and
Actix TRACE macros. Unsupported macro registrations fail closed.

The stronger gate found an existing routing defect: advertising health was
registered directly on the app at `/ads/health`, although its handler annotation,
permission catalog, and OpenAPI contract specify `/api/v1/platform/ads/health`.
The registration now uses the documented full path. The gate failed with
`runtime route differs from metadata: GET /ads/health` before that correction.
Clients using the undocumented old path must use the documented URL. No schema
or OpenAPI change was needed. Permission-table omissions on older direct
registrations are explicitly classified in metadata rather than silently accepted.

### Slice 11.5b record — 2026-09-24

Completed the IDOR sweep in this slice's commit
(`test(security): sweep cross-tenant isolation over the route matrix`).
The enumeration is derived from the committed matrix:
`apps/api/tests/integration/idor_sweep.rs` loads `route-metadata.json` at
test time and drives every request from it, so routes phase 10+ land are
covered as they appear. This is the first test that runs the fully
assembled app (`build_app` with tenancy, identity, access, audit,
platform_admin, localization, theming, domains, payments, advertising and
the media gateway mounted) against a real `test_db()` Postgres; Kratos,
Keto and the hitox media upstream are stubbed on ephemeral ports. The Keto
stub allows exactly the attacker's tenant-A role objects (roles seeded per
tenant granting every `CurrentTenant` permission the matrix declares), so
a 403 is the membership denial, not a backend outage — an unreachable Keto
would surface as 503 and fail. Three sweeps run: all 108 `CurrentTenant`
routes under a foreign host (tenant B resolved, attacker session) must
403 unless the permission is `Authenticated`-scope or an explicit matrix
exemption (those must not 5xx or leak); every id-bearing tenant route is
re-issued on the attacker's own host with tenant B's *real* seeded ids
(advertising connections/campaigns/ad groups/ads/creatives/audiences/
conversion events/budget alerts, payments connections/checkouts/payments,
domains, invitations, members, roles, media assets) and must answer
403/404 — a 204 is accepted only for idempotent `DELETE`s whose foreign
row is then verified intact through a tenant-scoped probe — and a
nonexistent-id twin must answer with the same status, closing the
existence-oracle hole; and every mounted platform route must 403 for a
tenant-scoped session. Tenant B's rows all carry a marker string no
response may echo. A coverage test pins the enumeration: every
`CurrentTenant` route in the matrix is swept, and id-bearing ones are in
sweep 2 unless named in the reasoned `SHAPE_A_EXCLUSIONS` list (OAuth
start/callback and theme assets: enum slots, not entity ids). Billing and
privacy platform routes are excluded by handler context because those
contexts are deliberately not mounted; their own suites cover them.

One finding, fixed in the same commit: `PUT
/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap` with a foreign
campaign id answered **500** — `PutBudgetCap::execute` never resolved the
campaign inside the tenant, so the cap upsert hit the
`budget_caps → campaigns` foreign key and surfaced as a storage error.
The use case now checks campaign existence in the tenant scope first and
answers `CampaignNotFound` (404); `BudgetCapRepository` gained the
`campaign_exists` probe. The sweep fails against the vulnerable commit —
it is the finding's regression test.

Harness facts recorded here because they shape every later slice:
actix extracts `Json`/`Query` before `RequiredPermission` on many
handlers, so the sweep maintains per-route valid bodies and query
parameters keyed by the matrix pattern (a bodyless or paramless request
400s before the gate and would silently weaken any assertion); payments
validates `Idempotency-Key` as a UUID; snowflake ids deserialize from
decimal strings; and tenant-row existence probes must run through
`TxManager::begin_for` — a bare app-pool query has no tenant GUC and
reports every row missing. Per-request timing comparison is not
attempted (flaky in CI); the twin-id status comparison is the automatable
half of the no-existence-leak requirement, and the timing half belongs to
the parked penetration test.

Verification: `just check` and workspace-scoped clippy clean;
`cargo fmt --all`; `scripts/test.sh -p sanvi-api` 46/46;
`scripts/test.sh -p sanvi-advertising` 259/259 (the budget-cap fix's
crate); `just route-matrix` complete and current (domain/adapter files
are not fingerprinted, and handlers.rs is unchanged);
`just test-idor` 4/4.

## Steps

### Step 1 · slice 11.5g: Refresh the threat model

**Files:** Modify `docs/security/threat-model.md`

**Do:** STRIDE per trust boundary across all thirteen contexts, explicitly covering the surfaces phases
08–10 added: tenant custom domains **and the subdomain-takeover class**, the public tracking endpoint,
ad-platform OAuth tokens, Stripe Connect keys and webhooks, and the impersonation path from phase 03.

**Verify:** every trust boundary in the document names at least one mitigation that resolves to a test
file path or a CI gate.

### Step 2 · slice 11.5a: Generate the route/permission matrix and let the gate fail

**Files:** Create `scripts/generate-route-matrix.sh`, `docs/security/route-matrix.md`; Modify
`justfile`

**Do:** derive the matrix from the OpenAPI documents plus handler metadata: every route, its required
permission, its tenant scoping, and its rate-limit class. Commit the output. A route appearing without
a permission, a tenant scoping, or a rate-limit class fails CI.

Because there is no second reviewer, the review substitute is mechanical: **the committed matrix's diff
must be explicitly acknowledged in the PR description**, so an added route with a weakened permission
cannot slip through as an invisible regeneration.

**Verify:** `just route-matrix` → fails on the current tree if any route is unmapped; passes once
mapped. Add a throwaway unmapped route and confirm it fails.

### Step 3 · slice 11.5c: Extend the RLS matrix to the full schema, partitions by name

**Files:** Create `apps/api/tests/integration/rls_matrix.rs` (see the path corrections above — the
phase-01 matrix is `crates/contexts/identity/tests/integration/identity_rls.rs`, which stays as the
per-context test it already is); Modify `apps/api/tests/integration/main.rs`

**Do:** re-run the phase-01 matrix against every table added in phases 08–10, and against **every child
partition enumerated by name** rather than by parent.

**Verify:** `just rls-matrix` passes; drop the policy from one child partition in a scratch database
and confirm the test fails naming that partition.

### Step 4 · slice 11.5b: Run the IDOR sweep and convert every finding to a test

**Files:** Create `apps/api/tests/integration/idor_sweep.rs`; Modify `apps/api/tests/integration/main.rs`

**Do:** for every ID-bearing path segment, query parameter, header, and body field, issue the request
with a valid session for the **wrong** tenant. Assert 404 or 403, and assert existence does not leak
through a message or a timing difference.

**Verify:** `just test-idor` passes across the enumerated surface, and the enumeration is derived from
the route matrix rather than hand-listed, so phase 10's routes are covered as they land.

### Step 5 · slice 11.5d: Specify the security headers

**Files:** Modify `crates/platform/http/**`; Create `apps/api/tests/integration/headers_snapshot.rs`

**Do:** HSTS, frame-ancestors, referrer policy, permissions policy, and CSP, asserted per app in a
snapshot test rather than left to whatever the framework defaults to.

**Verify:** the snapshot test fails when a header is removed and passes on the specified set.

### Step 6 · slice 11.5e: Add abuse controls in shadow mode

**Files:** Modify `crates/platform/http/**`

**Do:** per-tenant and per-IP quotas on the surfaces that cost money or reputation — signup,
invitation, password reset, the public tracking endpoint, domain purchase, outbound notification
sends. Bot filtering on tracking; velocity limits on invitations.

Ship counting live and enforcement off. Thresholds cannot be tuned without traffic, so the tuning is
deliberately left as a configuration change later rather than a code change (recorded in
`needs-humans.md`).

**Verify:** a load test at 3× the harness peak records the would-have-blocked rate; the rate is
committed in the PR. Enforcement flips on only for the tracking endpoint, where a flood test proves the
stricter bucket holds.

### Step 7 · slice 11.5f: Inventory secrets and extend the redaction tests

**Files:** Create `docs/security/secrets-inventory.md`; Modify
`crates/platform/telemetry/src/redaction.rs` and its tests

**Do:** every secret's storage, rotation path, and blast radius. Extend the phase 09 and 10 redaction
tests to every secret-bearing type: debug, display, serde, and tracing capture. Envelope-encrypted
material (phase-05 DEKs, ad tokens, Connect keys) verified unlogged and unserialised.

**Verify:** the redaction test enumerates secret-bearing types from the codebase rather than from a
hand-written list, and fails when a new one is added without redaction.

### Step 8 · slice 11.5f: Finish the scanner set and publish the SBOM

**Files:** Modify `.github/workflows/pr.yml`, `deny.toml`, `docs/security/dependency-policy.md`

**Do:** keep all scanners blocking (TASK-031 added SAST). Give every accepted finding an expiry date
in `deny.toml` and fail the build on an expired acceptance. Publish the SBOM with the release tag, not
only as a CI artifact. Write the dependency review policy.

**Verify:** an acceptance with a past expiry fails `just audit`.

## Definition of done

- [ ] The threat model covers the phases 08–10 surfaces, each mitigation resolving to a test or a gate.
- [ ] `just route-matrix` is green and fails on an unmapped route; the matrix is committed and its diff
      is acknowledged in the PR.
- [ ] `just rls-matrix` covers every table and every child partition by name, and fails on a partition
      with a dropped policy.
- [ ] `just test-idor` enumerates from the route matrix and passes; every wrong-tenant request returns
      404 or 403 with no existence leak.
- [ ] Security header snapshots pass per app and fail on a removed header.
- [ ] Abuse controls count on every named surface; the tracking endpoint enforces and holds under a
      flood; the would-have-blocked rate is committed.
- [ ] Redaction tests enumerate secret-bearing types automatically and fail on a new unredacted type.
- [ ] All scanners block; an expired finding acceptance fails the build; the SBOM publishes with the tag.
- [ ] **Every finding from this task has a regression test that fails against the vulnerable commit.**

## Verification

Run this **once, in slice 11.5g**. While working a slice, run that slice's scoped commands from
`## Execution slices` instead.

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

Credential rotation drills (TASK-025 — this task produces the inventory that scopes them). Per-plan
rate limits and quotas, which are commercial limits rather than security controls (TASK-026). Frontend
CSP tightening and bundle secret scanning (frontend TASK-024).

## Parked

The third-party penetration test and its remediation window, two-person review of the route matrix,
and IaC scanning — see [`../../release/needs-humans.md`](../../release/needs-humans.md). The pen test
is the largest single unmitigated gap in this phase and is named as such in the GA checklist.

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
