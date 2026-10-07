# 24-hour soak — TASK-021 step 8 (slice 11.2i)

**Status: RUNNING — attempt 2 (launched 2026-10-07 10:36:30 +09:00,
completes ~10:36 on 2026-10-08).** This record's result table is appended by
`summarize.sh` when the run ends; the slice record in the task file
records the verdict separately.

**Attempt 1 aborted (09:57–10:34).** The host entered Modern Standby on its
5-minute idle timeout at ~10:07 while the soak ran — every process froze
together (13-minute hole in the sampler CSV, Postgres connections shed,
working sets trimmed) — and an idle-timeout standby would recur throughout
any 24 h run. Attempt 1's partial artifacts are preserved under
`attempt1-aborted/` (sampler CSV through 10:34; the k6 logs are empty — the
processes were force-killed before any summary was written, so no soak24.json
exists for the attempt). Fix: `powercfg /change standby-timeout-ac 0` (and
`standby-timeout-dc 0`) — **prior values to restore after the run: AC 300 s
(0x12c), DC 180 s (0xb4)**; hibernate was already 0 on AC. The API and worker
processes were not restarted (start times 08:56 / 08:54, before both
attempts); attempt 2 measures the same processes.

**Attempt-2 memory baseline (pass condition 1, restated).** The API entered
attempt 2 at ~94 MB working set — the post-standby trim of the ~457 MB
plateau process, not a fresh process (private commit is unchanged; the
sampler reads `WorkingSet64`). Flatness is judged on slope across the 24 h
window, not on the absolute level; the 457 MB plateau and its cause remain
as recorded in [spike10x-2026-10-07.md](../spike10x-2026-10-07.md).

**Lab, not production.** Same posture as
[`../load3x-2026-10-07.md`](../load3x-2026-10-07.md): seed 1 scale data,
compose Postgres 18 (port 5433), release build 0.10.0, Redis warm, rate
limiting disabled, tracking abuse quota raised, host shared with unrelated
Docker stacks.

- **Shape:** all five k6 shapes **concurrently at the offered rates**
  (50 / 20 / 10 / 100 / 50 iterations/s) for **86,400 s**, tag `soak24`.
  The API carries the offered-rate peak for one day; the worker runs its
  normal schedules against the same database the whole time (outbox,
  schedulers, sweeps) — the conversion upload sweep's success ratio
  (journey 5's budget) is read from its metrics, not from HTTP.
- **Artifacts:** `samples-soak24.txt` (sampler, 30 s cadence, one row per
  sample), `k6-<scenario>.log`, and the five `benchmarks/<scenario>/soak24.json`
  summaries written by k6 at the end.
- **Sampler columns:** timestamp, host CPU, api/worker RSS, api/worker
  threads, api/worker handles, api/worker CPU, api pool
  size/idle/acquire-count/acquire-sum/acquire-failures,
  pg_stat_activity total/api/worker/active.
- **Task counts — the standing gap, restated:** per-task counts are not
  instrumented (the tokio-console gap recorded since 11.2c: unwired, TUI
  not headless-capturable). The OS-level proxies this soak watches —
  thread count, handle count, RSS — are what "no unbounded task
  accumulation" means here, and the caveat transfers to the reader.

## Pass conditions (step 8's verify lines)

1. **Flat memory** — api/worker RSS max ≈ last (slope ≈ 0 over 24 h). The
   process entered the soak at ~457 MB: the Windows-heap plateau left by
   the 10× spikes ([spike10x-2026-10-07.md](../spike10x-2026-10-07.md)),
   not the ~100 MB of a fresh process. Flat-at-457 is the pass; growth
   from 457 is the failure.
2. **No connection growth** — pg totals and the api pool's
   size/idle/acquire counters return to their idle shape between and after
   k6's steady load; acquire failures stay at the spike-era count + 0.
3. **No unbounded task accumulation** — threads and handles flat.
4. **0 failed / 0 dropped** on every shape at the offered rates (the same
   load the rc0/rc.1 captures hold).

## Result (appended at completion)

*pending — `bash benchmarks/soak-2026-10-07/summarize.sh` writes the
verdict block below when k6 exits.*

## Reproducing

```bash
# start (detaches; k6 + sampler survive the shell)
bash benchmarks/soak-2026-10-07/soak.sh            # PERF_DURATION default 86400s
# verdict
bash benchmarks/soak-2026-10-07/summarize.sh
```
