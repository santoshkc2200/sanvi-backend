# TASK-002: 09.1 Provider-agnostic domain, ports & catalog

**Phase:** 09
**Status:** done
**Requirement(s):** FR-902, NFR-906, NFR-907
**Depends on:** TASK-001
**Created:** 2026-08-20

**Slice:** 09.1 — Provider-agnostic domain & catalog
**Prerelease:** `v0.10.0-alpha.2` · **Flag:** `payments.enabled`

## Context

Model payments without Stripe in the room. The domain, the ports and the provider catalog are built
first and proven against a **fake provider** adapter, so Stripe becomes one implementation of an
interface that already exists rather than an interface discovered by extracting Stripe.

The fake provider is not throwaway test scaffolding. It is the phase acceptance criterion "adding a
second provider requires only a new adapter", and it is what lets the frontend build the 09.2–09.6
screens before anyone needs sandbox credentials.

## What to do

- [ ] **Domain** (`src/domain/`) — the aggregates from the parent plan: `PaymentProviderConnection`,
      `Checkout`, `PaymentRecord`, `RefundRecord`, and the value objects `ProviderKind`,
      `ConnectionStatus`, `CapabilitySet`, `RequirementSummary`, `Money`, `CountryCode`, `Currency`,
      `MerchantOrderRef`. Invariants live here: a connection cannot be `Active` without an active
      card-payments capability; `Money` arithmetic is currency-checked and minor-unit only.
- [ ] **Status state machine** — `Pending → Onboarding → {Active, Restricted, Rejected} →
      Disconnected` as an explicit transition function returning an error for illegal transitions.
      TASK-004 drives it from webhooks; defining it here means the driving code has no choices to
      make.
- [ ] **Ports** (`src/domain/ports.rs`, matching the `domains` context convention) —
      `PaymentProviderPort` (create account, account session, fetch status, create checkout, refund,
      sync), `PayoutReadPort`, `WebhookVerifierPort`, plus the repository ports. Ports speak domain
      types only: no `stripe::` type may appear in a port signature, enforced by
      `scripts/check-layering.sh`.
- [ ] **Fake provider adapter** (`src/adapters/fake_provider.rs`) — in-memory, deterministic,
      scriptable: it can be told to return `Restricted`, to require specific requirements, to fail a
      refund, to emit an out-of-order status. Compiled into non-production builds and used by tests.
- [ ] **Provider registry** — a `ProviderKind → Arc<dyn PaymentProviderPort>` map assembled at
      startup; the catalog endpoint reads from it. Entitlement is checked once, at the boundary.
- [ ] **Catalog endpoint, populated** — `GET /api/v1/tenant/payments/providers` now returns entries
      derived from the registry, never a hardcoded list in the handler:
      ```jsonc
      { "providers": [
        { "kind": "stripe_connect", "available": true,  "display_name": "Stripe",
          "supported_countries": ["US","JP","GB","DE"], "requires_onboarding": true },
        { "kind": "fake", "available": false, "display_name": "Test provider",
          "supported_countries": ["US","JP"], "requires_onboarding": true }
      ]}
      ```
      `fake` is `available: true` only in non-production environments.
- [ ] **Events** — declare `ProviderConnectionStarted`, `…Activated`, `…Restricted`,
      `ProviderDisconnected`, `CheckoutCreated`, `PaymentSucceeded`, `PaymentFailed`, `RefundIssued`,
      `DisputeOpened`, `PayoutPaid`, and wire them to the phase-01 outbox now even though most are
      not emitted until later slices. Payloads carry no PII and no card data.

## Acceptance criteria

- [ ] Domain and ports merged with no Stripe dependency anywhere below the adapters layer —
      `just layering` proves it.
- [ ] The state machine has a table test covering every legal transition and rejecting every illegal
      one.
- [ ] `Money` rejects cross-currency arithmetic, handles JPY minor units, and has defined overflow
      behaviour.
- [ ] The fake provider passes a full connect → active → checkout → refund script.
- [ ] Adding a provider to the registry in a test makes it appear in the catalog response; a
      production-config build contains no `fake` entry.
- [ ] The outbox emits `ProviderConnectionStarted` end to end for the fake provider.

## Verification

```bash
just fmt
just clippy
just layering
just connect-invariants
cargo test -p sanvi-payments
just test
just openapi
```

## Out of scope

Any Stripe call or Stripe adapter (TASK-003), webhook transport (TASK-004), and persistence of
anything beyond the connection aggregate.

## Files likely touched

- `crates/contexts/payments/src/domain/{connection,provider,money,ports,events,error}.rs`
- `crates/contexts/payments/src/application/catalog.rs`
- `crates/contexts/payments/src/adapters/{fake_provider,http,repos,events}.rs`
- `api/payments.yaml` (catalog response schema)
- `scripts/check-layering.sh` (payments entry)

## Notes / gotchas

- The catalog must derive from the registry. A hardcoded list passes tests today and lies to the UI
  the day a provider is added — that is the whole point of the slice.
- Compile the fake provider behind a non-production feature so it cannot ship enabled.
- No external side effects exist yet, so rollback is flag-off with nothing to unwind.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-002 done --note "<PR or commit>"`
