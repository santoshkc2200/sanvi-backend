# TASK-026: 11.7 API v1 freeze, deprecation machinery, rate limits & quotas

**Phase:** 11 · **Status:** done · **Size:** M
**Requirement(s):** FR-1122, FR-1123, FR-1124, FR-1125, FR-1126, NFR-1106
**Depends on:** TASK-021, TASK-023, **phase 10 merged**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flags:** `platform.rate_limits`, `platform.quotas`

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Turn fourteen independently-grown OpenAPI documents into one public contract, freeze it behind a gate
that the build enforces, and put enforceable, **visible** limits around it.

## Why this shape

**This is the last task in which breaking changes are cheap.** After it, every change costs a
deprecation window. An inconsistency found here is a fix; found later it is a six-month sunset. That is
also why the freeze lives here and not in TASK-019: freezing a contract while phase 10 is still writing
one is freezing the wrong thing.

Per-plan limits are derived from TASK-021's capacity constants, and `429` semantics must be decided
together with TASK-023's `503`, or clients get backoff wrong in both directions.

This task carries **the phase's one permitted new user-facing capability**: the usage surface.
Everything else the backlog wants to add "while we're in there" goes to post-GA.

## Constraints

- **The spec freeze does not roll back.**
- `platform.rate_limits` and `platform.quotas` off return to unlimited **while keeping the counting path
  live**, so usage data keeps accruing and limits can be switched on with real numbers behind them.
  Both roll out counting-only first.
- The usage endpoint and the enforcement path **must read the same counter**. Two counters that agree
  today diverge the first time one of them is restarted.

## File ownership map

- all fourteen `api/*.yaml` plus `api/shared/*.yaml` — problem types, pagination parameters
- `crates/platform/http/**` — rate-limit middleware, `RateLimit-*` and deprecation headers
- a new quota-counting module — Postgres reconciliation with a Redis fast path
- `scripts/spec-lint.sh`, `scripts/spec-freeze-check.sh`, `tests/replay/**`
- `docs/api/{style-guide,deprecation-policy}.md`

## Steps

### Step 1: Run the consistency pass across all fourteen documents

**Files:** Modify `api/*.yaml`, `api/shared/*.yaml`

**Do:** resource naming, pluralisation, casing, pagination (cursor everywhere, one parameter set),
filtering and sorting grammar, timestamp and money representation, enum naming, and the
problem-details envelope with one namespaced type registry.

**Verify:** read all fourteen as one API and record the inconsistencies fixed in the PR. `just openapi`
clean.

### Step 2: Extract the style guide and enforce it

**Files:** Create `docs/api/style-guide.md`, `scripts/spec-lint.sh`; Modify `justfile`

**Do:** write the guide from what the consistency pass actually decided, then enforce it with a
spectral-style linter.

**Verify:** `just spec-lint` passes, and fails on a deliberately inconsistent endpoint added in the PR.

### Step 3: Freeze the contract behind a diff gate

**Files:** Create `scripts/spec-freeze-check.sh`, `api/.frozen/**`; Modify `justfile`,
`.github/workflows/pr.yml`

**Do:** snapshot the specs; a CI job diffs against the snapshot and fails on any breaking change
without an explicit, reviewed exception label.

**Verify:** `just spec-freeze-check` fails on a removed field, a narrowed enum, and a changed
required-ness; passes on an additive change.

### Step 4: Build the deprecation machinery before anything is deprecated

**Files:** Modify `crates/platform/http/**`; Create `docs/api/deprecation-policy.md`, a deprecation
registry

**Do:** `Deprecation` and `Sunset` header emission, changelog generation, and a registry of deprecated
elements with sunset dates — so the policy is enforced by the build rather than remembered.

**Verify:** a test deprecation emits both headers and produces a changelog entry end to end.

### Step 5: Add rate limiting, counting first

**Files:** Modify `crates/platform/http/**`

**Do:** per-plan limits at the HTTP boundary, keyed by tenant **and** by API credential, with a
separate stricter bucket for unauthenticated and public endpoints — the phase-10 tracking endpoint and
phase-08 domain verification most of all. Limits derive from TASK-021's capacity constants, **with the
derivation written down next to the numbers**.

**Verify:**
- `429` carries `Retry-After`, `RateLimit-Limit`, `RateLimit-Remaining`, `RateLimit-Reset`.
- A test asserts `429` is never returned where `503` is meant, and vice versa.
- The public tracking endpoint's stricter bucket holds under a flood.

### Step 6: Count quotas so they survive a restart

**Files:** Create the quota-counting module; Modify `api/tenancy.yaml`

**Do:** metered dimensions per plan — API requests, tracked conversions, storage, custom domains,
seats, notification sends — counted with a Redis fast path reconciled from Postgres. Expose
`GET /api/v1/tenant/usage` returning current-period usage and limit per dimension. Enforce with an
**entitlement-aware error that names the upgrade path** rather than a bare `403`.

**Verify:**
- Counters reconcile correctly after a worker restart **and** after a Redis flush.
- A test asserts `/usage` and the enforcement path read the same counter.
- A quota breach returns an error naming the upgrade path.

### Step 7: Build the replay suite from the contract and e2e corpus

**Files:** Create `tests/replay/**`; Modify `justfile`

**Do:** there is no staging client traffic to record, so the corpus is generated from the contract tests
and the e2e suite instead. Smaller than recorded traffic, but still a real compatibility gate: it
asserts status, shape, and header compatibility per release candidate.

**Verify:** `just replay-suite` is green against this candidate and **fails on a deliberately broken
response shape**.

### Step 8: Generate the public API documentation

**Files:** `docs/api/**`

**Do:** generated from the spec, with worked examples per journey and per locale. **JPY and
zero-decimal handling appear in the examples, not in a footnote.**

**Verify:** the generated docs build and contain a JPY example on every money-bearing journey.

## Definition of done

- [x] All fourteen specs pass `just spec-lint`, and the linter blocks a deliberately inconsistent
      endpoint.
- [x] `/api/v1` is frozen; the gate fails on a removed field, a narrowed enum, and a changed
      required-ness, and passes on an additive change.
- [x] Deprecation headers and changelog generation work end to end against a test deprecation.
- [x] Per-plan rate limits enforce with correct headers, and the derivation from the capacity constants
      is recorded next to the numbers.
- [x] `429` and `503` are never conflated, proven by a test.
- [x] Quota counting survives a worker restart and a Redis flush.
- [x] `GET /api/v1/tenant/usage` returns the same numbers enforcement uses.
- [x] A quota breach names the upgrade path.
- [x] `just replay-suite` is green and fails on a broken response shape.
- [x] Public API docs are generated with per-journey, per-locale examples including JPY.

## Verification

```bash
just fmt
just clippy
just test
just openapi
just spec-lint
just spec-freeze-check
just replay-suite rc.8
just test-quotas
```

## Execution record

Completed 2026-10-09 on `codex/task-026-api-freeze-quotas` in the managed
`task026-api-freeze` worktree, based on `3d9fe2c`. Implementation and both completion
records share the commit titled `feat(platform): freeze API v1 and add durable quotas`.

The inventory contains fourteen contexts, including media. The consistency pass
bundles reachable schemas, namespaces twelve colliding schema declarations,
standardizes pagination on `after`/`limit` while retaining legacy input aliases,
and records timestamp, money, and problem-response conventions. Readiness 503
annotations now describe the problem response emitted by the assembled app.
The immutable launch snapshot and PR-base comparison protect both initial and
subsequently added public fields. Breaking exceptions require an independent
collaborator's approval of the current head as well as the reviewed label.

Verified equivalents were used because `just` is not installed on this Windows
host; `PYTHONUTF8=1` and `SQLX_OFFLINE=true` were set for portable checks.

| Required verification | Observed result |
| --- | --- |
| `cargo fmt --check` | Pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass |
| `bash scripts/test.sh --quiet --test-threads 4` | 1,461/1,461 across 69 binaries; nextest `c1f97d76-48d1-425c-9ba6-d614f40c6a6e` |
| `target/debug/sanvi-cli.exe openapi --check` | All fourteen contexts pass |
| `bash scripts/spec-lint.sh` | All fourteen contexts pass |
| `bash scripts/spec-freeze-check.sh --base-ref 3d9fe2c3031958e557c19b218c62febbbf0e1788` | Pass; fifteen frozen YAML files including shared registry |
| `bash scripts/replay-suite.sh rc.8` | 21/21 Rust cases; real system and tenant usage captures validate |
| `bash scripts/test-quotas.sh` | 20/20 counter/reservation tests plus 1/1 HTTP usage test |
| `bash scripts/api-docs.sh --check` | Fourteen generated context documents; en-US/USD and ja-JP/JPY journeys |
| Python contract / deprecation mutation suites | 16/16 and 4/4; registry and changelog check pass |

An actual captured usage response was copied and deliberately corrupted: a string
`dimensions.api_requests.used` and missing `limit` both fail the production replay
CLI. Mutation proofs also reject removed fields, narrowed enums, requiredness
changes, repeated-reference restrictions, and removal of fields added after launch.
Independent review findings were fixed with regressions for verified-subject quota
accounting, correlated problem responses retaining rate headers, canonical-stack
tracking floods, ambiguous media timeouts, and nested usage maps.

Counters cover all six dimensions. PostgreSQL remains authoritative; Redis reads
are validated against the committed version and value. Producer triggers and
external reservations preserve atomicity and retry safety. Both enforcement flags
default to false while counting stays live. Unknown external outcomes retain
reservations for reconciliation; shadow-mode accounting outages can leave usage
incomplete, as documented in the API policy.

Layering, migration compatibility, ID policy, secret scan, and the regenerated
route matrix passed locally. A default-concurrency run hit a transient existing
Redis healthy-control timeout; its isolated rerun and the final full run passed.
The additional CI test-coverage gate reports the pre-existing `recovery-tests`
exclusion in the base workflow; this task does not change that exclusion, and the
local full suite includes those tests. Remote CI and production threshold tuning
were not executed. The worktree is preserved; no merge or deployment is included.

## Out of scope

Frontend `429` handling, quota surfaces, and the platform-admin override view (frontend TASK-026).
Abuse quotas on specific surfaces, which are a security control rather than a commercial limit
(TASK-024). Removing the flags and setting GA defaults (TASK-030).

## Parked

A replay corpus built from real recorded client traffic, and rate-limit thresholds tuned against an
observed false-positive rate — see [`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
