-- V5: Promote idx_comm_message_external_id to a durable UNIQUE index.
--
-- The comm pack schema plan previously applied a non-unique index under this
-- name.  On existing databases that index exists and IF NOT EXISTS would
-- silently no-op, leaving the non-unique variant in place and uniqueness
-- never enforced.  DROP INDEX IF EXISTS removes whichever variant is present
-- so the UNIQUE index can be created unconditionally.
--
-- Duplicate-row reconciliation (safe on production databases):
-- Before creating the UNIQUE index, locate all groups of notes that share the
-- same (namespace, kind, external_id) and would violate the new constraint.
-- For each such group the earliest row by rowid is the canonical record and
-- keeps its external_id unchanged.  Every later duplicate has its external_id
-- cleared via json_remove, which makes json_extract return NULL so the partial
-- index WHERE clause excludes those rows.  The message body and all other
-- properties are preserved; only the redundant dedup key is cleared.
-- Rows that already have a NULL or empty external_id are unaffected.
UPDATE notes
SET properties = json_remove(properties, '$.external_id')
WHERE rowid NOT IN (
    SELECT MIN(rowid)
    FROM notes
    WHERE deleted_at IS NULL
      AND json_extract(properties, '$.external_id') IS NOT NULL
      AND json_extract(properties, '$.external_id') != ''
    GROUP BY namespace, kind, json_extract(properties, '$.external_id')
)
  AND deleted_at IS NULL
  AND json_extract(properties, '$.external_id') IS NOT NULL
  AND json_extract(properties, '$.external_id') != '';

DROP INDEX IF EXISTS idx_comm_message_external_id;
CREATE UNIQUE INDEX idx_comm_message_external_id
    ON notes(namespace, kind, json_extract(properties, '$.external_id'))
    WHERE deleted_at IS NULL
      AND json_extract(properties, '$.external_id') IS NOT NULL
      AND json_extract(properties, '$.external_id') != '';
