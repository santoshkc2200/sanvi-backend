-- Persist publishing intent and stable mutation id before remote provider calls.

ALTER TABLE advertising.campaigns
  ADD COLUMN publishing_mutation_id uuid;
