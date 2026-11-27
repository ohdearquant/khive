-- ADR-221 section 2: other exact names of entities.
--
-- An alias row stays with its entity through a soft delete; reads see live
-- entities only. The same alias may sit on several entities, so only the
-- (namespace, entity, alias) triple is unique.
CREATE TABLE IF NOT EXISTS entity_aliases (
    namespace  TEXT NOT NULL,
    entity_id  TEXT NOT NULL,
    alias      TEXT NOT NULL,
    origin     TEXT NOT NULL CHECK (origin IN ('caller', 'rename', 'merge')),
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_aliases_entity
    ON entity_aliases(namespace, entity_id, alias);

-- Resolve looks an alias up across the namespace's entities.
CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
    ON entity_aliases(namespace, alias);

-- A hard delete removes the entity's alias rows with it.
CREATE TRIGGER IF NOT EXISTS entity_aliases_drop_on_hard_delete
AFTER DELETE ON entities
BEGIN
    DELETE FROM entity_aliases WHERE entity_id = OLD.id;
END;
