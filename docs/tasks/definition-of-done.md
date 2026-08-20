# Definition of Done — per task

Applies to **every** task in `tasks/`, from phase 09 onward. Inherited from the roadmap's
phase-level DoD and tightened to the slice: a task is a thin vertical cut through the stack that can
be merged, flagged off, and deployed to staging on its own, so it carries the full DoD by itself
rather than deferring it to the end of the phase.

- [ ] Tests green — unit, integration, and the task's own end-to-end path.
- [ ] `api/{context}.yaml` updated; handler/spec divergence fails CI.
- [ ] Migration is reversible and applied in staging.
- [ ] Logs, metrics and traces wired for the new paths.
- [ ] Threat model reviewed for anything touching money, keys, or PII.
- [ ] Docs updated — this task file, plus the phase implementation plan if a decision changed.
- [ ] Feature flag defined and defaulted **off**; rollback plan written.
- [ ] Deployed to staging behind the flag. **Merged ≠ done.**

### Phase 11 additions — hardening tasks

A hardening task ships a number, a report, or a rehearsed drill rather than a feature, so from phase 11
onward these three lines are added to the list above:

- [ ] **Evidence artifact committed** — a report, a benchmark JSON, a drill log, or an audit result with
      a date and a named runner. **A task with no artifact did not happen.**
- [ ] Numbers compared against the stored baseline; a regression is explained or reverted.
- [ ] Runbook written or updated for every alert the task adds — including alerts on the hardening
      machinery itself.

The flag line is read as "*if* the task ships behaviour": several phase 11 tasks ship tooling, evidence,
or documentation and have no flag. Those say so in their header rather than inventing one.

Status is recorded only through the sdlc-planner script, never by hand-editing the task file and
[`backlog.md`](backlog.md) separately:

```bash
python3 ~/.claude/skills/sdlc-planner/scripts/sdlc.py status TASK-00N done --note "<PR or commit>"
```

## Contract first

Backend and frontend run concurrently against the OpenAPI contract, which is written and merged
**before** either track starts. Every task's first checklist item is therefore its **Contract**
item; that item is the handshake with the other repo, and it lands ahead of the implementation on
both sides.

Where a task is blocked on work in the other repository, it says so in a `**Blocked by (cross-repo):**`
line in its header. Those lines are the only cross-track coordination there is — there is no shared
schedule document to consult.

## Release convention

Each task tags a prerelease off the phase branch (`vX.Y.0-alpha.N`, recorded in the task's
`**Prerelease:**` header line) so staging always has a nameable artifact. The phase version
`vX.Y.0` is tagged only by the phase's final task, when every task in the phase is `done` and the
feature flags are on by default.
