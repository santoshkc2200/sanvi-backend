# Log field standard — TASK-020 step 5

What every log line and span in the platform may carry, under which names,
and at which level. Consistent field names are what makes a query written
during one incident still work during the next one: OpenObserve queries
are written against field names, and a field that drifts between contexts
(`tenant`, `tenantId`, `tenant_id`) silently halves the graph.

## Canonical fields

| Field | Meaning | Emitted by |
| --- | --- | --- |
| `tenant_id` | The tenant scope of the operation, if it has one. Every tenant-scoped use-case span carries it (enforced by the trace-coverage matrix's `tenant refs` column, reviewed per slice). | use-case spans |
| `request_id` | The `x-request-id` correlation id minted (or propagated) per HTTP request. | request root span |
| `actor_id` | Who did it — a `UserId`, `platform`, or `system:<sweep>` for worker sweeps. Never an email. | use-case spans where an actor is in scope |
| `sanvi.context` | Bounded context derived from the route (`billing`, `ads`, …). | request root span |
| `http.method` / `http.target` / `http.status_code` / `http.client_ip` | Request facts, following the W3C/OTel http attribute names already in use. | request root span |
| `sanvi.duration_ms` | Wall-clock request duration. | request root span |
| `error` | The error rendering, `error = %err` — one field, not `err`/`e`/`error_message`. | any level |
| `trace_id` (response bodies) | The W3C trace id, also carried by every problem-details body and the `traceparent` response header (step 1). | error layer |

Names are snake_case, lowercase, and stable. New fields get added here
before they get emitted in three different spellings.

## Level policy (production volume, with cost estimate)

Levels are a budget, not adjectives. The estimate below is for the lab
profile of `benchmarks/baseline-2026-09-17.md` (five k6 shapes, ~230 rps
offered) scaled ×10 as a production allowance; per-line JSON render costs
~1 µs CPU and ~300 bytes wire, so the log stream is bounded by decisions
made here, not by traffic.

| Level | What belongs here | Estimate at 10× lab | Notes |
| --- | --- | --- | --- |
| `error` | The request failed and a user noticed: 5xx, unrecoverable upstream, invariant violation. Paging reads this level first. | ≤ 0.1 % of requests | Never a rejected validation — a 400 is the client's error, not ours. |
| `warn` | Recovered degradation worth knowing: rate limits tripped, fail-open Redis, retries, suppression of a conversion for missing consent. | ≤ 1 % of requests | The alert-rule input level. If a warn cannot ever page, question it. |
| `info` | Use-case boundaries (every span from the trace-coverage matrix) and domain events with audit weight. | 1 line/use case ≈ 3–8 % of requests | The default level. This is the bulk of the volume and why use-case spans carry ids, not payloads. |
| `debug` | Everything decision-relevant that did not survive the info cut. Local default for `log_format = "text"` runs. | unbounded, off in production | Never PII by position — the PII corpus scanner runs at info; do not rely on debug being filtered elsewhere. |
| `trace` | Off everywhere until something can consume it. | — | — |

The PII rule cuts across all levels: sensitive values are wrapped in
`Redacted` (enforced by `just log-pii-scan` over
`crates/platform/telemetry/tests/pii_corpus.rs`); the level filter is not
a redaction strategy.

## Sampling

Head sampling keeps `telemetry.sampling.head_ratio` of traces at span
start; with `telemetry.sampling.tail_sampling` off (the default) that is
the whole policy. With the flag on, the tail processor decides instead: it
keeps every error span and every span slower than
`telemetry.sampling.latency_threshold_ms`, and applies `head_ratio` as a
per-trace coin to the rest — a trace is exported whole or dropped whole.
Both positions are unit-tested in
`crates/platform/telemetry/src/sampling.rs`, including the
collector-unavailable invariant: losing traces must never cost requests.
