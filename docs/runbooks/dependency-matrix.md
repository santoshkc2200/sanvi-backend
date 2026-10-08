# Dependency matrix — behaviour when a dependency misbehaves

**Owner:** platform engineering · **Source task:** TASK-023 (FR-1112) · **Gate:**
`just test-dependency-matrix`

One row per dependency per failure mode. Each row **is** the contract: the behaviour
column states what the system does, and the test column names the injection test that
proves it — `scripts/check-dependency-matrix.sh` fails when a row's test file or test
function does not exist, and `just test-dependency-matrix` runs every named test. A
behaviour change without a matching test change fails CI, and so does the reverse.

Shared vocabulary:

- **Timeout** — the transport deadline on the outbound call (`NFR-1105`: no unbounded
  call exists; `just check-timeouts` proves it). Circuit breakers and timeouts are not
  flag-gated.
- **Retry** — the transport-level retry policy. `429` (the caller asked too often,
  TASK-026) and `503 platform/overloaded` (the platform is too busy) are never
  produced by this matrix; they are admission outcomes, not dependency outcomes.
- **Breaker** — the `sanvi_resilience::CircuitBreaker` thresholds (`failures` to open,
  `cooldown` before the half-open probe). While a breaker is open the call does not
  run at all.
- **Degraded** — the reason slug the adapter flips on
  `sanvi_resilience::DegradationRegistry` while the row's degradation is active; the
  `platform.degraded_mode` flag turns these into an `X-Sanvi-Degraded` response header.
- **Never shed** — requests under `server.load_shedding.exempt_prefixes` (webhooks,
  payment callbacks, health, the build probe) are admitted ahead of everything else,
  whatever the load.

| Dependency | Mode | Behaviour | Timeout | Retry | Breaker | User-visible consequence | Test |
|---|---|---|---|---|---|---|---|
| Postgres | slow | The statement timeout cancels the query (`SQLSTATE 57014`, "canceling statement due to statement timeout"); the adapter maps that text to the `request-timeout` problem. A lock timeout is *not* this class. | `database.statement_timeout_ms` (API 10 s, worker 60 s) | none — the caller retries | none — the pool is the bound | `504` problem `request-timeout` on the affected route | `tests/resilience/tests/postgres.rs#postgres_slow_maps_statement_timeout_to_request_timeout` |
| Postgres | erroring | Transient SQLSTATEs (40001 serialization, 40P01 deadlock, 08\* connection, 57P\* cancellation) classify as retryable by the messaging layer; anything else is opaque. | statement timeout as above | transient errors retry in the outbox/consumer (bounded attempts, then DLQ) | none | retried work settles or dead-letters; interactive requests surface `5xx` | `tests/resilience/tests/postgres.rs#postgres_transient_sqlstates_classify_as_retryable` |
| Postgres | down | Every DB-backed route fails opaque (`internal-error`); `/healthz` stays `200` because liveness never touches the database (TASK-025 owns the probe contract); the pool acquire timeout bounds how long a request waits. Pool exhaustion from saturation (not down-ness) is the load-shedding pool-wait signal's job. | `acquire_timeout_secs` (10 s) | none interactive; outbox/consumer resume on reconnect | none | `500` problems on DB-backed routes; health stays green | `tests/resilience/tests/postgres.rs#postgres_down_fails_opaque_while_pool_bounds_the_wait` |
| Redis | slow | The response timeout bounds every command; the rate limiter's own 100 ms budget fires first and fails open. | `response_timeout_ms` (500 ms), connect 1000 ms | none — commands are single-shot | none — the fail-open posture is the breaker's replacement | rate limiting skips (counted), caches miss, responses gain `X-Sanvi-Degraded: redis` | `tests/resilience/tests/redis.rs#redis_down_fails_open_on_the_rate_limiter_and_flags_degraded` |
| Redis | erroring | Same fail-open posture as down: caches read as a miss, abuse counting falls back to the in-process window, scheduler leadership lapses until reconnect. | as above | none | none | as above | `tests/resilience/tests/redis.rs#redis_erroring_degrades_to_cache_misses` |
| Redis | down | Streams stop flowing (outbox delivery and consumer groups pause and resume on reconnect; nothing is lost — the outbox table is the durability boundary). | as above | redelivery on reconnect (XAUTOCLAIM reclaims idle pending entries) | none | messaging pauses; API serves from Postgres at reduced throughput — the load shedder halves the non-essential ceiling while `redis` is degraded | `tests/resilience/tests/redis.rs#redis_down_pauses_streams_without_losing_the_outbox` |
| Kratos | slow | The whoami call times out and the authn middleware fails closed: `503` service-unavailable, never an anonymous pass-through of an authenticated route. | 5 s total, 5 s connect | none — the browser retries | 5 failures / 30 s cooldown on the whoami client | `503` on authenticated routes while Kratos is unreachable | `tests/resilience/tests/kratos.rs#kratos_down_fails_authn_closed_with_503` |
| Kratos | erroring | Same as slow: any verifier error fails closed with `503`. | as above | none | as above | as above | `tests/resilience/tests/kratos.rs#kratos_erroring_fails_authn_closed_with_503` |
| Kratos | down | Same as slow (the admin/session surfaces likewise report unavailable). | as above | none | as above | as above | `tests/resilience/tests/kratos.rs#kratos_down_fails_authn_closed_with_503` |
| Keto | slow | The membership check times out; the degraded authorizer takes over: **writes deny by default; reads serve the cached decision for ≤ 30 s (the decision cache TTL), then deny** — access is never granted because the backend was down. | 5 s total, 5 s connect | none — a wrong allow is unbounded damage, a deny is cheap to retry | 5 failures / 30 s cooldown | writes `403`; reads `403` after the cache window; `X-Sanvi-Degraded: keto` | `tests/resilience/tests/keto.rs#keto_down_denies_writes_and_serves_cached_reads_for_the_grace_window` |
| Keto | erroring | Same degradation path as slow — the breaker opens on consecutive failures regardless of whether they are timeouts or 5xx. | as above | none | as above | as above | `tests/resilience/tests/keto.rs#keto_erroring_takes_the_same_degraded_path` |
| Keto | down | Breaker open: `check` fails fast without a network call; the degraded authorizer's deny/cached-read rule applies for the whole outage. Tuple writes pause (rebuild re-syncs on recovery). | n/a while open — fail fast | tuple writes resume after the breaker closes (the rebuild job reconciles) | 5 failures / 30 s cooldown | as above | `tests/resilience/tests/keto.rs#keto_down_denies_writes_and_serves_cached_reads_for_the_grace_window` |
| Stripe | slow | The request deadline cancels the call; the error surfaces as a connection failure to the use case. | 20 s total, 5 s connect | 3 attempts, 200 ms exponential backoff, on 429/409/5xx and transport errors only | 5 failures / 30 s cooldown | the checkout/charge operation fails honestly (`502`/`503` problem); no partial state | `tests/resilience/tests/stripe.rs#stripe_slow_times_out_and_reports_a_connection_failure` |
| Stripe | erroring | `5xx` (and 429/409) retry under the same **Idempotency-Key**, so a retried mutation can never double-charge; `4xx` other than 429/409 surface immediately without retry. | as above | as above | as above | delayed success or an honest failure; never a double charge | `tests/resilience/tests/stripe.rs#stripe_erroring_retries_under_the_same_idempotency_key` |
| Stripe | down | Retries exhaust into `StripeConnection`/`StripeService`; the use case maps it to a problem response. Connect (payments) behaves identically. | as above | as above | as above | `5xx` problem naming the dependency; queued work resumes on recovery | `tests/resilience/tests/stripe.rs#stripe_down_exhausts_retries_into_a_connection_error` |
| DNS/ACME | slow | Each resolver/probe carries its own deadline; a slow resolver is indistinguishable from a failing one and drops out of the majority. | per-request `timeout_secs` (5 s) | none within a verification round — the poll job re-queues | none — N-of-M majority is the redundancy | domain verification waits; the domain stays `pending` | `tests/resilience/tests/dns_acme.rs#dns_slow_resolver_drops_out_of_the_majority` |
| DNS/ACME | erroring | The N-majority check tolerates failing resolvers: 2 of 3 agreeing still verifies; all failing fails the round. | as above | next poll retries | none | verification succeeds on majority or waits for the next poll | `tests/resilience/tests/dns_acme.rs#dns_erroring_resolvers_tolerated_up_to_the_majority` |
| DNS/ACME | down | Every resolver unreachable: the round fails, the domain stays pending, the verification poll retries on its schedule. | as above | as above | none | issuance delayed, nothing corrupted | `tests/resilience/tests/dns_acme.rs#dns_down_fails_the_round_and_stays_pending` |
| Google Ads | slow | The client deadline cancels the upload/audience/token call; the sweep's attempt accounting sees a failure like any other. | 20 s total, 5 s connect | none at transport level — the conversion sweep owns attempt accounting (5 attempts, then Parked) | 5 failures / 30 s cooldown | uploads delayed; conversions **queue with their directive snapshot and are never dropped** (FR-1113) | `tests/resilience/tests/google_ads.rs#google_ads_outage_parks_conversions_with_their_consent_snapshot` |
| Google Ads | erroring | Each failing sweep attempt increments the platform's attempt count; after `MAX_ATTEMPTS` the conversion parks (the conversion DLQ) with its consent snapshot intact; `RetryConversion` is the operator path back. | as above | sweep-level: 5 attempts | as above | as above, plus `advertising_conversion_dlq_depth` gauge | `tests/resilience/tests/google_ads.rs#google_ads_outage_parks_conversions_with_their_consent_snapshot` |
| Google Ads | down | Connection refused behaves as erroring; nothing uploads while the platform is down, and nothing drops. | as above | as above | as above | as above | `tests/resilience/tests/google_ads.rs#google_ads_outage_parks_conversions_with_their_consent_snapshot` |
| Meta | slow | As Google Ads. | 20 s total, 5 s connect | sweep-level: 5 attempts | 5 failures / 30 s cooldown | as Google Ads | `tests/resilience/tests/meta.rs#meta_outage_parks_conversions_with_their_consent_snapshot` |
| Meta | erroring | As Google Ads: per-platform attempt accounting parks the conversion with its snapshot; the other platform's upload state is untouched. | as above | as above | as above | as above | `tests/resilience/tests/meta.rs#meta_outage_parks_conversions_with_their_consent_snapshot` |
| Meta | down | As Google Ads. | as above | as above | as above | as above | `tests/resilience/tests/meta.rs#meta_outage_parks_conversions_with_their_consent_snapshot` |
| Object storage | slow | The deadline cancels the call; reads fail into the placeholder path, uploads fail honestly. | 20 s total, 5 s connect | none — uploads are idempotent by key, the client retries | 5 failures / 30 s cooldown | asset reads serve placeholders; uploads get `503` `media_unavailable` | `tests/resilience/tests/object_storage.rs#object_storage_down_serves_placeholders_and_blocks_uploads` |
| Object storage | erroring | Same as down: the breaker opens on consecutive 5xx; reads degrade to placeholders (`url: None` + the degraded header at the HTTP edge), uploads/mutations return `MediaError::Unavailable` → `503`. | as above | none | as above | as above | `tests/resilience/tests/object_storage.rs#object_storage_down_serves_placeholders_and_blocks_uploads` |
| Object storage | down | Breaker open: calls fail fast; the adapter flips `object_storage` degraded; recovery is automatic on the half-open probe. | n/a while open | none | as above | as above | `tests/resilience/tests/object_storage.rs#object_storage_down_serves_placeholders_and_blocks_uploads` |

## Rules this matrix pins

1. **Nothing here produces `429`.** Rate limits and quotas are TASK-026; overload is
   `503 platform/overloaded` from the load shedder, never from a dependency row.
2. **A conversion queued without its directive snapshot is an upload we cannot later
   justify.** The snapshot travels with the event or the event does not upload — the
   ad-platform rows assert this explicitly.
3. **Every row's test injects the failure** (a fault server, a closed port, a failing
   port implementation, or a real query against the test Postgres) — no row is proven
   by reading code.
4. Adding a dependency means adding three rows and three tests; the check script
   fails the build on a row without a test or a test without a row.
