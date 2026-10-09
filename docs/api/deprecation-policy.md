# API v1 deprecation and limit policy

API v1 is frozen. Additive changes remain available; removing a resource or field,
narrowing an enum, or changing requiredness needs a reviewed freeze exception and
a published migration. The freeze is not disabled by operational feature flags.

`api/deprecations.json` is the source for deprecation dates, affected operation
templates, and replacements. Entries identify either an operation or an element
within it. A sunset must follow deprecation by at least six calendar months.
The registry validator rejects duplicate element identifiers, missing replacements,
invalid operation methods, and shorter notice. An operation carrying several
deprecated elements emits the earliest sunset so a client sees the first deadline.

Affected responses carry `Deprecation: @<Unix timestamp>` (RFC 9745) and `Sunset`
as an HTTP date (RFC 8594). Templates match the router's operation pattern rather
than user-supplied URLs. The headers also cover error responses returned by the
affected operation. A release changelog is generated with
`python scripts/deprecation-policy.py --write`; CI checks it with `--check`.
No v1 elements are currently deprecated, so the initial registry is empty.

Rate limits ship with `platform.rate_limits = false`: every eligible request
still increments the shared Redis counter. Enabling the flag enforces the already
observed counters; disabling it restores unlimited commercial HTTP access while
keeping counting live. TASK-024 abuse controls remain separate security controls.
Health, metrics, and CORS OPTIONS requests are excluded from commercial counting.

The starting thresholds derive from TASK-021's measured healthy throughput of
840 requests/second across 147 tenants: `floor(840 / 147) = 5` requests/second
per tenant. TASK-021 measured 1.5 core-ms/request. The lab supports a starting
allocation, not a production capacity promise; real traffic tuning is parked in
`docs/release/needs-humans.md`. Plan weights 1, 4 and 16 allocate Starter,
Professional and Enterprise 5, 20 and 80 requests/second, respectively.

| Plan | Tenant requests/minute | Credential requests/minute |
| --- | ---: | ---: |
| Starter | 300 | 300 |
| Professional | 1200 | 600 |
| Enterprise | 4800 | 1200 |

Credentials receive weights 1, 2 and 4 of the base allocation, preventing one
credential from consuming every upgraded tenant's capacity. Every authenticated
request increments both buckets atomically, including denied requests. Tenant
identity comes from verified resolution and authentication; a credential key is
the SHA-256 digest of the verified session identifier, qualified by tenant.
Client plan, credential and forwarded-IP headers never determine an allocation.
Plan tier comes from authoritative billing through the composition root adapter.

Unauthenticated and public routes use a stricter peer-IP bucket of 60/minute,
one fifth of Starter. Tracking uses 30/minute, one tenth of Starter, and domain
verification uses 12/minute, one twenty-fifth of Starter. Domain verification
also consumes authenticated tenant and credential buckets when present. Public
identity uses the TCP peer IP; deployments must place the limiter where the peer
identity is meaningful. Arbitrary forwarded headers are not trusted.

Redis uses one atomic script and its server clock for one-minute fixed windows.
Each key expires after the window, bounding live state by recent traffic rather
than all credentials ever seen. A window seam can admit twice the limit across
two adjacent windows. There is no process-local fallback that could grant extra
capacity across replicas. Redis failures pass requests during counting-only
operation and produce `503` during enforcement, signalling infrastructure failure.
Unavailable billing still counts using Starter budgets; enforced requests receive
`503` while the tier cannot be verified. Deploy Redis without evicting active
counters before enabling enforcement; a Redis flush resets this short-lived
rate window, unlike durable monthly quotas.

Ordinary denials return RFC 9457 `429` with `Retry-After`, `RateLimit-Limit`,
`RateLimit-Remaining`, and `RateLimit-Reset` (seconds until reset). Healthy
enforced responses expose the same limit metadata. Platform load shedding and
counter infrastructure failures use `503`, never caller `429`; downstream `503`
responses retain their existing headers. Tracking flood drops keep its uniform
empty `202` security contract and expose no rate metadata. Its stricter bucket
still prevents the handler from accepting the excess work.

External media uploads and notification sends reserve quota before dispatch.
The ledger stores SHA-256 digests of the idempotency key and serialized payload;
it does not store message content or API credentials. Reusing a key with a
different payload is rejected. A definite upstream rejection refunds once only
after all registered attempts finish and none succeeded or had an unknown
outcome. Released keys are retired: subsequent work needs a new key. Successful
asset deletion releases storage once; a retry returning upstream `404` repairs
any missing release while preserving that response status.

Upstream timeouts, uncertain responses, and process crashes retain reservations
conservatively. Each key permits at most 128 outstanding attempts; enforcement
returns `503` at that bound. Counting-only mode can continue, recording the
unknown outcome so a later rejection cannot refund an untracked attempt.
Reservations with unknown outcomes require reconciliation against upstream
evidence before release; there is no automatic expiry that could undercount
successful work. Accounting infrastructure failures allow dispatch while quotas
are disabled and can leave usage incomplete until reconciliation. Enforcement
returns `503` so callers can retry rather than silently lose quota accounting.
