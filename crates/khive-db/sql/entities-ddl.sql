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
    version        INTEGER NOT NULL DEFAULT 1,
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

-- The row version is independent of updated_at and cannot be caller-selected.
CREATE TRIGGER IF NOT EXISTS entities_version_insert_guard
BEFORE INSERT ON entities
WHEN typeof(NEW.version) != 'integer' OR NEW.version != 1
BEGIN
    SELECT RAISE(ABORT, 'entity insert version must be 1');
END;

CREATE TRIGGER IF NOT EXISTS entities_version_update_guard
BEFORE UPDATE ON entities
WHEN OLD.version = 9223372036854775807
  OR typeof(NEW.version) != 'integer'
  OR NEW.version != OLD.version + 1
BEGIN
    SELECT RAISE(ABORT, 'entity update version must advance by exactly one');
END;
