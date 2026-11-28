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

-- Resolve looks an alias up across the namespace's entities. The index
-- carries entity_id so the lookup reads it alone; without that column SQLite
-- prefers idx_entity_aliases_entity, which covers the lookup but is ordered by
-- entity, and walks every alias row of the namespace.
CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
    ON entity_aliases(namespace, alias, entity_id);

-- A hard delete removes the entity's alias rows with it. The namespace term
-- lets the delete seek idx_entity_aliases_entity, whose first column it is.
CREATE TRIGGER IF NOT EXISTS entity_aliases_drop_on_hard_delete
AFTER DELETE ON entities
BEGIN
    DELETE FROM entity_aliases
    WHERE namespace = OLD.namespace AND entity_id = OLD.id;
END;
