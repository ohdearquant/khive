-- V35: keep delegated unread probes inside the string-recipient partition.
-- JSON objects/arrays can have the same json_extract text as an actor label;
-- the recipient type must be a seek key, not a residual filter after LIMIT's
-- matching-row bound. Keep the older index for own/legacy mailbox predicates.
-- All key expressions and the unread partial predicate match the note filter
-- compiler; ADR-187 pins this index only for the complete typed exact shape.

CREATE INDEX IF NOT EXISTS idx_notes_unread_probe_recipient_type_direction
    ON notes(namespace, kind,
             json_type(properties, '$.to_actor'),
             ifnull(json_extract(properties, '$.to_actor'), ''),
             json_extract(properties, '$.direction'),
             created_at DESC, id ASC)
    WHERE (json_type(properties, '$.read') IS NULL
           OR json_type(properties, '$.read') != 'true')
      AND deleted_at IS NULL;
