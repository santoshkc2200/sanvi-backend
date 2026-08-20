# Backlog — sanvi-backend

Single source of truth for task status. Generated and updated by the sdlc-planner
script — don't hand-edit rows or hand-number IDs; the script keeps this table and
the task files in sync, and `check` flags it when they drift.

```bash
SDLC="python3 ~/.claude/skills/sdlc-planner/scripts/sdlc.py"

$SDLC check                                     # traceability + status drift (run from this repo root)
$SDLC status TASK-002 in-progress               # updates the task file AND the row below
$SDLC status TASK-002 done --note "PR #123"
$SDLC new-task 09 "<title>" --requirement FR-903 --depends TASK-002
```

Each task traces to a numbered requirement in [`../requirements.md`](../requirements.md); the *why*
behind each one lives in [`../phase-09-tenant-payments/implementation-plan.md`](../phase-09-tenant-payments/implementation-plan.md)
and the slice order in [`phase-09/README.md`](phase-09/README.md). Every task must clear
[`definition-of-done.md`](definition-of-done.md).

Statuses: `todo` · `in-progress` · `blocked` · `done`

| ID | Phase | Title | Status | Depends on | Notes |
|---|---|---|---|---|---|
| TASK-001 | 09 | 09.0 Contract, schema & foundations | done | phase 03, 04, 08 | shipped: payments crate, migration 20260823000000, catalog route, connect-invariants gate |
| TASK-002 | 09 | 09.1 Provider-agnostic domain, ports & catalog | todo | TASK-001 | |
| TASK-003 | 09 | 09.2 Stripe Connect onboarding (Accounts v2) | todo | TASK-002 | |
| TASK-004 | 09 | 09.3 Connection lifecycle & Connect webhooks | todo | TASK-003 | |
| TASK-005 | 09 | 09.4 Checkout on the connected account | todo | TASK-004 | |
| TASK-006 | 09 | 09.5 Payments, refunds & disputes | todo | TASK-005 | |
| TASK-007 | 09 | 09.6 Payouts, tax liability & application fee | todo | TASK-004, TASK-005 | |
| TASK-008 | 09 | 09.7 Disconnect, hardening & release | todo | TASK-006, TASK-007 | |
