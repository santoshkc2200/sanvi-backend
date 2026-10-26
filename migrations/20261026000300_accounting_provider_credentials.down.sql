-- Revoke provider grants first: dropping these tables discards the only copy
-- of the refresh tokens but leaves the provider-side consent in place.
DROP TABLE accounting.provider_credentials;
DROP TABLE accounting.tenant_secret_keys;
DROP TABLE accounting.oauth_states;
