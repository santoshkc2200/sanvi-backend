# Definition of Done — per task

Applies to **every** task in `tasks/`, from phase 09 onward. Inherited from the roadmap's
phase-level DoD and tightened to the slice: a task is a thin vertical cut through the stack that can
be merged, flagged off, and exercised against the local stack on its own, so it carries the full DoD
by itself rather than deferring it to the end of the phase.

**Every line below is falsifiable by running a command on this machine.** There is no deployment
target, no image registry, and no remote to push to, so the lines that used to require one have moved
to [`../release/needs-humans.md`](../release/needs-humans.md) with the local check that replaces each.
A line that cannot be falsified by a command does not belong here — see the phase 11 README for the
rule this follows.

- [ ] Tests green — unit, integration, and the task's own end-to-end path.
- [ ] `api/{context}.yaml` updated; handler/spec divergence fails CI.
- [ ] Migration is reversible — applied, rolled back, and re-applied against the compose database in
      one run (`just migrate`, `just migrate-down`, `just migrate`), with the down path asserted to
      leave no orphaned object.
- [ ] Logs, metrics and traces wired for the new paths.
- [ ] Threat model reviewed for anything touching money, keys, or PII.
- [ ] Docs updated — this task file, plus the phase implementation plan if a decision changed.
- [ ] Feature flag defined and defaulted **off**; rollback plan written.
- [ ] The flagged path exercised against the `docker-compose` stack in **both** flag positions, and
      the commands that did so recorded in the task file. **Merged ≠ exercised.**

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

Status is recorded by hand in two places **in the same commit** — the `**Status:**` line at the top
of the task file, and that task's row in [`backlog.md`](backlog.md), with the PR or commit as the
note. Updating one without the other is the failure mode this rule exists to prevent.

## Contract first

Backend and frontend run concurrently against the OpenAPI contract, which is written and merged
**before** either track starts. Every task's first checklist item is therefore its **Contract**
item; that item is the handshake with the other repo, and it lands ahead of the implementation on
both sides.

Where a task is blocked on work in the other repository, it says so in a `**Blocked by (cross-repo):**`
line in its header. Those lines are the only cross-track coordination there is — there is no shared
schedule document to consult.

## Release convention

Phases 09–10: each task tags a prerelease off the phase branch (`vX.Y.0-alpha.N`, recorded in the
task's `**Prerelease:**` header line) so a drill can name the exact build it ran against. The phase
version `vX.Y.0` is tagged only by the phase's final task, when every task in the phase is `done` and
the feature flags are on by default.

Phase 11 overrides this, and its README is authoritative: GA-half tasks tag a release candidate off
the phase branch; **gate-half tasks tag nothing** — they change CI and platform crates, not product
behaviour, so they carry no `**Prerelease:**` line and none should be invented for them.
