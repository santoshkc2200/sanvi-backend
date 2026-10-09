# Database and Redis credential rotation

Owner: platform engineering. Local rehearsal: `just rotate-credentials --rehearse`.
Evidence: `benchmarks/dr/rotation-local.json`. Random drill secrets and disposable
containers never modify the compose stack or deployment credentials.

1. Create a replacement secret in the deployment secret store. Record incident
   start time, including deployment/propagation time in the final duration.
2. Postgres: under the administrator, `ALTER ROLE sanvi_app PASSWORD '<new>'`.
   Update `SANVI__DATABASE__PASSWORD` for API and worker; drain/restart both.
   Changing the password does not revoke existing authenticated connections.
3. Redis: replace the password/ACL on the service and the credential in the
   configured Redis URL. Drain/restart API and worker connections. With ACLs,
   revoke the retired user's password and kill that user's existing clients.
4. Open fresh connections using the replacement credentials and perform a DB
   query / Redis `PING`. Fresh connections using the retired credentials must
   fail authentication (Postgres SQLSTATE `28P01`; Redis authentication error).
   A timeout or unreachable service does **not** prove rejection.
5. Verify `/api/v1/system/ready`, resume workers, record finish time. Keep only
   secret-free evidence. If deployment fails, restore the prior secret only
   when it is not compromised; otherwise roll forward with another new secret.

The CLI includes service startup and fresh-connection proofs in the local
duration. It does not measure a production rollout or revoke existing sessions.
