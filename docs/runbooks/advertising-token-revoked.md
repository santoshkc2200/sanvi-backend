# Runbook — advertising token revoked or uninstalled

**Alert:** `AdConnectionRevoked` (see [advertising-alerts](./advertising-alerts.md)).

A connected Google Ads or Meta account has had its OAuth refresh token revoked,
scopes reduced, or app installation removed on the platform side by the tenant
admin.

## Domain lifecycle behavior (designed, not yet wired)

`crates/contexts/advertising/src/domain/connection_lifecycle.rs` defines the
correct reaction to a revoked or permission-reduced refresh via
`refresh_failure_transition(RefreshFailureKind::Revoked | PermissionRevoked, already_notified)`:
status to `Disconnected`, `schedule_retry = false`, `pause_dependent_jobs = true`
(persisted as `connections.dependent_jobs_paused_at` via
`Connection::mark_dependent_jobs_paused`), and a one-shot
`notify_tenant`/`disconnected_notified_at` via `Connection::mark_disconnected_notified`.
`Connection::record_permanent_refresh_failure` and `apply_credentials` (which
clears both columns and `last_error` on a successful reconnect) are real,
tested domain methods, and both columns exist on `advertising.connections`
(migration `20260826030000_advertising_connection_lifecycle.sql`).

**Known gap:** nothing in `apps/worker/src/main.rs` or `application/` calls
`refresh_failure_transition` — there is no scheduled sweep that proactively
refreshes a token and reacts to a revoke (same root cause noted in
[advertising-alerts.md](./advertising-alerts.md)'s "Known gaps": TASK-011's
refresh scheduler was never wired up). Today `status` only ever moves to
`disconnected` via the tenant's own `DELETE /api/v1/tenant/ads/connections/{connection_id}`
(`DeleteConnection::execute`, which calls the bare `Connection::disconnect()` —
it does **not** set `last_error`, `disconnected_notified_at`, or
`dependent_jobs_paused_at`) or a direct operator UPDATE. A platform-side
revocation you haven't been told about surfaces only as upload/sync failures
against that connection, not as a status flip — check symptom 2 below.

The connection health computed in `ConnectionHealth::from_connection` returns
`reconnect_required = true`, `can_sync = false`, and `can_upload_conversions = false`
once status is `disconnected`/`expired`, and reports `scopes_missing` for a
narrower permission reduction that leaves status `active`.

## 1. Identify

Inspect disconnected or degraded connections:

```sql
SELECT id, tenant_id, platform, external_account_id, account_name,
       status, scopes, last_error, last_synced_at,
       disconnected_notified_at, dependent_jobs_paused_at
FROM advertising.connections
WHERE status IN ('disconnected', 'expired')
   OR (last_error IS NOT NULL AND updated_at > now() - interval '24 hours')
ORDER BY updated_at DESC;
```

Check the tenant-facing endpoint output (`GET /api/v1/tenant/ads/connections`):
the connection object returns `status: "disconnected"`, `reconnect_required: true`,
and `scopes_missing` if permissions were revoked.

Given the gap above, a revocation that hasn't already flipped `status` shows
up first as `ads_sync_failures_total{platform}` or
`conversion_upload_total{result="failed"}` climbing for every campaign/event
on one connection while other connections on the same platform stay healthy
— correlate the failing `connection_id` from adapter error logs against the
query above.

## 2. Reconnection workflow

Sanvi preserves the original connection ID, credentials vault slot, and campaign
history during reconnection (`crates/contexts/advertising/src/application/connections.rs`):

1. **Initiate OAuth start**:
   The tenant admin starts re-authorization via `POST /api/v1/tenant/ads/connections/{platform}/oauth/start`.
   The endpoint returns a platform consent URL with an HMAC-signed state parameter.
2. **Platform consent**:
   The tenant approves the required scopes on Google Ads or Meta.
3. **Redeem callback**:
   The callback hits `POST /api/v1/tenant/ads/connections/{platform}/oauth/callback`
   with the returned `code` and `state`.
4. **Re-activate connection**:
   The exchange stores the new encrypted refresh token in `domain::TokenVault`
   via `store_refresh_token` and updates the existing connection row via
   `AdvertisingConnectionRepository::upsert`.
   `apply_credentials` resets `status` to `ConnectionStatus::Active`, clears
   `last_error`, `disconnected_notified_at`, and `dependent_jobs_paused_at`.

## 3. Verify

Confirm the connection has returned to active health:

```sql
SELECT id, tenant_id, platform, external_account_id, status,
       last_error, disconnected_notified_at, dependent_jobs_paused_at
FROM advertising.connections
WHERE id = <connection_id>;
```

1. `status` is `'active'`, `last_error` is `NULL`, `disconnected_notified_at` is `NULL`.
2. Worker sweeps automatically resume processing conversions and metrics on the next
   scheduled cycle (`advertising_conversion_upload_interval_secs`,
   `advertising_metrics_ingestion_interval_secs`).
