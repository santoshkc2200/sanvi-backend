# Accounting Date Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route accounting postings by date across provider periods, make switches take effect automatically, give postings a resolvable status lifecycle, bound activation backfill, and split user from system actors.

**Architecture:** All changes stay inside the `sanvi-accounting` bounded context (domain → application → PostgreSQL adapter) plus its unshipped migration. The book's period list becomes the only switch state; postings own a domain state machine persisted as flat columns guarded by a terminal-state trigger.

**Tech Stack:** Rust, chrono, SQLx/PostgreSQL (RLS, column grants, plpgsql trigger), outbox via `sanvi_messaging::OutboxWriter`.

**Spec:** `docs/superpowers/specs/2026-10-26-accounting-date-routing-design.md`

## Global Constraints

- `LATE_EVENT_GRACE` = 60 days; `MAX_BACKFILL` = 365 days; both domain constants, not configuration.
- `failure_reason` and `resolution_note`: 1–1024 bytes, not whitespace-only.
- Posting statuses (DB text values): `pending`, `submitted`, `delivered`, `failed`, `resolved_manually`. Terminal: `delivered`, `resolved_manually`.
- Outbox operations: `posting_assigned`, `posting_submitted`, `posting_delivered`, `posting_failed`, `posting_resolved` (event type `accounting.<operation>`).
- `Export` permission is `Actor::System` only; `Read`/`Manage` are `Actor::User` only.
- Never change a posting's `accounting_at`, `connection_id`, `fingerprint` or `source_event_id` after insert.
- Edit `migrations/20261026000200_accounting_destinations{,.down}.sql` in place (never deployed).
- Run tests only via `scripts/test.sh -p sanvi-accounting` (never bare `cargo test`/`nextest`).
- Domain stays free of `sqlx`/`actix` (`scripts/check-layering.sh`).
- Commits: Conventional Commits, stage only the paths listed in the task. The working tree holds unrelated changes; ask the user before each commit unless they authorised commits for this plan.

## Review Focus

- Event at exactly `cutover_at` goes to the new binding; one microsecond earlier goes to the old one (Task 1 test `cutover_boundary_is_half_open`).
- Late event at exactly `ends_at + 60 days` is rejected; one microsecond earlier is accepted (Task 1 test `late_grace_boundary_is_exclusive`).
- `CancelSwitch` at exactly `cutover_at` is a conflict, not a silent no-op (Task 1 test `cancel_at_cutover_instant_conflicts`).
- `RecordDelivery` on a never-submitted (`pending`) posting is a conflict (Task 3 test `delivery_requires_submission`).
- Failed → submitted retry increments `attempts` to 2 and clears nothing else silently (Task 3 test `retry_after_failure_counts_attempts`).

---

### Task 1: Periods replace the pending switch; date routing; bounded backfill

**Files:**
- Modify: `crates/contexts/accounting/src/domain/model.rs` (Book, routing, unit tests)
- Modify: `crates/contexts/accounting/src/domain/error.rs` (remove `NotDue`, `UnresolvedExports`, `CutoverDue`)
- Modify: `crates/contexts/accounting/src/application/mod.rs` (delete `CompleteSwitch`; `BookChange` closures take `&mut Book` only)
- Modify: `crates/contexts/accounting/src/application/ports.rs` (`BookChange`)
- Modify: `crates/contexts/accounting/src/adapters/postgres.rs` (`change`: drop unresolved query)
- Test: `crates/contexts/accounting/tests/accounting_integration.rs`

**Interfaces:**
- Produces:
  - `pub const LATE_EVENT_GRACE: TimeDelta` (60 days), `pub const MAX_BACKFILL: TimeDelta` (365 days) in `domain`.
  - `Book::active_connection(&self, now: DateTime<Utc>) -> Option<ConnectionId>` (period covering `now`).
  - `Book::scheduled_switch(&self, now: DateTime<Utc>) -> Option<&DestinationPeriod>` (last period if `starts_at > now`).
  - `Book::draining_connections(&self, now: DateTime<Utc>) -> Vec<ConnectionId>` (closed periods with `now < ends_at + LATE_EVENT_GRACE`).
  - `Book::schedule_switch(&mut self, ConnectionId, cutover_at, now)`, `Book::cancel_switch(&mut self, now)`, `Book::activate_initial(&mut self, ConnectionId, starts_at, now)`, `Book::destination_for(&self, accounting_at, now) -> Result<ConnectionId, AccountingError>` — same signatures as today, new rules.
  - `pub type BookChange = Box<dyn FnOnce(&mut Book) -> Result<AccountingEvent, AccountingError> + Send>;`
  - `PendingSwitch`, `Book::pending_switch`, `Book::complete_switch`, `CompleteSwitch` no longer exist.

- [ ] **Step 1: Replace the gate-based unit tests in `model.rs` with these (use existing `at()`, `book()`, `connection()` helpers; `book()` activates binding 3 at `at()`):**

```rust
#[test]
fn switch_takes_effect_at_cutover_without_completion() {
    let mut book = book();
    let cutoff = at() + TimeDelta::days(1);
    book.schedule_switch(id(4), cutoff, at()).unwrap();
    assert_eq!(book.active_connection(at()), Some(id(3)));
    assert_eq!(book.active_connection(cutoff), Some(id(4)));
    assert_eq!(book.periods()[0].ends_at, Some(cutoff));
    book.validate().unwrap();
}
#[test]
fn cutover_boundary_is_half_open() {
    // schedule at at()+1d, then route at now = cutoff + 1h
    assert_eq!(book.destination_for(cutoff - TimeDelta::microseconds(1), now), Ok(id(3)));
    assert_eq!(book.destination_for(cutoff, now), Ok(id(4)));
}
#[test]
fn late_event_routes_to_closed_period_inside_grace() {
    // event dated before cutoff, now = cutoff + 30 days → id(3); draining_connections(now) == vec![id(3)]
}
#[test]
fn late_grace_boundary_is_exclusive() {
    // now = cutoff + LATE_EVENT_GRACE → HistoricalImport for an event dated before cutoff;
    // now = cutoff + LATE_EVENT_GRACE - 1µs → Ok(id(3)); draining_connections(cutoff + LATE_EVENT_GRACE) is empty
}
#[test]
fn events_before_first_period_or_in_future_are_rejected() {
    // at() - 1s → HistoricalImport; at() + 1s with now = at() → Err(Invalid(_))
}
#[test]
fn only_one_switch_can_be_scheduled() {
    // second schedule_switch(id(5), ...) before first cutover → Err(Conflict(_))
    // after first cutover passes (now >= cutoff), scheduling a fresh binding id(5) succeeds
}
#[test]
fn cancel_before_cutover_restores_open_period() {
    // cancel_switch(at()) → periods().len() == 1, periods()[0].ends_at == None, validate ok
}
#[test]
fn cancel_at_cutover_instant_conflicts() {
    // cancel_switch(cutoff) → Err(Conflict(_)); book unchanged
}
#[test]
fn activation_backfill_is_bounded() {
    // fresh book: activate_initial(id(3), at() - MAX_BACKFILL, at()) → Ok
    // fresh book: activate_initial(id(3), at() - MAX_BACKFILL - 1µs, at()) → Err(Invalid(_))
}
```

Keep `forged_persisted_periods_fail_closed`, `returning_to_a_previous_provider_uses_a_fresh_binding` (rewritten without `complete_switch`: schedule, then pass time), `cannot_reschedule_or_reuse_a_historical_binding` (same), `missing_tax_mappings_and_duplicate_bindings_are_rejected`. Delete tests asserting `NotDue`/`UnresolvedExports`/`CutoverDue`. Add a local `fn id(n: i64) -> ConnectionId`.

- [ ] **Step 2: Run `scripts/test.sh -p sanvi-accounting`** — expected: compile failure (methods/constants missing).

- [ ] **Step 3: Implement in `model.rs`**
  - Remove `PendingSwitch` and the `pending_switch` field; `validate` drops its pending checks and stays time-independent.
  - `schedule_switch`: `Conflict` if no period covers `now`, if `scheduled_switch(now)` is some, or if the binding appears in any period; `Invalid` if `cutover_at <= now`. Set current `ends_at = Some(cutover_at)`, push new open period.
  - `cancel_switch`: `Conflict("switch already took effect")` unless last period `starts_at > now`; pop it and set the new last `ends_at = None`.
  - `activate_initial`: add `Invalid("activation cannot start more than 365 days in the past")` when `starts_at < now - MAX_BACKFILL`.
  - `destination_for`: order exactly as spec §Date-based routing (future → find containing period → grace check).
  - Remove the three gate variants from `error.rs`.

- [ ] **Step 4: Application/adapter** — delete `CompleteSwitch`; update every `Box::new(move |book, _| …)` to `|book|`; in `PostgresAccountingStore::change` delete the `unresolved` query and call `change(&mut book)`.

- [ ] **Step 5: Integration tests** — remove `CompleteSwitch` import. Replace `switch_drains_pending_work_and_preserves_old_assignment_on_retries` with `late_event_after_cutover_routes_to_previous_binding`: assign 4001 at `now`, schedule 3002 at `now+1h`, set clock to `now+2h`; assigning a new event 4002 dated `now+30m` returns connection 3001; event 4003 dated `now+90m` returns 3002; replaying 4001 returns the original posting id and connection 3001; changed fingerprint → `IdempotencyConflict`. Replace `cutover_racing_new_assignment_never_assigns_to_retired_destination` with `concurrent_assignments_at_cutover_route_by_date`: clock at cutoff, join two assigns dated `cutoff - 1µs` and `cutoff`, assert connections 3001 and 3002. Update `active_connection()` calls to `active_connection(harness.clock.now())`.

- [ ] **Step 6: Verify** — `SQLX_OFFLINE=true cargo clippy -p sanvi-accounting --all-targets --all-features -- -D warnings` clean; `scripts/test.sh -p sanvi-accounting` all pass. (The removed unresolved query leaves one stale `.sqlx` file; Task 3 regenerates the cache.)

- [ ] **Step 7: Commit** `refactor(accounting): route postings by date and drop cutover gate` (paths: `crates/contexts/accounting`).

---

### Task 2: User and system actors

**Files:**
- Modify: `crates/contexts/accounting/src/application/ports.rs`
- Modify: `crates/contexts/accounting/src/application/mod.rs`
- Modify: `crates/contexts/accounting/src/adapters/postgres.rs`
- Test: `crates/contexts/accounting/tests/accounting_integration.rs`

**Interfaces:**
- Consumes: Task 1 `BookChange`.
- Produces:
  - `pub enum Actor { User { tenant_id: TenantId, user_id: UserId }, System { tenant_id: TenantId, service: &'static str } }` with `Actor::tenant_id(&self) -> TenantId`. Derive `Debug, Clone, Copy`.
  - `Services::authorize(&self, actor: Actor, permission: Permission) -> Result<(), AccountingError>` (private to `application` is fine): `Export` + `User` → `Forbidden`; `Read|Manage` + `System` → `Forbidden`; `Read|Manage` + `User` → `self.access.require(actor, permission)`; `Export` + `System` → `Ok(())`. Every use case calls this first instead of `access.require`.
  - `AccountingStore` methods take `tenant_id: TenantId` instead of `actor: Actor`: `change(tenant_id, book_id, revision, change)`, `assign(tenant_id, command, now)`, `record_delivery(tenant_id, posting_id, external_record_id)` (replaced in Task 3).

- [ ] **Step 1: Write failing integration test `export_is_system_only_and_management_is_user_only`** — harness gains `user: Actor` (tenant 1001, user 2001) and `system: Actor` (tenant 1001, service `"accounting-test"`); assert `AssignPosting.execute(harness.user, …)` → `Forbidden`, `CreateBook.execute(harness.system, …)` → `Forbidden`, `GetBook.execute(harness.system, …)` → `Forbidden`, and `AssignPosting.execute(harness.system, …)` → `Ok`.

- [ ] **Step 2: Run `scripts/test.sh -p sanvi-accounting`** — expected: compile failure.

- [ ] **Step 3: Implement** the enum, accessor, `authorize`, and store signature change. In `TestAccess`, keep the boolean behaviour.

- [ ] **Step 4: Migrate existing tests** — management/read calls use `harness.user`; `AssignPosting`/`RecordDelivery` use `harness.system`; cross-tenant variants build `Actor::User`/`Actor::System` with tenant 1002; privacy test uses user id 2001 and tenant 1001 literals or harness fields.

- [ ] **Step 5: Verify** — clippy clean; `scripts/test.sh -p sanvi-accounting` all pass.

- [ ] **Step 6: Commit** `feat(accounting): separate user and system actors`.

---

### Task 3: Posting status lifecycle (domain, migration, store, use cases)

**Files:**
- Create: `crates/contexts/accounting/src/domain/posting.rs` (move `Posting`, `PostingId`/`PostingTag` stay in `model.rs` or move here — implementer's choice; re-export from `domain/mod.rs`)
- Modify: `crates/contexts/accounting/src/domain/mod.rs`
- Modify: `migrations/20261026000200_accounting_destinations.sql`, `.down.sql`
- Modify: `crates/contexts/accounting/src/application/{mod.rs,ports.rs}`
- Modify: `crates/contexts/accounting/src/adapters/postgres.rs`
- Modify: `.sqlx/` (regenerated)
- Test: `crates/contexts/accounting/src/domain/posting.rs` (unit), `crates/contexts/accounting/tests/accounting_integration.rs`

**Interfaces:**
- Consumes: Task 2 `Actor`, `Services::authorize`, tenant-scoped store signatures.
- Produces:
  - ```rust
    pub enum PostingStatus {
        Pending,
        Submitted,
        Delivered { external_record_id: String },
        Failed { reason: String },
        ResolvedManually { note: String, external_record_id: Option<String>, resolved_by: UserId },
    }
    ```
    with `PostingStatus::as_str(&self) -> &'static str` (DB values from Global Constraints) and `PostingStatus::operation(&self) -> &'static str` (`posting_submitted` / `posting_delivered` / `posting_failed` / `posting_resolved`; `Pending` → `posting_assigned`).
  - `Posting` gains `status: PostingStatus`, `attempts: i32`, `status_changed_at: DateTime<Utc>`; loses `external_record_id`. `Posting::new_pending(id, tenant_id, book_id, connection_id, source_event_id, fingerprint, accounting_at, now) -> Posting`.
  - Transitions on `Posting`, each returning `Result<bool, AccountingError>` where `false` = idempotent no-op:
    - `submit(&mut self, now)` — from `Pending|Failed` → `Submitted`, `attempts += 1`; from `Submitted` → `Ok(false)`.
    - `deliver(&mut self, external_record_id: String, now)` — from `Submitted` only; already `Delivered` with same id → `Ok(false)`; validate 1–256 bytes.
    - `fail(&mut self, reason: String, now)` — from `Submitted` only; already `Failed` with same reason → `Ok(false)`.
    - `resolve(&mut self, note: String, external_record_id: Option<String>, by: UserId, now)` — from `Pending|Submitted|Failed`; already resolved with identical data → `Ok(false)`.
    - Any other transition → `Conflict("posting status does not allow this transition")`. Text validation → `Invalid(_)`.
  - Store port replaces `record_delivery` with `transition(&self, tenant_id: TenantId, posting_id: PostingId, change: PostingChange) -> Result<Posting, AccountingError>` where `pub type PostingChange = Box<dyn FnOnce(&mut Posting) -> Result<bool, AccountingError> + Send>;`. Locks book then posting (`FOR UPDATE`), applies, and only when `true` updates status columns and appends an outbox event with `operation = posting.status.operation()`.
  - Use cases (all `execute(&self, actor: Actor, posting_id: PostingId, …) -> Result<Posting, AccountingError>`): `MarkSubmitted` (Export), `RecordDelivery(external_record_id: &str)` (Export), `RecordFailure(reason: &str)` (Export), `ResolvePosting(note: &str, external_record_id: Option<&str>)` (Manage; `resolved_by` from `Actor::User`).

- [ ] **Step 1: Unit tests in `posting.rs`:**
  - `pending_submits_and_delivers` — attempts == 1, status Delivered, second `deliver` same id → `Ok(false)`, different id → `Conflict`.
  - `delivery_requires_submission` — `deliver` on Pending → `Conflict`.
  - `retry_after_failure_counts_attempts` — submit, fail("locked period"), submit → attempts == 2, status Submitted.
  - `resubmit_is_idempotent` — submit twice → second returns `Ok(false)`, attempts == 1.
  - `manual_resolution_from_any_open_state` — Pending, Submitted, Failed each resolve; resolve after Delivered → `Conflict`.
  - `terminal_states_reject_further_changes` — after resolve: submit/fail/deliver → `Conflict`.
  - `text_limits` — reason/note of 1025 bytes or `"  "` → `Invalid`; 1024 bytes → Ok.

- [ ] **Step 2: Run `scripts/test.sh -p sanvi-accounting`** — expected: compile failure.

- [ ] **Step 3: Implement `posting.rs`** per Interfaces; `status_changed_at = now` on every `true` transition.

- [ ] **Step 4: Migration** — on `accounting.postings` add the columns in spec §Persistence (`status` default `'pending'`, `attempts` default 0, `failure_reason`, `resolution_note`, `resolved_by`, `status_changed_at timestamptz NOT NULL`) and explicit per-status row CHECKs: `delivered` ⇒ `external_record_id` NOT NULL; `failed` ⇒ `failure_reason` NOT NULL; `resolved_manually` ⇒ `resolution_note` and `resolved_by` NOT NULL; `pending|submitted` ⇒ `external_record_id`, `resolution_note`, `resolved_by` NULL. Length checks 1–1024 on the two text columns; `resolved_by > 0`.
  Replace the write-once trigger with `accounting.postings_terminal_immutable()`: `BEFORE UPDATE` raises `check_violation` when `OLD.status IN ('delivered','resolved_manually')`. Grants: `UPDATE (status, attempts, failure_reason, resolution_note, resolved_by, status_changed_at, external_record_id)`. Replace `postings_unresolved` with `postings_open ON (tenant_id, book_id, status) WHERE status NOT IN ('delivered','resolved_manually')`. Update `.down.sql` to drop the renamed function.

- [ ] **Step 5: Store** — `assign` inserts `status`, `attempts`, `status_changed_at`; `find_posting` and a new `lock_posting` map rows to `PostingStatus` (unknown/inconsistent combination → `Storage`); implement `transition`; delete `record_delivery`.

- [ ] **Step 6: Use cases** — add the four in `application/mod.rs`; each calls `authorize`, validates nothing the domain already validates, and passes `self.0.clock.now()` into the closure.

- [ ] **Step 7: Regenerate SQLx cache**
  ```bash
  eval "$(./scripts/test-db.sh up)"
  rg -l 'accounting\.' .sqlx | xargs rm
  PGPASSWORD=postgres psql -q -h 127.0.0.1 -p "$SANVI_TEST_PG_PORT" -U postgres -c "DROP DATABASE IF EXISTS accounting_prepare" -c "CREATE DATABASE accounting_prepare"
  DATABASE_URL=postgres://postgres:postgres@127.0.0.1:$SANVI_TEST_PG_PORT/accounting_prepare sqlx migrate run --source migrations
  touch crates/contexts/accounting/src/lib.rs
  DATABASE_URL=postgres://postgres:postgres@127.0.0.1:$SANVI_TEST_PG_PORT/accounting_prepare SQLX_OFFLINE_DIR=$PWD/.sqlx cargo check -p sanvi-accounting --all-targets
  PGPASSWORD=postgres psql -q -h 127.0.0.1 -p "$SANVI_TEST_PG_PORT" -U postgres -c "DROP DATABASE accounting_prepare"
  ```
  Expected: `git status .sqlx` shows only accounting query files changed/added; then `SQLX_OFFLINE=true cargo check -p sanvi-accounting --all-targets` passes.

- [ ] **Step 8: Integration tests**
  - Rewrite `duplicate_legal_entity_and_conflicting_delivery_are_rejected`: system `MarkSubmitted` then `RecordDelivery("external-1")` twice (same posting returned), `"external-2"` → `Conflict`; direct SQL `UPDATE accounting.postings SET status = 'pending' WHERE id = $1` under app role → SQLSTATE `23514`.
  - `posting_lifecycle_writes_outbox_events`: assign → submit → fail → submit → deliver; count `platform.outbox` rows for the book with event types `accounting.posting_assigned`, `…_submitted` (2), `…_failed`, `…_delivered`; idempotent repeat adds none.
  - `user_can_resolve_stuck_posting`: system submits; `ResolvePosting.execute(harness.user, id, "entered by hand in freee", Some("FREEE-123"))` → status `ResolvedManually` with `resolved_by` 2001; `ResolvePosting` with `harness.system` → `Forbidden`; `MarkSubmitted` with `harness.user` → `Forbidden`.
  - Update `tenant_rls_and_column_permissions_protect_history` to use `RecordFailure`/`MarkSubmitted` for the cross-tenant `NotFound` check; keep the `connection_id` update → `42501`.
  - `accounting_migration_reverses_without_touching_other_contexts` must still pass unchanged.

- [ ] **Step 9: Verify** — clippy clean; `scripts/test.sh -p sanvi-accounting` all pass; `./scripts/check-migration-expand-contract.sh` reports 0 violations.

- [ ] **Step 10: Commit** `feat(accounting): add posting status lifecycle` (paths: `crates/contexts/accounting`, `migrations/20261026000200_accounting_destinations*.sql`, `.sqlx`).

---

### Task 4: Docs and full gates

**Files:**
- Modify: `crates/contexts/accounting/README.md`
- Modify: `docs/superpowers/plans/2026-10-26-accounting-connections.md`
- Modify: `docs/superpowers/plans/2026-10-26-accounting-verification.md`

- [ ] **Step 1: README** — replace the cutover-pause/completion paragraphs with: date routing, 60-day late grace, 365-day backfill, automatic cutover, posting status table, actor rules, the terminal-state trigger. Remove `CompleteSwitch` from the composition section; note exporters must treat a timeout as `submitted` and query before retrying.

- [ ] **Step 2: Connections plan** — update invariants 5–6 and the "Completion, recovery and privacy" section to match; remove "Wire `CompleteSwitch` …"; keep credential revocation and re-dating as future work (old binding must stay authorised while in `draining_connections`).

- [ ] **Step 3: Gates** — run `just ci-quiet` (or its `scripts/*.sh` equivalents if `just` is missing). Expected: all green except the pre-existing `scripts/test-deployment-migrations.py` classification failure on `20261015000100_privacy_rights_enforcement.sql`; record actual results in the verification doc, including test counts.

- [ ] **Step 4: Commit** `docs(accounting): describe date routing and posting lifecycle`.
