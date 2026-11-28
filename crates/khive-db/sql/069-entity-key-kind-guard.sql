-- ADR-221 section 1: a key is held under its entity's kind.
--
-- A live entity holding a key may not change kind, and an entity whose keys
-- were released under another kind may not be restored, since either would
-- leave a key row naming a kind its holder no longer has. A merge moves keys
-- to the kept entity under the kept entity's kind and changes no entity's
-- kind, so it never fires this.
CREATE TRIGGER IF NOT EXISTS entity_keys_kind_guard
BEFORE UPDATE OF kind, deleted_at ON entities
WHEN NEW.deleted_at IS NULL
 AND EXISTS (
    SELECT 1 FROM entity_keys
    WHERE entity_id = NEW.id
      AND kind <> NEW.kind
      AND (released_at IS NULL OR OLD.deleted_at IS NOT NULL)
 )
BEGIN
    SELECT RAISE(ABORT, 'entity kind differs from the kind its keys are held under');
END;
