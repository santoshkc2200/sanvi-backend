-- Phase 14 (shopper storefront): which of a tenant's ecommerce stores the
-- tenant's hosts serve. The public scope resolves host → tenant → at most
-- one published store; an unbound tenant answers 404-shaped. Rows are
-- platform-scoped (no RLS): tenant scoping is the `tenant_id` key itself,
-- and validity (the store exists, is active, belongs to the tenant) is
-- re-checked against ecommerce-service at write time and read time, because
-- the store of record lives in the service, not here.
CREATE TABLE platform.ecommerce_public_store_bindings (
    tenant_id bigint PRIMARY KEY,
    store_id bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT, INSERT, UPDATE ON platform.ecommerce_public_store_bindings TO sanvi_app;
