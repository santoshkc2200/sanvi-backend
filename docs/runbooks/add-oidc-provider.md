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
providers on this list can be recorded as `auto_verified` by the local
webhook projection after Kratos has linked their credentials. This list
does not make Kratos merge identities during sign-in.

The deployed self-hosted Kratos v26.2.0 links matching-email accounts
during sign-in after the user proves ownership with an existing credential.
The UI does not offer manual "Link Google" or "Link Microsoft" actions in
settings. Existing connections can still be removed when Kratos allows it.
Subsequent sign-ins use the linked provider directly.

Do not enable `account_linking_mode: automatic` expecting silent linkage
on this deployment: the pinned version only honors it in Ory Network.
See the [pinned provider configuration](https://github.com/ory/kratos/blob/v26.2.0/selfservice/strategy/oidc/provider_config.go)
and [Ory account-linking documentation](https://www.ory.com/docs/kratos/social-signin/link-multiple-provider-account).

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
3. Sign out and sign in with a different provider using the same email.
   Complete Kratos's existing-credential challenge in the login flow. For
   a verified email and a provider on the trust list,
   `identity.linked_accounts` gains a row with `linked_via = auto_verified`.
   Sign out again and confirm the new provider signs in directly, with the
   same Kratos identity and local user ID.
4. Sign in with a provider **not** on the trust list → no link row is
   created and `identity.account_link_rejected` appears in the outbox.

## What must never change without a code review

- `trusted_email_verified_providers` (it is the auto-link guard rail).
- The webhook `can_interrupt` flags in `kratos.yml` (they trade flow
  availability against projection guarantees).
- The webhook JSONnet templates (the API parses them defensively, but the
  trait shape is the contract).
