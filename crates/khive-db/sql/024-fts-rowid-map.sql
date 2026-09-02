-- V24: sidecar rowid map for the FTS5 tables' (namespace, subject_id) point
-- lookups. `namespace` and `subject_id` are UNINDEXED FTS5 columns (the
-- production tokenizer is trigram, and indexing UUID fragments would pollute
-- text queries), so a delete or get keyed on those columns forced a full
-- virtual-table scan, reading every row's body overflow pages. This map
-- turns that into a primary-key lookup instead.
--
-- Backfill keeps the highest rowid for any duplicate (namespace, subject_id)
-- pair (ORDER BY rowid ASC feeding INSERT OR REPLACE, so the last one wins):
-- legacy duplicates predate the delete-then-insert upsert being fully
-- consistent, and the newest write is the intended survivor.
--
-- Orphan sweep, after backfill, removes two classes of row:
--   1. Legacy duplicates: any FTS row whose rowid the backfill above did not
--      keep for its (namespace, subject_id) key.
--   2. Rows whose subject no longer exists in the backing table at all
--      (hard-deleted, or left over from an interrupted write that never
--      committed its row). fts_entities is backed by exactly one table
--      (entities.id) and fts_notes by exactly one (notes.id), regardless of
--      which granular `record_kind` (`concept`, `memory`, `task`, ...) a row
--      carries or whether that classifier is empty — every pack's granular
--      kinds are entity or note substrate rows in these same two tables, so
--      there is no second table this check could be guessing wrong. This is
--      also why soft-deleted subjects are NOT swept here: `deleted_at` being
--      set does not remove the row from `entities`/`notes`, so the subject
--      still "exists" by this check — hiding a soft-deleted subject from a
--      search result is a view-layer filter, never a data-layer deletion.

CREATE TABLE IF NOT EXISTS fts_entities_rowids (
    namespace  TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    rowid      INTEGER NOT NULL,
    PRIMARY KEY (namespace, subject_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS fts_notes_rowids (
    namespace  TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    rowid      INTEGER NOT NULL,
    PRIMARY KEY (namespace, subject_id)
) WITHOUT ROWID;

INSERT OR REPLACE INTO fts_entities_rowids (namespace, subject_id, rowid)
SELECT namespace, subject_id, rowid FROM fts_entities ORDER BY rowid ASC;

INSERT OR REPLACE INTO fts_notes_rowids (namespace, subject_id, rowid)
SELECT namespace, subject_id, rowid FROM fts_notes ORDER BY rowid ASC;

-- Sweep 1: legacy duplicate rows the backfill above did not keep.
DELETE FROM fts_entities
WHERE rowid NOT IN (SELECT rowid FROM fts_entities_rowids);

DELETE FROM fts_notes
WHERE rowid NOT IN (SELECT rowid FROM fts_notes_rowids);

-- Sweep 2: rows whose subject no longer exists in its backing table.
DELETE FROM fts_entities
WHERE subject_id NOT IN (SELECT id FROM entities);

DELETE FROM fts_notes
WHERE subject_id NOT IN (SELECT id FROM notes);

-- Keep the map consistent with sweep 2's removals (sweep 1's removals were
-- never in the map to begin with, by construction).
DELETE FROM fts_entities_rowids
WHERE NOT EXISTS (
    SELECT 1 FROM fts_entities WHERE fts_entities.rowid = fts_entities_rowids.rowid
);

DELETE FROM fts_notes_rowids
WHERE NOT EXISTS (
    SELECT 1 FROM fts_notes WHERE fts_notes.rowid = fts_notes_rowids.rowid
);
