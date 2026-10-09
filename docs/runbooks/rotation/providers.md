# Provider and deployment credentials — parked live operations

Owner: platform engineering. Scope decision for TASK-025 (2026-10-09): complete
the local rehearsal and park live provider operations. These rows have no
measured duration and no claimed old-credential rejection. Track the execution
gate in `docs/release/needs-humans.md`; inventory is `docs/security/secrets-inventory.md`.

| Credential | Rotation procedure and required rejection proof |
| --- | --- |
| Stripe platform API key | Issue replacement, update billing config, restart clients, confirm a permitted request, revoke old key, assert old request fails authentication. |
| Stripe restricted Connect key | Issue replacement with the same least privileges, update payments config, restart, test permitted operation and denied scope, revoke old key, assert authentication failure. |
| Billing / Connect webhook provider secrets | Follow `signing-webhooks.md`; provider revocation and actual receiver delivery remain parked, although local signature verification is rehearsed. |
| Google Ads developer token / OAuth client secret | Issue replacement through the account's provider controls, update adapter configuration, reconnect/refresh test connection, revoke old credential, assert an authentication failure using old material. |
| Meta OAuth client secret | Issue replacement, update adapter config and restart, verify fresh authorization/refresh, revoke old secret, prove old material fails. |
| Ad refresh tokens | Refresh through the existing token-refresh use case; reconnect after provider revocation. Provider token invalidation must be proven at the provider. |
| Kratos webhook / admin / session signing | Update both sender and backend webhook secret, restart, assert old callback denied and new accepted. Rotate Kratos-owned session keys and invalidate old sessions at Kratos. No backend session signing key exists. |
| Keto | No backend-held shared credential in this deployment; inspect deployment authentication before provisioned rollout. |
| Media API key | Reissue at service, update config/restart, test permitted read/upload, revoke old key, assert unauthorized. |
| Notification send/read keys | Rotate each permission separately, update API/worker gateway config, smoke-test send/read, revoke each old key and prove unauthorized. |
| OTLP auth token | Reissue at OpenObserve, update emitter config/restart, prove new ingest accepted and old ingest unauthorized. |
| Domains internal secret | Update internal-job sender and receiver config/restart together, assert new authenticated call works and old returns unauthorized. |

For every row, record start/end timestamps, wall-clock duration including
overlap/propagation, environment and secret-free positive/negative results.
Do not accept service outage, authorization denial for an unrelated scope, or
an expired token as proof of retired credential invalidation. Ephemeral Connect
onboarding secrets and export passphrases are per-operation, not deployment keys.
