# Runbook — rolling restart (and revert) under load

**Owner:** platform engineering · **Source task:** TASK-023 (FR-1112) · **Drill:**
`scripts/rolling-restart.sh`

Every staging deploy runs `docker compose up -d` (recreate in place,
single replica) — this drill proves the recreate drops nothing: k6 load
runs across a roll to the new image and a revert to the previous one,
and zero non-2xx may be attributable to either pass. In-flight requests
drain through SIGTERM grace (`stop_grace_period` exceeds the server's 30 s
`shutdown_timeout_secs`); the api healthcheck gates the replacement; the
worker finishes its batch and unacked stream entries redeliver via
XAUTOCLAIM (handlers are idempotent), so a worker roll loses no work.

## Preconditions

- On the staging host (`STAGING_HOST`), in `/opt/sanvi`, with `API_IMAGE`
  and `WORKER_IMAGE` pinned to sha tags (the pipeline sets both; the
  compose `:latest` fallback is never used for a drill).
- k6 installed; scale data loaded (`sanvi-cli seed-scale`).
- `./scripts/rolling-restart.sh --dry-run` green (compose
  restart-readiness: grace periods + healthcheck).

## The drill

### 1. Roll forward

```bash
API_IMAGE=<new> WORKER_IMAGE=<new> ./scripts/rolling-restart.sh \
    --tag sha-<commit> --base-url http://127.0.0.1:8080
```

The script starts the 5-minute `storefront_read` shape, rolls the worker
then the api with health-gated waits (`up -d --no-deps --wait`), and
fails the run on any non-2xx in the shape artifact
(`benchmarks/storefront_read/<tag>.json`).

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

The worker rolls first because its contract is weaker in the useful
direction: it may replay a bounded tail (at-least-once redelivery) but
never loses one. The api rolls second because its contract is stronger:
in-flight requests must complete, not replay. If the api roll ever
produces non-2xx while the worker roll stays clean, the gap is in
serving (drain/health gating), not in background work.
