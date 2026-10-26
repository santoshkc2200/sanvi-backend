DROP TABLE privacy.jurisdiction_mappings;
-- Refuse to discard a baseline already assigned to real subjects.
DELETE FROM privacy.jurisdiction_profiles WHERE code = 'global';
ALTER TABLE privacy.jurisdiction_profiles DROP CONSTRAINT jurisdiction_profiles_regime_check;
ALTER TABLE privacy.jurisdiction_profiles ADD CONSTRAINT jurisdiction_profiles_regime_check CHECK (regime IN ('gdpr', 'uk_gdpr', 'appi', 'us_state'));
