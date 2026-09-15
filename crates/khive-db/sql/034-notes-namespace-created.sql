-- ADR-186: serve live note pages in their stable creation order.
CREATE INDEX IF NOT EXISTS idx_notes_namespace_created
    ON notes(namespace, created_at DESC, id ASC)
    WHERE deleted_at IS NULL;
