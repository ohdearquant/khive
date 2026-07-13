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
    merge_event_id TEXT,
    content_ref    TEXT
);

CREATE INDEX IF NOT EXISTS idx_entities_namespace ON entities(namespace);
CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(namespace, kind);
CREATE INDEX IF NOT EXISTS idx_entities_kind_entity_type ON entities(namespace, kind, entity_type);
CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(namespace, name);
CREATE INDEX IF NOT EXISTS idx_entities_created ON entities(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_entities_merged_into ON entities(namespace, merged_into);
-- BlobStore content_ref reference (khive#292) — mirrors migration 010 for
-- callers that create the table via ensure_entities_schema() without ever
-- running the versioned migration chain (e.g. StorageBackend::memory() test
-- setups that apply ENTITIES_DDL directly).
CREATE INDEX IF NOT EXISTS idx_entities_content_ref ON entities(content_ref) WHERE content_ref IS NOT NULL;
