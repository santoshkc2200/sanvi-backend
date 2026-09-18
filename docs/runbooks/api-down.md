# Runbook: API is down

**Alerts:** `ApiErrorRatioHigh` — `sum(rate(http_server_errors_total[10m])) / sum(rate(http_server_requests_total[10m])) > 0.05` (see the [alerts registry](../../observability/alerts/rules.json)).

## Symptoms

- `/healthz` or `/readyz` failing or timing out.
- Error-rate burn alert firing (`http.server.errors`).
- `503 Service Unavailable` from the load balancer.

## 1. Triage (30 seconds)

```bash
# Is the process alive?
curl -v --max-time 5 http://<host>:8080/healthz

# Which dependency is degraded?
curl -s http://<host>:8080/readyz   # 503 body lists failing checks
```

- `healthz` 200 but `readyz` 503 → a dependency is down, not the API.
- `healthz` failing → the API itself is down or unreachable; go to 2.
- 429 → rate limiting is (correctly) rejecting traffic; check for abuse.

## 2. The API itself

1. Check the process: `docker ps` (or `systemctl status sanvi-api`).
2. Check logs for panics/OOM:
   `docker logs sanvi-api --tail 200` — look for `request handler panicked`.
3. Check saturation in Prometheus: request rate vs `http.server.duration`,
   pool wait metrics, container memory.

## 3. Dependencies

- **Postgres down/refusing**: check `pg_isready`, connection limits
  (`max_connections`), long transactions holding locks. The pool sizes are in
  `configs/production.toml`.
- **Redis down**: rate limiter fails open (or closed in production!) and
  readiness reports `redis: degraded`. Restart Redis; check memory
  (`maxmemory` evictions).

## 4. Recovery / rollback

- If the last deploy caused it: `git log` the deployed SHA, redeploy the
  previous tag — the deploy job is `docker compose pull && up`; rollback is
  the same with the previous image tag. Runbook-only procedure:
  ```bash
  cd /opt/sanvi
  docker compose -f docker-compose.staging.yml pull api worker
  docker compose -f docker-compose.staging.yml up -d --no-deps api worker
  ```
- Schema migrations run pre-deploy; if a migration caused it, revert with
  `sanvi-cli migrate down --target <previous>` against the database before
  redeploying the matching tag.

## 5. Post-incident

- Capture the relevant request ids from logs; find the traces in OpenObserve.
- Update this runbook if a step was missing; refresh the threat model if the
  cause was security-relevant.
