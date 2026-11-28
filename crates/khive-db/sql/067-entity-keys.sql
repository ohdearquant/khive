-- ADR-221 section 1: caller keys for entities.
--
-- A key row stays with its entity through a soft delete, marked released, so
-- a restore can take the key back. The unique index binds held rows only.
CREATE TABLE IF NOT EXISTS entity_keys (
    namespace   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    key         TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    released_at INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_keys_held
    ON entity_keys(namespace, kind, key)
    WHERE released_at IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_keys_entity
    ON entity_keys(entity_id, kind, key);

-- A soft delete releases every key the entity holds.
CREATE TRIGGER IF NOT EXISTS entity_keys_release_on_delete
AFTER UPDATE OF deleted_at ON entities
WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL
BEGIN
    UPDATE entity_keys SET released_at = NEW.deleted_at
    WHERE entity_id = NEW.id AND released_at IS NULL;
END;

-- A restore takes every released key back. A key another live entity holds
-- fails idx_entity_keys_held, which aborts the restoring statement whole.
CREATE TRIGGER IF NOT EXISTS entity_keys_reclaim_on_restore
AFTER UPDATE OF deleted_at ON entities
WHEN OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS NULL
BEGIN
    UPDATE entity_keys SET released_at = NULL
    WHERE entity_id = NEW.id AND released_at IS NOT NULL;
END;

-- A hard delete removes the entity's key rows with it.
CREATE TRIGGER IF NOT EXISTS entity_keys_drop_on_hard_delete
AFTER DELETE ON entities
BEGIN
    DELETE FROM entity_keys WHERE entity_id = OLD.id;
END;
