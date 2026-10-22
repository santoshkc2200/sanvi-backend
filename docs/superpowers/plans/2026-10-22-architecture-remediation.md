# Architecture review remediation implementation plan

> **For agentic workers:** Use the debugging and regression-test workflows task by task. Independent subsystem fixes may execute in parallel with exclusive file ownership.

**Goal:** Fix the ten concrete implementation findings in the 2026-10-22 backend review.

**Architecture:** Retain the modular monolith, scoped transactions, outbox, and provider adapters. Enforce authoritative tenant authorization; make external projections repairable, events recoverable, financial audits durable, and deployments executable.

**Tech stack:** Rust, Actix, SQLx/PostgreSQL, Redis Streams, Docker Compose, GitHub Actions.

**Spec:** The review accepted by the user's “Fix them” instruction: authorization propagation/checkpoints/ordering, stream retention, forwarding-header trust, deployment configuration/health/images, refund audit, worker metrics, maintenance migrations, checkout payload idempotency. Scope excludes new self-service flows and a general context refactor.

## Global constraints

- Follow CONTRIBUTING.md, CLAUDE.md, and the Snowflake ID policy.
- Preserve unrelated user changes; no commits or deployments requested.
- Add focused regression tests before fixes, observe red/green where the environment permits, and report verification limitations honestly.
- Use scripts/test.sh with its shared Postgres/Redis harness for database tests; never invoke database test targets without the harness environment.
- Keep generated OpenAPI current if transport contracts change.
- Every mutation and event remains transactional. No destructive historical migration rewrite.

## Review focus

- A stopped worker with healthy Keto must not preserve revoked tenant access.
- Retries, concurrent events, and reversed event order must converge to authoritative permissions.
- Consumer downtime past the old stream cap must preserve required events and DLQ work.
- Proxy forgery must not influence client identity; legitimate traffic must retain meaningful quotas.
- Provider success followed by a crash must leave a durable attributable financial intent and recoverable final audit.

## Tasks

- [x] **1. Authorization consistency** — access synchronization and identity verification, with targeted authorization regression tests. Preserve platform operator/impersonation semantics. Checkpoints represent completed work; serialize/reconcile snapshots rather than applying unordered deltas. Own `crates/contexts/access`, `crates/contexts/identity`, and access-related API tests. Any root/worker composition changes require coordination.
- [x] **2. Financial correctness** — refund audit durability and checkout request fingerprint validation. Own `crates/contexts/payments` plus additive payments migrations. Persist attribution before provider calls and audit atomically with finalization or through a durable audit command. Test provider-success recovery and same-key changed-input conflicts.
- [x] **3. Deployment and proxy trust** — own `.github/workflows/main.yml`, `Dockerfile`, `docker-compose.staging.yml`, `scripts/deploy-staging.sh`, deployment tests, and forwarding-header boundary fixes. Propagate exact image digest, inject required runtime configuration, give workers valid probes, classify maintenance migrations, and sanitize proxy forwarding inputs. Regression tests include actual config semantics and deployment controller behavior.
- [x] **4. Messaging durability** — own `crates/platform/messaging`, messaging recovery tests, and applicable CLI replay integration. Remove consumer-blind destructive caps, provide durable retained-event recovery/checkpoints where required, preserve DLQ work, and verify lagging consumers and Redis recovery. Document the recovery/retention contract.
- [x] **5. Worker management and integration** — add private worker health/metrics, supervise task failures, compose all agent changes without overlap, and update runbooks and precise delivery/spend requirement language. Do not build new product subsystems.
- [x] **6. Verification and independent review** — format, workspace typecheck/clippy, focused and full shared-harness tests, architectural/security/contract gates. Review the combined diff and fix regressions. Record all blocked checks without claiming success.

## Progress

- Baseline checkout clean at `3038d9d`; cargo workspace typecheck started. Git Bash and Docker initially denied by sandbox; investigating elevated execution for required verification.

- Completed all ten implementation findings, independent review and regression fixes. Final workspace verification: 1,547/1,547 tests, strict Clippy and formatting pass. See [verification record](2026-10-22-remediation-verification.md) for complete evidence, rollout constraints and the unchanged 13-finding advertising API-freeze baseline exception.
