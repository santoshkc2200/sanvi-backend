# Phase 05 — Privacy & Compliance, GDPR/APPI/US state laws (backend)

**Target version:** 0.6.0
**Depends on:** phases 01, 02, 03, 04 (the data that must be exported/erased must exist first)
**Unlocks:** legally operating in the EU/UK, Japan, and the United States; consent and opt-out
signals used by phase 10

## Goal

A dedicated `privacy` bounded context that can answer, for any data subject: *what do we hold, why,
under what basis, for how long, and can you have it back or have it deleted?* — as an automated,
auditable workflow rather than a manual database session.

Sanvi is a **processor** for tenant end-user data and a **controller** for tenant account data. Both
roles are modelled explicitly because the obligations differ. Under US state law the same two roles
are named **service provider/contractor** and **business**; the mapping is recorded once, in the
jurisdiction profile, rather than duplicated per feature.

> Legal caveat: this plan implements the mechanisms. Which retention periods, legal bases,
> jurisdictions, applicability thresholds, and disclosures apply is a decision for counsel and is
> recorded as **configuration**, not code. In particular, no US state list is hardcoded — states are
> rows in `privacy.jurisdiction_profiles`, so a new state law is a data change plus a test, not a
> refactor.

## Jurisdictions in scope

| Regime | Territory | Model for non-essential processing | Primary obligations this phase implements |
|---|---|---|---|
| GDPR | EU/EEA | **Opt-in** before processing | DSR (access/erasure/rectification/restriction/portability/objection), Art. 30 records, DPIA, 72-hour breach notice, transfer safeguards |
| UK GDPR + DPA 2018 | UK | **Opt-in** | As GDPR; ICO as supervisory authority; UK IDTA/Addendum for transfers |
| APPI | Japan | Notice of **purpose of use**; opt-in for sensitive and for third-party provision | Purpose notice, third-party provision records, cross-border disclosure, PPC breach report |
| US state comprehensive laws (CCPA/CPRA + the VCDPA-family states) | US, per state | **Notice + opt-out** of sale/share, targeted advertising and profiling; **opt-in** for sensitive data in most non-CA states and for known minors | Notice at collection, opt-out incl. universal signals (GPC), limit use of sensitive PI, access/delete/correct/portability, appeals, non-discrimination, risk assessments, state breach matrix |
| US sectoral & health-adjacent (WA My Health My Data, NV SB370, COPPA) | US | Consent-first for consumer health data; under-13 blocked | Flagged data classes, tenant attestation, hard block rather than silent collection |

Two consequences drive the design:

1. **There is no single global consent model.** EU/UK/JP require permission before processing; US
   states require an honoured refusal after notice. The engine resolves a *directive per purpose per
   subject*, and the jurisdiction decides the **default** when the subject has said nothing.
2. **Sanvi does not decide the tenant's jurisdiction exposure — it measures it.** A tenant selling
   into California is subject to CCPA regardless of where it is incorporated, so jurisdiction is
   resolved from the *subject*, with the tenant's configuration only ever widening protection.

## Scope

**In**
- Data inventory (registry of personal-data fields per context) with legal basis, **sensitivity
  class**, and retention.
- Jurisdiction resolution engine + jurisdiction profiles as configuration.
- Data Subject Requests: access/export, erasure, rectification, restriction, portability, objection,
  **opt-out of sale/share/targeted advertising, opt-out of profiling, limit use of sensitive PI**,
  **authorized-agent submission**, and **appeal of a refusal**.
- Consent, opt-out and preference resolution (marketing, analytics, ads personalisation, sale/share)
  with proof of consent *and* proof that an opt-out was honoured.
- Universal opt-out signal handling (`Sec-GPC` / GPC), including pre-authentication and its later
  binding to an identified subject.
- Retention scheduler and deletion executor (incl. cascade across contexts and backups policy).
- Processing records (Art. 30), **risk assessments (DPIA / US state data-protection assessments)**,
  sub-processor registry with **service-provider contract metadata**, breach-notification workflow.
- Data residency enforcement using `tenancy.region` (`eu`, `jp`, `us`) and transfer-mechanism records.
- APPI specifics: notice of purpose of use, third-party provision records, cross-border transfer
  disclosure, request handling by the tenant as the "personal information handling business operator".
- US specifics: notice at collection, "Do Not Sell or Share" and "Limit the Use of My Sensitive
  Personal Information" directives, non-discrimination checks, minors' opt-in, per-state breach rules,
  annual request metrics.

**Out**
- Legal texts themselves (supplied by counsel; stored as versioned content).
- Frontend privacy centre UI (frontend phase 05).
- Sector regimes we do not knowingly process data for — HIPAA, GLBA, FERPA. The data map flags the
  classes that would trigger them so the gap is visible instead of assumed away.
- US data-broker registration and CPPA ADMT-specific opt-outs — tracked as post-GA work; the
  jurisdiction profile already carries the flags so enabling them is configuration.

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Erasure strategy | **Crypto-shredding for immutable stores + row deletion/anonymisation for mutable ones** | Event logs and backups cannot be surgically edited; destroying the per-subject key makes the payload unrecoverable |
| Per-subject encryption | Field-level envelope encryption: DEK per data subject, wrapped by a KEK in KMS | Makes shredding a single key destruction |
| Data map | Declarative, in code, next to the entity (`#[personal_data(category = "contact", sensitivity = "standard", basis = "contract", retention = "P7Y")]`) with a compile-time-collected registry | A registry that lives away from the code goes stale in a month; `sensitivity` is what drives the CA "limit use" right and the non-CA opt-in |
| Request handling | Asynchronous saga with per-context participants and explicit acknowledgement | Erasure spans 10+ contexts and external processors; a synchronous delete is a lie |
| Jurisdiction | Resolved per **subject** (declared residency > billing/shipping country > tenant default > IP country), stored with the evidence that produced it; unknown ⇒ **most protective** profile | Guessing wrong in the permissive direction is a violation; guessing wrong in the strict direction costs a banner |
| Consent vs opt-out | One `PurposeDirective` per purpose with a resolved state (`Allowed`/`Denied`) and a source (`Consent`, `OptOut`, `Signal`, `Default`); the jurisdiction profile supplies the **default** | This is the only difference between "opt-in EU" and "opt-out US" — encoding it as a default keeps one code path |
| Universal opt-out signals | `Sec-GPC: 1` (and any configured UOOM) is treated as a **valid, binding opt-out** of sale/share and targeted advertising for US subjects, applied before any identification, and persisted to the subject on login | Required in a growing list of states; honouring it everywhere is cheaper than a per-state matrix and never harms |
| Sensitive data | Separate `SensitivityClass` on every mapped field; sensitive processing is opt-in in EU/UK/JP and in most non-CA US states, and subject to "limit use" in CA | A single `sensitive` boolean is not enough to drive three different rules |
| Deadlines | GDPR/UK 1 month (extendable +2 with notice); APPI "without delay"; **US state 45 days (+45 with notice), acknowledgement within 10 business days, opt-out honoured within 15 business days, appeal decided within the profile's window** — internal SLA 14 days for everything | Alerting is driven by the internal SLA, not the legal max; the legal max only drives the "overdue" alarm |
| Appeals | A refused or partially refused US request can be appealed once; the appeal is a first-class aggregate with its own clock and its outcome must include the state AG complaint route | VCDPA-family states require it, and it is the obligation teams most often discover during an audit |
| Authorized agents | Agent submissions require the agent's identity, written authorisation evidence, **and** verification of the consumer themselves before disclosure | An agent flow without consumer verification is a data-exfiltration API |
| Consent storage | Append-only consent/directive ledger (notice version, purpose, method, timestamp, IP, UA, locale, jurisdiction, signal source) | Consent must be *provable*, an opt-out must be provably honoured, and withdrawal must be as easy as granting |
| Minors | Under-16 US subjects require opt-in before sale/share; under-13 is blocked outright and the tenant must attest it does not target children | COPPA exposure is existential and cannot be delegated to a checkbox on a banner |
| Legal holds | A hold blocks erasure for named subjects/records and is itself audited | Litigation and statutory invoice retention override erasure |
| Backups | Erasure is applied to live data immediately; backups age out within the documented window and the subject is informed of that window | Rewriting backups is neither safe nor standard |
| Breach notification | One incident aggregate, many notification obligations computed from a per-jurisdiction rule table (GDPR 72h to the SA, APPI report to the PPC, US per-state consumer/AG thresholds and deadlines) | US breach law is 50+ statutes; a single "72 hours" timer would be quietly wrong in most of them |
| Risk assessments | Art. 30 records extended with a linked assessment artifact covering DPIA *and* US state data-protection assessments, triggered by data class + purpose | The triggers overlap almost entirely; maintaining two documents guarantees they diverge |

## Domain model

```rust
pub struct DataSubject { id: SubjectId, kind: SubjectKind /*TenantUser|EndUser|Contact*/,
                         tenant_id: Option<TenantId>, identifiers: Vec<SubjectIdentifier>,
                         jurisdiction: JurisdictionRef, jurisdiction_evidence: JurisdictionEvidence }
pub struct DataSubjectRequest { id, subject_ref, tenant_id, kind: DsrKind,
                                status: DsrStatus, received_at, acknowledged_at, due_at, verified_at,
                                submitted_by: Requester /*Subject|AuthorizedAgent|TenantOperator*/,
                                jurisdiction: JurisdictionRef,
                                participants: Vec<ParticipantState>, artifact: Option<ExportArtifact>,
                                appeal: Option<AppealId> }
pub enum DsrKind { Access, Export, Erasure, Rectification, Restriction, Objection,
                   OptOutSaleOrShare, OptOutTargetedAdvertising, OptOutProfiling,
                   LimitSensitiveUse, KnowCategories /* CCPA "right to know", 12-month default */ }
pub enum DsrStatus { AwaitingVerification, InProgress, PartiallyComplete, Completed,
                     Rejected, OnHold, UnderAppeal }
pub struct Appeal { id, request_id, reason, received_at, due_at,
                    outcome: Option<AppealOutcome>, decided_by, authority_notice_sent_at }

/// One resolved answer per purpose. The jurisdiction supplies the default when `source == Default`.
pub struct PurposeDirective { purpose: ProcessingPurpose, state: DirectiveState /*Allowed|Denied*/,
                              source: DirectiveSource /*Consent|OptOut|Signal|Default|Guardian*/,
                              jurisdiction: JurisdictionRef, notice_version: String,
                              evidence: DirectiveEvidence, effective_at, superseded_at }
pub enum ProcessingPurpose { Essential, Analytics, MarketingEmail, AdsPersonalisation,
                             AdsMeasurement, SessionReplay, SaleOrShare, TargetedAdvertising,
                             ProfilingWithSignificantEffects, SensitivePiUse }

pub struct JurisdictionProfile {
    code: JurisdictionRef,              // "eu", "uk", "jp", "us-ca", "us-va", "us-co", …
    regime: Regime,                     // Gdpr | UkGdpr | Appi | UsState | None
    consent_model: ConsentModel,        // OptIn | NoticeAndOptOut
    sensitive_model: SensitiveModel,    // OptIn | LimitOnRequest | NoticeOnly
    ack_days: Option<u16>, response_days: u16, extension_days: u16,
    opt_out_effective_business_days: Option<u16>,
    appeal_window_days: Option<u16>, appeal_response_days: Option<u16>,
    honours_universal_opt_out: bool,
    minor_opt_in_age: Option<u8>,       // e.g. 16 in CA for sale/share
    risk_assessment_required: bool,
    breach_rules: BreachRuleSet,
    authority: AuthorityContact,        // ICO, PPC, state AG, …
    effective_from: Date, source_note: String,
}

pub struct ConsentRecord { id, subject_ref, purpose: ProcessingPurpose, granted: bool,
                           notice_version: String, method: ConsentMethod, evidence: ConsentEvidence,
                           jurisdiction: JurisdictionRef, signal_source: Option<SignalSource>,
                           recorded_at, withdrawn_at }
pub struct RetentionRule { data_class: DataClass, sensitivity: SensitivityClass, basis: LegalBasis,
                           period: Duration, action: RetentionAction /*Delete|Anonymise|Archive*/,
                           disclosed_in_notice: bool }
pub struct LegalHold { id, scope: HoldScope, reason, created_by, released_at }
pub struct BreachIncident { id, discovered_at, contained_at, affected: AffectedSet,
                            data_classes: Vec<DataClass>, encrypted_at_rest: bool,
                            obligations: Vec<NotificationObligation> /* computed per jurisdiction */ }
```

Ports: `PersonalDataProvider` (implemented **by every context**), `CryptoShredder`, `ExportPackager`,
`SubjectVerifier`, `AgentAuthorizationVerifier`, `JurisdictionResolver`, `DirectiveResolver`,
`SubProcessorNotifier`, `BreachObligationCalculator`.

The key contract:

```rust
#[async_trait]
pub trait PersonalDataProvider: Send + Sync {
    fn context_name(&self) -> &'static str;
    async fn collect(&self, subject: &SubjectRef) -> Result<Vec<DataBundle>, PrivacyError>;
    async fn erase(&self, subject: &SubjectRef, mode: EraseMode) -> Result<EraseReport, PrivacyError>;
    async fn rectify(&self, subject: &SubjectRef, patch: &RectifyPatch) -> Result<(), PrivacyError>;
}
```

A CI test asserts **every context crate registers a provider**; adding a context without one fails
the build. That single rule is what stops the data map from rotting.

The second contract is the one every other context asks before processing anything non-essential:

```rust
#[async_trait]
pub trait DirectiveResolver: Send + Sync {
    /// Never returns "unknown": an unresolved purpose falls back to the jurisdiction default,
    /// and an unresolved jurisdiction falls back to the most protective profile.
    async fn resolve(&self, subject: &SubjectRef, purpose: ProcessingPurpose)
        -> Result<PurposeDirective, PrivacyError>;
    async fn snapshot(&self, subject: &SubjectRef) -> Result<DirectiveSnapshot, PrivacyError>;
}
```

`DirectiveState::Allowed` is the *only* value that permits processing. Callers cannot see whether the
answer came from an EU opt-in or an unexercised US opt-out, which is what keeps jurisdiction logic
from leaking into `notifications` and `advertising`.

## Deliverables

### Schema

```sql
CREATE SCHEMA privacy;

-- Jurisdiction rules are DATA. A new US state law is an INSERT plus a test, not a release.
CREATE TABLE privacy.jurisdiction_profiles (
  code text PRIMARY KEY,                       -- 'eu','uk','jp','us-ca','us-va','us-co','us-tx',…
  regime text NOT NULL,                        -- gdpr|uk_gdpr|appi|us_state
  consent_model text NOT NULL,                 -- opt_in|notice_and_opt_out
  sensitive_model text NOT NULL,               -- opt_in|limit_on_request|notice_only
  ack_days smallint, response_days smallint NOT NULL, extension_days smallint NOT NULL DEFAULT 0,
  opt_out_effective_business_days smallint,
  appeal_window_days smallint, appeal_response_days smallint,
  honours_universal_opt_out boolean NOT NULL DEFAULT false,
  minor_opt_in_age smallint, risk_assessment_required boolean NOT NULL DEFAULT false,
  breach_rules jsonb NOT NULL DEFAULT '{}',    -- thresholds, authority + consumer deadlines
  authority jsonb NOT NULL DEFAULT '{}',       -- name, complaint URL (shown in appeal outcomes)
  effective_from date NOT NULL, source_note text, updated_by uuid, updated_at timestamptz);

CREATE TABLE privacy.data_subject_requests (
  id uuid PRIMARY KEY, tenant_id uuid, subject_kind text NOT NULL, subject_ref jsonb NOT NULL,
  kind text NOT NULL, status text NOT NULL, received_at timestamptz NOT NULL DEFAULT now(),
  acknowledged_at timestamptz,                 -- CCPA: within 10 business days
  verified_at timestamptz, due_at timestamptz NOT NULL, extended_to timestamptz,
  completed_at timestamptz,
  jurisdiction text NOT NULL REFERENCES privacy.jurisdiction_profiles(code),
  submitted_by text NOT NULL DEFAULT 'subject', -- subject|authorized_agent|tenant_operator
  agent_id uuid, requester_ip inet, notes text);
CREATE TABLE privacy.authorized_agents (
  id uuid PRIMARY KEY, request_id uuid NOT NULL, agent_name text NOT NULL, agent_contact jsonb,
  authorization_evidence jsonb NOT NULL,        -- signed permission artifact ref + checksum
  consumer_verified_at timestamptz, verified_by uuid, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE privacy.request_appeals (
  id uuid PRIMARY KEY, request_id uuid NOT NULL UNIQUE, reason text NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now(), due_at timestamptz NOT NULL,
  outcome text, decided_at timestamptz, decided_by uuid, decision_reason text,
  authority_notice jsonb);                      -- AG/complaint route given to the consumer
CREATE TABLE privacy.request_participants (
  request_id uuid, context text, status text, report jsonb, updated_at timestamptz,
  PRIMARY KEY (request_id, context));

CREATE TABLE privacy.consents (
  id uuid PRIMARY KEY, tenant_id uuid, subject_ref jsonb NOT NULL, purpose text NOT NULL,
  granted boolean NOT NULL, notice_version text NOT NULL, method text NOT NULL,
  evidence jsonb NOT NULL, locale text, jurisdiction text NOT NULL,
  signal_source text,                           -- ui|api|gpc|uoom|guardian|import
  recorded_at timestamptz NOT NULL DEFAULT now());
-- Signals arrive before we know who the subject is; they bind on identification and never unbind.
CREATE TABLE privacy.preference_signals (
  id uuid PRIMARY KEY, tenant_id uuid, signal text NOT NULL,           -- 'gpc'
  device_ref text NOT NULL, subject_ref jsonb, jurisdiction text,
  purposes text[] NOT NULL, observed_at timestamptz NOT NULL DEFAULT now(),
  bound_at timestamptz, evidence jsonb NOT NULL);
-- Materialised resolution, rebuilt from the ledger; read path for every other context.
CREATE TABLE privacy.purpose_directives (
  subject_ref jsonb NOT NULL, purpose text NOT NULL, state text NOT NULL,  -- allowed|denied
  source text NOT NULL, jurisdiction text NOT NULL, notice_version text,
  effective_at timestamptz NOT NULL, superseded_at timestamptz,
  PRIMARY KEY (subject_ref, purpose));

CREATE TABLE privacy.retention_rules (data_class text PRIMARY KEY, sensitivity text NOT NULL,
  basis text, period interval, action text, disclosed_in_notice boolean NOT NULL DEFAULT true,
  updated_by uuid, updated_at timestamptz);
CREATE TABLE privacy.legal_holds (...);
CREATE TABLE privacy.processing_activities (...);   -- Art.30 record, versioned
CREATE TABLE privacy.risk_assessments (             -- DPIA + US state data-protection assessments
  id uuid PRIMARY KEY, activity_id uuid NOT NULL, jurisdictions text[] NOT NULL,
  trigger text NOT NULL,                            -- targeted_ads|sale|sensitive|profiling|…
  status text NOT NULL, findings jsonb, mitigations jsonb, reviewed_by uuid,
  reviewed_at timestamptz, next_review_at timestamptz);
CREATE TABLE privacy.subprocessors (name text PRIMARY KEY, purpose text, location text,
  role text NOT NULL,                               -- processor|service_provider|contractor|third_party
  contract_terms jsonb NOT NULL,                    -- DPA + CCPA service-provider clauses, flow-down
  dpa_url text, transfer_mechanism text,            -- sccs|uk_addendum|dpf|appi_equivalent|none
  added_at timestamptz, removed_at timestamptz);
CREATE TABLE privacy.subject_keys (subject_id uuid PRIMARY KEY, wrapped_dek bytea NOT NULL,
  kek_id text NOT NULL, destroyed_at timestamptz);
CREATE TABLE privacy.breach_incidents (...);
CREATE TABLE privacy.breach_obligations (           -- one row per jurisdiction per incident
  incident_id uuid, jurisdiction text, audience text,  -- authority|consumer|tenant
  due_at timestamptz NOT NULL, notified_at timestamptz, artifact jsonb,
  PRIMARY KEY (incident_id, jurisdiction, audience));
-- CCPA requires large businesses to publish annual request metrics; generate, never hand-count.
CREATE TABLE privacy.request_metrics_yearly (
  year int, jurisdiction text, kind text, received int, complied int, denied int,
  median_days numeric, PRIMARY KEY (year, jurisdiction, kind));
```

### DSR saga

```
receive request → resolve jurisdiction (subject evidence → profile) → set ack/response/appeal clocks
  → acknowledge receipt (US: within the profile's ack window; recorded, not just emailed)
  → verify identity (re-auth for logged-in subject; signed email link + OTP otherwise;
                     authorized agent ⇒ verify the agent's authorisation AND the consumer)
  → freeze: check legal holds and statutory retention
  → fan out `PersonalDataRequested{request_id, subject, kind}` to every registered provider
  → each provider acks with a DataBundle (export) or EraseReport (erasure), idempotently
  → notify external sub-processors that support deletion APIs (Stripe, ad platforms, mail provider)
  → package: export → machine-readable JSON + human-readable HTML/CSV, zipped, encrypted with a
             one-time passphrase delivered out of band, download link TTL 7 days, max N downloads
  → erasure → crypto-shred subject DEK, delete/anonymise mutable rows, tombstone the identity in
              Kratos, record what was retained and why
  → opt-out kinds → flip the directive immediately (no verification gate: an opt-out must not be
              blocked by verification), propagate to ad platforms and downstream processors within
              the profile's business-day window
  → close: audit entry, notify subject (rejections state the reason **and**, for US subjects, the
              appeal route and the state authority's complaint address), metrics
  → appeal (US) → re-review by a different operator, decision within the profile's window, outcome
              recorded with reasons and the authority contact
```

Timeouts per participant with escalation; partial completion is a first-class status, never a silent
success. Verification is *required before disclosure or deletion* and *forbidden as a gate on
opt-outs* — the CCPA is explicit that an opt-out may not be conditioned on verifying identity, so the
saga branches before the verification step for those kinds.

### Consent, opt-out and directive resolution

- Purposes: `essential` (never consentable), `analytics`, `marketing_email`, `ads_personalisation`,
  `ads_measurement`, `session_replay`, `sale_or_share`, `targeted_advertising`,
  `profiling_significant_effects`, `sensitive_pi_use`. Each maps to concrete downstream behaviour.
- **Resolution order** (first match wins): explicit subject action (consent or opt-out) → guardian
  decision for a known minor → universal opt-out signal → jurisdiction default → most protective.
- **Jurisdiction defaults**: `opt_in` profiles (EU/UK/JP) default every non-essential purpose to
  `Denied`; `notice_and_opt_out` profiles (US states) default `analytics`, `sale_or_share` and
  `targeted_advertising` to `Allowed` *after the notice at collection has been served*, and default
  `sensitive_pi_use` to `Denied` wherever the profile says `sensitive_model = opt_in`.
- **GPC**: a request carrying `Sec-GPC: 1` from a US subject records a `preference_signal`, flips
  `sale_or_share` and `targeted_advertising` to `Denied` for that device immediately, and binds to
  the subject on the next authenticated request. A later "accept all" from the same device does not
  silently override it — the UI must ask, and the answer is recorded with `signal_source = ui`.
- Withdrawal/opt-out takes effect immediately and emits `ConsentWithdrawn` / `ProcessingOptedOut`,
  which phase 10's conversion pipeline **must** honour (no server-side conversion upload without
  `ads_measurement`; no audience upload without `sale_or_share` and `targeted_advertising`).
- Notice versioning: changing a notice version invalidates consent for the affected purposes and
  re-prompts in opt-in jurisdictions; in US profiles it re-serves the notice at collection and is
  recorded, but does not reset an existing opt-out (a refusal survives a policy edit).
- Non-discrimination: a directive state is never an input to pricing, entitlements, or feature gating.
  A CI test asserts the entitlement resolver has no dependency on the privacy read model.

### API surface (`api/privacy.yaml`)

| Method | Path | Notes |
|---|---|---|
| POST | `/api/v1/privacy/requests` | Self-service DSR (authenticated, verified email, or authorized agent) |
| GET | `/api/v1/privacy/requests/{id}` | Status for the subject, incl. ack time and due date |
| GET | `/api/v1/privacy/requests/{id}/download` | One-time, TTL-bound export download |
| POST | `/api/v1/privacy/requests/{id}/appeal` | US appeal of a refusal; returns the authority contact |
| GET/PUT | `/api/v1/privacy/consents` | Read/update own consent |
| GET | `/api/v1/privacy/directives` | Resolved state per purpose + source + jurisdiction (what the UI renders) |
| POST | `/api/v1/privacy/opt-out` | Sale/share, targeted advertising, profiling — **no verification gate**, accepts `Sec-GPC` |
| POST | `/api/v1/privacy/limit-sensitive` | CPRA "limit the use of my sensitive personal information" |
| GET | `/api/v1/public/privacy/notice?locale=&jurisdiction=` | Current notice + version, jurisdiction-specific sections (phase 06 localised) |
| GET | `/api/v1/public/privacy/notice-at-collection` | Categories, purposes, retention, sale/share status — served before collection |
| GET | `/api/v1/public/privacy/metrics?year=` | Annual request metrics (CCPA disclosure), generated |
| GET | `/api/v1/public/subprocessors` | Public sub-processor list incl. role and transfer mechanism |
| GET/POST | `/api/v1/platform/privacy/requests` | Operator view, extend deadline, reject with reason |
| POST | `/api/v1/platform/privacy/requests/{id}/appeal-decision` | Appeal review by a second operator |
| GET/PUT | `/api/v1/platform/privacy/jurisdictions` | Jurisdiction profiles (audited configuration) |
| POST | `/api/v1/platform/privacy/holds` | Legal hold management |
| GET/POST | `/api/v1/platform/privacy/incidents` | Breach incidents + computed per-jurisdiction obligations |
| POST | `/api/v1/tenant/privacy/requests` | Tenant admin acting for their end users (processor / service-provider role) |

## Work breakdown

1. `#[personal_data]` attribute (now carrying `sensitivity`) + registry + the CI test that every
   context implements the provider.
2. Jurisdiction profiles: schema, seed for `eu`, `uk`, `jp` and the US states counsel confirms, the
   `JurisdictionResolver` with its evidence trail, and the most-protective fallback.
3. Envelope encryption: KMS-backed KEK, per-subject DEK, transparent encrypt/decrypt in repositories
   for fields marked `sensitive`, key rotation job.
4. DSR aggregate + saga orchestrator + participant protocol + retries/timeouts + per-jurisdiction
   clocks (acknowledgement, response, extension, opt-out effect, appeal).
5. Providers implemented in each existing context (tenancy, identity, access, billing, audit,
   notifications) — later phases add theirs as part of their own DoD.
6. Identity verification for unauthenticated requesters (signed link + OTP, rate-limited) and the
   authorized-agent flow (authorisation artifact + separate consumer verification).
7. Export packager (JSON + HTML + CSV, manifest, checksum, encryption, expiring link).
8. Erasure executor incl. crypto-shred, Kratos identity deletion, anonymisation of audit references
   (keep the audit entry, replace identifiers with a pseudonym).
9. Directive engine: consent ledger + opt-out ledger + preference signals + purpose registry +
   materialised `purpose_directives` + enforcement hooks in notifications and (later) advertising.
10. GPC/UOOM ingestion middleware: parse `Sec-GPC`, record per device, bind on authentication, expose
    in the directive snapshot, and re-assert on every session.
11. Appeals: aggregate, second-operator review, decision deadlines, authority contact in the outcome.
12. Retention rules + nightly scheduler + dry-run mode + per-tenant reporting + notice publication of
    retention periods per category (CPRA disclosure is generated from these rules).
13. Art. 30 records, risk assessments (DPIA + US state assessments) with triggers, sub-processor
    registry incl. service-provider/contractor terms and transfer mechanism.
14. Breach workflow: one incident, obligations computed per jurisdiction (GDPR 72 h to the SA, PPC
    report, per-state consumer/AG thresholds and deadlines, encryption safe-harbour evaluation).
15. Residency enforcement: reject provisioning/data movement that violates `tenancy.region`
    (`eu`/`jp`/`us`); record the transfer mechanism whenever data legitimately crosses a border.
16. Minors: age signal capture, under-13 hard block with tenant attestation, guardian opt-in path for
    the profile's minor threshold.
17. Annual metrics job producing `request_metrics_yearly` and the public metrics endpoint.
18. Runbooks: DSR handling, appeal review, breach response per jurisdiction, legal hold, key
    destruction verification.

## Testing

- End-to-end DSR test per kind, asserting **every** registered provider participated (count check
  against the registry — a new context that forgets its provider fails this test).
- Crypto-shred test: after erasure, previously readable encrypted fields are unrecoverable and the
  decrypt path returns a typed "shredded" error rather than a panic or a leak.
- Consent enforcement: withdrawing `marketing_email` stops sends within one cycle; withdrawing
  `ads_measurement` stops conversion uploads (contract test with phase 10's port fake).
- Retention: dry-run reports match the real run; legal hold blocks deletion; statutory invoice
  retention survives an erasure request and the response explains it.
- Export integrity: manifest checksums verify, link expires, second download beyond `max_downloads`
  is refused.
- Adversarial: DSR for another subject's data (must fail verification), export link enumeration,
  timing attacks on subject lookup, an authorized-agent submission without consumer verification
  (must not disclose), an appeal filed by someone other than the requester.
- **Jurisdiction matrix test**: a table-driven test runs the same subject journey against every
  seeded profile and asserts the resolved default per purpose, the due dates, the acknowledgement
  requirement, and the appeal availability. Adding a profile row without extending the expectation
  table fails the test.
- **Opt-out semantics**: a US subject who has done nothing has `analytics` allowed and
  `sensitive_pi_use` denied; the same subject in `eu` has both denied. An opt-out is honoured without
  identity verification and within the profile's business-day window.
- **GPC**: a request with `Sec-GPC: 1` denies `sale_or_share` and `targeted_advertising` before any
  login; after login the directive is bound to the subject and survives a session change; a
  subsequent "accept all" does not silently override it.
- Non-discrimination: entitlement resolution produces identical results for two identical tenants
  whose subjects differ only in directive state.
- Minors: an under-13 signal blocks collection outright; an under-16 US subject cannot have
  `sale_or_share` allowed without a recorded guardian opt-in.
- Breach obligations: a seeded incident affecting subjects in three jurisdictions produces three
  distinct deadlines, and encrypted-at-rest data suppresses the obligations the rule table marks as
  encryption-exempt while still recording the assessment.
- Metrics disclosure: the generated annual metrics match a recount from the request ledger.

## Security & compliance

- Export artifacts stored encrypted, in the tenant's region, deleted on expiry, never emailed as
  attachments.
- Verification required before anything is disclosed; failed verification attempts rate-limited and
  alerted.
- Every privacy action lands in the phase-03 audit chain, including operator rejections with reason.
- Sub-processor changes notify tenant admins with the contractual notice period; the registry records
  each vendor's role (processor / service provider / contractor / third party), because a "third
  party" designation is what turns an ad-platform upload into a *sale* under US law.
- Transfers are recorded with their mechanism (SCCs, UK Addendum, EU–US DPF certification, APPI
  equivalent-measures disclosure); a transfer with no recorded mechanism fails the residency check
  rather than proceeding.
- Encryption at rest for personal data is treated as a compliance control, not only a security one:
  several US breach statutes and the CCPA's private right of action turn on it, so the crypto-shred
  design and the breach workflow share the same key-state evidence.
- Privacy lock: a tenant with an open erasure request cannot be impersonated (phase 03 honours this).

## Observability

Metrics: `dsr_open_gauge{kind,jurisdiction}`, `dsr_age_days_histogram{jurisdiction}`,
`dsr_ack_latency_seconds{jurisdiction}`, `dsr_participant_failures_total{context}`,
`directive_state_gauge{purpose,source,jurisdiction}`, `opt_out_latency_seconds{purpose}`,
`gpc_signals_total{bound}`, `appeals_open_gauge`, `retention_deleted_rows_total{data_class}`,
`crypto_shred_total`, `export_download_total`, `breach_obligation_due_gauge{jurisdiction,audience}`.
Alerts: any DSR older than the internal SLA, any request approaching its **jurisdiction-specific**
legal deadline, an acknowledgement window missed, an opt-out not propagated within its business-day
window, any participant failing twice, retention job skipped, a breach obligation within 24 h of due.
The same series back the annual public metrics — the disclosure is a query, not a spreadsheet.

## Acceptance criteria

- [ ] A user submits an export request and receives a complete, verified package covering every
      context, within the SLA, with an audit trail.
- [ ] An erasure request renders the subject's personal data unrecoverable in live systems, retains
      only what a documented legal basis requires, and states exactly what was retained and why.
- [ ] Adding a new context without a `PersonalDataProvider` fails CI.
- [ ] Consent withdrawal demonstrably stops the corresponding downstream processing.
- [ ] The same code path serves an EU subject (nothing processed until opt-in) and a California
      subject (notice served, processing allowed until opt-out), with only the jurisdiction profile
      differing.
- [ ] A `Sec-GPC: 1` request denies sale/share and targeted advertising before login and stays denied
      after the subject authenticates.
- [ ] A US opt-out is honoured without identity verification and propagates to ad platforms within
      the profile's window.
- [ ] A refused US request produces an appeal route, a second-operator decision within the profile's
      window, and the relevant authority's complaint address.
- [ ] Retention dry-run for one tenant matches the actual run row-for-row, and the published notice's
      retention table is generated from the same rules.
- [ ] Art. 30 record, risk assessments, and sub-processor list (with role and transfer mechanism) are
      generated from live configuration, not hand-written.
- [ ] A simulated breach across three jurisdictions produces the correct distinct deadlines and
      audiences without manual research.
- [ ] Adding a new jurisdiction profile requires no code change and fails CI until its expectations
      are added to the matrix test.

## Risks

| Risk | Mitigation |
|---|---|
| A context silently holds personal data outside the registry | Compile-time registry + CI provider test + periodic DB column scan against the map |
| Erasure breaks referential integrity | Anonymise-in-place for foreign keys (pseudonym), never hard-delete rows other aggregates depend on |
| Backups contain erased data | Documented backup window in the privacy notice; keys destroyed so restored ciphertext is useless |
| Over-deletion destroys statutory records | Legal-hold and retention-override rules evaluated *before* the saga fans out; dry-run mandatory in production |
| Tenant misuses processor-mode requests | Requests scoped by RLS to the tenant's own subjects, audited, rate-limited |
| US state laws multiply faster than releases | Jurisdictions are configuration with an effective date; the matrix test forces explicit expectations per profile; counsel reviews the seed, not the code |
| Jurisdiction misresolved, so a US default is applied to an EU subject | Evidence recorded for every resolution, unknown ⇒ most protective profile, and a monthly report of subjects whose jurisdiction came only from IP |
| An ad-platform integration turns into a "sale"/"share" without anyone deciding it | Sub-processor role is a required field; phase 10 uploads assert the vendor's role and the subject's `sale_or_share` directive, and fail closed |
| GPC honoured in the UI but not server-side | Signal is recorded and enforced in the backend directive engine; the network-level e2e test is the proof, not the banner state |
| Health-adjacent data collected by a tenant triggers WA My Health My Data (private right of action) | Data classes flagged in the map; collecting a flagged class requires a tenant attestation and forces consent-first handling regardless of jurisdiction |
| Under-13 users on a tenant storefront | Hard block plus tenant attestation at onboarding; no "assume adult" default |
