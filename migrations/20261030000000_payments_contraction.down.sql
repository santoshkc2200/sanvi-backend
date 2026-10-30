-- Recreates the Payments schema exactly as it stood before the contraction,
-- empty: the rows now live in payments-service and are not copied back.
-- Generated with pg_dump --schema=payments --schema-only from a database
-- migrated to 20261028000600; the date-named webhook partitions are left to
-- platform.ensure_monthly_partitions, as at the original conversion.

CREATE SCHEMA payments;

CREATE TABLE payments.checkouts (
    id bigint NOT NULL,
    tenant_id bigint NOT NULL,
    connection_id bigint NOT NULL,
    reference text NOT NULL,
    customer_ref jsonb,
    amount_minor bigint NOT NULL,
    currency text NOT NULL,
    status text NOT NULL,
    external_session_id text,
    idempotency_key uuid,
    locale text,
    expires_at timestamp with time zone,
    completed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    request_fingerprint text,
    CONSTRAINT checkouts_amount_minor_check CHECK ((amount_minor >= 0)),
    CONSTRAINT checkouts_currency_check CHECK ((currency ~ '^[A-Z]{3}$'::text)),
    CONSTRAINT checkouts_request_fingerprint_check CHECK (((request_fingerprint IS NULL) OR (request_fingerprint ~ '^[a-f0-9]{64}$'::text))),
    CONSTRAINT checkouts_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'open'::text, 'complete'::text, 'expired'::text, 'canceled'::text])))
)
WITH (autovacuum_vacuum_scale_factor='0.05', autovacuum_vacuum_threshold='1000');

ALTER TABLE ONLY payments.checkouts FORCE ROW LEVEL SECURITY;

CREATE TABLE payments.disputes (
    id bigint NOT NULL,
    tenant_id bigint NOT NULL,
    payment_id bigint NOT NULL,
    external_dispute_id text NOT NULL,
    status text NOT NULL,
    amount_minor bigint NOT NULL,
    currency text NOT NULL,
    reason text,
    due_by timestamp with time zone,
    charge_id text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT disputes_amount_minor_check CHECK ((amount_minor >= 0)),
    CONSTRAINT disputes_currency_check CHECK ((currency ~ '^[A-Z]{3}$'::text)),
    CONSTRAINT disputes_status_check CHECK ((status ~ '^[a-z_]+$'::text))
);

ALTER TABLE ONLY payments.disputes FORCE ROW LEVEL SECURITY;

CREATE TABLE payments.payments (
    id bigint NOT NULL,
    tenant_id bigint NOT NULL,
    checkout_id bigint,
    external_payment_id text NOT NULL,
    status text NOT NULL,
    amount_minor bigint NOT NULL,
    currency text NOT NULL,
    fee_amount_minor bigint,
    fee_currency text,
    failure jsonb,
    captured_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    payout_status text,
    CONSTRAINT payments_amount_minor_check CHECK ((amount_minor >= 0)),
    CONSTRAINT payments_currency_check CHECK ((currency ~ '^[A-Z]{3}$'::text)),
    CONSTRAINT payments_fee_amount_minor_check CHECK (((fee_amount_minor IS NULL) OR (fee_amount_minor >= 0))),
    CONSTRAINT payments_fee_currency_check CHECK (((fee_currency IS NULL) OR (fee_currency ~ '^[A-Z]{3}$'::text))),
    CONSTRAINT payments_payout_status_check CHECK (((payout_status IS NULL) OR (payout_status = ANY (ARRAY['pending'::text, 'paid'::text, 'failed'::text])))),
    CONSTRAINT payments_status_check CHECK ((status = ANY (ARRAY['requires_action'::text, 'processing'::text, 'succeeded'::text, 'failed'::text, 'canceled'::text])))
)
WITH (autovacuum_vacuum_scale_factor='0.05', autovacuum_vacuum_threshold='1000');

ALTER TABLE ONLY payments.payments FORCE ROW LEVEL SECURITY;

CREATE TABLE payments.payouts (
    id bigint NOT NULL,
    tenant_id bigint NOT NULL,
    connection_id bigint,
    external_payout_id text NOT NULL,
    amount_minor bigint NOT NULL,
    currency text NOT NULL,
    status text NOT NULL,
    arrival_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT payouts_amount_minor_check CHECK ((amount_minor >= 0)),
    CONSTRAINT payouts_currency_check CHECK ((currency ~ '^[A-Z]{3}$'::text)),
    CONSTRAINT payouts_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'paid'::text, 'failed'::text])))
);

ALTER TABLE ONLY payments.payouts FORCE ROW LEVEL SECURITY;

CREATE TABLE payments.provider_connections (
    id bigint NOT NULL,
    tenant_id bigint NOT NULL,
    provider text NOT NULL,
    external_account_id text NOT NULL,
    status text NOT NULL,
    capabilities jsonb DEFAULT '{}'::jsonb NOT NULL,
    requirements jsonb DEFAULT '{}'::jsonb NOT NULL,
    country text,
    default_currency text,
    connected_at timestamp with time zone,
    last_synced_at timestamp with time zone,
    disconnected_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT provider_connections_currency_check CHECK (((default_currency IS NULL) OR (default_currency ~ '^[A-Z]{3}$'::text))),
    CONSTRAINT provider_connections_provider_check CHECK ((provider = ANY (ARRAY['stripe_connect'::text, 'fake'::text]))),
    CONSTRAINT provider_connections_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'onboarding'::text, 'restricted'::text, 'active'::text, 'rejected'::text, 'disconnected'::text])))
)
WITH (autovacuum_vacuum_threshold='100000');

ALTER TABLE ONLY payments.provider_connections FORCE ROW LEVEL SECURITY;

CREATE TABLE payments.refunds (
    id bigint NOT NULL,
    tenant_id bigint NOT NULL,
    payment_id bigint NOT NULL,
    external_refund_id text,
    amount_minor bigint NOT NULL,
    currency text NOT NULL,
    reason text,
    status text NOT NULL,
    created_by bigint,
    idempotency_key uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    audit_request jsonb,
    audit_ready boolean DEFAULT false NOT NULL,
    audited_at timestamp with time zone,
    CONSTRAINT refunds_amount_minor_check CHECK ((amount_minor >= 0)),
    CONSTRAINT refunds_currency_check CHECK ((currency ~ '^[A-Z]{3}$'::text)),
    CONSTRAINT refunds_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'succeeded'::text, 'failed'::text])))
);

ALTER TABLE ONLY payments.refunds FORCE ROW LEVEL SECURITY;

CREATE TABLE payments.tax_settings (
    tenant_id bigint NOT NULL,
    enabled boolean DEFAULT false NOT NULL,
    liability_account text DEFAULT ''::text NOT NULL,
    provider_status text DEFAULT 'unknown'::text NOT NULL,
    registration_count integer DEFAULT 0 NOT NULL,
    warning jsonb,
    last_checked_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT tax_settings_registration_count_check CHECK ((registration_count >= 0))
)
WITH (autovacuum_vacuum_threshold='100000');

ALTER TABLE ONLY payments.tax_settings FORCE ROW LEVEL SECURITY;

CREATE TABLE payments.webhook_event_dedup (
    stripe_event_id text NOT NULL
);

CREATE TABLE payments.webhook_events (
    stripe_event_id text CONSTRAINT webhook_events_p_stripe_event_id_not_null NOT NULL,
    account_id text,
    type text CONSTRAINT webhook_events_p_type_not_null NOT NULL,
    payload jsonb CONSTRAINT webhook_events_p_payload_not_null NOT NULL,
    api_version text,
    received_at timestamp with time zone DEFAULT now() CONSTRAINT webhook_events_p_received_at_not_null NOT NULL,
    processed_at timestamp with time zone,
    attempts integer DEFAULT 0 CONSTRAINT webhook_events_p_attempts_not_null NOT NULL,
    error text,
    created boolean DEFAULT false CONSTRAINT webhook_events_p_created_not_null NOT NULL
)
PARTITION BY RANGE (received_at);

ALTER TABLE ONLY payments.checkouts
    ADD CONSTRAINT checkouts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY payments.disputes
    ADD CONSTRAINT disputes_pkey PRIMARY KEY (id);

ALTER TABLE ONLY payments.payments
    ADD CONSTRAINT payments_pkey PRIMARY KEY (id);

ALTER TABLE ONLY payments.payouts
    ADD CONSTRAINT payouts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY payments.provider_connections
    ADD CONSTRAINT provider_connections_pkey PRIMARY KEY (id);

ALTER TABLE ONLY payments.refunds
    ADD CONSTRAINT refunds_pkey PRIMARY KEY (id);

ALTER TABLE ONLY payments.tax_settings
    ADD CONSTRAINT tax_settings_pkey PRIMARY KEY (tenant_id);

ALTER TABLE ONLY payments.webhook_event_dedup
    ADD CONSTRAINT webhook_event_dedup_pkey PRIMARY KEY (stripe_event_id);

ALTER TABLE ONLY payments.webhook_events
    ADD CONSTRAINT webhook_events_pkey PRIMARY KEY (stripe_event_id, received_at);

CREATE INDEX checkouts_customer_ref_idx ON payments.checkouts USING gin (customer_ref);

CREATE INDEX checkouts_external_idx ON payments.checkouts USING btree (external_session_id) WHERE (external_session_id IS NOT NULL);

CREATE UNIQUE INDEX checkouts_idempotency_idx ON payments.checkouts USING btree (tenant_id, idempotency_key) WHERE (idempotency_key IS NOT NULL);

CREATE INDEX checkouts_status_idx ON payments.checkouts USING btree (status, updated_at);

CREATE INDEX checkouts_tenant_idx ON payments.checkouts USING btree (tenant_id);

CREATE UNIQUE INDEX disputes_external_idx ON payments.disputes USING btree (tenant_id, external_dispute_id);

CREATE INDEX disputes_payment_idx ON payments.disputes USING btree (payment_id);

CREATE INDEX disputes_status_idx ON payments.disputes USING btree (tenant_id, status);

CREATE INDEX disputes_tenant_idx ON payments.disputes USING btree (tenant_id, created_at DESC, id DESC);

CREATE UNIQUE INDEX one_active_provider_per_tenant ON payments.provider_connections USING btree (tenant_id, provider) WHERE (disconnected_at IS NULL);

CREATE UNIQUE INDEX payments_external_idx ON payments.payments USING btree (tenant_id, external_payment_id);

CREATE INDEX payments_status_idx ON payments.payments USING btree (status, updated_at);

CREATE INDEX payments_tenant_created_id_idx ON payments.payments USING btree (tenant_id, created_at DESC, id DESC);

CREATE INDEX payments_tenant_currency_idx ON payments.payments USING btree (tenant_id, currency);

CREATE INDEX payments_tenant_payout_status_idx ON payments.payments USING btree (tenant_id, payout_status);

CREATE UNIQUE INDEX payouts_external_idx ON payments.payouts USING btree (tenant_id, external_payout_id);

CREATE INDEX payouts_tenant_idx ON payments.payouts USING btree (tenant_id);

CREATE UNIQUE INDEX provider_connections_account_idx ON payments.provider_connections USING btree (provider, external_account_id);

CREATE INDEX provider_connections_status_idx ON payments.provider_connections USING btree (status, updated_at);

CREATE INDEX provider_connections_tenant_idx ON payments.provider_connections USING btree (tenant_id);

CREATE UNIQUE INDEX refunds_external_idx ON payments.refunds USING btree (tenant_id, external_refund_id) WHERE (external_refund_id IS NOT NULL);

CREATE UNIQUE INDEX refunds_idempotency_idx ON payments.refunds USING btree (tenant_id, idempotency_key) WHERE (idempotency_key IS NOT NULL);

CREATE INDEX refunds_payment_idx ON payments.refunds USING btree (payment_id);

CREATE INDEX refunds_pending_audit_idx ON payments.refunds USING btree (created_at, id) WHERE (audit_ready AND (audited_at IS NULL));

CREATE INDEX webhook_events_account_idx ON ONLY payments.webhook_events USING btree (account_id);

CREATE INDEX webhook_events_unprocessed_idx ON ONLY payments.webhook_events USING btree (received_at) WHERE ((processed_at IS NULL) AND (attempts < 5));

ALTER TABLE ONLY payments.checkouts
    ADD CONSTRAINT checkouts_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES payments.provider_connections(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.checkouts
    ADD CONSTRAINT checkouts_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.disputes
    ADD CONSTRAINT disputes_payment_id_fkey FOREIGN KEY (payment_id) REFERENCES payments.payments(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.disputes
    ADD CONSTRAINT disputes_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.payments
    ADD CONSTRAINT payments_checkout_id_fkey FOREIGN KEY (checkout_id) REFERENCES payments.checkouts(id) ON DELETE SET NULL;

ALTER TABLE ONLY payments.payments
    ADD CONSTRAINT payments_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE;

ALTER TABLE payments.webhook_events
    ADD CONSTRAINT payments_webhook_events_dedup_fkey FOREIGN KEY (stripe_event_id) REFERENCES payments.webhook_event_dedup(stripe_event_id);

ALTER TABLE ONLY payments.payouts
    ADD CONSTRAINT payouts_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES payments.provider_connections(id) ON DELETE SET NULL;

ALTER TABLE ONLY payments.payouts
    ADD CONSTRAINT payouts_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.provider_connections
    ADD CONSTRAINT provider_connections_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.refunds
    ADD CONSTRAINT refunds_payment_id_fkey FOREIGN KEY (payment_id) REFERENCES payments.payments(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.refunds
    ADD CONSTRAINT refunds_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE;

ALTER TABLE ONLY payments.tax_settings
    ADD CONSTRAINT tax_settings_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE;

ALTER TABLE payments.checkouts ENABLE ROW LEVEL SECURITY;

ALTER TABLE payments.disputes ENABLE ROW LEVEL SECURITY;

ALTER TABLE payments.payments ENABLE ROW LEVEL SECURITY;

ALTER TABLE payments.payouts ENABLE ROW LEVEL SECURITY;

ALTER TABLE payments.provider_connections ENABLE ROW LEVEL SECURITY;

ALTER TABLE payments.refunds ENABLE ROW LEVEL SECURITY;

ALTER TABLE payments.tax_settings ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON payments.checkouts USING (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))) WITH CHECK (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint)));

CREATE POLICY tenant_isolation ON payments.disputes USING (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))) WITH CHECK (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint)));

CREATE POLICY tenant_isolation ON payments.payments USING (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))) WITH CHECK (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint)));

CREATE POLICY tenant_isolation ON payments.payouts USING (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))) WITH CHECK (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint)));

CREATE POLICY tenant_isolation ON payments.provider_connections USING (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))) WITH CHECK (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint)));

CREATE POLICY tenant_isolation ON payments.refunds USING (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))) WITH CHECK (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint)));

CREATE POLICY tenant_isolation ON payments.tax_settings USING (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))) WITH CHECK (((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint)));

GRANT USAGE ON SCHEMA payments TO sanvi_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE payments.checkouts TO sanvi_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE payments.disputes TO sanvi_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE payments.payments TO sanvi_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE payments.payouts TO sanvi_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE payments.provider_connections TO sanvi_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE payments.refunds TO sanvi_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE payments.tax_settings TO sanvi_app;

GRANT SELECT,INSERT,DELETE ON TABLE payments.webhook_event_dedup TO sanvi_app;

GRANT SELECT,INSERT,UPDATE ON TABLE payments.webhook_events TO sanvi_app;

INSERT INTO platform.partitioned_tables (schema_name, table_name, key_column, create_ahead_months, reloptions, enabled)
VALUES ('payments', 'webhook_events', 'received_at', 1,
        'autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.01, autovacuum_vacuum_insert_scale_factor = 0.02',
        true);
SELECT platform.ensure_monthly_partitions(current_date);
