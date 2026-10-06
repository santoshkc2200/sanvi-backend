-- TASK-021 slice 11.2d: index review of the keyset reads and the slug lookup.
--
-- Cursor pagination (11.2b) made `WHERE tenant_id = $1 … ORDER BY id DESC
-- LIMIT n` the read shape of every collection endpoint, but most of the
-- supporting indexes predate it: the planner walks the primary key backward
-- and filters by tenant as it goes, so a page costs reading every newer row
-- on the platform — the waste grows with platform size, not tenant size.
-- Measured on the 11.2d scratch seed (3 tenants × 2,000 invoices): the
-- invoices first page removed 4,000 of 6,000 rows by filter; the audit
-- seq-keyset removed ~3,000 per 50-row page. Plans are pinned by
-- `apps/api/tests/perf/explain_assertions.rs`.
--
-- Every change here is behaviour-preserving: same rows, same order, same
-- uniqueness. Rollback is the paired .down.sql.

-- ---------------------------------------------------------------------------
-- tenancy.tenants: slug lookups are `WHERE slug = $1` (find_by_slug, the
-- public slug-availability check — a profiled endpoint). The unique index was
-- an expression on lower(slug), which an exact-equality predicate cannot use:
-- availability seq-scanned tenants. The CHECK constraint `slug = lower(slug)`
-- makes uniqueness on `slug` and on `lower(slug)` equivalent, so the
-- expression form is redundant — keep the name, drop the expression.
-- ---------------------------------------------------------------------------
DROP INDEX tenancy.tenants_slug_unique;
CREATE UNIQUE INDEX tenants_slug_unique ON tenancy.tenants (slug);

-- Platform tenant browse/search keyset (`ORDER BY created_at DESC, id DESC`).
CREATE INDEX tenants_created_idx ON tenancy.tenants (created_at DESC, id DESC);

-- ---------------------------------------------------------------------------
-- Keyset alignment: the paginated read is `ORDER BY id DESC`; the tenant
-- indexes that predate it were shaped for `created_at` (since replaced) or
-- bare tenant scoping. `(tenant_id, id)` serves both the keyset and every
-- `WHERE tenant_id = …` prefix the old index served.
-- ---------------------------------------------------------------------------
DROP INDEX billing.invoices_tenant_idx;
CREATE INDEX invoices_tenant_idx ON billing.invoices (tenant_id, id DESC);

DROP INDEX domains.custom_domains_tenant_idx;
CREATE INDEX custom_domains_tenant_idx ON domains.custom_domains (tenant_id, id);

DROP INDEX domains.orders_tenant_idx;
CREATE INDEX orders_tenant_idx ON domains.orders (tenant_id, id);

-- Same shape as payments (`(created_at, id) < cursor ORDER BY created_at DESC,
-- id DESC`): the payments read model got this index in 20260824050000, the
-- disputes read model — the identical query — never did.
DROP INDEX payments.disputes_tenant_idx;
CREATE INDEX disputes_tenant_idx ON payments.disputes (tenant_id, created_at DESC, id DESC);

-- ---------------------------------------------------------------------------
-- Tenant keysets with no tenant index at all: the read seq-scanned per page.
-- ---------------------------------------------------------------------------
CREATE INDEX dsr_tenant_idx ON privacy.data_subject_requests (tenant_id, id);
CREATE INDEX invitations_tenant_idx ON identity.invitations (tenant_id, id);

-- The audit read is keyset on `seq` (the chain's ordering authority) with an
-- optional tenant filter; `audit_entries_tenant_idx` is on (tenant_id,
-- occurred_at), which cannot serve seq order — a tenant-filtered page walked
-- the seq chain and discarded the other tenants' rows (measured: 2,999 of
-- 3,049 rows removed per 50-row page at 3 tenants).
CREATE INDEX audit_entries_tenant_seq_idx ON audit.entries (tenant_id, seq);
