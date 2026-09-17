# GA checklist (phase 00-11 acceptance criteria)

Assembled by TASK-019 (FR-1104). One line per acceptance criterion from
every phase's implementation plan. Every criterion line carries a state:

`automated` — a CI gate or a repo command re-proves the line today; the
verify note names it.
`manual` — a documented procedure against a running stack re-proves it;
the verify note is that procedure.
`parked` — the line cannot be verified without people, a vendor, live
credentials, or traffic; `docs/release/needs-humans.md` says what unblocks
it.

There is no owner column: there is one owner. TASK-030 closes this file
and tags `v1.0.0`; `just ga-checklist-check` fails the build while any
criterion line has no state.

## 00 · Foundations Devsecops

- manual — `git clone && ./scripts/dev.sh up && curl :8080/healthz` works on a clean machine in < 10 min.
  - verify: clean clone; `./scripts/dev.sh up`; `./scripts/smoke.sh`
- automated — `cargo test --workspace` green; coverage reported; layering lint proven to fail on violation.
  - verify: `just ci-quiet`
- parked — CI runs all gates in < 12 min on cache hit; image signed and SBOM published.
  - verify: blocked on people/vendor/credentials/traffic — `docs/release/needs-humans.md`
- parked — Staging deploy is automated and reversible; rollback documented and rehearsed once.
  - verify: blocked on people/vendor/credentials/traffic — `docs/release/needs-humans.md`
- manual — `/metrics` scraped, traces visible in OpenObserve for a sample request.
  - verify: `./scripts/dev.sh up --full`; request a sample route; confirm `/metrics` and OpenObserve traces
- parked — ADRs 0001–0003 merged; README quickstart validated by someone who did not write it.
  - verify: blocked on people/vendor/credentials/traffic — `docs/release/needs-humans.md`

## 01 · Tenancy Core

- automated — A request to `acme.platform.sanvi.app` resolves tenant `acme` and every query in that request is automatically restricted to it.
  - verify: `scripts/test.sh -p sanvi-tenancy`
- manual — Removing the `SET LOCAL` line makes integration tests fail loudly (verified by mutation).
  - verify: remove the `SET LOCAL` line in `TxManager::begin_for`, rerun tenancy integration tests, confirm loud failure, revert
- automated — Tenant lifecycle transitions emit events that reach a consumer through the outbox.
  - verify: `scripts/test.sh -p sanvi-tenancy` (outbox round-trip)
- automated — Suspended tenant returns 423 on tenant routes and 200 on the billing allow-list.
  - verify: `scripts/test.sh -p sanvi-http` + tenancy middleware tests
- manual — p99 added latency from resolution + RLS ≤ 5 ms under the 1k-tenant load test.
  - verify: seed scale data (`SANVI__DATABASE__NAME=sanvi_scale just seed-scale`), run `just perf <tag> storefront_read`, check p95/p99 in the artifact

## 02 · Identity Access

- automated — Sign in with Google, Microsoft, and email OTP; all three land on the same user when the email matches and both sides are verified, and are refused (with challenge) when they do not.
  - verify: `scripts/test.sh -p sanvi-identity` (provider link tests)
- automated — Adding a third provider requires only config + secret — demonstrated with a test provider.
  - verify: `scripts/test.sh -p sanvi-identity` (test-provider demonstration)
- automated — Every non-public route rejects unauthenticated requests and requires a declared permission (CI test enumerates the router).
  - verify: `scripts/test.sh -p sanvi-api` (permission_catalog)
- automated — Revoking a role removes access within 30 s without a restart.
  - verify: `scripts/test.sh -p sanvi-access` (cache TTL tests)
- automated — Kratos webhook failure prevents orphan identities (verified by fault injection).
  - verify: `scripts/test.sh -p sanvi-identity` (webhook failure injection)

## 03 · Platform Admin Entitlements

- automated — An operator can grant `advertising.google_ads` to one tenant and the gate flips for that tenant only, within 5 s, with an audit entry.
  - verify: `scripts/test.sh -p sanvi-platform-admin`
- automated — A tenant admin can create a custom role, assign it, and cannot escalate beyond their own grants.
  - verify: `scripts/test.sh -p sanvi-access`
- automated — Impersonation produces a session that is time-boxed, read-only, banner-flagged, and audited.
  - verify: `scripts/test.sh -p sanvi-platform-admin`
- automated — Every optional capability in the codebase is declared in the feature catalog (CI test).
  - verify: `scripts/test.sh -p sanvi-access` (feature catalog CI test)
- automated — Audit log is provably append-only and chain-verified.
  - verify: `just audit-verify` + append-only trigger tests

## 04 · Platform Billing

- automated — A tenant subscribes via Checkout and the matching entitlements are live within 10 s of the webhook, not of the redirect.
  - verify: `scripts/test.sh -p sanvi-billing` (stripe-mock webhook -> entitlement)
- manual — Upgrade/downgrade through the Customer Portal changes entitlements correctly, with proration handled by Stripe.
  - verify: Stripe sandbox: open the portal, change the plan, confirm proration and entitlement change
- automated — A failed payment moves the tenant through dunning and, after grace, to the limited entitlement set; recovery restores it.
  - verify: `scripts/test.sh -p sanvi-billing` (dunning ladder)
- automated — Replaying a month of webhook events changes nothing (idempotent).
  - verify: `scripts/test.sh -p sanvi-billing` (webhook replay idempotence)
- automated — `automatic_tax` is only enabled where an active registration exists, and the ops check proves it.
  - verify: `just billing-tax-check` + billing tests
- automated — No Stripe secret exists outside the secret manager (scan is green).
  - verify: gitleaks scanner (pr.yml blocking job)

## 05 · Privacy Compliance

- automated — A user submits an export request and receives a complete, verified package covering every context, within the SLA, with an audit trail.
  - verify: `scripts/test.sh -p sanvi-privacy`
- automated — An erasure request renders the subject's personal data unrecoverable in live systems, retains only what a documented legal basis requires, and states exactly what was retained and why.
  - verify: `scripts/test.sh -p sanvi-privacy` (erasure + retained list)
- automated — Adding a new context without a `PersonalDataProvider` fails CI.
  - verify: `scripts/test.sh -p sanvi-api` (privacy_providers::every_context_crate_registers)
- automated — Consent withdrawal demonstrably stops the corresponding downstream processing.
  - verify: `scripts/test.sh -p sanvi-privacy`
- automated — The same code path serves an EU subject (nothing processed until opt-in) and a California subject (notice served, processing allowed until opt-out), with only the jurisdiction profile differing.
  - verify: `scripts/test.sh -p sanvi-privacy` (jurisdiction matrix)
- automated — A `Sec-GPC: 1` request denies sale/share and targeted advertising before login and stays denied after the subject authenticates.
  - verify: `scripts/test.sh -p sanvi-privacy` (GPC tests)
- parked — A US opt-out is honoured without identity verification and propagates to ad platforms within the profile's window.
  - verify: blocked on people/vendor/credentials/traffic — `docs/release/needs-humans.md`
- automated — A refused US request produces an appeal route, a second-operator decision within the profile's window, and the relevant authority's complaint address.
  - verify: `scripts/test.sh -p sanvi-privacy` (appeal route tests)
- automated — Retention dry-run for one tenant matches the actual run row-for-row, and the published notice's retention table is generated from the same rules.
  - verify: `scripts/test.sh -p sanvi-privacy` (dry-run vs run equality)
- automated — Art. 30 record, risk assessments, and sub-processor list (with role and transfer mechanism) are generated from live configuration, not hand-written.
  - verify: `scripts/test.sh -p sanvi-privacy` (generated records tests)
- automated — A simulated breach across three jurisdictions produces the correct distinct deadlines and audiences without manual research.
  - verify: `scripts/test.sh -p sanvi-privacy` (breach deadline matrix)
- automated — Adding a new jurisdiction profile requires no code change and fails CI until its expectations are added to the matrix test.
  - verify: `scripts/test.sh -p sanvi-privacy` (matrix test failure on new profile)

## 06 · I18N L10N

- automated — Every backend-emitted string a user can see is in the catalog — no string literals in handlers (lint enforced).
  - verify: `just i18n`
- automated — Switching a tenant to `ja` changes emails, errors, invoices, and notices, including from the worker.
  - verify: `scripts/test.sh -p sanvi-localization` (worker render tests)
- automated — A JPY plan bills, renders, and exports with no decimal artefacts.
  - verify: `scripts/test.sh -p sanvi-payments` (JPY money tests)
- automated — CI fails when a new key lacks a `ja` translation.
  - verify: `just i18n`
- manual — A Japanese invoice PDF renders with correct glyphs and a Japanese-formatted address.
  - verify: render the ja invoice, visually confirm glyphs and address format

## 07 · Theming

- manual — A tenant switches theme, overrides brand color/font/logo, previews it, publishes it, and the live storefront reflects it within one request.
  - verify: admin: edit draft -> preview -> publish; request `/api/v1/public/theme` and the storefront, confirm one-request propagation
- automated — Rollback restores the previous look exactly, including assets.
  - verify: `scripts/test.sh -p sanvi-theming` (rollback snapshot tests)
- automated — Publishing a theme whose semantic pairs fail WCAG AA is refused by default.
  - verify: `scripts/test.sh -p sanvi-theming` (contrast validation)
- manual — A new theme is installed by publishing an artifact — no application deploy (demonstrated).
  - verify: `cargo run -p sanvi-cli -- theme publish <artifact dir>`; confirm the new theme resolves without redeploy
- automated — A premium theme is invisible to tenants without the entitlement.
  - verify: `scripts/test.sh -p sanvi-theming` (entitlement gate tests)
- automated — No path allows theme-supplied script execution (verified by the security test suite).
  - verify: `scripts/test.sh -p sanvi-theming` (security test suite)

## 08 · Domains Tls

- manual — Connecting an externally-registered domain succeeds end to end with copy-pasteable records and a working `https://` within minutes of DNS propagation.
  - verify: sandbox registrar: connect an external domain, set the DNS records shown, verify + issue, open https://
- manual — Purchasing a domain results in a live, TLS-secured storefront with no human step.
  - verify: sandbox: purchase a domain through the API; confirm DNS+cert without human steps
- automated — `/internal/tls/authorize` refuses every hostname we do not serve (matrix test green).
  - verify: `scripts/test.sh -p sanvi-domains` (tls authorize matrix)
- manual — Certificates renew automatically; a simulated renewal failure pages before expiry.
  - verify: force a renewal failure in the sandbox; confirm the expiry warning fires at the configured thresholds
- automated — A domain removed from a tenant stops resolving and its cert is no longer served.
  - verify: `scripts/test.sh -p sanvi-domains` (removal tests)
- automated — Health monitor detects an intentional DNS change and notifies the tenant with the fix.
  - verify: `scripts/test.sh -p sanvi-domains` (health monitor tests)

## 09 · Tenant Payments

- manual — A tenant connects Stripe through embedded onboarding and reaches `Active` with `configuration.merchant.capabilities.card_payments.status == 'active'`.
  - verify: Stripe sandbox test keys: run embedded onboarding to Active with card_payments active
- manual — A customer pays on the tenant's own domain and the funds land in the **tenant's** Stripe balance — verified in the sandbox, with no platform balance movement.
  - verify: sandbox: pay on the tenant checkout; confirm funds land on the connected account, not the platform
- manual — The notification banner renders in the tenant dashboard and surfaces new requirements.
  - verify: create test requirements on the connection; confirm the dashboard banner surfaces them
- automated — Refunds work, are idempotent, and are correct in JPY and USD.
  - verify: `scripts/test.sh -p sanvi-payments` (refund idempotence, JPY/USD)
- automated — No deprecated Connect pattern exists in the codebase (grep gate for `type: 'express'|'custom'|'standard'`, `charges_enabled`, `payouts_enabled`, `payment_method_types`).
  - verify: `just connect-invariants`
- automated — Adding a second provider requires only a new adapter (proven by a fake provider in tests).
  - verify: `scripts/test.sh -p sanvi-payments` (fake provider)

## 10 · Advertising

- parked — A tenant connects Google Ads and Meta, creates a campaign in Sanvi, and it appears live on the platform with matching budget and schedule.
  - verify: blocked on people/vendor/credentials/traffic — `docs/release/needs-humans.md`
- parked — A storefront purchase produces exactly one conversion on each connected platform, valued from the server-side payment record.
  - verify: blocked on people/vendor/credentials/traffic — `docs/release/needs-humans.md`
- automated — With `ads_measurement` withdrawn, or a US opt-out of sale/share including one expressed only via GPC, no conversion is uploaded and the suppression is visible in diagnostics with the purpose and the signal source that caused it.
  - verify: `scripts/test.sh -p sanvi-advertising` (directive gate tests)
- parked — The dashboard shows spend, clicks, conversions, revenue, and ROAS for a date range, matching the platform's own reporting within the documented restatement window.
  - verify: blocked on people/vendor/credentials/traffic — `docs/release/needs-humans.md`
- automated — A campaign paused natively is reported as drifted rather than overwritten.
  - verify: `scripts/test.sh -p sanvi-advertising` (drift tests)
- automated — An opted-out subject is absent from new audience lists and removed from existing ones on refresh.
  - verify: `scripts/test.sh -p sanvi-advertising` (audience opt-out tests)
- automated — Adding a third network requires only an adapter plus a capability matrix — proven with the fake.
  - verify: `scripts/test.sh -p sanvi-advertising` (capability matrix + fake)

## 11 · Hardening Ga

- automated — Every context crate's tests run in CI, enforced by a check that fails on an unrun target.
  - verify: `just ci-test-coverage` (+ self-test)
- manual — Every use case has a span and every route has a permission, both enforced by a CI gate that fails when a new one is added without it.
  - verify: run the TASK-020 span-coverage gate once slice 11.1 lands (permission half is already CI-enforced)
- manual — The log PII scanner blocks with zero findings on a corpus containing every PII shape held.
  - verify: run the TASK-020 log-PII scanner against its corpus once slice 11.1 lands
- manual — Every dependency-matrix row has an injection test asserting the documented behaviour.
  - verify: run the TASK-023 injection suite once slice 11.4 lands
- manual — No unbounded outbound call remains; the timeout gate proves it.
  - verify: run the TASK-023 timeout gate once slice 11.4 lands
- manual — Cross-tenant isolation holds across the RLS matrix (child partitions by name) and the IDOR sweep.
  - verify: run the TASK-024 RLS matrix (child partitions by name) and IDOR sweep once slice 11.5 lands
- manual — Load at 3× the harness's projected peak holds every latency budget on the pinned profile.
  - verify: TASK-021: `just perf` at 3x peak on the pinned profile; `just perf-compare` against the stored baseline
- manual — A full restore, a PITR, and a single-tenant restore each complete in a timed drill with the duration committed; a neighbour tenant's rows are byte-identical before and after.
  - verify: TASK-025: rehearse the timed restore, PITR and single-tenant restore drills; commit the durations
- manual — Every credential rotates end to end, and every old credential is proven rejected.
  - verify: TASK-025: run the per-credential rotation drill; confirm every old credential is rejected
- manual — `/api/v1` is frozen, rate-limited, quota-enforced, and green on the replay suite.
  - verify: TASK-026: run the freeze diff gate, rate-limit and quota checks, and the replay suite once slice 11.7 lands
- manual — Rolling restart and revert under load drop zero requests.
  - verify: TASK-023: rolling restart and revert under load; confirm zero dropped requests
- manual — Offboarding exports, purges, and passes a residual scan; the purge report separates removed from lawfully retained.
  - verify: TASK-028: run offboarding export-then-purge with the residual scan; check the purge report
- manual — Evidence packs generate deterministically from live configuration and name their build.
  - verify: TASK-029: generate an evidence pack twice; confirm byte-identical output naming the build
- manual — `docs/release/ga-checklist.md` is closed and `v1.0.0` is tagged.
  - verify: close every line above, then tag v1.0.0 (TASK-030; needs frontend TASK-030 too)
