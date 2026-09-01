-- TASK-018: Register Google and Meta in the phase-05 sub-processor registry.
INSERT INTO privacy.subprocessors (name, purpose, location, role, contract_terms, dpa_url, transfer_mechanism)
VALUES
  ('Google Ads', 'Advertising and conversions measurement', 'United States', 'service_provider', '{"framework": "google_ads_data_processing_terms", "appi_cross_border": true}', 'https://business.safety.google/adsprocessorterms/', 'dpf'),
  ('Meta', 'Advertising and conversions measurement', 'United States', 'third_party', '{"framework": "meta_business_tools_terms", "appi_cross_border": true}', 'https://www.facebook.com/legal/terms/dataprocessing', 'dpf');
