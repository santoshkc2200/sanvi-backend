# TASK-001: 09.0 Contract, schema & foundations

**Phase:** 09
**Status:** done
**Requirement(s):** FR-902, NFR-902, NFR-903, NFR-905, NFR-907
**Depends on:** phase 03, 04, 08
**Created:** 2026-08-20

**Slice:** 09.0 — Contract, schema & foundations
**Prerelease:** `v0.10.0-alpha.1` · **Flag:** `payments.enabled` (off)

## Context

The skeleton and the guardrails, before any Stripe call exists. At the end of this task the
`payments` context compiles, owns a schema with RLS, is reachable through an entitlement-gated route
that returns an empty catalog, and the CI gates protecting the "never merchant of record" invariant
already fail on violations.

Nothing user-visible ships, and that is the point: the expensive mistakes in a payments context are
all made in the first week, and they are all structural.

## What to do

The contract is merged **first** — `api/payments.yaml`, registered in `apps/api/src/openapi.rs`
alongside the existing eleven contexts — then the code.

- [ ] **Contract** — new `api/payments.yaml` with one endpoint,
      `GET /api/v1/tenant/payments/providers` (`payments.read` + entitlement), returning
      `{ "providers": [] }` until TASK-002. Also fixed here because every later slice depends on the
      shapes being stable:
      - the `payments/*` problem-details error namespace: `provider-unavailable`,
        `connection-not-active`, `capability-missing`, `currency-unsupported`, `idempotency-conflict`;
      - money as minor units + ISO-4217 code, never a float — JPY examples in the spec now;
      - `Idempotency-Key` documented on every mutating payments endpoint.
- [ ] **Crate scaffolding** — `crates/contexts/payments/` following the `domains` and `billing`
      layout: `src/{lib,config}.rs`, `src/domain/`, `src/application/`,
      `src/adapters/{http,repos,events,metrics,stubs}.rs`. Wired into the workspace `Cargo.toml`,
      `apps/api` and `apps/worker`.
- [ ] **Migration** — `migrations/20260823000000_tenant_payments.sql` (+ `.down.sql`), next in
      sequence after `20260822000000_domains_core`. Creates the `payments` schema and the five tables
      (`provider_connections`, `checkouts`, `payments`, `refunds`, `webhook_events`) with the partial
      unique index `(tenant_id, provider) WHERE disconnected_at IS NULL`. Money columns are `bigint`
      minor units plus a `text` currency column — never `numeric`.
- [ ] **RLS** — tenant isolation policies on all tenant-scoped tables, per the phase-01 pattern.
      `payments.webhook_events` is platform-scoped (events arrive before we know the tenant) and is
      therefore **not** tenant-readable; it joins to a tenant only through `external_account_id`.
- [ ] **Permissions** — register `payments.read`, `payments.manage`, `payments.checkout`,
      `payments.refund` in the `access` seed and add them to tenant-owner and tenant-admin roles.
      `payments.refund` is deliberately **not** in the default staff role.
- [ ] **Entitlement** — `payments.stripe_connect` in the `platform-admin` entitlement catalog, granted
      per plan. The route returns `403 entitlement-required`, not 404, so the frontend can show an
      upgrade prompt rather than a dead end.
- [ ] **Config & secrets** — `configs/` entries for the Connect webhook signing secret, the Connect
      restricted API key, the pinned Stripe API version, and `payments.application_fee.enabled`
      (default `false`). Distinct per environment; the Connect RAK carries only Connect permissions
      and is a different key from the phase-04 platform key.
- [ ] **Feature flag** — `payments.enabled`, default off, checked at the HTTP boundary.
- [ ] **CI gates** (the important part of this task):
      - `scripts/check-connect-invariants.sh` fails on `type: 'standard'|'express'|'custom'`,
        `charges_enabled`, `payouts_enabled`, `payment_method_types` anywhere in the repo, and has a
        `--self-test` mode proving it catches a deliberate violation;
      - a test asserting the Connect webhook secret does **not** validate a phase-04 platform event,
        and vice versa;
      - a build-output scan asserting no `sk_`/`rk_` pattern appears in any frontend bundle.

## Acceptance criteria

- [ ] `api/payments.yaml` is served and `just openapi` passes; handler/spec divergence fails CI.
- [ ] `just migrate` then `just migrate-down` round-trips cleanly on a seeded database.
- [ ] Tenant A cannot select tenant B's `provider_connections` rows — asserted through the repo layer
      **and** directly in SQL.
- [ ] `GET /api/v1/tenant/payments/providers` returns 403 without the entitlement, 403 without
      `payments.read`, and 200 with both.
- [ ] `payments.read`/`manage`/`checkout`/`refund` and the `payments.stripe_connect` entitlement are
      visible in the platform admin console.
- [ ] `just connect-invariants-self-test` demonstrates all gates fail on a deliberate violation.
- [ ] No Stripe SDK call exists anywhere in the codebase yet.

## Verification

```bash
just fmt
just clippy
just layering
just connect-invariants
just connect-invariants-self-test
cargo test -p sanvi-payments
just test
just openapi
```

## Out of scope

Any Stripe API call, the domain model beyond what the empty catalog needs, ports, the fake provider
adapter (all TASK-002), and every webhook handler (TASK-004+).

## Files likely touched

- `api/payments.yaml`, `apps/api/src/openapi.rs`
- `crates/contexts/payments/**`, `Cargo.toml`, `apps/api/**`, `apps/worker/**`
- `migrations/20260823000000_tenant_payments.sql` + `.down.sql`
- `configs/base.toml` + per-env overlays
- `scripts/check-connect-invariants.sh`, `justfile`

## Notes / gotchas

- `payments.webhook_events` being platform-scoped is deliberate; do not add a `tenant_id` RLS policy
  to it out of consistency.
- The migration timestamp must stay after `20260822000000_domains_core`.
- Rollback for this slice is flag-off; the migration stays, since empty tables are harmless.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
