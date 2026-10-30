-- Preserve operational settings so rollback restores the exact previous state.
CREATE TABLE platform.advertising_partition_handoff AS
SELECT schema_name, table_name, enabled FROM platform.partitioned_tables
WHERE schema_name = 'advertising';
UPDATE platform.partitioned_tables SET enabled = false WHERE schema_name = 'advertising';
