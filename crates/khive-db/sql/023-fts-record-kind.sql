-- V23: index the granular source-record kind inside the shared FTS tables.
-- The copy preserves every indexed row, including stale rows whose base record
-- no longer exists; those rows receive an empty classifier and remain visible
-- to unscoped text searches exactly as before.

CREATE VIRTUAL TABLE fts_entities_v23 USING fts5(
    subject_id UNINDEXED,
    kind UNINDEXED,
    title,
    body,
    tags UNINDEXED,
    namespace UNINDEXED,
    metadata UNINDEXED,
    updated_at UNINDEXED,
    record_kind,
    tokenize = 'trigram'
);

INSERT INTO fts_entities_v23(
    subject_id,
    kind,
    title,
    body,
    tags,
    namespace,
    metadata,
    updated_at,
    record_kind
)
SELECT
    f.subject_id,
    f.kind,
    f.title,
    f.body,
    f.tags,
    f.namespace,
    f.metadata,
    f.updated_at,
    coalesce(e.kind, '')
FROM fts_entities AS f
LEFT JOIN entities AS e
    ON e.id = f.subject_id
   AND e.deleted_at IS NULL;

DROP TABLE fts_entities;
ALTER TABLE fts_entities_v23 RENAME TO fts_entities;

CREATE VIRTUAL TABLE fts_notes_v23 USING fts5(
    subject_id UNINDEXED,
    kind UNINDEXED,
    title,
    body,
    tags UNINDEXED,
    namespace UNINDEXED,
    metadata UNINDEXED,
    updated_at UNINDEXED,
    record_kind,
    tokenize = 'trigram'
);

INSERT INTO fts_notes_v23(
    subject_id,
    kind,
    title,
    body,
    tags,
    namespace,
    metadata,
    updated_at,
    record_kind
)
SELECT
    f.subject_id,
    f.kind,
    f.title,
    f.body,
    f.tags,
    f.namespace,
    f.metadata,
    f.updated_at,
    coalesce(n.kind, '')
FROM fts_notes AS f
LEFT JOIN notes AS n
    ON n.id = f.subject_id
   AND n.deleted_at IS NULL;

DROP TABLE fts_notes;
ALTER TABLE fts_notes_v23 RENAME TO fts_notes;
