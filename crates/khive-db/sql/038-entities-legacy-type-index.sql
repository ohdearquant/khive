CREATE INDEX IF NOT EXISTS idx_entities_legacy_type
    ON entities(namespace, kind, json_extract(properties, '$.type'))
    WHERE entity_type IS NULL;
