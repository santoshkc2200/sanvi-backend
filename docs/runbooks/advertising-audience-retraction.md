# Runbook — audience list & conversion retraction issues

**Alerts:** `ConversionRetractionUnpropagatedSpike`, `AudienceSyncFailure` (see [advertising-alerts](./advertising-alerts.md)).

An audience list member removal or conversion event retraction failed to propagate
to an ad platform (Google Ads / Meta) following a user's privacy opt-out or consent
withdrawal.

## Background & Privacy mechanics

Customer privacy consent directives strictly govern advertising data in Sanvi:
1. **Audience customer lists** (`crates/contexts/advertising/src/application/tracking/audiences.rs`):
   - Building and syncing audiences requires BOTH `ProcessingPurpose::SaleOrShare` AND
     `ProcessingPurpose::TargetedAdvertising` to be in `DirectiveState::Allowed`.
   - `SyncAudiences` runs periodically (`advertising_audience_sync_interval_secs = 3600`).
   - Opted-out subjects are removed from `eligible_identifiers` on each pass via
     `compute_audience_diff`.
2. **Conversion event retractions** (`crates/contexts/advertising/src/application/tracking/retraction.rs`):
   - `RetractWithdrawnConversions` runs periodically (`advertising_conversion_retraction_interval_secs = 60`).
   - If consent for `ProcessingPurpose::AdsMeasurement` was withdrawn post-upload, it issues
     deletion/suppression requests to connected ad platforms.
3. **Explicit unpropagated state**:
   - If an ad platform does not support deletion/removal for that entity type
     (`capability_unsupported`), the event transitions to `UploadState::Unpropagated { reason }`
     rather than silently pretending it was removed.

## 1. Identify unpropagated retractions and audience sync failures

Check for unpropagated conversion retractions:

```sql
SELECT id, tenant_id, event_id, name, occurred_at, upload_states, created_at
FROM advertising.conversion_events
WHERE upload_states::text LIKE '%unpropagated%'
ORDER BY occurred_at DESC
LIMIT 50;
```

Check audience list status and errors (`advertising.audiences` has no
`last_error`/`member_count` columns — failures land in `failure_reason` and
membership is the `included_identifiers` array):

```sql
SELECT id, tenant_id, platform, name, external_ref, status, failure_reason,
       jsonb_array_length(included_identifiers) AS member_count,
       last_synced_at, updated_at
FROM advertising.audiences
WHERE status != 'active' OR failure_reason IS NOT NULL
ORDER BY updated_at DESC;
```

## 2. Triage & Determine root cause

1. **Transient Network or Rate-Limiting Error**:
   - The platform returned a 5xx or 429 during retraction/audience sync.
   - Status will retry automatically on the next scheduled worker sweep.
2. **Platform Limitation (`capability_unsupported`)**:
   - Meta or Google Ads API may not support point deletion for historical events past
     their retention horizon (e.g. conversions older than platform upload limits).
   - An explicit `Unpropagated` state is preserved in our database for audit compliance.
3. **Connection Revocation / Missing Permissions**:
   - The ad account connection lacks the audience management scope (`ads.manage` / `ads_management`).
   - Follow [token revoked](./advertising-token-revoked.md).

## 3. Manual refresh & Remediation

Trigger an immediate on-demand audience refresh:

```bash
curl -X POST -H "Authorization: Bearer $TENANT_TOKEN" \
  https://api.sanvi.internal/api/v1/tenant/ads/audiences/<audience_id>/refresh
```

`RefreshAudience` (`crates/contexts/advertising/src/application/tracking/audiences.rs`) will:
1. Re-evaluate all eligible identifiers for the tenant across all conversion events.
2. Diff against existing list members via `compute_audience_diff`.
3. Send the updated member delta (additions and removals) to the platform.
4. Update `included_identifiers` and `last_synced_at`.

For conversion events in `UploadState::Unpropagated`:
- Review the `reason` field under the affected platform's key in `upload_states`.
- If caused by platform constraints, ensure legal/privacy records reflect the unpropagated
  reason for statutory data protection compliance.

## 4. Verify

1. Confirm `audience_sync_total{result="synced"}` increments.
2. Verify audience status is active:
   ```sql
   SELECT id, name, status, jsonb_array_length(included_identifiers) AS member_count,
          last_synced_at, failure_reason
   FROM advertising.audiences
   WHERE id = <audience_id>;
   ```
3. Confirm `ConversionRetractionUnpropagatedSpike` alert clears.
