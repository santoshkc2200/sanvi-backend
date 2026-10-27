-- Personal data recorded by ecommerce-service (such as created_by user
-- attribution) is exported and anonymised through the service's internal privacy
-- routes. Disclosed in the platform notice and subject to statutory retention.
INSERT INTO privacy.retention_rules
  (data_class, sensitivity, basis, period, action, disclosed_in_notice, updated_by)
VALUES
  ('ecommerce_attribution', 'standard', 'contract',
   make_interval(days => 2555), 'anonymise', true, '100')
ON CONFLICT (data_class) DO NOTHING;
