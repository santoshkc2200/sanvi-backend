# Signing and webhook rotation

Owner: platform engineering. Rehearse: `just rotate-credentials --rehearse`.

The CLI calls the actual tenant-header, tracking-site, OAuth-state, preview,
billing and Connect signature verifiers. Each proof first accepts old material,
then accepts new material and rejects the old signature under the new key.
No provider endpoint is called. Timings in `benchmarks/dr/rotation-local.json`
cover local verification only; live overlap and revocation are parked.

1. Record start time and issue distinct new keys for each trust boundary.
2. Tenant headers: add K2 to the verification set; set K2 active for minting;
   deploy all minting/verifying processes; wait out the configured skew window;
   remove K1. The rehearsal also verifies the K1/K2 overlap.
3. Tracking: replace the signing secret and regenerate/publish site keys in
   storefronts. OAuth state: replace the secret; outstanding OAuth redirects
   must restart. Preview: replace the token secret; outstanding previews must
   be reissued. Drain/restart API and workers so cached old configuration exits.
4. Billing and Connect: update each endpoint's **distinct** webhook secret in
   its own environment setting and deploy receivers. Revoke the old secret at
   the provider after the planned overlap; exercise each receiver with a new
   signed delivery and an old signed delivery. Preserve idempotency records.
5. Confirm a freshly minted value works and a still-unexpired old value fails
   for every surface. Record finish time after overlap/propagation, never just
   command time. A missing nonce or an expired token is not signature proof.

Kratos shared-webhook and session keys require Kratos-side configuration and
session invalidation; provider operations are documented in `providers.md`.
Domains internal shared-secret deployment also requires sender/receiver rollout.
