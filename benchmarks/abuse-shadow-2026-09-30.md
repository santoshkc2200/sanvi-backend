# Abuse-quota shadow measurement at 3× peak (TASK-024, slice 11.5e)

**Lab, not production** — same honesty rules as `baseline-2026-09-17.md`.
Every number below is a 3× -rate k6 run against a local API in the debug
build on one laptop, against the corrected seed-1 scale data. It is the
"load test at 3× the harness peak records the would-have-blocked rate"
acceptance of FR-1117, committed here as the slice's evidence.

- **Date:** 2026-09-30 · **seed:** 1 (post-erratum: 147 tenants, 5748 payments)
- **Build stamp:** `{"version":"0.10.0","commit":"unknown","built_at":"unknown","environment":"local"}`
- **Machine profile:** `{"cores":10,"mem_gb":16,"os":"Darwin 27.0.0","arch":"arm64"}`
- **Posture (run A, shipping defaults):** `SANVI__DATABASE__NAME=sanvi_scale`,
  per-IP and per-tenant token-bucket limiters **off** (the lab baseline
  posture — otherwise the 10 r/s shared bucket saturates first and measures
  nothing about the abuse quota), `features.advertising` on with dev-only
  signing secrets + token KEK, `server.abuse` untouched (tracking enforced,
  everything else shadow). API on :8081, compose Redis.
- **Posture (run B):** identical, plus
  `SANVI__SERVER__ABUSE__TRACKING__ENFORCE=false` — the pure-shadow flip.
- **Load:** the five TASK-019 shapes at 3× their offered rates
  (storefront_read 150 rps, admin_write 60, checkout_create 30,
  webhook_flood 300, conversion_upload 150), 60 s each. Artifacts:
  `benchmarks/<scenario>/abuse-3x.json` and
  `benchmarks/conversion_upload/abuse-3x-shadow.json`.

## The flood: `POST /api/v1/public/track` at 150 rps for 60 s (9,000 beacons, one IP, one Origin)

Quotas: per-IP 120/min, per-Origin 600/min (fixed windows, keyed in Redis).

| Series (`abuse_quota_events_total`, surface=tracking) | Run A (enforced) | Run B (shadow) |
| --- | --- | --- |
| ip / counted | 240 (= 120 × 2 windows) | 120 (window phase: run started inside a still-hot window) |
| ip / blocked (A) · would_block (B) | 8,760 → **97.3 %** | 8,880 → **98.7 %** |
| origin / counted | 1,200 (= 600 × 2 windows) | 600 |
| origin / blocked (A) · would_block (B) | 7,800 → 86.7 % | 8,400 → 93.3 % |

Both dimensions count every request, so a request over quota on both keys
appears twice; the request-level rate is governed by whichever key
exhausts first (always per-IP here). The A/B difference is window phase,
not behaviour: a 60 s run against a 60 s window lands on 1 or 2 windows
depending on when it starts — the documented fixed-window seam (a burst at
a boundary admits up to 2× `limit`).

**The flip is invisible to the caller.** Both runs completed 9,000/9,000
iterations with 0 dropped and 0 failed requests; p95 latency was
2.846 ms (enforced) vs 2.845 ms (shadow) — statistically identical, and
every response in both postures was the beacon's uniform empty `202`.

## The other FR-1117 surfaces under the same run

| Surface | counted | would_block / blocked |
| --- | --- | --- |
| invitation (create/resend) | 0 | 0 |
| domain_purchase | 0 | 0 |
| signup (internal Kratos registration webhook) | 0 | 0 |
| notifications (gateway sends) | 0 | 0 |

All zero, and the reason matters more than the number: **the lab cannot
drive the authenticated shadow surfaces.** Invitations and domain orders
sit behind Kratos sessions the local harness does not have — an
unauthenticated flood 401s at the authn middleware, *outside* the abuse
gate (which is deliberately post-authentication: an unauthenticated flood
is the shared per-IP limiter's job, not a per-tenant quota's). Counting is
live on every surface — the zeros are absence of traffic, not absence of
controls; the driver gap is the same lab honesty caveat the admin-write
shape carries. Notification sends are zero because the local run has no
notification service configured (`base_url` empty ⇒ gateway absent). The
tuning that turns these zeros into rates needs real traffic
(`needs-humans.md`).

## What the 3× stress run says beyond the abuse surfaces

storefront_read at 150 rps dropped 284 iterations (p95 1.25 s) and
admin_write at 60 rps dropped 94 (p95 2.4 s): the debug build on this
laptop saturates on the hot read path at 3×. That is a TASK-021 profiling
input, not an abuse finding — neither path is abuse-classified, and the
abuse-relevant shape (conversion_upload) held its ceiling with zero drops.
checkout_create (30 rps) and webhook_flood (300 rps) completed clean.

## Reproduce

```bash
./scripts/dev.sh up && ./scripts/migrate.sh   # postgres :5433, redis :6380 per .env
just seed-scale -- --seed 1                   # into SANVI__DATABASE__NAME=sanvi_scale
# API posture: the env block from "Posture (run A)" above, port 8081
for s in storefront_read:150 admin_write:60 checkout_create:30 webhook_flood:300 conversion_upload:150; do
  PERF_RATE="${s##*:}" just perf abuse-3x "${s%%:*}"
done
curl -s localhost:8081/metrics | grep '^abuse_quota'
```
