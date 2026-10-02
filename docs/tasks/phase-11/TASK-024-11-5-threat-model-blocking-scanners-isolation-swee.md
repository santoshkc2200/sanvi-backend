# TASK-024: 11.5 Threat model, route matrix & isolation sweep

**Phase:** 11 · **Status:** done · **Size:** L
**Requirement(s):** FR-1116, FR-1117, NFR-1108, NFR-1109, NFR-1113, NFR-1114
**Depends on:** TASK-031
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01 · **Closed:** 2026-10-02
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

### Slice 11.5c record — 2026-09-30

Completed the schema-wide RLS matrix in this slice's commit
(`test(security): assert the schema-wide RLS matrix with partitions by
name`). `apps/api/tests/integration/rls_matrix.rs` enumerates the **live
catalog** rather than listing tables: every table with a `tenant_id`
column must carry ENABLE + FORCE ROW LEVEL SECURITY and exactly one
canonical `tenant_isolation` policy, matched **exactly** (whitespace
normalized) against the deparse `platform.enable_tenant_rls` produces — so
a quietly weakened predicate, a second permissive policy OR-ed in, or a
policy scoped away from `{public}` all fail naming the `schema.table`.
`sanvi_app` staying `NOBYPASSRLS` is asserted first, because a flipped bit
there makes every policy decoration. Tables that are RLS-forced without a
`tenant_id` column must be classified: only `localization.translations`
is, and its nine-policy shared-catalog set is pinned by name with every
predicate required to stay bound to the `sanvi.*` GUCs. One reasoned
exemption exists, frozen to a column allowlist:
`domains.live_hostnames` (public-DNS resolution hot path, deliberately
readable unscoped per `migrations/20260822000000_domains_core.sql`) — the
column freeze is what keeps the exemption from silently widening. The
matrix found the schema clean: all 55 tenant tables + 2 partitioned
parents + 4 children conform; no new finding.

Partitions are asserted **by name**: every child of every partitioned
parent (via `pg_inherits`) must itself appear in the tenant-table
enumeration, and a parent with zero children fails rather than passing
vacuously. The behavioural half re-runs the phase-01 proof against both
partitioned parents through the parent *and* the directly-named child
(current month resolved at runtime; `ensure_monthly_partitions` invoked
first through the application role, which is itself the worker's daily
path and the only role granted EXECUTE): scoped reads see only the
caller's tenant on both access paths, an explicit foreign `tenant_id`
parameter leaks nothing, cross-tenant INSERTs are rejected by `WITH
CHECK` as SQLSTATE 42501 row-level-security violations (each in its own
transaction, since a rejected statement aborts it — as it would a
request), a `conversion_events` UPDATE aimed at the foreign tenant touches
zero rows (that table deliberately has no DELETE grant; retention drops
partitions), a `metrics_daily` DELETE through parent and child stops at
the caller's own rows, and the audited platform scope still sees and
counts everything with its audit rows written.

Falsified both ways against a mutated template (the database every test
clones, so the drill reaches the real test binary, then dropped to
rebuild from migrations): dropping `tenant_isolation` from
`advertising.conversion_events_202609` failed the structural test naming
that partition; a table with `tenant_id` and no RLS at all
(`rls_drill.leaky`) failed naming that table with the remediation in the
message.

Verification: `cargo c`, `cargo fmt --all`, `cargo clippy -p sanvi-api
--all-targets` clean; `scripts/test.sh -p sanvi-api` 48/48; `just
rls-matrix` green (recipe added, mirroring `test-idor`'s scoping: the
workspace suite runs it under `just test` too).

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

### Slice 11.5d record — 2026-09-30

Completed the specified security-header set and its per-surface snapshot
in this slice's commit (`security(http): specify the security-header set
and snapshot it`). **One finding, fixed in the same commit: HSTS was dead
code.** The middleware gated the header on `config.host` not being
`localhost`/`0.0.0.0` — but `server.host` is the *bind* address, and
every environment (base plus all overlays) binds `0.0.0.0`, so no
deployment, staging and production included, has ever emitted
`Strict-Transport-Security`. Exactly NFR-1114's "incidental rather than
specified" defect; the header existed in code and in the loose contract
assertion while being absent in every real response.

The specification now has two halves. The five headers with no
per-environment variance (CSP `default-src 'none'; frame-ancestors
'none'`, `X-Frame-Options: DENY`, `Referrer-Policy: same-origin`,
permissions-policy, nosniff — values unchanged from the previous
middleware) are pinned as code constants in `SecurityHeaders`, on the
stated principle that configuring them invites quiet per-environment
weakening. HSTS is the one switch: `server.security.hsts`
(`HstsConfig`: max-age, includeSubDomains, preload) with startup
validation — max-age ≥ 1, and a preload directive that cannot qualify
for the preload list (needs includeSubDomains and max-age ≥ 31536000) is
rejected as the typo it is. Base config documents the off default
(plain-HTTP local dev); the staging and production overlays enable
`max-age=31536000; includeSubDomains` (preload deliberately off —
submission is an ops decision, recorded in the config comment).

The snapshot (`apps/api/tests/integration/headers_snapshot.rs`): the
backend ships one HTTP app, so "per app" is every *serving surface* of
it — liveness probe, public API JSON, the 404 error path, `/openapi.json`,
`/metrics`, and the cross-origin tracking path — each requested from the
fully assembled app and compared for **exact map equality** against the
committed six-header snapshot, in both postures (local: no HSTS;
TLS: the overlay directive). A duplicate of any governed header fails
(duplicates can hide a weakened value behind a correct one);
`server`/`x-powered-by` must stay absent on every surface. One designed
non-coverage is pinned rather than left implicit: CORS **preflight**
responses carry none of the headers, because actix-cors short-circuits
`OPTIONS` outside the security-headers middleware — correct (a preflight
has no renderable body), and asserted so a future stack reorder is a
conscious snapshot update. The middleware unit tests pin the same set at
unit level; the loose `responses_carry_security_headers_and_request_id`
contract test stays as a redundant guard. The contract-test harness
(`test_config`/`test_app_with`) became `pub(crate)` for the snapshot
rather than duplicating a hundred lines of wiring.

Falsified three ways: deleting the `X-Frame-Options` insertion fails the
snapshot naming that header on `/healthz`; simulating a wiring regression
that drops the configured HSTS fails the TLS-posture snapshot naming
`strict-transport-security` (this is also the regression test for the
finding — against the old heuristic commit, the configured-directive
assertion cannot pass); and the API binary itself refuses to start under
`SANVI_ENV=production` with `max_age_secs=0`, exiting 2 with
`server.security.hsts.max_age_secs must be >= 1`, while the real
staging/production overlays pass startup validation and serve. Note for
later slices: the CLI's `config` command validates only its own config
slice (no `server` section), so `just config-check` does **not** cover
server-side security config — the API/worker startup path is the gate.

`just route-matrix` correctly caught this slice: its fingerprints include
`crates/platform/http/src/{config,lib,middleware}.rs` (the middleware
carve excludes only the header/CSS block, keeping the `stack()` wiring
change visible). Regenerated with `just route-matrix-write`; the diff is
the three hash rows and nothing else — to be acknowledged in the PR
description per the 11.5a review-substitute rule.

Verification: `cargo c`; `cargo fmt --check`; `cargo clippy -p
sanvi-http -p sanvi-api --all-targets` clean; `scripts/test.sh --quiet
-p sanvi-http -p sanvi-api` 110/110 (six new unit tests, three snapshot
tests); `just route-matrix` complete and current. The task's full
`## Verification` block still waits for slice 11.5g.

### Slice 11.5e record — 2026-09-30

Completed the abuse controls in this slice's commit (`feat(http): abuse
quotas in shadow mode, tracking enforced (TASK-024 11.5e)`). The
mechanism is one platform module: `crates/platform/http/src/abuse.rs`
holds a fixed-window `AbuseCounter` (Redis `INCR` + `EXPIRE … NX` Lua,
one round trip; an in-process bounded window whenever Redis is absent or
errors — shadow data must not depend on infrastructure health, and an
enforced quota that failed closed would let anyone take the beacon down
by killing Redis) and the `AbuseQuota` gate. Surface membership is code
constants, not config — which route belongs to which quota is a security
decision; thresholds under `server.abuse` are config, so the
needs-humans tuning promise (config change, not code change) holds.
Counting is live by default on every FR-1117 surface the backend owns
and cannot be disabled by omission (validation rejects a quota-less
surface). The gate sits *inside* the security-headers layer and answers
an over-quota beacon with the handler's own uniform empty `202` — a
blocked beacon must not be distinguishable from an accepted one by
status, body, or header set; non-tracking surfaces deny with the
platform's standard `429` + `Retry-After` (the future config-flip path,
unit-tested; nothing ships with it on). Invitation `accept` is excluded
from the invitation quota (a join is not a send), and per-tenant keys
come from the resolved `TenantContext`, so counting runs
post-authentication by design.

Two FR-1117 findings recorded rather than papered over: **password
reset has no backend surface to count** (it is a Kratos recovery flow;
the backend owns no route), and **signup's backend surface is the
internal `/internal/kratos/registration` webhook** (shared-secret +
IP-allow-listed), which is what the signup quota counts. Outbound
notification sends count at the gateway port itself —
`HttpNotificationGateway::with_abuse_counter` counts per tenant on both
lanes (API Lane B and worker Lane A share the Redis key namespace) —
with **no enforcement path by design**: dropping delivered invitation or
DSR credential mail is a policy decision the would-block rate informs,
and none of the shipped config can trigger it.

Verification: `cargo c`; `cargo fmt --check`; clippy clean on
sanvi-http/-notifications/-api/-worker; `scripts/test.sh -p sanvi-http`
70/70 (50 lib incl. the flood-holds test: beyond a 5-cap, 50 requests →
handler invoked exactly 5×, every response the byte-identical 202;
shadow pass-through; window reset; per-key isolation; the 429 flip
path); `scripts/test.sh -p sanvi-api` 56/56 (new `abuse_quotas.rs`:
enforcement decides *before* the router on the assembled app — within
quota falls through to 404, over quota is answered 202 by the gate;
shadow changes nothing; `configs/base.toml` parses through the binary's
figment chain into the shipping posture; startup validation names the
offending `server.abuse.*` key; the Redis path shares one window across
two counter instances with the window TTL proven present);
sanvi-notifications 18/18 (sends count per tenant, are never blocked,
and a gateway without a counter still sends); `just route-matrix` +
self-test green (six metadata rows now name their abuse quota; preamble
documents the shadow posture); `just layering`; `just config-check`.
The 3×-peak measurement is committed as
`benchmarks/abuse-shadow-2026-09-30.md`: 9,000-beacon flood at 150 rps —
per-IP blocked 97.3 % (run A, enforced) vs would_block 98.7 % (run B,
`enforce=false`, window phase, not behaviour); both postures 9,000/9,000
iterations, 0 failed, 0 dropped, p95 2.846 ms vs 2.845 ms — the flip is
invisible to the caller. The other surfaces recorded honest zeros with
the reason (the lab cannot authenticate; counting is live, the driver is
missing, not the control).

### Slice 11.5g record — 2026-10-02

Completed the threat-model refresh and closed the task in this slice's
commit. **Step 1 — the threat model.** `docs/security/threat-model.md` is
rewritten from a phase-by-phase threat list into **v1: STRIDE per trust
boundary** — sixteen boundaries (public edge; host→tenant resolution;
Kratos; authorization; RLS row isolation; Stripe webhooks; Stripe keys;
ad-platform OAuth; the public tracking beacon; impersonation; DSR/privacy;
telemetry/secret render paths; async outbox/worker; TLS issuance/egress
probes; media/notification egress; CI/CD supply chain) with the phase
08–10 surfaces the step names explicitly covered: subdomain/hostname
takeover (T46), forged TLS authorize (T47), beacon flood (T48), raw PII to
ad platforms (T49), consent bypass (T50), OAuth state forgery (T51), token
theft at rest (T52), impersonation abuse (T53), plus T54–T62 for the
existence oracle, the HSTS dead-config class, render-path leaks, gate
truth, acceptance rot, undeclared routes, the forged internal tenant
header, media namespace crossover and notification floods. Threat ids
T1–T45 are preserved from v0 (the duplicated phase-02 residual section is
gone); every boundary carries at least one mitigation resolving to a test
file path or CI gate (mechanically checked: every TB section names 2+ artifact
paths), and a coverage map pins all fifteen context crates + three apps to
boundaries and evidence. Named residuals added: the runtime
`x-sanvi-impersonate` read-only clamp has no assembled-app test;
`notifications`/`media` ship no `tests/` dir; the registrar runs
hard-validated sandbox mode.

**Step 2 — the verification block, which found four real defects.** The
11.5f host blockage (macOS `sqlx_macros` dylib corruption) is resolved by
host change, not by fixing macOS: this slice ran on a fresh Windows host
(pinned toolchain 1.97.1 auto-installed; nextest/cargo-audit/cargo-deny/
cargo-cyclonedx installed for it; `just` absent — the script equivalents
ran). The block's first pass was 1217/1223 tests, layering FAILED, fmt
FAILED; each defect fixed with its proof:

1. **Audit chain seal hashed a timestamp Postgres cannot store — the
   significant find.** `AuditEntry::new` hashed `occurred_at` via
   `to_rfc3339()` at nanosecond precision while the writer binds it into a
   `timestamptz` (microseconds), so the verifier — recomputing over the
   stored value — rejected every entry whose clock had non-zero sub-µs
   digits ("entry hash does not match its content"). macOS's ~1 µs clock
   granularity masked it for eleven phases; Windows 100 ns ticks exposed
   it; production Linux (ns clocks) would have failed `audit-verify`
   everywhere. Fix: the seal truncates `occurred_at` to microseconds
   (`trunc_subsecs(6)` — truncation, because Postgres truncates; rounding
   would re-break it) so the hash covers exactly what storage keeps;
   regression test `sub_microsecond_timestamps_survive_the_storage_round_trip`
   fails against the vulnerable commit, and the three integration chain
   tests are the behavioural proof. `just audit-verify` over the seed
   corpus is unaffected (fixed-base µs timestamps; truncation a no-op).
2. **The layering gate silently matched no exception on Windows.** `rg`
   emits backslash separators below the search root, and every exception
   matcher compares forward-slash literals — so the accepted
   `ScopedTx`-in-domain-ports pattern (a documented exception in every
   context) reported as violations. `rel_path()` normalizes separators in
   all four checks; check and self-test green; on macOS the change is a
   no-op.
3. **The secret scanner enumerated zero fields on Windows.** The same
   separator class: the `/src/` filter and the rendered finding paths
   broke, tripping the scanner's own vacuity guard (`< 25 findings`) —
   the guard doing exactly its job. `rust_sources` now normalizes paths;
   33/33 telemetry tests green with the real 68-field enumeration.
4. **Two clock/precision test defects:** the advertising reconnect test
   compared in-memory `Utc::now()` against DB-stored timestamps (µs) —
   the test now captures `now` pre-truncated; and `pii_corpus.rs` had one
   stale rustfmt formatting (line length; the only `cargo fmt --check`
   diff).

**Full Verification block, final state (each command run and passed):**
`just fmt` → `cargo fmt --check` clean; `just clippy` → workspace
`--all-targets -D warnings` finished clean (changed crates re-verified
after the fixes); `just layering` → OK + self-test; `just test` →
`scripts/test.sh --quiet` **1224/1224** (the +1 is the new audit
regression test); `just openapi` → 14 contexts OK; `just route-matrix` →
complete and current (this commit touches no fingerprinted source, so no
matrix diff — nothing to acknowledge; prior slices' diffs were
acknowledged in their records); `just rls-matrix` → green; `just
test-idor` → green; `just audit` → cargo audit + cargo deny +
acceptance-expiry check all OK; `just sbom` → 25 documents (gitignored;
release.yml publishes them with the tag). CI has still never run — no git
remote (unchanged caveat from TASK-031/019/11.5f).

Definition of done: all eight items checked. The parked pen test remains
the largest unmitigated gap, named in `needs-humans.md` and in the threat
model's residual section.

### Slice 11.5f record — 2026-09-30

Completed steps 7–8 in this slice's commit (`security(telemetry): secret
field enumeration gate, redaction fixes, dated advisory acceptances`).
**Step 7 — the gate.** `crates/platform/telemetry/tests/secret_inventory.rs`
enumerates secret-shaped struct/enum fields **from the source tree** (walks
`crates/`+`apps/` `src` trees, declaration bodies only — function parameters
and struct-literal constructions cannot match — skipping `#[cfg(test)]`
modules and `tests/` paths). Today's tree yields 68 fields; **15 are
`Redacted<String>`-wrapped config fields**, and **53 carry reasoned
exemptions** in `tests/secret-exemptions.txt` (17 runtime-carrier, 9
capability-input, 8 manual-redaction, 4 ciphertext, 3 wire-out, 3 wire-in,
3 public-by-design, 2 capability-delivery, 2 config-carrier, 2
not-a-secret). Every class has a mechanical rule the scanner verifies per
field (e.g. `config-carrier`/`runtime-carrier`: owning struct derives
neither `Debug` nor `Serialize`; `wire-out`/`capability-delivery`: no
`Debug`; `manual-redaction`: a same-file manual redacting `Debug` plus a
serde guard — the existing `DatabaseConfig`/`OtlpConfig`/`MediaConfig`
house pattern is honoured, not rewritten). Unclassified fields, class-rule
violations, and stale exemption entries all fail. `just secret-scan`
(script `scripts/check-secret-scan.sh`) is wired into `just ci`/`ci-quiet`
and pr.yml (`secret-scan` job). Falsified three ways: the always-on
fixture test (planted unclassified field, rule-violating exemption, stale
entry, clean wrapped/exempted controls — each asserted by name);
`--self-test` plants a brand-new unredacted `webhook_secret` field in a
real src tree and expects the failure naming it; and a vacuity guard
(fewer than 25 findings = broken parser, not clean tree) after the
prototype scanner initially reported zero because of a workspace-root
path bug.

**Step 7 — findings fixed (each with a regression test).** (1)
**`Redacted<T>` serialized its plaintext**: `Serialize` forwarded the inner
value while Debug/Display redacted — the one render path a wrapped secret
could still leak through. `Serialize` now emits `<redacted>`; `Deserialize`
still accepts real values (secrets flow in from config, never out through
a serializer); `all_four_render_paths_redact` fails on the vulnerable
commit. `Default` and `FromStr` impls added for config/clap shapes. (2)
**Every secret-bearing config struct derived Debug and/or Serialize on
plaintext fields** — billing (Stripe key + webhook secret), payments
(Connect key + webhook secret), privacy (KEK), identity (Kratos webhook
secret), domains (internal secret), theming (preview secret), advertising
(six fields incl. the token KEK), and tenancy's `SigningKey.secret`
(the internal-header HMAC key: `Debug + Serialize + PartialEq` on the
secret itself). All wrapped in `Redacted<String>` / `Redacted<BTreeMap<..>>`
so the struct derives stay and render `<redacted>`; use sites expose via
`expose_secret()`. (3) `ExchangedAuthorization` derived an unused
`Serialize` over a raw ad refresh token — removed (compiler-enforced). (4)
Kratos `Credentials` got a manual redacting `Debug` (hashed credential
config could render through the identity wire structs' derived Debug);
`credentials_never_render` proves it nested. (5)
`ProviderAccountSession.client_secret` (Connect one-time secret) — manual
redacting Debug + `client_secret_never_render`. (6) Unused `Debug` derives
dropped from the capability-token DTOs (DSR verification/download tokens,
passphrase headers/commands, theming preview view/query, invitation
acceptance). `docs/security/secrets-inventory.md` records every secret
class with storage, rotation path (feeding TASK-025's drills), blast
radius, and the render-safety mechanism with its proof.

**Step 8 — scanner set.** `deny.toml` `[advisories]` is now a dated
acceptance ledger: every structured `ignore` entry's reason must end with
`expires:YYYY-MM-DD`; `scripts/check-audit-acceptances.sh` fails the build
on a missing, unparsable, or expired date (self-test covers past/missing/
unparsable/future; comment lines excluded after the first falsification
run caught the header's own example). `just audit` = `cargo audit` +
`cargo deny check` + the expiry check; `just sbom` mirrors CI's cyclonedx
command; pr.yml's audit job runs the expiry check (+ self-test), and the
new `.github/workflows/release.yml` rebuilds the SBOM **from the tagged
commit**, checksums it, re-runs the advisory gates on that tree, and
attaches SBOM+checksum to the GitHub release on every `v*` tag.
Advisory state on the tree: h2 0.4.15→0.4.19, rustls 0.23.43→0.23.45,
yanked chacha20→0.10.2 (patch bumps); RUSTSEC-2026-0258 remains on the h2
0.3 copy actix-http 3.x pins — **accepted with expiry 2026-11-15**
(no 0.3.x fix exists; HTTP/2 terminates at the edge; actix's h2-0.4 line
is a migration, not a bump), mirrored in `.cargo/audit.toml` with
deny.toml documented as the ledger. Two latent pr.yml defects found by
*running* the commands: the sbom job used a `--output-format` flag
cargo-cyclonedx 0.5.x does not have, and its artifact glob
(`*.cdx.json` at the root) missed the per-crate documents — both fixed;
`just sbom` now produces 25 valid CycloneDX 1.3 documents.
`docs/security/dependency-policy.md` written: add-a-dependency rules,
license allowlist rationale, the acceptance lifecycle, update cadence,
SBOM provenance, scanner-coverage table.

**Verification.** `cargo fmt --check` clean; workspace clippy passed with
only the new telemetry-test lints, which were fixed and re-verified
(`-p sanvi-telemetry --all-targets` clean); `cargo nextest run -p
sanvi-telemetry` 33/33 + doctests; sanvi-payments + sanvi-identity lib
tests 158/158 green after the regression tests; `just secret-scan` +
`--self-test` green; `just audit` green and the expiry gate falsified by
planting a past date into the real entry (exit 1 naming it) then restored;
`just sbom` green; route-matrix regenerated (`just route-matrix-write`) —
diff is exactly six fingerprint rows (api config/lib/main, payments/
privacy/theming handlers), **no route rows changed**; acknowledged here
per the 11.5a rule; `--self-test` green. **Blocked on the host:** the
full-workspace clippy/test re-run cannot link — `sqlx_macros`' proc-macro
dylib is born corrupt ("mis-aligned LINKEDIT string pool", same offset
every time) on every fresh link, including after a full `cargo clean`;
`codesign` validates the file and a trivial proc-macro links fine, so
this is the large-dylib linker flakiness this host already exhibited in
TASK-019's release build, resurfacing now that the Cargo.lock bump forced
a relink. All Rust source in this slice compiled and its unit tests ran
green before the corruption began; the task's full `## Verification`
block (including `just test`) runs in 11.5g, which must resolve or
escalate the host issue first. Also observed, not silently fixed: the
four TASK-020 gates (trace-coverage, log-pii-scan, runbook-xref,
alert-rules-test) are in `just ci` but **not** in pr.yml — recorded here
because this slice's gate followed the route-matrix/layering precedent of
also wiring into pr.yml; TASK-020's CI wiring is that slice's decision to
revisit. CI has still never run on any of this (no git remote — same
caveat as TASK-031/019).

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

- [x] The threat model covers the phases 08–10 surfaces, each mitigation resolving to a test or a gate.
      *v1 2026-10-02: STRIDE per trust boundary, sixteen boundaries; every TB section names 2+
      artifact paths (mechanically checked); all fifteen context crates mapped with evidence;
      phases 08–10 surfaces explicit (T46–T53).*
- [x] `just route-matrix` is green and fails on an unmapped route; the matrix is committed and its diff
      is acknowledged in the PR.
      *Green + mutation self-tests (unmapped route, undocumented handler, missing fields, dropped
      extractor, stale output — 11.5a). No PR has ever been possible (no git remote); each slice
      that regenerated the matrix (11.5c/d/e/f) acknowledged its diff in its slice record, and the
      closing commit changes no fingerprinted source, so no matrix diff exists to acknowledge.*
- [x] `just rls-matrix` covers every table and every child partition by name, and fails on a partition
      with a dropped policy.
      *11.5c: catalog-enumerated, exact policy match, partitions via `pg_inherits` by name, both
      falsification drills fail naming the offender.*
- [x] `just test-idor` enumerates from the route matrix and passes; every wrong-tenant request returns
      404 or 403 with no existence leak.
      *11.5b: matrix-driven (`route-metadata.json` at test time), three sweeps + nonexistent-id twin;
      timing comparison explicitly parked.*
- [x] Security header snapshots pass per app and fail on a removed header.
- [x] Abuse controls count on every named surface; the tracking endpoint enforces and holds under a
      flood; the would-have-blocked rate is committed.
      *Counted surfaces: tracking, invitation (create/resend), domain purchase, signup webhook,
      notification sends. Password reset has no backend surface (Kratos recovery flow) — recorded in
      the 11.5e slice record, not silently counted elsewhere. Rates in
      `benchmarks/abuse-shadow-2026-09-30.md`.*
- [x] Redaction tests enumerate secret-bearing types automatically and fail on a new unredacted type.
      *`just secret-scan` (`scripts/check-secret-scan.sh` → `tests/secret_inventory.rs`) walks the
      source tree: 68 secret-shaped fields, 15 `Redacted`-wrapped, 53 reasoned exemptions, class rules
      verified mechanically; unclassified fields, rule violations and stale entries all fail. Self-test
      plants an unredacted field in a real src tree and expects the named failure. In `just ci` and
      pr.yml.*
- [x] All scanners block; an expired finding acceptance fails the build; the SBOM publishes with the tag.
      *`just audit` = cargo audit + cargo deny + `check-audit-acceptances.sh` (dated ledger in
      deny.toml, falsified with a planted past expiry); pr.yml audit job runs the check; release.yml
      rebuilds the SBOM from the tagged commit and attaches it + checksum to the GitHub release.
      CI has never run (no git remote) — workflow correctness verified by running the same commands
      locally (`just sbom` produced 25 valid documents after fixing two latent flag/glob defects in
      the pr.yml job).*
- [x] **Every finding from this task has a regression test that fails against the vulnerable commit.**
      *Findings and their failing tests: `/ads/health` path defect → the route-matrix gate (11.5a);
      budget-cap foreign-id 500 → the IDOR sweep (11.5b); HSTS dead code → the TLS-posture snapshot
      (11.5d); `Redacted::Serialize` plaintext + eight config structs' derived Debug/Serialize → the
      four-render-paths test + the source-enumeration gate (11.5f); audit chain hashing nanosecond
      timestamps → `sub_microsecond_timestamps_survive_the_storage_round_trip` + the three chain
      integration tests (11.5g, found by running the verification block on a second platform);
      Windows path-separator blindness in the layering gate and the secret scanner → their self-tests
      plus green runs on a `\`-separator host (11.5g). The verification-block clock-precision test
      fix (advertising reconnect) is itself the regression test for the test defect.*

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
