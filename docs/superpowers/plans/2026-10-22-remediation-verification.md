# Architecture remediation verification

All ten concrete findings from the architecture review are implemented in the
working tree. The modular monolith and existing provider/version choices remain
in place. No commits, deployments, live payments, or provider credential changes
were performed.

## Implemented

- Tenant authorization requires current active membership and role definitions;
  outage read grace is bound to an authoritative eligibility revision. Serialized
  snapshot reconciliation replaces unordered authorization deltas, and applied
  checkpoints commit after external effects. Checkpoint references participate
  in privacy export/erasure.
- Required event streams and DLQs no longer trim blindly at 10,000 entries.
  Consumers recover retained PostgreSQL events independently of Redis. Consumer
  failures have durable parking and audited replay.
- Every HTTP client-IP consumer uses the shared explicit trusted-proxy policy.
  The ingress discards forged forwarding claims and has a known private address.
- Checkout idempotency rejects changed input. Refund requests preserve original
  attribution before provider calls; queued audit delivery is transactional.
  API/webhook and competing-webhook finalization serialize and publish a refund
  success event only on an actual SQL transition.
- Deployment propagates immutable digests, injects private environment files,
  uses worker readiness, and enforces image/schema compatibility and maintenance
  migrations before serving changes. Arbitrary historical binary rollback is
  deliberately rejected.
- Worker health/readiness/metrics have a private HTTP endpoint, and task failures
  fail the process while siblings drain. Notifications use recoverable stream
  startup with their existing maximum-event-age policy.
- Requirements FR-1010 and FR-1013 now state realistic external delivery and
  delayed spend-guardrail guarantees.

Three additive reversible migrations were added. See
[deployment compatibility](../../../deployment/README.md) and
[event recovery](../../runbooks/event-recovery.md) for rollout requirements.

## Final evidence

Command: `cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --offline --message-format short -- -D warnings && scripts/test.sh --quiet`.

Result: exit 0. Nextest run `cd1c02bc-92cf-4275-978a-e19b06829ef0` ran **1,547
tests across 77 binaries: 1,547 passed, zero skipped**, in 75.951 seconds after
compilation. Tests used the repository's shared PostgreSQL/Redis harness.

Additional checks passed: architecture gate and planted-violation self-tests,
Connect/advertising invariants, ID policy, expand/contract classification,
outbound timeout gate, secret inventory, PII corpus, CI package coverage and
CRLF self-test, all 14 OpenAPI documents, API docs/lint, route matrix, deployment
controller (12 tests), migration policy (7), Compose configuration (2), trusted
edge validation (2), real nginx ingress (1), and rolling-restart dry-run.

Independent review found one refund-event race, which was reproduced and fixed;
the scoped re-review found no further actionable issues. Additional scoped
review of storage completion, IP unification and coverage parsing was clean.

The full runs exposed two verification defects and one additional real storage
defect: the retention regression mistook a zero-publish retry batch for an empty
queue; the PITR helper inspected exit status before command completion; and
immutable theme writes returned before Tokio's queued filesystem write completed.
Each was corrected with regression evidence, preserving recovery assertions.

## Existing baseline exception

The immutable API-freeze check still reports **13 advertising requiredness
findings**. The same 13 findings were reproduced byte-for-byte against unchanged
HEAD contracts copied into `target/architecture-head-api`; `api/advertising.yaml`
is unchanged by this patch. The frozen baseline was not rewritten and the gate
was not waived. Generated OpenAPI consistency, lint, documentation and route
checks pass; this separate historical freeze discrepancy remains visible.

## Limits

Local tests do not establish a live rollout, provider credential validity,
production RPO/RTO, or production performance. The first hardened image must be
built and verified before recording its compatibility-floor digest. Runtime
secret files, proxy trust, collector scraping, and the maintenance/restore plan
must be provisioned on the deployment host. The disposable SQLx preparation
database was removed; shared test servers/templates were retained.
