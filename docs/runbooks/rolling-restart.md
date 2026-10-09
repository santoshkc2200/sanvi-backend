# Runbook — rolling restart (and revert) under load

**Owner:** platform engineering · **Source task:** TASK-023 (FR-1112) · **Drill:**
`scripts/rolling-restart.sh`

Every staging deploy and revert runs `bash scripts/deploy-staging.sh`.
The stable `api_proxy` listener switches between `api` and `api_next`, each
with a distinct Snowflake node ID. The controller starts and health-checks
the inactive slot before reloading nginx, then waits for the old proxy
worker PIDs to exit. Both API slots stay alive throughout the switch, so
new connections have a destination while earlier requests finish. A slow
drain blocks the next deployment; it never forces the serving slot down.
The worker may replay unacked stream entries (handlers are idempotent).

The first installation, including conversion of the old API's direct
8080 binding to the stable proxy, requires a maintenance window. Bootstrap
with pinned images using `bash scripts/deploy-staging.sh`, then run the
live drills. Plain `docker compose up` is not a rolling-deploy procedure.
Do not delete `.deploy/proxy/active.conf`: it records the serving slot and
survives source updates. An interrupted switch leaves `.deploy/switch.pending`;
the next invocation reloads and drains that configuration before reusing
either slot. Preserve the marker until the controller completes recovery.
The host requires Bash, util-linux `flock`,
Docker Compose, and (for the drill) Python 3 and k6.

## Preconditions

- On the staging host (`STAGING_HOST`), in `/opt/sanvi`, with `API_IMAGE`
  and `WORKER_IMAGE` pinned to sha tags (the pipeline sets both; the
  compose `:latest` fallback is never used for a drill).
- k6 installed; scale data loaded (`sanvi-cli seed-scale`), with themes published.
- The two-slot stack already bootstrapped, with a healthy proxy. API and
  worker images must be pinned tags or digests.
- `PERF_HOST_DOMAIN` matches the configured tenant domain (default
  `platform.sanvi.app` for the production overlay). `PERF_RATE` defaults
  to 5 iterations/s (10 requests/s), below the 20/s production IP bucket;
  raising it requires a matching rate-limit budget for the drill source.
- `./scripts/rolling-restart.sh --dry-run` green (compose
  restart-readiness: grace periods + healthcheck).

## The drill

### 1. Roll forward

```bash
API_IMAGE=<new> WORKER_IMAGE=<new> ./scripts/rolling-restart.sh \
    --tag sha-<commit> --base-url http://127.0.0.1:8080
```

The script starts the 5-minute `rolling_restart` shape and waits for its
successful setup probe before invoking the deploy controller. The shape
requires every storefront request to return 200 and rejects redirects.
`scripts/check-restart-result.py` reads the artifact's nested `metrics`
and fails on any failure rate, dropped iteration, empty run, or missing
measurement (`benchmarks/rolling_restart/<tag>.json`). A nonzero k6 exit
also fails the drill. CI tests the controller's forward/revert sequence,
failed candidate/configuration, drain guard, and artifact failure cases.

### 2. Revert

Repeat step 1 with the previous image tag. The revert is the same
procedure as the deploy — that symmetry is what makes reverting safe,
not a separate runbook nobody has rehearsed.

### 3. Record

Commit the two artifacts and note the tags and counts in
`docs/release/ga-checklist.md` (the rolling-restart line). A non-zero
count blocks the release: triage with `docs/runbooks/api-down.md`
(serving) and `docs/runbooks/capacity.md` (saturation vs. deploy gap).

## Why this order

The candidate API and worker are started before traffic switches. The
current API is never recreated by that invocation. nginx reload uses
graceful worker retirement; a successful signal alone is insufficient,
so the controller waits for all old worker PIDs to disappear and a new
worker to be present. The previous API is retained until its slot is
needed by a later deploy. An unsuccessful candidate or configuration
validation leaves the previous listener serving traffic.
