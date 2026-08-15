# ADR 0003 — OpenAPI contract-first

- **Status:** accepted
- **Date:** 2026-08-15
- **Deciders:** platform engineering

## Context

Backend and frontend are developed in parallel against a shared HTTP
contract. Hand-written specs drift from implementations; code-first specs
drift from what reviewers approved. Either drift becomes a frontend bug.

## Decision

The OpenAPI document is **generated from code annotations** (`utoipa`) and
split per bounded context into committed `api/{context}.yaml` files. CI
generates the document and diffs it against the committed specs
(`cargo run -p sanvi-cli -- openapi --check`), failing on any drift. The
frontend generates its typed client from these committed files. Contract
changes are therefore reviewable diffs in the PR, and the committed spec is
always the truth.

## Consequences

- **Positive:** no hand-maintained spec; drift is a CI failure, not a
  support ticket; contract-first sequencing of backend/frontend phases works.
- **Negative:** annotations must be maintained alongside code (mechanical but
  mandatory); splitting logic (by tag) imposes a naming convention: each
  context tags every one of its paths with its context name.
