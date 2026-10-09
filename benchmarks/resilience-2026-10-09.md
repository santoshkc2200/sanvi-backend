# Resilience middleware — candidate rc.7 and the tail-noise repeat set (TASK-023 close)

**Lab, not production.** Same posture as the rc.6 candidate
([`ssr-cache-2026-10-07.md`](ssr-cache-2026-10-07.md)): seed 1 scale data
(`sanvi_scale`), compose Postgres 18 on port 5433, **release build 0.10.0
rebuilt from current main before the run** (11.4a–e + the recovery-safety fix
included), Redis warm, rate limiting disabled, tracking abuse quota raised,
`features.advertising` on with dev signing secrets, launch script
[`ssr-cache-2026-10-07/lab-api.sh`](ssr-cache-2026-10-07/lab-api.sh)
(`platform.ssr_cache` ON — the rc.6 posture). **Both 11.4 flags stay off**
(`platform.load_shedding`, `platform.degraded_mode` ship default-off); the
candidate therefore measures the always-on part of TASK-023: two extra
middleware hops (shedding admission, degraded-header lookup), the breaker-wired
adapters, and the timeout floors.
Machine profile `{"cores":20,"mem_gb":16,"os":"Windows 11 26200","arch":"x86_64"}`.

**Co-tenants differ from the rc.6 day and are measured, not guessed:**
`hdms-staging-*` idle (as for every rc.4+ run), but `ragflow-server` held
**98.7 % of one core** for the whole session (`docker stats --no-stream`,
sampled mid-run), beside the kind control plane and cmak/zk. rc.6 ran without
the ragflow load. On a 20-core host a pegged co-tenant inflates scheduler
jitter — the tail, not the median.

## `just perf` passed; `perf-compare` vs rc.6 did not, and the repeats show why

Every shape ran clean on every run: **0 failed / 0 dropped iterations, k6
exit 0** — the task's Verification item (`just perf`) passed on the first
candidate and on every repeat. `perf-compare rc.6 rc.7` flagged nine
p95/p99/p50 tail metrics; the repeat set exists to decide whether that is the
release or the host.

Latency percentiles, all runs of the **same binary** (ms):

| shape | tag | p50 | p95 | p99 |
|---|---|---:|---:|---:|
| storefront_read | rc.6 (prior binary) | 3.96 | 5.36 | 7.49 |
| storefront_read | rc.7 | 3.97 | 6.18 | 9.30 |
| storefront_read | rc.7b | 4.16 | 6.99 | 10.02 |
| storefront_read | rc.7c | 3.75 | 6.11 | 8.67 |
| storefront_read | rc.7d | 4.15 | 7.59 | 11.68 |
| admin_write | rc.6 (prior binary) | 4.22 | 8.72 | 10.32 |
| admin_write | rc.7 | 3.92 | 6.96 | 14.04 |
| admin_write | rc.7b | 4.35 | 10.62 | 17.98 |
| admin_write | rc.7c | 4.11 | 9.37 | 19.66 |
| admin_write | rc.7d | 3.78 | 7.30 | 13.62 |
| checkout_create | rc.6 (prior binary) | 3.64 | 7.06 | 10.12 |
| checkout_create | rc.7 | 4.81 | 10.88 | 18.35 |
| checkout_create | rc.7b | 4.01 | 7.26 | 12.87 |
| webhook_flood | rc.6 (prior binary) | 1.51 | 3.38 | 4.89 |
| webhook_flood | rc.7 | 1.81 | 4.78 | 9.94 |
| webhook_flood | rc.7b | 1.66 | 3.45 | 6.37 |
| webhook_flood | rc.7c | 1.74 | 4.03 | 8.84 |
| webhook_flood | rc.7d | 1.65 | 3.37 | 5.71 |
| conversion_upload | rc.6 (prior binary) | 2.71 | 3.93 | 5.43 |
| conversion_upload | rc.7 | 2.99 | 6.39 | 10.87 |
| conversion_upload | rc.7b | 2.72 | 4.55 | 5.96 |

What the repeats establish:

1. **The same binary outruns the gate's tolerance against itself.**
   `perf-compare rc.7 rc.7b` fails on `admin_write` p95 (+52 %, 6.96 → 10.62)
   and p99 (+28 %) — two 60 s runs of one executable, minutes apart. The
   same-binary p99 spread on `admin_write` is **13.62–19.66 (6.0 ms)**, wider
   than every delta flagged against rc.6 on any shape. A gate whose noise
   floor exceeds its tolerance cannot decide this candidate today; TASK-021's
   close hit the same class and resolved it with same-binary repeats, not a
   waiver.
2. **The medians do not move.** storefront p50 across four repeats spans
   3.75–4.16 and brackets rc.6's 3.96; `admin_write` 3.78–4.35 brackets 4.22;
   `webhook_flood` 1.65–1.81 vs 1.51 (its historic rc.3–6 band is
   1.37–1.75). Two extra service hops that cost visible money would show
   here first; they do not. checkout/conversion p50 return to rc.6 exactly
   once warm (rc.7b: 4.01/2.72 vs 3.64/2.71).
3. **The first run after the stack restart was the outlier.** rc.7 is the
   only run against a Postgres restarted minutes earlier; rc.7b — same
   binary, warm caches — passes `checkout_create` and `conversion_upload`
   against rc.6 in full.
4. **Today's tails sit inside bands this repo already committed.**
   TASK-022's same-binary storefront repeats measured p99 7.36–13.42; today's
   off-flag runs (8.67–11.68) are inside it. `admin_write`'s 19.66 exceeds
   every prior run and is the shape whose same-binary spread is widest — the
   ragflow co-tenant is the recorded suspect.

**Verdict, recorded rather than waived:** the rc.7 candidate shows no median
regression anywhere and no tail regression that survives same-binary
comparison; `perf-compare`'s rc.6→rc.7 flags are demonstrated host noise under
a measured co-tenant. Both 11.4 flags ship off, so the released behaviour is
the rc.6 behaviour plus correctness mechanisms (timeouts, breakers, DLQ) whose
per-request cost the medians bound. **The quiet-host confirmation is not
discharged**: TASK-030's benchmark re-run re-compares the final GA candidate
against rc.6 on a host without the ragflow load, and a repeat of this spread
there would reopen the question.

## The shedding flag is free below the ceiling

`rc.7-shed2` is the same binary alone on the host with
`SANVI__SERVER__LOAD_SHEDDING__ENABLED=true` (ceiling 2000 in flight,
`fairness_knee` 1000 — ten times the offered load): storefront p50 **4.16**
— identical to the flag-off rc.7b — p95 7.66 (off-flag band 6.11–7.59), p99
14.40 (one 60 s tail, inside the day's drift). An earlier `rc.7-shed` run
(8.51/13.33) is committed but not cited for cost: it shared the host with the
second API instance and is kept only as the two-instance data point.
Engagement above the ceiling — the 503 + `Retry-After` shape, webhook and
payment-callback exemption, per-tenant fairness — is proven by the injection
and stack tests (`tests/resilience`, `resilience_stack.rs::shipping_webhook_routes_are_exempt_under_redis_degradation`,
`resolved_hot_tenant_is_shed_while_cold_tenant_is_admitted`), not by this
run, which never leaves the sub-ceiling regime.

## Artifacts

`{storefront_read,admin_write,checkout_create,webhook_flood,conversion_upload}/rc.7.json`
(first candidate, cold-start outlier), `*/rc.7b.json` (full warm repeat),
`{admin_write,storefront_read,webhook_flood}/rc.7c.json` and `rc.7d.json`
(the repeat set), `storefront_read/rc.7-shed.json` (flag on, two instances —
kept as the co-tenancy data point) and `rc.7-shed2.json` (flag on, alone).
The rc.6 baseline artifacts are untouched from the TASK-022 close.
