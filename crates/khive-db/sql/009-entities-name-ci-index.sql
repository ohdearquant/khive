-- V9: Support bounded case-insensitive entity-name candidate lookups.

CREATE INDEX IF NOT EXISTS idx_entities_namespace_name_ci
    ON entities(namespace, LOWER(name));
