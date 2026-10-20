-- ADR-014 Amendment 7: one obligation per entity in the domain store.
CREATE TABLE pending_entity_indexes (
    entity_id         TEXT PRIMARY KEY NOT NULL,
    namespace         TEXT NOT NULL,
    source_version    INTEGER NOT NULL CHECK (typeof(source_version) = 'integer' AND source_version > 0),
    generation        TEXT NOT NULL UNIQUE,
    first_pending_at  INTEGER NOT NULL CHECK (typeof(first_pending_at) = 'integer'),
    targets_json      TEXT NOT NULL CHECK (json_valid(targets_json) AND json_type(targets_json) = 'array')
);

-- This applies to every entity DELETE, including writers with foreign_keys OFF.
CREATE TRIGGER entities_delete_pending_indexes
AFTER DELETE ON entities
BEGIN
    DELETE FROM pending_entity_indexes WHERE entity_id = OLD.id;
END;
