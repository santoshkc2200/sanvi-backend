# Accounting integration gateway

Runtime ownership has moved to the standalone `accounting-service` repository and
its own PostgreSQL database. The platform database no longer contains an
`accounting` schema (contracted in migration `20261030000400_accounting_contraction`).

This platform crate (`sanvi-accounting`) supplies what the platform needs to front
the service:

- Request-bound HMAC-SHA256 delegation and gateway proxying for tenant accounting routes.
- Published HTTP contract types (for OpenAPI contract generation; bodies are relayed byte for byte).
- Remote privacy provider delegating subject export and purge to the accounting service.
- Typed identifiers (`BookId`, `ConnectionId`) and provider enums for route parameters and internal deliveries.

See [the extraction guide](../../../docs/accounting-service-extraction.md).
