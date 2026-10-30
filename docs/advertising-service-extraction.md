# Advertising service extraction

Advertising runs in the separate local repository `../sanvi-advertising`, with its own
PostgreSQL database and container. The platform crate is a gateway and contract snapshot;
it contains no Advertising use cases, provider clients, repositories or scheduler jobs.
Existing tenant Advertising, platform health and public tracking paths are preserved.

## Platform configuration

Keep `features.advertising` as the route/job rollout switch. Set these environment values:

- `SANVI__ADVERTISING_SERVICE__BASE_URL`: Advertising origin (local service port 8085).
- `SANVI__ADVERTISING_SERVICE__SIGNING_SECRET`: matches the service platform inbound key.
- `SANVI__ADVERTISING_SERVICE__INBOUND_SECRET`: matches the service platform signing key.

Use distinct directional keys of at least 32 bytes. Provider credentials, OAuth/site-key
secrets, token KEK, adapter modes and Advertising job settings now live exclusively in
the service under `SANVI__ADVERTISING_SERVICE__ADVERTISING__*` and
`SANVI__ADVERTISING_SERVICE__SCHEDULER__*`. Give Advertising a dedicated Payments service
credential pair; it still obtains recorded payment values over the signed Payments API.

The gateway preserves GPC recording on the storefront's first beacon, signing the minted
device ID and original browser metadata into the forwarded request. Permission and
route entitlement checks remain on the platform. The service rechecks platform-specific
entitlements remotely and accepts only signed, single-use delegation identities.

Signed internal endpoints:

- `POST /internal/advertising/events`: digest-protected, idempotent event ingestion;
  new conversion quota usage and outbox insertion share the receipt transaction.
- `POST /internal/advertising/platform`: current directives, snapshots, vendor role,
  timezone, tenant label, Advertising entitlement, retention policy and tracking quota admission.

API and worker privacy registries use the remote Advertising provider. An unavailable
service leaves collect/erase/rectify and ad suppression pending rather than reporting success.
The platform continues to render and send Advertising notification intents from outbox events.

## Data and deployment

Apply the new receipt, nonce and partition-handoff migrations during the coordinated
freeze. Historical Advertising migrations and data remain intact. Partition handoff
saves and disables only Advertising registry rows; rollback restores their prior settings.
Only the service schedules Advertising partition creation and retention after cutover.

Follow `sanvi-advertising/docs/extraction.md` for copy verification, exact writer stop/start
ordering, credential continuity and rollback. The guarded data-copy tool preserves existing
Snowflakes, ciphertext, pending OAuth state, historical partitions and operation idempotency.
Historical platform outbox rows stay on the platform to avoid double-counting conversions.
No production database or running deployment is changed by this repository extraction.

The service owns `api/advertising.yaml`. The platform consumes its frozen copy at
`crates/contexts/advertising/contract.yaml`; the merged OpenAPI contract remains available
to frontend tooling. Update both artifacts together for future API changes.
