# Accounting: date-based routing, posting status, bounded backfill, system actors

Status: design, pending approval. Scope: `crates/contexts/accounting` and its
unshipped migration `20261026000200_accounting_destinations.sql`.

## Why

The first accounting milestone (see `docs/superpowers/plans/2026-10-26-accounting-connections.md`)
routes every new event to the *current* destination and pauses all routing once
a switch is due until every unresolved posting is reconciled. Review found four
problems that would hurt as soon as ingestion and export exist:

1. Late events (offline POS sync, events in flight at cutover) dated before the
   current period are rejected as `HistoricalImport` with no recovery path.
2. A due switch can block all routing for a legal entity forever: one lost
   provider response leaves a posting unresolved, completion is impossible, and
   cancellation is forbidden once due.
3. Initial activation may be backdated without limit, which bypasses the
   "historical imports need a reviewed migration" rule.
4. `Actor` requires a `UserId`, so ingestion/delivery workers would borrow a
   fake user, and any tenant user with `Export` could record delivery IDs.

Comparable systems (A2X, Amaka/Square→Xero, Codat, Stripe, Xero lock dates)
converge on: history stays in the provider it was posted to; sync start dates
with a bounded backfill; per-item push status with retry of failed items and a
human resolution path; never silently re-dating an event; integrations acting
as the authorised connection, not a user.

## Goals

- Route each event by its accounting date to the period that contains it, even
  after that period has ended, within a fixed late-event grace window.
- Make a switch take effect automatically at `cutover_at`; nothing blocks it.
- Give every posting an explicit status with a human resolution path so no
  state can wedge the book.
- Bound initial activation backdating.
- Separate user actors from system actors; delivery bookkeeping is system-only.

## Non-goals

- Exporter, ingestion adapter, provider API calls, OAuth, credential
  storage/revocation, HTTP routes, OpenAPI, admin UI.
- Automatic re-dating of rejected events or linking refunds to original
  invoices. Re-dating is a future ingestion concern (a new source event with a
  new ID and date); this milestone only records a manual resolution.
- Historical import tooling beyond rejecting out-of-window events.

## Anti-goals (failure even if it compiles)

- Any path that changes an event's `accounting_at` or destination after
  assignment.
- Any state in which a book can no longer accept in-window events without a
  database edit.
- A tenant user being able to mark a posting delivered or submitted.
- Re-sending or "succeeding" a posting because a request timed out.

## Design

### Periods replace the pending switch

- Remove `PendingSwitch` and `Book::pending_switch`. `schedule_switch` closes
  the current period at `cutover_at` and appends the new period starting at
  `cutover_at` immediately. A period whose `starts_at > now` is "scheduled".
- Structural invariants (checked in `Book::validate`, which stays
  time-independent): periods are contiguous with strictly increasing starts as
  today, the last period is open (`ends_at = None`), and no binding appears in
  two periods.
- `schedule_switch(connection, cutover_at, now)`: requires an active period
  covering `now` and no scheduled period (the last period's `starts_at <= now`),
  so at most one switch is ever scheduled; a never-used binding; and
  `cutover_at > now`.
- `cancel_switch(now)`: allowed only while the last period is scheduled
  (`starts_at > now`); pops it and reopens the previous period. After
  `cutover_at` it returns `Conflict("switch already took effect")`; the way
  back is scheduling a fresh binding.
- `active_connection(now)`: the period covering `now`.
- Remove `complete_switch`, the `CompleteSwitch` use case, the `unresolved`
  argument of `BookChange`, and the unresolved-postings query in
  `PostgresAccountingStore::change`. Remove errors that only served the gate
  (`NotDue`, `UnresolvedExports`, `CutoverDue`).

### Date-based routing

`destination_for(accounting_at, now)`:

1. `accounting_at > now` → `Invalid("posting timestamp cannot be in the future")`.
2. Find the period with `starts_at <= accounting_at < ends_at` (or open). None
   (before the first period) → `HistoricalImport`.
3. If that period is closed and `now >= ends_at + LATE_EVENT_GRACE` →
   `HistoricalImport`.
4. Otherwise return its connection.

`LATE_EVENT_GRACE` is a domain constant of **60 days** (my call: covers a
monthly close plus offline-sync lag; not configuration).

Dedup is unchanged: `(tenant_id, book_id, source_event_id)` returns the
original posting regardless of later switches; changed fingerprint/timestamp
conflicts.

`Book::draining_connections(now)` returns bindings of closed periods still
inside the grace window. It is informational for the future credential and UI
work (an old provider must stay authorised while draining); nothing enforces it
in this milestone.

### Bounded backfill

`activate_initial(connection, starts_at, now)` additionally requires
`starts_at >= now - MAX_BACKFILL`, with `MAX_BACKFILL` a domain constant of
**365 days** (matches Amaka's one-year history import). Older data requires
the separate reviewed-import path.

### Posting status

Postings gain a status lifecycle. Terminal states are immutable.

| From | To | Use case | Actor |
| --- | --- | --- | --- |
| (new) | `pending` | `AssignPosting` | system |
| `pending`, `failed` | `submitted` | `MarkSubmitted` | system |
| `submitted` | `delivered` | `RecordDelivery(external_record_id)` | system |
| `submitted` | `failed` | `RecordFailure(reason)` | system |
| `pending`, `submitted`, `failed` | `resolved_manually` | `ResolvePosting(note, external_record_id?)` | user, `Manage` |

- `submitted` means an attempt may have reached the provider. A timeout leaves
  it `submitted`; the future exporter must query the provider before retrying.
  `failed` means the provider definitely did not create the record.
- `submitted → submitted` (re-marking) is idempotent; other repeated
  transitions to the same terminal state with the same data are idempotent;
  anything else is `Conflict`.
- `delivered` keeps today's write-once `external_record_id`.
- `resolved_manually` covers "entered by hand in the provider" (optional
  external reference) and "superseded/abandoned" (note only). The note is
  1–1024 bytes; `resolved_by` records the user.
- `failed` stores a reason of 1–1024 bytes (provider message or code, e.g. a
  Xero lock-date rejection).
- `attempts` counts transitions into `submitted`.
- Every transition appends an outbox event:
  `accounting.posting_submitted|posting_delivered|posting_failed|posting_resolved`.
- Postings never block switches. Draining visibility comes from status counts.

Domain owns the state machine (`PostingStatus` enum + transition methods on
`Posting`); the store persists the result under the existing book-then-posting
lock order.

### Actors

```rust
pub enum Actor {
    User { tenant_id: TenantId, user_id: UserId },
    System { tenant_id: TenantId, service: &'static str },
}
```

- `Actor::tenant_id()` accessor; storage and RLS keep using it.
- `Read` and `Manage` require `Actor::User` and go through `AccountingAccess`.
- `Export` (assign, mark submitted, record delivery/failure) requires
  `Actor::System`; a user actor gets `Forbidden` before any access check.
  System actors are constructed only by composition code for trusted workers;
  `AccountingAccess` is not consulted for them.
- `ResolvePosting` is `Manage` (user) and records `resolved_by`.

### Persistence

The migration is untracked and has never been deployed, so it is edited in
place (local databases that ran it must be recreated).

`accounting.postings` gains:

- `status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','submitted','delivered','failed','resolved_manually'))`
- `attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0)`
- `failure_reason text` (1–1024 bytes when present)
- `resolution_note text` (1–1024 bytes when present)
- `resolved_by bigint` (positive when present)
- `status_changed_at timestamptz NOT NULL`
- A row CHECK tying fields to status (e.g. `delivered` requires
  `external_record_id`; `resolved_manually` requires `resolution_note` and
  `resolved_by`; `failed` requires `failure_reason`).

The write-once trigger becomes a terminal-state trigger: once `status` is
`delivered` or `resolved_manually`, no column may change. The runtime role gets
`UPDATE` only on the status columns above plus `external_record_id`. Replace
the `postings_unresolved` partial index with one on
`(tenant_id, book_id, status)` where status is non-terminal.

Book documents drop `pending_switch`. No production data exists, so no
document migration is needed.

`status_changed_at` is set on insert and on every transition from the use
case's clock.

### Tests

Unit (domain): routing into a closed period inside grace; rejection after
grace and before the first period; switch takes effect at `cutover_at` with no
completion step; cancel before vs after cutover; one scheduled period max;
backfill boundary at exactly 365 days; every allowed and forbidden status
transition.

Integration (PostgreSQL): late event after cutover goes to the old binding;
dedup across a switch; full status lifecycle with outbox events; terminal
immutability enforced by the trigger under the app role; user actor rejected
for `Export`; system actor rejected for `Manage`; concurrent assign during
cutover routes by date; migration down/up.

Remove tests that assert the old gate (`CutoverDue`, `UnresolvedExports`,
completion re-verification).

### Docs

Update the crate `README.md` and the connections plan to describe date
routing, grace, backfill, status lifecycle and actors, and drop the
completion/cutover-gate text.

## Left to the builder

- Exact Rust names of the new use cases and helpers, error message text, and
  whether transition methods live on `Posting` or a small `PostingStatus` impl.
- Module split if `model.rs` grows too large.
- How the integration harness constructs system vs user actors.

## Calls I made (check these)

- `LATE_EVENT_GRACE` = 60 days, `MAX_BACKFILL` = 365 days, both constants.
- New binding is not re-verified at `cutover_at`; revoked access surfaces as
  `failed` postings instead of blocking.
- `resolved_manually` is reachable from `pending`/`submitted`/`failed`, so a
  human can always unwedge a posting.
- Edit the unshipped migration in place instead of adding a follow-up.
