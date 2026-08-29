-- Phase 08: custom domains, DNS & TLS.
--
-- Context: domains. Reversible counterpart:
-- 20260822000000_domains_core.down.sql
--
-- Invariants:
-- - `normalized_hostname` is the global uniqueness key, IDN-normalized
--   (punycode) in the `Hostname` value object — homographs never share a
--   key. The partial unique index `one_live_domain_per_hostname` admits
--   exactly one live claim per hostname platform-wide: the first tenant to
--   verify wins, everyone else's claim closes with
--   `verified_by_other_tenant` (a domain is never permanently squatted).
-- - `one_primary_per_tenant` admits one primary per tenant.
-- - `orders` survive the purchase saga; registration idempotency keys on
--   the order id, and `idempotency_key` makes the HTTP command replay-safe.
-- - `certificates` hold issuance/renewal facts only — private keys live in
--   the edge proxy's storage, never in the application database.
-- - `dns_records` is the DNS provider adapter's backing store for
--   purchased-domain zones (the reference sandbox deployment).

CREATE SCHEMA IF NOT EXISTS domains;

-- ---------------------------------------------------------------------------
-- The custom-domain aggregate: claims, verification state, failures.
-- ---------------------------------------------------------------------------

CREATE TABLE domains.custom_domains (
  id                    bigint        PRIMARY KEY,
  tenant_id             bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  hostname              text        NOT NULL,
  normalized_hostname   text        NOT NULL,
  kind                  text        NOT NULL,
  role                  text        NOT NULL,
  status                text        NOT NULL,
  challenge_token       text,
  challenge_type        text,
  challenge_created_at  timestamptz,
  challenge_expires_at  timestamptz,
  detected_registrar    text,
  verified_at           timestamptz,
  last_checked_at       timestamptz,
  next_check_at         timestamptz,
  verification_attempts int         NOT NULL DEFAULT 0,
  failure_code          text,
  failure_detail        jsonb,
  created_at            timestamptz NOT NULL DEFAULT now(),
  updated_at            timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT custom_domains_kind_check
    CHECK (kind IN ('connected', 'purchased')),
  CONSTRAINT custom_domains_role_check
    CHECK (role IN ('primary', 'alias', 'redirect')),
  CONSTRAINT custom_domains_status_check
    CHECK (status IN ('pending_setup', 'verifying', 'verified', 'issuing_cert',
                      'live', 'degraded', 'removed'))
);

-- Exactly one live claim per hostname, across all tenants.
CREATE UNIQUE INDEX one_live_domain_per_hostname
  ON domains.custom_domains (normalized_hostname)
  WHERE status IN ('verified', 'issuing_cert', 'live', 'degraded');

-- Exactly one primary per tenant.
CREATE UNIQUE INDEX one_primary_per_tenant
  ON domains.custom_domains (tenant_id)
  WHERE role = 'primary' AND status <> 'removed';

CREATE INDEX custom_domains_tenant_idx ON domains.custom_domains (tenant_id);
CREATE INDEX custom_domains_due_idx
  ON domains.custom_domains (status, next_check_at);

SELECT platform.enable_tenant_rls('domains', 'custom_domains');

-- ---------------------------------------------------------------------------
-- The live-hostname projection: the host→tenant mapping for resolution and
-- the TLS authorize gate's hot path.
--
-- Deliberately not RLS-protected: it holds only hostnames that hold a live
-- claim (verified/issuing_cert/live/degraded) — public DNS information —
-- and unscoped pool connections cannot see RLS rows, so resolution must not
-- depend on a scoped transaction. The partial unique index on
-- `custom_domains` remains the source of truth; this table is maintained
-- transactionally with every transition in/out of the live set.
-- ---------------------------------------------------------------------------

CREATE TABLE domains.live_hostnames (
  normalized_hostname text        PRIMARY KEY,
  tenant_id           bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  status              text        NOT NULL,
  updated_at          timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT live_hostnames_status_check
    CHECK (status IN ('verified', 'issuing_cert', 'live', 'degraded'))
);

-- ---------------------------------------------------------------------------
-- The purchase order aggregate (the saga's durable record).
-- ---------------------------------------------------------------------------

CREATE TABLE domains.orders (
  id                 bigint        PRIMARY KEY,
  tenant_id          bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  hostname           text        NOT NULL,
  normalized_hostname text       NOT NULL,
  term_years         int         NOT NULL,
  price_minor        bigint      NOT NULL,
  currency           text        NOT NULL,
  status             text        NOT NULL,
  auto_renew         boolean     NOT NULL DEFAULT true,
  whois_privacy      boolean     NOT NULL DEFAULT true,
  registrant_contact jsonb       NOT NULL,
  ordered_by         bigint,
  idempotency_key    uuid,
  external_order_ref text,
  charge_id          text,
  registered_at      timestamptz,
  expires_at         timestamptz,
  renewal_attempts   int         NOT NULL DEFAULT 0,
  next_renewal_at    timestamptz,
  failure_detail     jsonb,
  created_at         timestamptz NOT NULL DEFAULT now(),
  updated_at         timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT orders_status_check
    CHECK (status IN ('pending', 'charged', 'registered', 'configuring',
                      'active', 'failed', 'refunded')),
  CONSTRAINT orders_term_check CHECK (term_years BETWEEN 1 AND 10)
);

CREATE UNIQUE INDEX orders_idempotency_idx
  ON domains.orders (tenant_id, idempotency_key)
  WHERE idempotency_key IS NOT NULL;

CREATE INDEX orders_tenant_idx ON domains.orders (tenant_id);
CREATE INDEX orders_status_idx ON domains.orders (status, updated_at);
CREATE INDEX orders_renewal_idx
  ON domains.orders (status, expires_at, next_renewal_at);

SELECT platform.enable_tenant_rls('domains', 'orders');

-- ---------------------------------------------------------------------------
-- Certificate facts (issuance/renewal metadata — no key material).
-- ---------------------------------------------------------------------------

CREATE TABLE domains.certificates (
  id                 bigint        PRIMARY KEY,
  domain_id          bigint        NOT NULL REFERENCES domains.custom_domains(id) ON DELETE CASCADE,
  tenant_id          bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  hostname           text        NOT NULL,
  issuer             text        NOT NULL,
  serial             text        NOT NULL,
  not_before         timestamptz NOT NULL,
  not_after          timestamptz NOT NULL,
  renewed_at         timestamptz,
  expiry_warned_days int,
  status             text        NOT NULL,
  last_error         text,
  CONSTRAINT certificates_status_check
    CHECK (status IN ('active', 'revoked', 'failed'))
);

CREATE INDEX certificates_domain_idx ON domains.certificates (domain_id);
CREATE INDEX certificates_renewal_idx ON domains.certificates (status, not_after);

SELECT platform.enable_tenant_rls('domains', 'certificates');

-- ---------------------------------------------------------------------------
-- Managed DNS records for purchased domains (the provider adapter's store).
-- ---------------------------------------------------------------------------

CREATE TABLE domains.dns_records (
  id          bigint        PRIMARY KEY,
  domain_id   bigint        REFERENCES domains.custom_domains(id) ON DELETE CASCADE,
  tenant_id   bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  record_type text        NOT NULL,
  name        text        NOT NULL,
  value       text        NOT NULL,
  ttl         int         NOT NULL,
  managed     boolean     NOT NULL DEFAULT true,
  CONSTRAINT dns_records_type_check
    CHECK (record_type IN ('A', 'AAAA', 'ALIAS', 'CNAME', 'NS', 'TXT'))
);

CREATE INDEX dns_records_domain_idx ON domains.dns_records (domain_id);
CREATE UNIQUE INDEX dns_records_identity_idx
  ON domains.dns_records (domain_id, record_type, name);

SELECT platform.enable_tenant_rls('domains', 'dns_records');

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the NOBYPASSRLS application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA domains TO sanvi_app;

GRANT SELECT, INSERT, UPDATE ON domains.custom_domains TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON domains.orders TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON domains.certificates TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON domains.dns_records TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON domains.live_hostnames TO sanvi_app;
