-- Core-table indexes are migrated independently of schedule pack loading.
CREATE INDEX IF NOT EXISTS idx_schedule_trigger
ON notes(namespace, kind, json_extract(properties, '$.trigger_at'))
WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_schedule_creator_provenance
ON events(namespace, verb, target_id, outcome);
