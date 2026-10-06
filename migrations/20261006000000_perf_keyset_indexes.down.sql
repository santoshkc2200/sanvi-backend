-- Reverse of 20261006000000_perf_keyset_indexes.sql (TASK-021 11.2d).

DROP INDEX audit.audit_entries_tenant_seq_idx;
DROP INDEX identity.invitations_tenant_idx;
DROP INDEX privacy.dsr_tenant_idx;

DROP INDEX payments.disputes_tenant_idx;
CREATE INDEX disputes_tenant_idx ON payments.disputes (tenant_id);

DROP INDEX domains.orders_tenant_idx;
CREATE INDEX orders_tenant_idx ON domains.orders (tenant_id);

DROP INDEX domains.custom_domains_tenant_idx;
CREATE INDEX custom_domains_tenant_idx ON domains.custom_domains (tenant_id);

DROP INDEX billing.invoices_tenant_idx;
CREATE INDEX invoices_tenant_idx ON billing.invoices (tenant_id, created_at DESC);

DROP INDEX tenancy.tenants_created_idx;
DROP INDEX tenancy.tenants_slug_unique;
CREATE UNIQUE INDEX tenants_slug_unique ON tenancy.tenants (lower(slug));
