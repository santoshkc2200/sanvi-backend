-- Phase 06 hardening: audited platform operations used by privacy DSRs and
-- replacement-style platform translation updates must be able to reach the
-- tenant rows that FORCE RLS otherwise hides.

CREATE POLICY translations_platform_scope_read
    ON localization.translations
    FOR SELECT
    USING (NULLIF(current_setting('sanvi.platform_scope', true), '') = 'on');

CREATE POLICY translations_platform_scope_update
    ON localization.translations
    FOR UPDATE
    USING (NULLIF(current_setting('sanvi.platform_scope', true), '') = 'on')
    WITH CHECK (NULLIF(current_setting('sanvi.platform_scope', true), '') = 'on');

CREATE POLICY translations_platform_scope_delete
    ON localization.translations
    FOR DELETE
    USING (NULLIF(current_setting('sanvi.platform_scope', true), '') = 'on');
