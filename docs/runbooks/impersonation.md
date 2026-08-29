# Impersonation Runbook

Operator procedure for the support impersonation feature (phase 03):
grants are time-boxed (≤ 30 min), read-only by default, banner-flagged,
and fully audited.

## Start an impersonation grant

Impersonation requires `platform.impersonate` **and a fresh AAL2 login**
(within `platform_admin.fresh_aal2_max_age_secs`, default 300 s). The
`sanvi-cli` cannot mint grants (it has no MFA); use the API from the
platform admin console:

```bash
curl -X POST https://admin.sanvi.app/api/v1/platform/impersonations \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "tenant_id": "<tenant-snowflake-id>",
    "target_user_id": "<member-snowflake-id>",
    "reason": "support ticket #1234",
    "mode": "read_only",
    "duration_minutes": 15
  }'
```

The response carries the grant id. The target user **must be an active
member** of the tenant; the tenant must not have a privacy lock (phase 05).
Read-only is the default and the only mode support staff should use.

## Use the grant (exchange for a scoped session)

Send every subsequent tenant-scoped request with the header:

```bash
curl -H "x-sanvi-impersonate: <grant-id>" \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://acme.sanvi.app/api/v1/tenant/context
```

- The permission checks and handlers see the **target user**.
- Responses carry `x-sanvi-impersonation: <grant-id>:read_only` — the UI
  must banner the session on this header.
- Read-only grants reject unsafe methods with 403.
- Expired or revoked grants reject every request with 403.

## Revoke (immediate)

Revocation is checked per request — the moment it is written, the next
exchange fails:

```bash
curl -X DELETE https://admin.sanvi.app/api/v1/platform/impersonations/<grant-id> \
  -H "Authorization: Bearer $ADMIN_TOKEN"
```

CLI equivalent: `cargo run -p sanvi-cli -- impersonation revoke --id <grant-id>`
and `cargo run -p sanvi-cli -- impersonation list [--tenant <slug>]`.

## Monitoring

- Metric: `impersonation_active_gauge` (alert if it rises without an active
  incident).
- Monthly review: `sanvi-cli audit list --action platform_admin.impersonation_started`
  — review frequency, reasons, and durations with the support team.

## Incident: impersonation cannot be exchanged

| Symptom | Cause | Fix |
|---|---|---|
| 403 "grant is invalid, expired or revoked" | Grant expired (≤ 30 min) or revoked | Create a new grant |
| 403 "read-only grant attempted a write" | `mode: read_only` | Use read-write grants only with explicit approval |
| 403 "target is not an active member" | Membership removed/suspended | Restore membership, then create a new grant |
| 403 "privacy lock" | Phase-05 jurisdiction lock | Never bypass; escalate to the privacy team |
| 503 storage errors | Postgres down | See `api-down.md` |

Every grant, exchange-relevant mutation and revocation is recorded in the
audit chain with the acting operator as `actor_id` and the target as
`on_behalf_of`.
