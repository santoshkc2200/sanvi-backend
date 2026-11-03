-- Reverses 20261104000000_platform_ecommerce_public_store_bindings_rls.sql:
-- back to the phase-14 shape (platform-scoped, no RLS). Do not run the two
-- shapes mixed: the pre-RLS adapter build reads this table unscoped.
DROP POLICY IF EXISTS tenant_isolation ON platform.ecommerce_public_store_bindings;
ALTER TABLE platform.ecommerce_public_store_bindings NO FORCE ROW LEVEL SECURITY;
ALTER TABLE platform.ecommerce_public_store_bindings DISABLE ROW LEVEL SECURITY;
