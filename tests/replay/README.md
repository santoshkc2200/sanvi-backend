# Release candidate replay

`just replay-suite rc.8` (portable: `bash scripts/replay-suite.sh rc.8`) captures
observed JSON responses from the real assembled Actix application using the
existing HTTP contract fixture. It also runs the HTTP routing, cache and security
header contract corpus. No recorded production traffic is required.

`target/replay/<candidate>.json` and its `.usage.json` sidecar record actual
statuses, headers and bodies. Separate fixture files avoid concurrent test writes.
The validator resolves generated OpenAPI references and checks required fields,
types, enums, media types and documented headers. Its mutation tests deliberately
remove a required response field, change status and change content type.

Current capture covers health, readiness, build, version, the RFC9457 error
envelope and the database-backed tenant usage journey. The production replay gate
fails if that observed successful tenant response is missing. Context integration
suites continue to cover other database and payment-provider journeys. This
corpus does not claim recorded-traffic coverage.
