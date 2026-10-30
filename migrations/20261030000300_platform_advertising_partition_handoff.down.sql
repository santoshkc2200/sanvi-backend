UPDATE platform.partitioned_tables AS registry
SET enabled = previous.enabled
FROM platform.advertising_partition_handoff AS previous
WHERE registry.schema_name = previous.schema_name AND registry.table_name = previous.table_name;
DROP TABLE platform.advertising_partition_handoff;
