# Public API v1 style guide

The Rust annotations generate all fourteen context specifications. Run
`cargo run -p sanvi-cli -- openapi --write`, `just spec-lint`,
`just spec-freeze-check` and `just api-docs --check` before review.

Resource routes use `/api/v1`, lower-case kebab-case nouns and plural collections.
`tenant`, `me`, `system`, `public` and action routes are deliberate singleton or
command namespaces. Path variables and JSON fields use snake_case. Operation IDs
are unique snake_case verbs. Snowflake IDs cross the transport as decimal strings;
external correlation IDs keep their documented provider formats.

Operational health probes and `/internal/tls/authorize` retain their existing
non-versioned addresses. `/platform/audit` names the audit stream; theme
`/available` is an established catalog projection. Design-token `$type`, `$value`
and `$description` follow the external token format and are preserved explicitly.
Map-valued money fields have integer leaves; `is_minor` denotes age, not money.

Every paginated collection uses optional `after` and `limit`. `after` is the
previous page's cursor, passed unchanged; compound keys are encoded in one string.
Limits declare minimum 1, a finite maximum no greater than 500 and an in-range
default. Bounds and defaults remain resource-specific capacity controls from
TASK-021. Legacy payment `cursor` and admin compound fields remain accepted as
undocumented input aliases; public documentation only emits `after`.

Filters are explicit snake_case query fields; unknown sorting/filter DSLs are not
introduced. Existing collection order is documented by each operation; cursors
must be used with the same filters and order as the first request.

Timestamps are RFC3339 UTC strings with `date-time` schema format. Monetary amounts
are integer minor units plus an ISO4217 currency code, never floating-point money.
JPY 1200 is ¥1,200; USD 1299 is $12.99. Generated journey examples show both
en-US/USD and ja-JP/JPY for requests and successful responses.

String enum values are snake_case. Schema names follow Rust type names, with
context prefixes whenever a name could identify more than one wire model.
RFC9457 errors use `application/problem+json` and the shared `ProblemDetail` schema:
required `type`, `title`, `status`, `detail`; optional `instance`, `trace_id`,
`reason` and namespaced extensions. Stable types live under
`https://sanvi.dev/problems`; the generated shared registry lists their slugs.
Readiness 503 uses the assembled API's problem-details envelope. Successful
readiness responses retain their documented dependency states; HTTP 429
indicates client limits.

The frozen baseline is immutable after introduction. Optional fields/endpoints and
enum additions are allowed; removals, narrowed enums, requiredness changes,
new required inputs, schema restrictions, removed statuses/content types/headers
and changed authorization fail the gate. CI anchors snapshot integrity to the PR
base commit.
The gate also compares against the PR base's published specs once the freeze
exists, so a later additive field cannot subsequently disappear unnoticed.
An exception needs `api-breaking-change-reviewed` plus an
independent collaborator's approval of the exact current head; it cannot replace
the frozen snapshot. Deprecation policy governs the exception's migration window.

Merge-queue candidates enforce compatibility without exceptions: their synthetic
head cannot reuse a review of a PR head. A reviewed breaking exception uses the
repository's approved direct-merge path with all other required checks passing.
Queue-only repositories must arrange that explicit administrative exception
before attempting a breaking merge; the gate does not infer approval from a label.

The shipped locale seeds and explicit negotiation allow-list are `en` and `ja`
(`configs/base.toml`, the localization seed migration). Examples use regional
Accept-Language headers `en-US`/`ja-JP`, which negotiate those language catalogs;
locale-valued request fields use the supported `en`/`ja` codes.

Consistency pass: previously every context omitted referenced component schemas;
components are now bundled transitively. Problem responses incorrectly advertised
JSON rather than problem+json; generation now reflects middleware behavior.
Timestamp strings now carry their format. Payment cursor names and the admin
compound key now share `after`, preserving legacy inputs. All fourteen contexts,
including media, participate in lint, freeze and docs generation.

Different wire shapes require context-qualified component names, even when their
Rust types share a name. The linter checks source declarations before the merged
OpenAPI component map can hide collisions, then compares shared definitions
across context specs. The shared `ImpersonationMode` enum is checked directly in
the CLI tests. Domain pricing retains its existing integer `minor` field;
advertising and payments retain integer `amount_minor` fields.
