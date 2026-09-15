# Phase 11 — work that needs people, money, or traffic

Phase 11 as originally written assumed a team, external vendors, cloud
infrastructure, and a running beta with real users. None of those exist. This
file is where the items that depend on them live, so that the task graph in
`docs/tasks/phase-11/` contains only work that can actually be executed and
verified today.

Nothing here is cancelled. Each item names what unblocks it. When that
precondition arrives, the item becomes a task with an ID; until then it is not
a dependency of anything, and no acceptance criterion anywhere refers to it.

The rule that keeps this file honest: **an acceptance criterion that cannot be
falsified by running a command is not an acceptance criterion.** Items below
fail that test for reasons of resourcing rather than of engineering.

## Needs a second person

| Item | Was | Unblocked by |
| --- | --- | --- |
| Two-person review of the generated route/permission matrix | TASK-024 | A second engineer. Replaced meanwhile by a committed matrix whose diff must be explicitly acknowledged in the PR, and a CI gate that fails on an unmapped route. |
| Two-person confirmation on tenant purge | TASK-028 | A second operator. Replaced meanwhile by dry-run, a verified export precondition, and a typed confirmation naming the tenant. |
| Runbook walked through by someone who did not write it | TASK-020 | A second engineer. Replaced meanwhile by a runbook that is executed end to end against the docker-compose stack, with the commands in it actually run. |
| Chaos drills as scheduled game days with a rotating runner | TASK-023 | A team. Replaced meanwhile by an automated fault-injection suite: one test per dependency-matrix row, run in CI. |
| Rollback rehearsed by someone who did not write the release | TASK-030 | A second engineer. |
| Onboarding path verified by a new engineer following it unaided | frontend TASK-030 | A new engineer. |

## Needs a vendor or a budget

| Item | Was | Unblocked by |
| --- | --- | --- |
| Third-party penetration test, findings triage, remediation window | TASK-024, NFR-1110 | Budget and a booked engagement. Replaced meanwhile by the self-run isolation sweep (RLS matrix, IDOR sweep, route matrix) and the blocking scanner set. |
| External WCAG 2.2 AA audit | frontend TASK-027 | Budget and a booked engagement. Replaced meanwhile by blocking axe, keyboard-only e2e, a VoiceOver pass on macOS, and published-theme contrast gates. |
| NVDA and JAWS screen-reader passes | frontend TASK-027 | A Windows machine and licences. VoiceOver on macOS is run instead and the gap is named in the accessibility statement. |
| Counsel review of the notice diff | TASK-029, FR-1135 | Legal counsel. The generator that produces the diff is built regardless; only the approval step waits. |
| Real mid-range Android and iOS device testing | frontend TASK-022, TASK-030 | Devices or a device-farm subscription. Pinned Playwright emulation profiles are used instead, and the substitution is recorded next to every number they produce. |

## Needs production infrastructure

The repositories currently target local development and `docker-compose`. There
is no cloud IaC — no Terraform, no cloud account referenced anywhere in the
tree, no CDN, and no managed database. What infrastructure code exists is the
`Dockerfile` and the `docker-compose*.yml` files; the trivy job already scans
those for vulnerabilities and secrets, and their misconfiguration scanning is
the parked item below.

| Item | Was | Unblocked by |
| --- | --- | --- |
| Continuous WAL archiving, daily base backups, encrypted backup store with independently audited access | TASK-025, FR-1119 | A managed Postgres or a provisioned host. Replaced meanwhile by a timed `pg_basebackup` + WAL replay drill against the compose stack, which exercises the same restore procedure at a smaller scale. |
| Multi-AZ posture, RPO ≤ 5 min / RTO ≤ 1 h validated against production | NFR-1107 | Cloud infrastructure. The drill still runs and still records a duration; the number is a local one and is labelled as such. |
| External status probes from outside the perimeter | TASK-025 | A host outside the perimeter. `/health` and `/ready` are built now, because they are the thing a probe would call. |
| Staged canary rollout with a percentage ramp and automatic halt | TASK-030, FR-1137 | An orchestrator that can route a percentage of traffic. Replaced meanwhile by a rehearsed single-host rolling restart and revert under load. |
| Provenance attestation on release artifacts | TASK-024 | A release pipeline that produces signed artifacts. The SBOM is generated today and is published with the tag. |
| Misconfiguration scanning for the tracked container IaC (`Dockerfile`, `docker-compose*.yml`) | TASK-024, NFR-1108 | Triaging what trivy's misconfig rules flag on the existing files. Their vulnerabilities and secrets are scanned already; only the misconfig ruleset is parked. |
| CDN image transforms and edge cache configuration | frontend TASK-022 | A CDN. |

## Needs real traffic

This is the largest deletion, and the one that changes the shape of the phase.
The original plan is built on four weeks of field telemetry from staging and
beta, and every optimisation slice is judged against it. There is no beta, there
are no users, and there is therefore no field data — so every SLO "confirmed
against the baseline", every field p75, and every RUM segmentation acceptance
criterion was unsatisfiable as written.

| Item | Was | Unblocked by |
| --- | --- | --- |
| Four weeks of field telemetry summarised per journey | TASK-019 | Real traffic. Replaced by a load-harness baseline run against `seed-scale` data, which is reproducible and comparable but is a lab number. |
| SLO targets confirmed or renegotiated against observed data | TASK-020, FR-1106 | Real traffic. The six journeys keep **lab latency budgets** measured on a pinned machine profile. They become SLOs when there is traffic to burn a budget. |
| Error budget policy freezing feature merges on burn rate | TASK-020 | Real traffic and a team to freeze. |
| Field Core Web Vitals at p75 over a week on the new build | frontend TASK-022, NFR-1101 | Real traffic. Replaced by pinned-profile Lighthouse and bundle budgets, which regress-guard the same work. |
| RUM segmentation queryable by tenant, locale, device class, theme revision | frontend TASK-020 | Real traffic and a collector endpoint. The client-side collector is still built, so that turning it on later is configuration rather than a project. |
| Backwards-compatibility replay suite built from recorded staging requests | TASK-026, FR-1126 | Real client traffic. Replaced by a replay corpus generated from the contract and e2e suites, which is smaller but is a real compatibility gate. |
| Per-tenant and per-IP abuse thresholds tuned against observed false-positive rates | TASK-024 | Real traffic. Thresholds ship in shadow mode with counting live, so the tuning is a configuration change later rather than a code change. |

## What this costs

Two things get materially weaker, and it is worth saying so plainly rather than
discovering it at GA.

**Security assurance.** Without an external test, the isolation sweep is written
and run by the same person who wrote the code being tested. That is a real
limitation and no amount of automated coverage removes it. The mitigation is
that every finding, wherever it comes from, becomes a regression test — so the
suite only ever grows.

**Performance confidence.** Lab numbers on one machine tell you about
regressions, not about production. `perf-compare` catching a 20 % slowdown
between two release candidates is genuinely useful; a 3× peak load test on a
laptop is not a capacity model. The capacity work is scoped to what one host can
tell you, and labelled accordingly.
