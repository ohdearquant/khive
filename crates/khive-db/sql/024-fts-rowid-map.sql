-- V24: sidecar rowid map for the FTS5 tables' (namespace, subject_id) point
-- lookups. `namespace` and `subject_id` are UNINDEXED FTS5 columns (the
-- production tokenizer is trigram, and indexing UUID fragments would pollute
-- text queries), so a delete or get keyed on those columns forced a full
-- virtual-table scan, reading every row's body overflow pages. This map
-- turns that into a primary-key lookup instead.
--
-- Rows with a NULL namespace or subject_id predate this map (FTS5 permits
-- NULL in UNINDEXED columns) and cannot be attributed to a (namespace,
-- subject_id) key at all: they are excluded from the backfill below and from
-- both sweeps, and stay reachable only via MATCH. Deleting a row the data
-- layer cannot attribute is a correctness violation, not a cleanup
-- (data-vs-view).
--
-- Backfill keeps the row with the newest `updated_at` for any duplicate
-- (namespace, subject_id) pair: `ORDER BY updated_at ASC, rowid ASC` feeds
-- `INSERT OR REPLACE`, so the last row processed wins the map entry, and a
-- duplicate rowid is not itself a write timestamp — ties on `updated_at`
-- break toward the higher rowid.
--
-- Orphan sweep, after backfill, removes two classes of row (NULL-key rows
-- excluded from both, per above):
--   1. Legacy duplicates: any FTS row whose rowid the backfill above did not
--      keep for its (namespace, subject_id) key.
--   2. Rows whose subject no longer exists in the backing table, checked
--      NULL-safely (`NOT EXISTS`, not `NOT IN`, which a single NULL `id`
--      would otherwise silence for every row) and scoped to the row's own
--      namespace (a row whose subject exists only in a DIFFERENT namespace
--      is still an orphan of this namespace's row). fts_entities is backed
--      by exactly one table (entities.id) and fts_notes by exactly one
--      (notes.id), regardless of which granular `record_kind` (`concept`,
--      `memory`, `task`, ...) a row carries or whether that classifier is
--      empty — every pack's granular kinds are entity or note substrate rows
--      in these same two tables, so there is no second table this check
--      could be guessing wrong. This is also why soft-deleted subjects are
--      NOT swept here: `deleted_at` being set does not remove the row from
--      `entities`/`notes`, so the subject still "exists" by this check —
--      hiding a soft-deleted subject from a search result is a view-layer
--      filter, never a data-layer deletion.

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
SELECT namespace, subject_id, rowid FROM fts_entities
WHERE namespace IS NOT NULL AND subject_id IS NOT NULL
ORDER BY updated_at ASC, rowid ASC;

INSERT OR REPLACE INTO fts_notes_rowids (namespace, subject_id, rowid)
SELECT namespace, subject_id, rowid FROM fts_notes
WHERE namespace IS NOT NULL AND subject_id IS NOT NULL
ORDER BY updated_at ASC, rowid ASC;

-- Sweep 1: legacy duplicate rows the backfill above did not keep. NULL-key
-- rows were never eligible for the map, so they are excluded here rather
-- than swept as unmapped duplicates.
DELETE FROM fts_entities
WHERE rowid NOT IN (SELECT rowid FROM fts_entities_rowids)
  AND namespace IS NOT NULL AND subject_id IS NOT NULL;

DELETE FROM fts_notes
WHERE rowid NOT IN (SELECT rowid FROM fts_notes_rowids)
  AND namespace IS NOT NULL AND subject_id IS NOT NULL;

-- Sweep 2: rows whose subject no longer exists in its backing table, scoped
-- to the row's own namespace and excluding NULL-key rows (never
-- attributable, so never swept).
DELETE FROM fts_entities AS f
WHERE f.namespace IS NOT NULL AND f.subject_id IS NOT NULL
  AND NOT EXISTS (
    SELECT 1 FROM entities e WHERE e.id = f.subject_id AND e.namespace = f.namespace
  );

DELETE FROM fts_notes AS f
WHERE f.namespace IS NOT NULL AND f.subject_id IS NOT NULL
  AND NOT EXISTS (
    SELECT 1 FROM notes n WHERE n.id = f.subject_id AND n.namespace = f.namespace
  );

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
