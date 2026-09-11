-- Recipient seeks for read/all inbox listings (#2377, #2517).
-- Match EqOrLegacyIndexed's expression exactly, including the legacy empty
-- sentinel and direction key. Do not restrict kind: callers bind it.
CREATE INDEX IF NOT EXISTS idx_notes_message_recipient_direction
    ON notes(namespace, kind,
             ifnull(json_extract(properties, '$.to_actor'), ''),
             json_extract(properties, '$.direction'),
             created_at DESC, id ASC)
    WHERE deleted_at IS NULL;

-- The same-key partial index must follow the full index in the schema catalog
-- so unread queries keep their selective plan even before ANALYZE. This rebuild
-- is versioned once, never part of the per-store idempotent bootstrap.
DROP INDEX IF EXISTS idx_notes_unread_probe_recipient_direction;
CREATE INDEX idx_notes_unread_probe_recipient_direction
    ON notes(namespace, kind,
             ifnull(json_extract(properties, '$.to_actor'), ''),
             json_extract(properties, '$.direction'),
             created_at DESC, id ASC)
    WHERE (json_type(properties, '$.read') IS NULL
           OR json_type(properties, '$.read') != 'true')
      AND deleted_at IS NULL;
