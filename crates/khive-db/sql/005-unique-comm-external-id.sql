-- V5: Promote idx_comm_message_external_id to a durable UNIQUE index.
--
-- The comm pack schema plan previously applied a non-unique index under this
-- name.  On existing databases that index exists and IF NOT EXISTS would
-- silently no-op, leaving the non-unique variant in place and uniqueness
-- never enforced.  DROP INDEX IF EXISTS removes whichever variant is present
-- so the UNIQUE index can be created unconditionally.
--
-- Safety: external_id is populated only by comm.ingest (channel-ingested
-- messages).  comm.send dual-write does not set external_id, so no
-- reconciliation of pre-existing rows without external_id is needed and the
-- partial index (IS NOT NULL AND != '') creates cleanly with zero indexed
-- rows on databases that have never run the email channel.
DROP INDEX IF EXISTS idx_comm_message_external_id;
CREATE UNIQUE INDEX idx_comm_message_external_id
    ON notes(namespace, kind, json_extract(properties, '$.external_id'))
    WHERE deleted_at IS NULL
      AND json_extract(properties, '$.external_id') IS NOT NULL
      AND json_extract(properties, '$.external_id') != '';
