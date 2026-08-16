# Grant an Entitlement Override

Operator procedure for granting a feature to one tenant (phase 03). The
gate flips for that tenant only, within the entitlement cache TTL
(≤ 300 s), with an audit entry.

## Grant via CLI (operator session)

```bash
cargo run -p sanvi-cli -- entitlement grant \
  --tenant <slug> \
  --feature advertising.google_ads \
  --reason "sales trial until 2026-09-01" \
  --expires-at 2026-09-01T00:00:00Z
```

Optional flags: `--enabled false` (deny), `--limit <n>` (quota features
only). Listing and revoking:

```bash
cargo run -p sanvi-cli -- entitlement list --tenant <slug>
cargo run -p sanvi-cli -- entitlement revoke --tenant <slug> --feature advertising.google_ads
```

## Grant via API (fresh MFA on revoke)

`POST /api/v1/platform/tenants/{id}/entitlements` requires
`platform.entitlement.write`. `DELETE .../{feature}` additionally requires
fresh AAL2.

## Four-eyes

When `access.entitlement_approval_threshold` is set, overrides whose
`limit` exceeds it are **not applied immediately**: the API answers
`202 approval_required` with an approval id. A *different* platform admin
must approve (never the requester):

```bash
cargo run -p sanvi-cli -- approval list
cargo run -p sanvi-cli -- approval approve --id <approval-id>
cargo run -p sanvi-cli -- approval reject  --id <approval-id>
```

Approving executes the original grant (recorded as the requester, decided
by the approver); rejecting discards it. Both identities end up in the
audit chain.

## Resolution semantics

Precedence: `override > subscription (phase 04) > plan default (04) >
feature default`. Expired rows are ignored at resolution time and swept
by the worker (event `access.entitlement_expired`, hourly). Quota
consumption is atomic in Postgres and never trusts the cache.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Gate still off after granting | Entitlement cache staleness | Wait ≤ TTL (300 s) or `SANVI__ACCESS__ENTITLEMENT_CACHE_TTL_SECS`; check the worker consumed `access.entitlement_granted` (`sanvi-worker` logs) |
| `feature <key> not found` | Feature not in the catalog | `cargo run -p sanvi-cli -- feature list` / `feature define` |
| 202 approval_required | Four-eyes policy | Get a second admin to approve (see above) |
| Boolean feature with `--limit` | Kinds conflict | Booleans cannot carry limits; use a quota feature |

The feature catalog is seeded by `cargo run -p sanvi-cli -- seed features`
(idempotent) and extended at runtime via `GET/POST/PATCH/DELETE
/api/v1/platform/features` (`platform.feature.write`). Deprecation is
terminal and keeps existing entitlements resolvable.
