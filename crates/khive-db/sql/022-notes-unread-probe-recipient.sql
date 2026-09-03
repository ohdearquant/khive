-- V22: index unread notes by recipient for exact comm inbox counts.
--
-- The fresh-database baseline carries this index in schema.sql. This numbered
-- migration upgrades databases that already recorded V21 before the index was
-- introduced, including snapshots later opened through a read-only backend.

DROP INDEX IF EXISTS idx_notes_unread_probe;

CREATE INDEX IF NOT EXISTS idx_notes_unread_probe_recipient
    ON notes(namespace, kind,
             ifnull(json_extract(properties, '$.to_actor'), ''),
             created_at DESC, id ASC)
    WHERE (json_type(properties, '$.read') IS NULL
           OR json_type(properties, '$.read') != 'true')
      AND deleted_at IS NULL;
