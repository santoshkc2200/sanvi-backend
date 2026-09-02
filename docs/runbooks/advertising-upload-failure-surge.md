# Runbook — conversion upload failure surge

**Alerts:** `ConversionUploadFailureSpike`, `ConversionDeadLetterDepth`, `ConversionUploadLagHigh`
(see [advertising-alerts](./advertising-alerts.md)).

A surge in failed conversion event uploads to ad platforms (Google Ads / Meta)
exceeding the 5% error threshold, or events exhausting their retry budget and
landing in the dead-letter queue (DLQ).

## Background & Lifecycle

`UploadPendingConversions` (`crates/contexts/advertising/src/application/tracking/upload.rs`)
executes periodically on the worker (configured by `advertising_conversion_upload_interval_secs = 60`):
1. **Consent re-check**: Before each upload attempt, it reconstructs the subject
   from `SubjectKeyRef` and resolves `ProcessingPurpose::AdsMeasurement`. If consent
   was withdrawn post-capture, the event transitions to `UploadState::Failed` with
   reason `"suppressed_late"` (incrementing `conversion_suppressed_total`).
2. **Upload attempt**: If allowed, it invokes `ConversionSinkPort::upload` for each
   configured platform.
3. **Parked after 5 attempts**: Transient failures increment `attempt_count`. After
   `MAX_ATTEMPTS = 5`, the event transitions to `UploadState::Parked { reason, attempts: 5 }`
   and the `conversion_dlq_depth` gauge increments.

## 1. Assess DLQ and failure patterns

Check the depth and recent failure causes:

```sql
-- Count parked vs failing per-platform states (upload_states is a
-- platform-keyed jsonb map, e.g. {"google_ads": {"status": "parked", ...}})
SELECT s.key AS platform,
       s.value->>'status' AS status,
       s.value->>'reason' AS reason,
       count(*) AS event_count,
       min(occurred_at) AS oldest_event,
       max(occurred_at) AS newest_event
FROM advertising.conversion_events, jsonb_each(upload_states) s
WHERE s.value->>'status' IN ('failed', 'parked')
  AND occurred_at > now() - interval '24 hours'
GROUP BY 1, 2, 3
ORDER BY event_count DESC;
```

Inspect specific parked conversion records:

```sql
SELECT id, tenant_id, event_id, name, occurred_at,
       value_minor, currency, upload_states, created_at
FROM advertising.conversion_events
WHERE EXISTS (
  SELECT 1 FROM jsonb_each(upload_states) s WHERE s.value->>'status' = 'parked'
)
ORDER BY occurred_at DESC
LIMIT 20;
```

## 2. Inspect diagnostics for failing conversions

Call the diagnostics endpoint to get the structured lifecycle story:

```bash
curl -s -H "Authorization: Bearer $TENANT_TOKEN" \
  https://api.sanvi.internal/api/v1/tenant/ads/conversions/<conversion_id>/diagnostics | jq .
```

The returned `ConversionDiagnostics` model highlights:
- `consent`: Consent snapshot captured with the conversion.
- `identifiers`: Presence of hashed emails, phones, or device IDs.
- `upload_states`: Map of platform key to upload state, attempt counts, and specific error messages.

If failures are caused by upstream service degradation, follow
[advertising platform outage](./advertising-platform-outage.md). If caused by invalid
credentials or token revocation, follow [token revoked](./advertising-token-revoked.md).

## 3. Manual retry for parked events

Once the underlying issue (network, rate limit, credentials) is resolved, replay
parked events via the retry endpoint:

```bash
curl -X POST -H "Authorization: Bearer $TENANT_TOKEN" \
  https://api.sanvi.internal/api/v1/tenant/ads/conversions/<conversion_id>/retry
```

`RetryConversion` (`crates/contexts/advertising/src/application/tracking/retry.rs`):
- Verifies the event currently has at least one platform in `Parked` state (non-parked
  events return 409).
- Re-verifies privacy directives against the reconstructed `SubjectKeyRef`. If
  consent was withdrawn, it refuses with `409 Conflict` (`"retry refused: consent for ads measurement was withdrawn"`).
- If the event was captured under an email-identified `Contact` subject with no durable
  key material (preserving TASK-014's no-raw-PII invariant), retry refuses rather than
  re-deriving against the wrong subject.
- On success, flips `UploadState::Parked` back to `UploadState::Pending` with an
  incremented attempt counter, making it eligible for the next upload sweep.

## 4. Verify

1. `conversion_dlq_depth` gauge decrements towards 0.
2. `increase(conversion_upload_total{result="uploaded"}[10m]) > 0` shows successful ingestion.
3. `conversion_upload_lag_seconds` returns within baseline (< 60s).
