ALTER TABLE privacy.breach_obligations DROP CONSTRAINT breach_obligations_audience_check;
ALTER TABLE privacy.breach_obligations ADD CONSTRAINT breach_obligations_audience_check CHECK (audience IN ('authority', 'authority_final', 'consumer', 'tenant'));
