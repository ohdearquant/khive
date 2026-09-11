-- ADR-179: operation identity for keyed memory creation.
ALTER TABLE notes ADD COLUMN key TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_namespace_kind_key
    ON notes(namespace, kind, key)
    WHERE key IS NOT NULL AND deleted_at IS NULL;
