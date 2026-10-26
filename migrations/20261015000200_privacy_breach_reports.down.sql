DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM privacy.breach_obligations WHERE audience = 'authority_final') THEN
    RAISE EXCEPTION 'Cannot remove final-report support while final-report evidence exists';
  END IF;
END $$;
ALTER TABLE privacy.breach_obligations DROP CONSTRAINT breach_obligations_audience_check;
ALTER TABLE privacy.breach_obligations ADD CONSTRAINT breach_obligations_audience_check CHECK (audience IN ('authority', 'consumer', 'tenant'));
