-- Entities table and supporting indexes.
-- Applied idempotently by StorageBackend::entities_for_namespace on every store access.

CREATE TABLE IF NOT EXISTS entities (
    id             TEXT PRIMARY KEY,
    namespace      TEXT NOT NULL,
    kind           TEXT NOT NULL,
    entity_type    TEXT,
    name           TEXT NOT NULL,
    description    TEXT,
    properties     TEXT,
    tags           TEXT NOT NULL DEFAULT '[]',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    deleted_at     INTEGER,
    merged_into    TEXT,
    merge_event_id TEXT
);

CREATE INDEX IF NOT EXISTS idx_entities_namespace ON entities(namespace);
CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(namespace, kind);
CREATE INDEX IF NOT EXISTS idx_entities_kind_entity_type ON entities(namespace, kind, entity_type);
CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(namespace, name);
CREATE INDEX IF NOT EXISTS idx_entities_created ON entities(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_entities_merged_into ON entities(namespace, merged_into);

-- Durable list-cursor insertion order. This mirrors migration V13 as a
-- belt-and-suspenders path for fresh/direct store construction that applies
-- ENTITIES_DDL without running the core migration chain. Existing databases
-- are backfilled only by V13, in operator context.
CREATE TABLE IF NOT EXISTS entities_seq (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_id TEXT NOT NULL UNIQUE
);

CREATE TRIGGER IF NOT EXISTS assign_entity_list_seq
AFTER INSERT ON entities
BEGIN
    -- Do not use legacy `OR IGNORE`: an outer `INSERT OR REPLACE` overrides
    -- that trigger policy and would reassign this immutable sequence.
    INSERT INTO entities_seq (entity_id) VALUES (NEW.id)
    ON CONFLICT(entity_id) DO NOTHING;
END;
