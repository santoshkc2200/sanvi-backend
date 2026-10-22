# Hardened deployment compatibility

The current controller supports **sanvi-hardened-v1 images and newer compatible images**. The first eligible release is the first verified immutable image containing `/app/deployment/image-contract` with that value, its migration policy and SQL, and the worker management listener on port 9091. Record its digest when it is built and verified. This repository does not establish that any older image has actually been deployed.

API and worker must use the same immutable image digest. An image without the contract or policy is rejected with an actionable error before migrations, nginx configuration changes, or serving-container changes. There is no legacy probe bypass. The controller also rejects a candidate missing any database migration version already applied to the live schema, including in maintenance mode. Manual workflow dispatch is a deployment of a compatible image; it never downgrades the schema or promises rollback to arbitrary historical binaries.

## First hardened rollout

1. Schedule an explicit maintenance window. Take a verified PostgreSQL snapshot/PITR restore point before upgrading. Rehearse its restore rather than assuming a backup is usable.
2. Preserve the matching legacy image digests, controller and Compose revision, proxy configuration, slot state, and secret-manager configuration. Keep the pre-upgrade database restore point with that release record.
3. Provision the private runtime/migrator environment files and ingress trust configuration. Use the same hardened image digest for API and worker, and deploy with `DEPLOYMENT_MODE=maintenance`. If a previous Compose network conflicts with the dedicated subnet/static ingress address, recreate that network during the maintenance window.
4. Verify API and worker readiness, metrics ingestion, and core tenant workflows before resuming normal operation. Record the verified first hardened image digest as the compatibility floor for subsequent rollback decisions.

## Rollback and recovery

An ordinary rollback selects a verified image at or above the hardened floor that includes every database migration already applied. Migration changes that are compatible with old binaries still require explicit version-compatibility proof. The controller never runs down migrations automatically. Maintenance mode permits classified offline migrations; it does not make an older binary compatible with a newer schema.

If the initial hardened upgrade must be reversed to a pre-hardening application, use an incident maintenance procedure: stop hardened writers, restore the selected pre-upgrade database snapshot/PITR state, restore the **matching** legacy controller/Compose/proxy state and image digests, and verify compatibility before returning traffic. Decide explicitly how to reconcile writes after the restore point. Do not force an old image through the hardened controller, suppress failed probes, or connect legacy binaries to an upgraded database.

Preserve `.deploy/maintenance.pending` after a failed offline migration; the controller requires an explicit maintenance recovery attempt. Retain the active-slot and interrupted-switch state until recovery is complete.

Host configuration, trust boundaries, and focused verification are recorded in [the operations report](../docs/superpowers/plans/2026-10-22-operations-report.md).

## Remediation rollout notes

Apply the three additive `20261022000{1,2,3}00` migrations before starting the
new binaries. Checkout keys created before request fingerprints existed fail
closed with 409 on mutation replay; existing hosted sessions and read endpoints
remain available. Refund intent snapshots protect new requests and recoverable
pending requests; previously unrecorded historical notes/IP cannot be recreated.
Drain or preserve queued audit snapshots before any deliberate schema reversal.

Required consumers depend on retained PostgreSQL events and inbox history for
recovery. Configure and scrape the worker's private management endpoint and
follow [event recovery](../docs/runbooks/event-recovery.md) before introducing
outbox/stream retention or manual archival.
