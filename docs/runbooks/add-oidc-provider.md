# Runbook: adding an OIDC provider

Adding a sign-in provider is **configuration + a secret** — no code changes.
This runbook proves that claim end to end and lists every step.

## 1. Add the provider to `configs/ory/kratos.yml`

Under `selfservice.methods.oidc.config.providers`, add an entry:

```yaml
- id: github
  provider: github
  client_id: ${GITHUB_CLIENT_ID}
  client_secret: ${GITHUB_CLIENT_SECRET}
  scope:
    - user:email
  mapper_url: file:///etc/config/kratos/oidc.github.jsonnet
```

### The mapper

Create `configs/ory/oidc.github.jsonnet` (copy `oidc.google.jsonnet` as a
template). The mapper must produce:

```jsonnet
{
  identity: {
    traits: {
      email: ...,       // required — the schema's only required trait
      name: { first: ..., last: ... },
      locale: ...,      // optional
    },
    verifiable_addresses: [
      // verified: true ONLY when the provider actually verified the email
      // (see the trust list below — auto-linking depends on it)
      { value: ..., via: "email", verified: <bool>, status: <"completed"|"pending"> }
    ],
  },
}
```

**Never** mark `verified: true` unless the provider guarantees email
verification (Microsoft does; a random OIDC provider may not).

## 2. Trust (or don't) the provider's email verification

`configs/base.toml` → `[identity].trusted_email_verified_providers`. Only
providers on this list can **auto-link** their account to an existing
verified account. Everything else still works — users can link after a
proof-of-ownership challenge (`POST /api/v1/auth/link/challenge`).

## 3. Provide the secrets

Kratos v26 does **not** substitute `${VAR}` inside the config file. Secrets
reach the container through configx **environment overrides** (underscores
separate nesting, arrays are indexed):

```bash
# docker-compose already passes these through from your shell/.env:
GOOGLE_CLIENT_ID=...           # → SELFSERVICE_METHODS_OIDC_CONFIG_PROVIDERS_0_CLIENT_ID
GOOGLE_CLIENT_SECRET=...
MICROSOFT_CLIENT_ID=...        # → ...PROVIDERS_1_CLIENT_ID
```

For a newly added provider, add the matching environment keys to the
`kratos` service in `docker-compose.yml` (or set them in the deployment's
secret manager).

## 4. Restart Kratos

```bash
./scripts/dev.sh down && ./scripts/dev.sh up --full
```

## 5. Verify

1. `GET http://localhost:4433/self-service/registration/api` — the flow's
   UI now includes a node for the new provider.
2. Register with the provider; the identity lands in Kratos and the local
   projection appears in `identity.users` (the registration webhook).
3. Sign in with an existing account that has the same verified email and a
   provider on the trust list → `identity.linked_accounts` gains a row with
   `linked_via = auto_verified`.
4. Sign in with a provider **not** on the trust list → no link row is
   created and `identity.account_link_rejected` appears in the outbox.

## What must never change without a code review

- `trusted_email_verified_providers` (it is the auto-link guard rail).
- The webhook `can_interrupt` flags in `kratos.yml` (they trade flow
  availability against projection guarantees).
- The webhook JSONnet templates (the API parses them defensively, but the
  trait shape is the contract).
