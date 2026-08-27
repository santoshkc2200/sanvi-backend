-- Phase 04: platform billing & subscriptions (Stripe).
--
-- Context: billing (plans, prices, plan→entitlement maps, subscriptions,
-- invoices, webhook events). Reversible counterpart:
-- 20260818000000_platform_billing.down.sql
--
-- Invariants:
--   * Stripe is the source of truth for money; local rows are a projection
--     for entitlements, kept in sync by webhooks (deduplicated by
--     `billing.webhook_events.stripe_event_id`), a dunning sweep and a daily
--     reconciliation job. Webhook events are authoritative for status.
--   * One Stripe Product per plan tier (`stripe_product_id` is UNIQUE);
--     Prices only ever differ by billing variant (interval/currency).
--   * `billing.subscriptions.tenant_id` is UNIQUE: a tenant has at most one
--     platform subscription. Entitlements are derived from the subscription's
--     plan via `billing.plan_entitlements` and written into
--     `access.entitlements` with `source = 'subscription'` (phase 03 defines
--     the precedence: operator overrides still win).
--   * `plan_entitlements.feature_key` intentionally has no FK to
--     `access.features`: the feature catalog is seeded at runtime by the CLI
--     (and extended by operators), while plans are seeded by this migration.
--     The application validates feature keys on every write; the CI test
--     proves seed keys exist in the compile-time catalog.
--   * Subscriptions/invoices carry `tenant_id` and are RLS-protected like
--     every other tenant-scoped table; plans/prices/webhook_events are
--     platform-scoped.

CREATE SCHEMA IF NOT EXISTS billing;

-- ---------------------------------------------------------------------------
-- Plans: one row per sellable tier, backed by one Stripe Product.
-- ---------------------------------------------------------------------------

CREATE TABLE billing.plans (
  id                bigint        PRIMARY KEY,
  key               text        NOT NULL UNIQUE,
  name              text        NOT NULL,
  tier              text        NOT NULL,
  stripe_product_id text        NOT NULL UNIQUE,
  visibility        text        NOT NULL DEFAULT 'public',
  sort_order        int         NOT NULL DEFAULT 0,
  created_at        timestamptz NOT NULL DEFAULT now(),
  updated_at        timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT plans_tier_check CHECK (tier IN ('starter', 'professional', 'enterprise')),
  CONSTRAINT plans_visibility_check CHECK (visibility IN ('public', 'hidden')),
  CONSTRAINT plans_key_lowercase CHECK (key = lower(key)),
  CONSTRAINT plans_name_nonempty CHECK (length(btrim(name)) > 0)
);

-- ---------------------------------------------------------------------------
-- Plan → entitlement grants. Pure data: pricing changes never require a
-- deploy, and the recompute job derives tenant access from these rows.
-- ---------------------------------------------------------------------------

CREATE TABLE billing.plan_entitlements (
  plan_id     bigint    NOT NULL REFERENCES billing.plans(id) ON DELETE CASCADE,
  feature_key text    NOT NULL,
  enabled     boolean NOT NULL,
  "limit"     bigint,
  PRIMARY KEY (plan_id, feature_key),
  CONSTRAINT plan_entitlements_limit_check CHECK ("limit" IS NULL OR "limit" >= 0),
  CONSTRAINT plan_entitlements_key_lowercase CHECK (feature_key = lower(feature_key))
);

-- ---------------------------------------------------------------------------
-- Prices: Stripe Price objects for one plan (billing variants only).
-- ---------------------------------------------------------------------------

CREATE TABLE billing.plan_prices (
  id                 bigint   PRIMARY KEY,
  plan_id            bigint   NOT NULL REFERENCES billing.plans(id) ON DELETE CASCADE,
  stripe_price_id    text   NOT NULL UNIQUE,
  currency           text   NOT NULL,
  interval           text   NOT NULL,
  unit_amount_minor  bigint NOT NULL,
  trial_days         int,
  active             boolean NOT NULL DEFAULT true,
  created_at         timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT plan_prices_currency_lowercase CHECK (currency = lower(currency)),
  CONSTRAINT plan_prices_interval_check CHECK (interval IN ('month', 'year')),
  CONSTRAINT plan_prices_amount_check CHECK (unit_amount_minor >= 0),
  CONSTRAINT plan_prices_trial_days_check CHECK (trial_days IS NULL OR trial_days >= 0)
);

CREATE INDEX plan_prices_plan_idx ON billing.plan_prices (plan_id) WHERE active;

-- ---------------------------------------------------------------------------
-- Subscriptions: the local projection of the tenant's Stripe subscription.
-- `status` mirrors Stripe exactly so webhook mapping is total;
-- `collection_state` is our dunning state machine (ok → dunning →
-- grace_expired). Rows with `source = 'override'` are operator-granted
-- subscriptions (no Stripe object behind them); reconciliation skips them.
-- ---------------------------------------------------------------------------

CREATE TABLE billing.subscriptions (
  id                     bigint        PRIMARY KEY,
  tenant_id              bigint        NOT NULL UNIQUE REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  plan_id                bigint        NOT NULL REFERENCES billing.plans(id),
  price_id               bigint        REFERENCES billing.plan_prices(id),
  source                 text        NOT NULL DEFAULT 'stripe',
  stripe_customer_id     text,
  stripe_subscription_id text        UNIQUE,
  status                 text        NOT NULL,
  current_period_start   timestamptz,
  current_period_end     timestamptz,
  cancel_at_period_end   boolean     NOT NULL DEFAULT false,
  trial_end              timestamptz,
  trial_notified_at      timestamptz,
  collection_state       text        NOT NULL DEFAULT 'ok',
  collection_started_at  timestamptz,
  created_at             timestamptz NOT NULL DEFAULT now(),
  updated_at             timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT subscriptions_source_check CHECK (source IN ('stripe', 'override')),
  CONSTRAINT subscriptions_status_check CHECK (status IN (
    'trialing', 'active', 'past_due', 'unpaid', 'canceled',
    'incomplete', 'incomplete_expired', 'paused'
  )),
  CONSTRAINT subscriptions_collection_check CHECK (collection_state IN ('ok', 'dunning', 'grace_expired')),
  CONSTRAINT subscriptions_period_check CHECK (
    (current_period_start IS NULL AND current_period_end IS NULL)
    OR (current_period_start IS NOT NULL AND current_period_end IS NOT NULL
        AND current_period_end > current_period_start)
  ),
  CONSTRAINT subscriptions_collection_started_check CHECK (
    collection_state = 'ok' OR collection_started_at IS NOT NULL
  ),
  -- Stripe rows carry the full Stripe identity; override rows carry none.
  CONSTRAINT subscriptions_stripe_identity_check CHECK (
    (source = 'stripe' AND stripe_customer_id IS NOT NULL
      AND stripe_subscription_id IS NOT NULL AND price_id IS NOT NULL)
    OR (source = 'override' AND stripe_customer_id IS NULL
      AND stripe_subscription_id IS NULL)
  )
);

CREATE INDEX subscriptions_status_idx ON billing.subscriptions (status);
CREATE INDEX subscriptions_dunning_idx
  ON billing.subscriptions (collection_started_at)
  WHERE collection_state = 'dunning';

SELECT platform.enable_tenant_rls('billing', 'subscriptions');

-- ---------------------------------------------------------------------------
-- Invoices: mirrored for the tenant UI. Personal data → phase 05 data map,
-- with a legal-retention rule that overrides erasure requests.
-- ---------------------------------------------------------------------------

CREATE TABLE billing.invoices (
  id                bigint        PRIMARY KEY,
  tenant_id         bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  stripe_invoice_id text        NOT NULL UNIQUE,
  number            text,
  status            text        NOT NULL,
  total_minor       bigint      NOT NULL,
  currency          text        NOT NULL,
  hosted_url        text,
  pdf_url           text,
  period_start      timestamptz,
  period_end        timestamptz,
  created_at        timestamptz NOT NULL,
  paid_at           timestamptz,
  CONSTRAINT invoices_status_check
    CHECK (status IN ('draft', 'open', 'paid', 'uncollectible', 'void')),
  CONSTRAINT invoices_total_check CHECK (total_minor >= 0),
  CONSTRAINT invoices_currency_lowercase CHECK (currency = lower(currency))
);

CREATE INDEX invoices_tenant_idx ON billing.invoices (tenant_id, created_at DESC);

SELECT platform.enable_tenant_rls('billing', 'invoices');

-- ---------------------------------------------------------------------------
-- Webhook events: store first, process later. The PK on `stripe_event_id` is
-- free deduplication; `processed_at` marks success; `attempts`/`error` back
-- the retry policy and the dead-letter alert (unprocessed gauge > 5 min).
-- ---------------------------------------------------------------------------

CREATE TABLE billing.webhook_events (
  stripe_event_id text        PRIMARY KEY,
  type            text        NOT NULL,
  received_at     timestamptz NOT NULL DEFAULT now(),
  processed_at    timestamptz,
  api_version     text,
  payload         jsonb       NOT NULL,
  attempts        int         NOT NULL DEFAULT 0,
  error           text,
  created         timestamptz NOT NULL
);

CREATE INDEX webhook_events_unprocessed_idx
  ON billing.webhook_events (received_at)
  WHERE processed_at IS NULL AND attempts < 5;

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the NOBYPASSRLS application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA billing TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON billing.plans TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON billing.plan_entitlements TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON billing.plan_prices TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON billing.subscriptions TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON billing.invoices TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON billing.webhook_events TO sanvi_app;

-- ---------------------------------------------------------------------------
-- Seed plans. Stripe product ids are placeholders the operator replaces
-- after creating the real Products in the Dashboard (idempotent update via
-- the platform admin API; see docs/runbooks/billing.md).
-- ---------------------------------------------------------------------------

INSERT INTO billing.plans (id, key, name, tier, stripe_product_id, visibility, sort_order) VALUES
  ('1', 'starter',       'Starter',       'starter',       'prod_starter_placeholder',       'public', 1),
  ('2', 'professional',  'Professional',  'professional',  'prod_professional_placeholder',  'public', 2),
  ('3', 'enterprise',    'Enterprise',    'enterprise',    'prod_enterprise_placeholder',    'public', 3);

-- Starter: platform basics only.
INSERT INTO billing.plan_entitlements (plan_id, feature_key, enabled, "limit") VALUES
  ('1', 'tenancy.member_limit',      true, 5),
  ('1', 'advertising.campaign_limit', true, 1);

-- Professional: ads + custom domains + theming.
INSERT INTO billing.plan_entitlements (plan_id, feature_key, enabled, "limit") VALUES
  ('2', 'tenancy.member_limit',      true, 25),
  ('2', 'advertising.campaign_limit', true, 10),
  ('2', 'advertising.google_ads',    true, NULL),
  ('2', 'advertising.meta_ads',      true, NULL),
  ('2', 'domains.custom',            true, NULL),
  ('2', 'theming.custom_css',        true, NULL),
  ('2', 'localization.custom_locales', true, NULL);

-- Enterprise: everything professional has plus connect + purchase + templates.
INSERT INTO billing.plan_entitlements (plan_id, feature_key, enabled, "limit") VALUES
  ('3', 'tenancy.member_limit',      true, 250),
  ('3', 'advertising.campaign_limit', true, 100),
  ('3', 'advertising.google_ads',    true, NULL),
  ('3', 'advertising.meta_ads',      true, NULL),
  ('3', 'domains.custom',            true, NULL),
  ('3', 'domains.purchase',          true, NULL),
  ('3', 'theming.custom_css',        true, NULL),
  ('3', 'theming.custom_templates',  true, NULL),
  ('3', 'localization.custom_locales', true, NULL),
  ('3', 'payments.stripe_connect',   true, NULL);
