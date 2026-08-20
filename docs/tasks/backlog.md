# Backlog — sanvi-backend

Single source of truth for task status. Generated and updated by the sdlc-planner
script — don't hand-edit rows or hand-number IDs; the script keeps this table and
the task files in sync, and `check` flags it when they drift.

```bash
SDLC="python3 ~/.claude/skills/sdlc-planner/scripts/sdlc.py"

$SDLC check                                     # traceability + status drift (run from this repo root)
$SDLC status TASK-002 in-progress               # updates the task file AND the row below
$SDLC status TASK-002 done --note "PR #123"
$SDLC new-task 10 "<title>" --requirement FR-1003 --depends TASK-010
```

Each task traces to a numbered requirement in [`../requirements.md`](../requirements.md); the *why*
behind each one lives in that phase's implementation plan
([09](../phase-09-tenant-payments/implementation-plan.md) ·
[10](../phase-10-advertising/implementation-plan.md)) and the slice order in its
`phase-NN/README.md` ([09](phase-09/README.md) · [10](phase-10/README.md)). Every task must clear
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
| TASK-009 | 10 | 10.0 Contract, schema & foundations | todo | phase 03, 05, 08, 09 | |
| TASK-010 | 10 | 10.1 Domain model, capability matrix & fake adapter | todo | TASK-009 | |
| TASK-011 | 10 | 10.2 Connections, OAuth & token lifecycle | todo | TASK-010 | |
| TASK-012 | 10 | 10.3 Campaign tree, Google Ads adapter & drift | todo | TASK-011 | |
| TASK-013 | 10 | 10.4 Meta adapter & creatives | todo | TASK-012 | |
| TASK-014 | 10 | 10.5 Conversion capture & the directive gate | todo | TASK-011 | |
| TASK-015 | 10 | 10.6 Conversion upload, dedupe & diagnostics | todo | TASK-014 | |
| TASK-016 | 10 | 10.7 Metrics ingestion, rollups & ROAS query layer | todo | TASK-012, TASK-015 | |
| TASK-017 | 10 | 10.8 Budget guardrails & spend alerts | todo | TASK-016 | |
| TASK-018 | 10 | 10.9 Privacy integration, hardening & release | todo | TASK-013, TASK-015, TASK-017 | |
