-- V25: Repair the knowledge external-content FTS indexes and make atom
-- lifecycle transitions symmetric.
--
-- fts_knowledge historically named knowledge_atoms as its external content
-- table while deliberately omitting soft-deleted rows from the index. That
-- makes FTS5's rank-1 integrity check compare unlike row sets. It also made a
-- later hard delete issue a second FTS5 'delete' for a row already removed by
-- the soft-delete update trigger, which FTS5 reports as SQLITE_CORRUPT_VTAB.
--
-- A filtered view is the FTS5-supported external-content shape for this
-- contract: both the content object and the index contain exactly live atoms.
-- Recreating the virtual table also repairs stores already missing entries.

DROP TRIGGER IF EXISTS fts_knowledge_ai;
DROP TRIGGER IF EXISTS fts_knowledge_ad;
DROP TRIGGER IF EXISTS fts_knowledge_au;
DROP TABLE IF EXISTS fts_knowledge;
DROP VIEW IF EXISTS knowledge_atoms_fts_content;

CREATE VIEW knowledge_atoms_fts_content AS
SELECT rowid, id, namespace, slug, name, content
FROM knowledge_atoms
WHERE deleted_at IS NULL;

CREATE VIRTUAL TABLE fts_knowledge USING fts5(
    id        UNINDEXED,
    namespace UNINDEXED,
    slug,
    name,
    content,
    content=knowledge_atoms_fts_content,
    content_rowid=rowid,
    tokenize='trigram case_sensitive 0'
);

CREATE TRIGGER fts_knowledge_ai
AFTER INSERT ON knowledge_atoms WHEN new.deleted_at IS NULL BEGIN
    INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, content)
        VALUES (new.rowid, new.id, new.namespace, new.slug, new.name, new.content);
END;

CREATE TRIGGER fts_knowledge_ad
AFTER DELETE ON knowledge_atoms WHEN old.deleted_at IS NULL BEGIN
    INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, content)
        VALUES ('delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.content);
END;

CREATE TRIGGER fts_knowledge_au
AFTER UPDATE OF id, namespace, slug, name, content, deleted_at ON knowledge_atoms BEGIN
    INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, content)
        SELECT 'delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.content
        WHERE old.deleted_at IS NULL;
    INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, content)
        SELECT new.rowid, new.id, new.namespace, new.slug, new.name, new.content
        WHERE new.deleted_at IS NULL;
END;

INSERT INTO fts_knowledge(fts_knowledge) VALUES ('rebuild');

-- V2 stopped embedding-only UPDATE churn, but changing a trigger cannot heal
-- divergence that already exists. Rebuild the section index from its complete
-- external-content table so old stores and pre-trigger backfills converge.
INSERT INTO fts_sections(fts_sections) VALUES ('rebuild');
