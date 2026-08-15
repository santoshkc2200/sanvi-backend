# ADR 0001 — Modular monolith with bounded contexts

- **Status:** accepted
- **Date:** 2026-08-15
- **Deciders:** platform engineering

## Context

Sanvi is a multi-tenant SaaS platform with a long roadmap of bounded domains:
tenancy, identity, billing, payments, privacy, localization, theming,
domains, advertising, notifications, audit. We must ship fast early without
foreclosing independent scaling and extraction later.

## Decision

One Rust workspace, two deployable binaries (`apps/api`, `apps/worker`),
composed from independent **context crates** under `crates/contexts/`. Each
context owns its domain model, its application services, its adapters, and
its **own Postgres schema**. Contexts communicate only through:

- **Synchronous ports**: the caller's application layer defines the trait; the
  composition root wires an adapter that calls the other context's
  application service. No context imports another context's domain types.
- **Asynchronous events**: a transactional outbox (`platform.outbox`) written
  in the same transaction as the state change, relayed by the worker to
  Redis Streams, consumed idempotently by consumer groups.

Forbidden: cross-schema SQL joins, shared tables, direct repository reuse
across contexts.

## Consequences

- **Positive:** extraction becomes an adapter swap; independent schema
  evolution; clear ownership; CI can enforce the rules mechanically
  (`scripts/check-layering.sh`, per-context migration lint).
- **Negative:** an extra layer of discipline (ports, outbox) even for trivial
  features; eventual consistency across contexts must be designed into
  consumers from day one.
