-- Phase 05: privacy & compliance (GDPR / UK GDPR / APPI / US state laws).
--
-- Context: privacy. Reversible counterpart:
-- 20260819000000_privacy_compliance.down.sql
--
-- Invariants:
--   * Jurisdiction rules are DATA. A new US state law is an INSERT (plus a
--     matrix-test expectation), never a release. Profiles are seeded by this
--     migration and reviewed by counsel; the seeded values are the operating
--     defaults, not legal advice.
--   * Jurisdiction is resolved from the SUBJECT (declared residency >
--     billing/shipping country > tenant default region > IP country), with
--     the evidence chain stored; unresolved jurisdiction falls back to the
--     most protective profile (eu) in code.
--   * The consent/opt-out ledger is append-only: `privacy.consents` rows are
--     never updated, supersession is expressed by writing a newer row (and
--     the materialised `purpose_directives` row is what other contexts read).
--   * Opt-out records MUST NOT require identity verification: the opt-out
--     ledger (`privacy.consents` with `method = 'opt_out'`) and the directive
--     flip happen synchronously on submission; binding a pre-auth signal to a
--     subject never unbinds it.
--   * Erasure is crypto-shred (destroy the per-subject DEK in
--     `privacy.subject_keys`) plus per-context row deletion/anonymisation;
--     `privacy.legal_holds` block erasure and are themselves audited.
--   * Tenant-scoped tables (data_subjects, data_subject_requests, consents,
--     preference_signals, purpose_directives) are RLS-isolated;
--     platform-scoped governance tables are only reachable under the audited
--     PlatformScope.

CREATE SCHEMA IF NOT EXISTS privacy;

-- ---------------------------------------------------------------------------
-- Jurisdiction profiles: the rule table every deadline, default and appeal
-- right derives from. `breach_rules` and `authority` are jsonb so a new law
-- never needs a schema change.
-- ---------------------------------------------------------------------------

CREATE TABLE privacy.jurisdiction_profiles (
  code                            text        PRIMARY KEY,
  regime                          text        NOT NULL,
  consent_model                   text        NOT NULL,
  sensitive_model                 text        NOT NULL,
  ack_days                        smallint,
  response_days                   smallint    NOT NULL,
  extension_days                  smallint    NOT NULL DEFAULT 0,
  opt_out_effective_business_days smallint,
  appeal_window_days              smallint,
  appeal_response_days            smallint,
  honours_universal_opt_out       boolean     NOT NULL DEFAULT true,
  minor_opt_in_age                smallint,
  risk_assessment_required        boolean     NOT NULL DEFAULT false,
  breach_rules                    jsonb       NOT NULL DEFAULT '{}',
  authority                       jsonb       NOT NULL DEFAULT '{}',
  effective_from                  date        NOT NULL,
  source_note                     text,
  updated_by                      bigint,
  updated_at                      timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT jurisdiction_profiles_regime_check
    CHECK (regime IN ('gdpr', 'uk_gdpr', 'appi', 'us_state')),
  CONSTRAINT jurisdiction_profiles_consent_model_check
    CHECK (consent_model IN ('opt_in', 'notice_and_opt_out')),
  CONSTRAINT jurisdiction_profiles_sensitive_model_check
    CHECK (sensitive_model IN ('opt_in', 'limit_on_request', 'notice_only')),
  CONSTRAINT jurisdiction_profiles_days_check
    CHECK (response_days > 0 AND extension_days >= 0
           AND (ack_days IS NULL OR ack_days >= 0)
           AND (opt_out_effective_business_days IS NULL OR opt_out_effective_business_days >= 0)
           AND (appeal_window_days IS NULL OR appeal_window_days >= 0)
           AND (appeal_response_days IS NULL OR appeal_response_days >= 0))
);

-- Seed profiles. The values below are the platform's operating defaults for
-- review by counsel (see source_note); per-jurisdiction rules that differ
-- from these are configuration edits, not code.

INSERT INTO privacy.jurisdiction_profiles
  (code, regime, consent_model, sensitive_model, ack_days, response_days,
   extension_days, opt_out_effective_business_days, appeal_window_days,
   appeal_response_days, honours_universal_opt_out, minor_opt_in_age,
   risk_assessment_required, breach_rules, authority, effective_from, source_note)
VALUES
  ('eu', 'gdpr', 'opt_in', 'opt_in', NULL, 30, 30, NULL, NULL, NULL, true, 16, true,
   '{"authority_due_hours": 72, "authority_threshold": 0, "consumer_due_days": null, "consumer_threshold": null, "encryption_safe_harbour": true}',
   '{"name": "National supervisory authority (via EDPB)", "complaint_url": "https://edpb.europa.eu/"}',
   '2026-08-01',
   'GDPR Art. 12/15-22: 1 month extendable +2; Art. 33: 72h to the SA; Art. 35 DPIA. Seeded operating defaults — counsel review required.'),
  ('uk', 'uk_gdpr', 'opt_in', 'opt_in', NULL, 30, 30, NULL, NULL, NULL, true, 16, true,
   '{"authority_due_hours": 72, "authority_threshold": 0, "consumer_due_days": null, "consumer_threshold": null, "encryption_safe_harbour": true}',
   '{"name": "Information Commissioner''s Office (ICO)", "complaint_url": "https://ico.org.uk/make-a-complaint/"}',
   '2026-08-01',
   'UK GDPR + DPA 2018; ICO as supervisory authority; UK IDTA/Addendum for transfers. Seeded operating defaults — counsel review required.'),
  ('jp', 'appi', 'opt_in', 'opt_in', NULL, 14, 0, NULL, NULL, NULL, true, 16, false,
   '{"authority_due_days": 5, "authority_threshold": 0, "consumer_due_days": null, "consumer_threshold": null, "encryption_safe_harbour": false}',
   '{"name": "Personal Information Protection Commission (PPC)", "complaint_url": "https://www.ppc.go.jp/"}',
   '2026-08-01',
   'APPI: response "without delay" (internal SLA 14 days); breach report to the PPC "promptly"; no DPIA. Seeded operating defaults — counsel review required.'),
  ('us-ca', 'us_state', 'notice_and_opt_out', 'limit_on_request', 10, 45, 45, 15, NULL, NULL, true, 16, true,
   '{"authority_due_days": null, "authority_threshold": 500, "consumer_due_days": null, "consumer_threshold": 0, "encryption_safe_harbour": true}',
   '{"name": "California Privacy Protection Agency (CPPA)", "complaint_url": "https://cppa.ca.gov/"}',
   '2026-08-01',
   'CCPA/CPRA: ack 10 business days, response 45 (+45); opt-out effective 15 business days; no appeal right; risk assessments; encryption safe harbour. Seeded operating defaults — counsel review required.'),
  ('us-va', 'us_state', 'notice_and_opt_out', 'opt_in', 10, 45, 45, 15, 30, 60, true, 13, true,
   '{"authority_due_days": null, "authority_threshold": 500, "consumer_due_days": null, "consumer_threshold": 0, "encryption_safe_harbour": false}',
   '{"name": "Office of the Attorney General of Virginia", "complaint_url": "https://www.oag.state.va.us/consumer-protection/"}',
   '2026-08-01',
   'VCDPA: appeal decided within 60 days; data protection assessments; sensitive processing is opt-in. Seeded operating defaults — counsel review required.'),
  ('us-co', 'us_state', 'notice_and_opt_out', 'opt_in', 10, 45, 45, 15, 45, 45, true, 13, true,
   '{"authority_due_days": null, "authority_threshold": 500, "consumer_due_days": 30, "consumer_threshold": 0, "encryption_safe_harbour": false}',
   '{"name": "Colorado Attorney General", "complaint_url": "https://coag.gov/office-sections/consumer-protection/data-privacy/"}',
   '2026-08-01',
   'CPA: appeal within 45 days; data protection assessments; sensitive processing is opt-in; consumer breach notice 30 days. Seeded operating defaults — counsel review required.'),
  ('us-tx', 'us_state', 'notice_and_opt_out', 'opt_in', 10, 45, 45, 15, 30, 60, true, 13, true,
   '{"authority_due_days": null, "authority_threshold": 250, "consumer_due_days": null, "consumer_threshold": 0, "encryption_safe_harbour": false}',
   '{"name": "Texas Attorney General", "complaint_url": "https://www.texasattorneygeneral.gov/"}',
   '2026-08-01',
   'TDPSA: appeal decided within 60 days; data protection assessments; sensitive processing is opt-in. Seeded operating defaults — counsel review required.'),
  ('us-ut', 'us_state', 'notice_and_opt_out', 'opt_in', 10, 45, 45, 15, NULL, NULL, true, 13, false,
   '{"authority_due_days": null, "authority_threshold": 500, "consumer_due_days": null, "consumer_threshold": 0, "encryption_safe_harbour": false}',
   '{"name": "Utah Attorney General", "complaint_url": "https://attorneygeneral.utah.gov/"}',
   '2026-08-01',
   'UCPA: no appeal right, no DPIA; sensitive processing is opt-in. Seeded operating defaults — counsel review required.'),
  ('us-ct', 'us_state', 'notice_and_opt_out', 'opt_in', 10, 45, 45, 15, 45, 45, true, 13, true,
   '{"authority_due_days": null, "authority_threshold": 500, "consumer_due_days": null, "consumer_threshold": 0, "encryption_safe_harbour": false}',
   '{"name": "Connecticut Attorney General", "complaint_url": "https://portal.ct.gov/ag"}',
   '2026-08-01',
   'CTDPA: appeal within 45 days; data protection assessments; sensitive processing is opt-in. Seeded operating defaults — counsel review required.')
ON CONFLICT (code) DO NOTHING;

-- ---------------------------------------------------------------------------
-- Data subjects: the persisted identity anchor (kind + identifiers) and the
-- resolved jurisdiction with its evidence chain. One row per canonical
-- subject key; requests, directives and keys all reference it.
-- ---------------------------------------------------------------------------

CREATE TABLE privacy.data_subjects (
  id                     bigint        PRIMARY KEY,
  tenant_id              bigint,
  kind                   text        NOT NULL,
  canonical_key          text        NOT NULL,
  identifiers            jsonb       NOT NULL DEFAULT '[]',
  jurisdiction           text        NOT NULL REFERENCES privacy.jurisdiction_profiles(code),
  jurisdiction_evidence  jsonb       NOT NULL DEFAULT '[]',
  is_minor               boolean     NOT NULL DEFAULT false,
  created_at             timestamptz NOT NULL DEFAULT now(),
  updated_at             timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT data_subjects_kind_check
    CHECK (kind IN ('tenant_user', 'end_user', 'contact', 'device')),
  CONSTRAINT data_subjects_tenant_required
    CHECK ((kind = 'end_user') = (tenant_id IS NOT NULL)),
  CONSTRAINT data_subjects_key_format CHECK (length(canonical_key) BETWEEN 3 AND 512)
);
CREATE UNIQUE INDEX data_subjects_canonical_key_idx ON privacy.data_subjects (canonical_key);
SELECT platform.enable_tenant_rls('privacy', 'data_subjects');

-- ---------------------------------------------------------------------------
-- Data subject requests (DSR): one row per request, with the per-jurisdiction
-- clocks (ack, response, extension, appeal) computed at submission from the
-- subject's jurisdiction profile.
-- ---------------------------------------------------------------------------

CREATE TABLE privacy.data_subject_requests (
  id                 bigint        PRIMARY KEY,
  tenant_id          bigint,
  subject_id         bigint        NOT NULL REFERENCES privacy.data_subjects(id),
  subject_ref        jsonb       NOT NULL,
  kind               text        NOT NULL,
  status             text        NOT NULL,
  received_at        timestamptz NOT NULL DEFAULT now(),
  acknowledged_at    timestamptz,
  verified_at        timestamptz,
  due_at             timestamptz NOT NULL,
  extended_to        timestamptz,
  completed_at       timestamptz,
  jurisdiction       text        NOT NULL REFERENCES privacy.jurisdiction_profiles(code),
  submitted_by       text        NOT NULL DEFAULT 'subject',
  agent_id           bigint,
  requester_ip       inet,
  requester_note     text,
  rejection_reason   text,
  appeal_id          bigint,
  export_artifact_id bigint,
  created_at         timestamptz NOT NULL DEFAULT now(),
  updated_at         timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT dsr_kind_check CHECK (kind IN (
    'access', 'export', 'erasure', 'rectification', 'restriction', 'objection',
    'opt_out_sale_share', 'opt_out_targeted_advertising', 'opt_out_profiling',
    'limit_sensitive_use', 'know_categories')),
  CONSTRAINT dsr_status_check CHECK (status IN (
    'awaiting_verification', 'in_progress', 'partially_complete', 'completed',
    'rejected', 'on_hold', 'under_appeal')),
  CONSTRAINT dsr_submitted_by_check
    CHECK (submitted_by IN ('subject', 'authorized_agent', 'tenant_operator')),
  CONSTRAINT dsr_tenant_required
    CHECK ((submitted_by = 'tenant_operator') = (tenant_id IS NOT NULL))
);
CREATE INDEX dsr_subject_idx ON privacy.data_subject_requests (subject_id);
CREATE INDEX dsr_status_due_idx ON privacy.data_subject_requests (status, due_at);
CREATE INDEX dsr_jurisdiction_idx ON privacy.data_subject_requests (jurisdiction, received_at);
SELECT platform.enable_tenant_rls('privacy', 'data_subject_requests');

-- Authorized agents (CCPA): the agent's identity, the authorization artifact
-- (signed permission with checksum) and the separately-recorded consumer
-- verification. Disclosure requires BOTH.
CREATE TABLE privacy.authorized_agents (
  id                     bigint        PRIMARY KEY,
  request_id             bigint        NOT NULL REFERENCES privacy.data_subject_requests(id) ON DELETE CASCADE,
  agent_name             text        NOT NULL,
  agent_contact          jsonb,
  authorization_evidence jsonb       NOT NULL,
  consumer_verified_at   timestamptz,
  verified_by            bigint,
  created_at             timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT authorized_agents_name_nonempty CHECK (length(btrim(agent_name)) > 0)
);

-- Appeals (US states with an appeal right): first-class aggregate with its
-- own clock; the outcome records the state authority's complaint route.
CREATE TABLE privacy.request_appeals (
  id                 bigint        PRIMARY KEY,
  request_id         bigint        NOT NULL UNIQUE REFERENCES privacy.data_subject_requests(id) ON DELETE CASCADE,
  reason             text        NOT NULL,
  received_at        timestamptz NOT NULL DEFAULT now(),
  due_at             timestamptz NOT NULL,
  outcome            text,
  decided_at         timestamptz,
  decided_by         bigint,
  decision_reason    text,
  authority_notice   jsonb,
  CONSTRAINT request_appeals_outcome_check
    CHECK (outcome IS NULL OR outcome IN ('upheld', 'denied'))
);

-- Saga participants: one row per registered provider per request. Partial
-- completion is a first-class outcome, never a silent success.
CREATE TABLE privacy.request_participants (
  request_id  bigint        NOT NULL REFERENCES privacy.data_subject_requests(id) ON DELETE CASCADE,
  context     text        NOT NULL,
  status      text        NOT NULL DEFAULT 'pending',
  report      jsonb,
  attempts    int         NOT NULL DEFAULT 0,
  last_error  text,
  updated_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (request_id, context),
  CONSTRAINT request_participants_status_check
    CHECK (status IN ('pending', 'in_progress', 'completed', 'failed', 'retained'))
);

-- Verification challenges for unauthenticated requesters: one-time code over
-- a signed email link, rate-limited, hashed at rest.
CREATE TABLE privacy.verification_challenges (
  id           bigint        PRIMARY KEY,
  request_id   bigint        NOT NULL UNIQUE REFERENCES privacy.data_subject_requests(id) ON DELETE CASCADE,
  method       text        NOT NULL DEFAULT 'email_otp',
  token_hash   text        NOT NULL,
  code_hash    text        NOT NULL,
  expires_at   timestamptz NOT NULL,
  attempts     int         NOT NULL DEFAULT 0,
  consumed_at  timestamptz,
  created_at   timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT verification_challenges_method_check CHECK (method = 'email_otp')
);

-- ---------------------------------------------------------------------------
-- Consent / opt-out ledger: append-only. `granted=false` with
-- method='opt_out' is an exercised opt-out; with method='consent' it is a
-- withdrawal. Signal sources are recorded for proof (gpc, uoom, ui, api,
-- guardian, import).
-- ---------------------------------------------------------------------------

CREATE TABLE privacy.consents (
  id             bigint        PRIMARY KEY,
  tenant_id      bigint,
  subject_id     bigint        REFERENCES privacy.data_subjects(id),
  subject_ref    jsonb       NOT NULL,
  purpose        text        NOT NULL,
  granted        boolean     NOT NULL,
  notice_version text        NOT NULL,
  method         text        NOT NULL,
  evidence       jsonb       NOT NULL DEFAULT '{}',
  locale         text,
  jurisdiction   text        NOT NULL REFERENCES privacy.jurisdiction_profiles(code),
  signal_source  text,
  recorded_at    timestamptz NOT NULL DEFAULT now(),
  withdrawn_at   timestamptz,
  CONSTRAINT consents_purpose_check CHECK (purpose IN (
    'essential', 'analytics', 'marketing_email', 'ads_personalisation',
    'ads_measurement', 'session_replay', 'sale_or_share', 'targeted_advertising',
    'profiling_significant_effects', 'sensitive_pi_use')),
  CONSTRAINT consents_method_check
    CHECK (method IN ('consent', 'opt_out', 'withdrawal', 'guardian_consent')),
  CONSTRAINT consents_signal_source_check
    CHECK (signal_source IS NULL OR signal_source IN
      ('ui', 'api', 'gpc', 'uoom', 'guardian', 'import')),
  CONSTRAINT consents_nonessential_check
    CHECK (purpose <> 'essential' OR granted)
);
CREATE INDEX consents_subject_purpose_idx
  ON privacy.consents (subject_id, purpose, recorded_at DESC);
SELECT platform.enable_tenant_rls('privacy', 'consents');

-- Preference signals (GPC/UOOM): observed before identification, bound to a
-- subject on the next authenticated request; binding never unbinds.
CREATE TABLE privacy.preference_signals (
  id           bigint        PRIMARY KEY,
  tenant_id    bigint,
  signal       text        NOT NULL,
  device_ref   text        NOT NULL,
  subject_id   bigint        REFERENCES privacy.data_subjects(id),
  jurisdiction text        REFERENCES privacy.jurisdiction_profiles(code),
  purposes     text[]      NOT NULL,
  observed_at  timestamptz NOT NULL DEFAULT now(),
  bound_at     timestamptz,
  evidence     jsonb       NOT NULL DEFAULT '{}',
  CONSTRAINT preference_signals_signal_check CHECK (signal IN ('gpc', 'uoom')),
  CONSTRAINT preference_signals_purposes_check CHECK (
    purposes <@ ARRAY['sale_or_share', 'targeted_advertising', 'profiling_significant_effects']::text[])
);
CREATE INDEX preference_signals_device_idx
  ON privacy.preference_signals (device_ref, observed_at DESC);
CREATE INDEX preference_signals_unbound_idx
  ON privacy.preference_signals (subject_id) WHERE bound_at IS NULL;
SELECT platform.enable_tenant_rls('privacy', 'preference_signals');

-- Materialised resolution: the read path every other context uses. Rebuilt
-- from the ledger by the directive engine; state/source/jurisdiction are
-- exactly what the privacy centre UI renders.
CREATE TABLE privacy.purpose_directives (
  subject_id     bigint,
  subject_ref    jsonb       NOT NULL,
  purpose        text        NOT NULL,
  state          text        NOT NULL,
  source         text        NOT NULL,
  jurisdiction   text        NOT NULL REFERENCES privacy.jurisdiction_profiles(code),
  notice_version text,
  effective_at   timestamptz NOT NULL DEFAULT now(),
  superseded_at  timestamptz,
  tenant_id      bigint,
  PRIMARY KEY (subject_ref, purpose),
  CONSTRAINT purpose_directives_state_check CHECK (state IN ('allowed', 'denied')),
  CONSTRAINT purpose_directives_source_check
    CHECK (source IN ('consent', 'opt_out', 'signal', 'default', 'guardian')),
  CONSTRAINT purpose_directives_purpose_check CHECK (purpose IN (
    'essential', 'analytics', 'marketing_email', 'ads_personalisation',
    'ads_measurement', 'session_replay', 'sale_or_share', 'targeted_advertising',
    'profiling_significant_effects', 'sensitive_pi_use'))
);
SELECT platform.enable_tenant_rls('privacy', 'purpose_directives');

-- ---------------------------------------------------------------------------
-- Retention rules: one row per data class; drives the nightly sweep, the
-- CPRA retention disclosure and the privacy notice's retention table.
-- ---------------------------------------------------------------------------

CREATE TABLE privacy.retention_rules (
  data_class            text        PRIMARY KEY,
  sensitivity           text        NOT NULL,
  basis                 text,
  period                interval    NOT NULL,
  action                text        NOT NULL,
  disclosed_in_notice   boolean     NOT NULL DEFAULT true,
  updated_by            bigint,
  updated_at            timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT retention_rules_sensitivity_check
    CHECK (sensitivity IN ('standard', 'sensitive', 'special', 'flagged_health', 'minors')),
  CONSTRAINT retention_rules_basis_check
    CHECK (basis IS NULL OR basis IN ('contract', 'legal_obligation', 'consent',
      'legitimate_interest', 'notice_opt_out', 'statutory_retention')),
  CONSTRAINT retention_rules_action_check
    CHECK (action IN ('delete', 'anonymise', 'archive'))
);

-- Seed defaults (the compile-time data map's operational half); counsel
-- adjusts periods here without a release — the privacy notice's retention
-- table is generated from these rows.
INSERT INTO privacy.retention_rules
  (data_class, sensitivity, basis, period, action, disclosed_in_notice)
VALUES
  ('identity_profile', 'standard', 'contract', make_interval(days => 2555), 'delete', true),
  ('authentication', 'sensitive', 'contract', make_interval(days => 730), 'delete', true),
  ('membership', 'standard', 'contract', make_interval(days => 2555), 'delete', true),
  ('contact', 'standard', 'contract', make_interval(days => 2555), 'delete', true),
  ('billing', 'sensitive', 'contract', make_interval(days => 2555), 'delete', true),
  ('invoice', 'standard', 'statutory_retention', make_interval(days => 3650), 'archive', true),
  ('audit_log', 'standard', 'legal_obligation', make_interval(days => 3650), 'anonymise', true),
  ('consent_record', 'standard', 'legal_obligation', make_interval(days => 3650), 'anonymise', true),
  ('preference', 'standard', 'legal_obligation', make_interval(days => 365), 'delete', true),
  ('communication', 'standard', 'contract', make_interval(days => 730), 'delete', true),
  ('jurisdiction', 'standard', 'legal_obligation', make_interval(days => 3650), 'anonymise', true),
  ('request_record', 'standard', 'legal_obligation', make_interval(days => 3650), 'anonymise', true)
ON CONFLICT (data_class) DO NOTHING;

-- Legal holds: block erasure for named subjects/records; each hold is
-- audited and has an explicit release.
CREATE TABLE privacy.legal_holds (
  id           bigint        PRIMARY KEY,
  scope        jsonb       NOT NULL,
  reason       text        NOT NULL,
  created_by   bigint        NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  released_at  timestamptz,
  released_by  bigint,
  CONSTRAINT legal_holds_reason_nonempty CHECK (length(btrim(reason)) > 0)
);

-- ---------------------------------------------------------------------------
-- Governance: Art. 30 processing records, risk assessments (DPIA + US state
-- data-protection assessments), the sub-processor registry and breach
-- incidents with per-jurisdiction obligations.
-- ---------------------------------------------------------------------------

CREATE TABLE privacy.processing_activities (
  id            bigint        PRIMARY KEY,
  name          text        NOT NULL,
  purpose       text        NOT NULL,
  data_classes  text[]      NOT NULL,
  controller_role text      NOT NULL DEFAULT 'controller',
  basis         text,
  version       int         NOT NULL DEFAULT 1,
  supersedes    bigint,
  active        boolean     NOT NULL DEFAULT true,
  created_by    bigint,
  created_at    timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT processing_activities_name_nonempty CHECK (length(btrim(name)) > 0),
  CONSTRAINT processing_activities_role_check
    CHECK (controller_role IN ('controller', 'processor', 'business', 'service_provider'))
);

CREATE TABLE privacy.risk_assessments (
  id            bigint        PRIMARY KEY,
  activity_id   bigint        NOT NULL REFERENCES privacy.processing_activities(id) ON DELETE CASCADE,
  jurisdictions text[]      NOT NULL,
  trigger       text        NOT NULL,
  status        text        NOT NULL DEFAULT 'open',
  findings      jsonb,
  mitigations   jsonb,
  reviewed_by   bigint,
  reviewed_at   timestamptz,
  next_review_at timestamptz,
  created_at    timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT risk_assessments_trigger_check CHECK (trigger IN (
    'targeted_ads', 'sale', 'sensitive', 'profiling', 'large_scale', 'health_data')),
  CONSTRAINT risk_assessments_status_check
    CHECK (status IN ('open', 'in_review', 'approved', 'superseded'))
);

CREATE TABLE privacy.subprocessors (
  name               text        PRIMARY KEY,
  purpose            text        NOT NULL,
  location           text        NOT NULL,
  role               text        NOT NULL,
  contract_terms     jsonb       NOT NULL DEFAULT '{}',
  dpa_url            text,
  transfer_mechanism text,
  added_at           timestamptz NOT NULL DEFAULT now(),
  removed_at         timestamptz,
  CONSTRAINT subprocessors_role_check
    CHECK (role IN ('processor', 'service_provider', 'contractor', 'third_party')),
  CONSTRAINT subprocessors_transfer_check CHECK (
    transfer_mechanism IS NULL OR transfer_mechanism IN
      ('sccs', 'uk_addendum', 'dpf', 'appi_equivalent', 'none'))
);

-- Per-subject envelope-encryption keys: the DEK is wrapped by a KEK held
-- outside the database. Erasure destroys the row (crypto-shred).
CREATE TABLE privacy.subject_keys (
  subject_id   bigint        PRIMARY KEY REFERENCES privacy.data_subjects(id) ON DELETE CASCADE,
  wrapped_dek  bytea       NOT NULL,
  kek_id       text        NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  destroyed_at timestamptz
);

-- Packaged exports: encrypted in the tenant's region, one-time passphrase
-- delivered out of band, download-link TTL and a download cap.
CREATE TABLE privacy.export_artifacts (
  id              bigint        PRIMARY KEY,
  request_id      bigint        NOT NULL REFERENCES privacy.data_subject_requests(id) ON DELETE CASCADE,
  subject_id      bigint        NOT NULL REFERENCES privacy.data_subjects(id),
  download_token_hash text     NOT NULL,
  passphrase_verifier  text    NOT NULL,
  manifest_sha256 text        NOT NULL,
  path            text        NOT NULL,
  content_size    bigint      NOT NULL,
  expires_at      timestamptz NOT NULL,
  max_downloads   int         NOT NULL DEFAULT 3,
  download_count  int         NOT NULL DEFAULT 0,
  downloaded_at   timestamptz,
  created_at      timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT export_artifacts_sizes_check
    CHECK (content_size >= 0 AND max_downloads > 0 AND download_count >= 0)
);

-- Breach incidents: one aggregate; obligations are computed per jurisdiction
-- from the profile's breach_rules at record time and re-evaluated on edits.
CREATE TABLE privacy.breach_incidents (
  id                bigint        PRIMARY KEY,
  discovered_at     timestamptz NOT NULL,
  contained_at      timestamptz,
  affected          jsonb       NOT NULL,
  data_classes      text[]      NOT NULL,
  encrypted_at_rest boolean     NOT NULL DEFAULT false,
  notes             text,
  created_by        bigint,
  created_at        timestamptz NOT NULL DEFAULT now(),
  updated_at        timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE privacy.breach_obligations (
  incident_id  bigint        NOT NULL REFERENCES privacy.breach_incidents(id) ON DELETE CASCADE,
  jurisdiction text        NOT NULL REFERENCES privacy.jurisdiction_profiles(code),
  audience     text        NOT NULL,
  due_at       timestamptz NOT NULL,
  notified_at  timestamptz,
  artifact     jsonb,
  PRIMARY KEY (incident_id, jurisdiction, audience),
  CONSTRAINT breach_obligations_audience_check
    CHECK (audience IN ('authority', 'consumer', 'tenant'))
);

-- CCPA annual request metrics: generated by a yearly job from the request
-- ledger, never hand-counted; the public disclosure endpoint reads it.
CREATE TABLE privacy.request_metrics_yearly (
  year         int           NOT NULL,
  jurisdiction text          NOT NULL,
  kind         text          NOT NULL,
  received     int           NOT NULL DEFAULT 0,
  complied     int           NOT NULL DEFAULT 0,
  denied       int           NOT NULL DEFAULT 0,
  median_days  numeric,
  generated_at timestamptz   NOT NULL DEFAULT now(),
  PRIMARY KEY (year, jurisdiction, kind),
  CONSTRAINT request_metrics_yearly_counts_check
    CHECK (received >= 0 AND complied >= 0 AND denied >= 0 AND complied + denied <= received)
);

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA privacy TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.jurisdiction_profiles TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.data_subjects TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.data_subject_requests TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.authorized_agents TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.request_appeals TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.request_participants TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.verification_challenges TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON privacy.consents TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON privacy.preference_signals TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.purpose_directives TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.retention_rules TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.legal_holds TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.processing_activities TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.risk_assessments TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.subprocessors TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON privacy.subject_keys TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.export_artifacts TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.breach_incidents TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.breach_obligations TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON privacy.request_metrics_yearly TO sanvi_app;
